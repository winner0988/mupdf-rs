use std::env::{self, current_dir};
use std::error::Error;
use std::ffi::OsStr;
use std::fs::remove_dir_all;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::{fs, result};

mod docs;
use docs::DocsCallbacks;

mod make;
use make::Make;

mod msbuild;
use msbuild::Msbuild;

pub type Result<T> = result::Result<T, Box<dyn Error>>;

fn main() {
    if let Err(e) = run() {
        eprintln!("\n{e}");
        exit(1);
    }
}

fn run() -> Result<()> {
    if fs::read_dir("mupdf").map_or(true, |d| d.count() == 0) {
        Err(
            "The `mupdf` directory is empty, did you forget to pull the submodules?\n\
            Try `git submodule update --init --recursive`",
        )?
    }

    let target = Target::from_cargo().map_err(|e| {
        format!(
            "Unable to detect target: {e}\n\
            Cargo is required to build mupdf"
        )
    })?;

    let src_dir = current_dir().unwrap().join("mupdf");
    let out_dir =
        PathBuf::from(env::var_os("OUT_DIR").ok_or("Missing OUT_DIR environment variable")?);

    let sysroot = find_clang_sysroot(&target)?;

    let docs = env::var_os("DOCS_RS").is_some();
    if !docs {
        let build_dir = out_dir.join("build");
        let build_dir = build_dir.to_str().ok_or_else(|| {
            format!("Build dir path is required to be valid UTF-8, got {build_dir:?}")
        })?;

        if let Err(e) = remove_dir_all(build_dir) {
            if e.kind() != ErrorKind::NotFound {
                println!("cargo:warning=Unable to clear {build_dir:?}. This may lead to flaky builds that might not incorporate configurations changes: {e}");
            }
        }

        copy_recursive(&src_dir, build_dir.as_ref(), &[".git".as_ref()])?;
        patch_mupdf_sources(build_dir.as_ref())?;

        Build::new(&target).run(&target, build_dir)?;
        build_wrapper().map_err(|e| format!("Unable to compile mupdf wrapper:\n  {e}"))?;
    }

    generate_bindings(&target, &out_dir.join("bindings.rs"), sysroot)
        .map_err(|e| format!("Unable to generate mupdf bindings using bindgen:\n  {e}"))?;

    Ok(())
}

fn copy_recursive(src: &Path, dst: &Path, ignore: &[&OsStr]) -> Result<()> {
    if let Err(e) = fs::create_dir(dst) {
        if e.kind() != ErrorKind::AlreadyExists {
            Err(format!("Unable to create {dst:?}: {e}"))?;
        }
    }

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        if ignore.contains(&&*entry.file_name()) {
            continue;
        }

        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        let file_type = entry.file_type()?;

        #[cfg(any(unix, windows))]
        if file_type.is_symlink() {
            let link = fs::read_link(&src_path)
                .map_err(|e| format!("Couldn't read symlink {src_path:?}: {e}"))?;

            #[cfg(unix)]
            let err = std::os::unix::fs::symlink(&link, &dst_path);
            #[cfg(windows)]
            let err = if file_type.is_dir() {
                std::os::windows::fs::symlink_dir(&link, &dst_path)
            } else {
                std::os::windows::fs::symlink_file(&link, &dst_path)
            };

            match err {
                Ok(_) => continue,
                Err(e) => println!(
                    "cargo:warning=Couldn't create symlink {dst_path:?} pointing to {link:?}. This might increase the size of your target folder: {e}"
                ),
            }
        }

        if file_type.is_file() || fs::metadata(&src_path)?.is_file() {
            fs::copy(&src_path, &dst_path)
                .map_err(|e| format!("Couldn't copy {src_path:?} to {dst_path:?}: {e}"))?;
        } else {
            copy_recursive(&src_path, &dst_path, ignore)?;
        }
    }
    Ok(())
}

fn patch_mupdf_sources(build_dir: &Path) -> Result<()> {
    // MuPDF's special math/music/symbol/emoji fallbacks only consult compiled-in
    // Noto resources. When those resources are omitted to keep mupdf-sys small,
    // give the safe crate's runtime bundled-font/system-font hook a chance to
    // provide the same special fallback fonts by name.
    let path = build_dir.join("source/fitz/font.c");
    let mut source =
        fs::read_to_string(&path).map_err(|e| format!("Unable to read {}: {e}", path.display()))?;
    source = source.replace("\r\n", "\n");

    let patches = [
        (
            "Noto Sans Math",
            "\t\tif (data)\n\t\t\tctx->font->math = fz_new_font_from_memory(ctx, NULL, data, size, 0, 0);\n",
            "\t\tif (data)\n\t\t\tctx->font->math = fz_new_font_from_memory(ctx, NULL, data, size, 0, 0);\n\t\telse\n\t\t\tctx->font->math = fz_load_system_font(ctx, \"Noto Sans Math\", 0, 0, 0);\n",
        ),
        (
            "Noto Music",
            "\t\tif (data)\n\t\t\tctx->font->music = fz_new_font_from_memory(ctx, NULL, data, size, 0, 0);\n",
            "\t\tif (data)\n\t\t\tctx->font->music = fz_new_font_from_memory(ctx, NULL, data, size, 0, 0);\n\t\telse\n\t\t\tctx->font->music = fz_load_system_font(ctx, \"Noto Music\", 0, 0, 0);\n",
        ),
        (
            "Noto Sans Symbols",
            "\t\tif (data)\n\t\t\tctx->font->symbol1 = fz_new_font_from_memory(ctx, NULL, data, size, 0, 0);\n",
            "\t\tif (data)\n\t\t\tctx->font->symbol1 = fz_new_font_from_memory(ctx, NULL, data, size, 0, 0);\n\t\telse\n\t\t\tctx->font->symbol1 = fz_load_system_font(ctx, \"Noto Sans Symbols\", 0, 0, 0);\n",
        ),
        (
            "Noto Sans Symbols 2",
            "\t\tif (data)\n\t\t\tctx->font->symbol2 = fz_new_font_from_memory(ctx, NULL, data, size, 0, 0);\n",
            "\t\tif (data)\n\t\t\tctx->font->symbol2 = fz_new_font_from_memory(ctx, NULL, data, size, 0, 0);\n\t\telse\n\t\t\tctx->font->symbol2 = fz_load_system_font(ctx, \"Noto Sans Symbols 2\", 0, 0, 0);\n",
        ),
        (
            "Noto Emoji",
            "\t\tif (data)\n\t\t\tctx->font->emoji = fz_new_font_from_memory(ctx, NULL, data, size, 0, 0);\n",
            "\t\tif (data)\n\t\t\tctx->font->emoji = fz_new_font_from_memory(ctx, NULL, data, size, 0, 0);\n\t\telse\n\t\t\tctx->font->emoji = fz_load_system_font(ctx, \"Noto Emoji\", 0, 0, 0);\n",
        ),
    ];

    for (font_name, old, new) in patches {
        if source.contains(new) {
            continue;
        }
        if !source.contains(old) {
            println!(
                "cargo:warning=Unable to patch {} special fallback in {}; expected MuPDF font fallback snippet was not found",
                font_name,
                path.display()
            );
            continue;
        }
        source = source.replacen(old, new, 1);
    }

    fs::write(&path, source).map_err(|e| format!("Unable to write {}: {e}", path.display()))?;

    // fz_filter_store (used by pdf_empty_store when a document is dropped)
    // releases the values of removed items by decrementing their refcount
    // directly instead of going through fz_drop_key_storable. For key storable
    // values such as fz_image the store therefore never notices that the only
    // remaining references are store key references, so cached decoded pixmaps
    // keyed on those images survive document destruction and accumulate
    // indefinitely in long-running processes
    // (https://github.com/messense/mupdf-rs/issues/90).
    // Flag (or run) a reap pass after filtering so such entries are collected.
    let path = build_dir.join("source/fitz/store.c");
    let mut source =
        fs::read_to_string(&path).map_err(|e| format!("Unable to read {}: {e}", path.display()))?;
    source = source.replace("\r\n", "\n");

    let old = "\tfz_unlock(ctx, FZ_LOCK_ALLOC);\n\n\t/* Now drop the remove chain */\n\tfor (item = remove; item != NULL; item = remove)\n\t{\n\t\tremove = item->next;\n\n\t\t/* Drop a reference to the value (freeing if required) */\n\t\tif (item->prev) /* See above for our abuse of prev here */\n";
    let new = "\t/* The value refs dropped above bypass fz_drop_key_storable, so key\n\t * storables (e.g. fz_image) whose remaining references are all store\n\t * key references would never be reaped. Flag or run a reap pass so\n\t * the store entries keyed on them get collected. */\n\tif (remove != NULL && store->defer_reap_count > 0)\n\t{\n\t\tstore->needs_reaping = 1;\n\t\tfz_unlock(ctx, FZ_LOCK_ALLOC);\n\t}\n\telse if (remove != NULL)\n\t\tdo_reap(ctx); /* Drops FZ_LOCK_ALLOC */\n\telse\n\t\tfz_unlock(ctx, FZ_LOCK_ALLOC);\n\n\t/* Now drop the remove chain */\n\tfor (item = remove; item != NULL; item = remove)\n\t{\n\t\tremove = item->next;\n\n\t\t/* Drop a reference to the value (freeing if required) */\n\t\tif (item->prev) /* See above for our abuse of prev here */\n";
    if !source.contains(new) {
        if source.contains(old) {
            source = source.replacen(old, new, 1);
            fs::write(&path, source)
                .map_err(|e| format!("Unable to write {}: {e}", path.display()))?;
        } else {
            println!(
                "cargo:warning=Unable to patch fz_filter_store reaping in {}; expected MuPDF store snippet was not found",
                path.display()
            );
        }
    }

    // The OpenJPEG and lcms2 fixes are already in the versions bundled with MuPDF 1.28.6.
    // Tesseract 5.5.2 lacks bounds checking on count and length fields read
    // from traineddata files before allocating or indexing arrays, as well as
    // LSTM gate matrix and recoder sanity checks. Fixed upstream in
    // tesseract-ocr/tesseract:
    // - 82727cc11c34eaf1249af002d69f6bbae70993b9
    // - 55287a94b8044c05ce3fd10f5aca6ebbd238e518
    // - 103dc134eb36411ddc6833ec20aa2c76795bd0ff (adds WeightMatrix::Dim1()/Dim2() needed by b494ac18)
    // - b494ac18925f9d9aff9ef5815475de9943ab19bf
    // - c94a5532ee04db5a4919542832fd94caee5ea58f
    // These are those changes, word for word. A build that can apply neither
    // them nor find them already applied fails: silently building without the
    // checks is worse than not building.
    let tesseract_patches: &[(&str, &str, &str)] = &[
        (
            "thirdparty/tesseract/src/ccstruct/fontinfo.cpp",
            "  if (!f->DeSerialize(&size)) {\n    return false;\n  }\n  char *font_name = new char[size + 1];\n  fi->name = font_name;\n  if (!f->DeSerialize(font_name, size)) {",
            "  if (!f->DeSerialize(&size)) {\n    return false;\n  }\n  // Reject unreasonably large font names to prevent overflow in size + 1.\n  if (size > 100000) {\n    return false;\n  }\n  char *font_name = new char[size + 1];\n  fi->name = font_name;\n  if (!f->DeSerialize(font_name, size)) {",
        ),
        (
            "thirdparty/tesseract/src/ccstruct/fontinfo.cpp",
            "}\n\nbool read_spacing_info(TFile *f, FontInfo *fi) {\n  int32_t vec_size, kern_size;\n  if (!f->DeSerialize(&vec_size)) {\n    return false;\n  }\n  ASSERT_HOST(vec_size >= 0);\n  if (vec_size == 0) {\n    return true;\n  }\n  fi->init_spacing(vec_size);\n  for (int i = 0; i < vec_size; ++i) {\n    auto *fs = new FontSpacingInfo();\n    if (!f->DeSerialize(&fs->x_gap_before) || !f->DeSerialize(&fs->x_gap_after) ||\n        !f->DeSerialize(&kern_size)) {",
            "}\n\nbool read_spacing_info(TFile *f, FontInfo *fi) {\n  uint32_t vec_size;\n  int32_t kern_size;\n  if (!f->DeSerialize(&vec_size)) {\n    return false;\n  }\n  if (vec_size == 0) {\n    return true;\n  }\n  fi->init_spacing(vec_size);\n  for (uint32_t i = 0; i < vec_size; ++i) {\n    auto *fs = new FontSpacingInfo();\n    if (!f->DeSerialize(&fs->x_gap_before) || !f->DeSerialize(&fs->x_gap_after) ||\n        !f->DeSerialize(&kern_size)) {",
        ),
        (
            "thirdparty/tesseract/src/ccstruct/fontinfo.h",
            "  bool DeSerialize(TFile *fp);\n\n  // Reserves unicharset_size spots in spacing_vec.\n  void init_spacing(int unicharset_size) {\n    spacing_vec = new std::vector<FontSpacingInfo *>(unicharset_size);\n  }\n  // Adds the given pointer to FontSpacingInfo to spacing_vec member",
            "  bool DeSerialize(TFile *fp);\n\n  // Reserves unicharset_size spots in spacing_vec.\n  void init_spacing(uint32_t unicharset_size) {\n    spacing_vec = new std::vector<FontSpacingInfo *>(unicharset_size);\n  }\n  // Adds the given pointer to FontSpacingInfo to spacing_vec member",
        ),
        (
            "thirdparty/tesseract/src/ccutil/bitvector.cpp",
            "  if (swap) {\n    ReverseN(&new_bit_size, sizeof(new_bit_size));\n  }\n  Alloc(new_bit_size);\n  int wordlen = WordLength();\n  if (!tesseract::DeSerialize(fp, &array_[0], wordlen)) {",
            "  if (swap) {\n    ReverseN(&new_bit_size, sizeof(new_bit_size));\n  }\n  // Reject unreasonably large bit vectors.\n  if (new_bit_size > 500000000) {\n    return false;\n  }\n  Alloc(new_bit_size);\n  int wordlen = WordLength();\n  if (!tesseract::DeSerialize(fp, &array_[0], wordlen)) {",
        ),
        (
            "thirdparty/tesseract/src/ccutil/serialis.cpp",
            "  if (!DeSerialize(&len)) {\n    return false;\n  }\n  return Skip(len * size);\n}\n\nbool TFile::DeSerialize(std::string &data) {\n  uint32_t size;\n  if (!DeSerialize(&size)) {\n    return false;\n  } else if (size > 0) {\n    // TODO: optimize.\n    data.resize(size);",
            "  if (!DeSerialize(&len)) {\n    return false;\n  }\n  // Check for overflow: len * size must not overflow size_t.\n  if (size != 0 && len > SIZE_MAX / size) {\n    return false;\n  }\n  return Skip(len * size);\n}\n\nbool TFile::DeSerialize(std::string &data) {\n  uint32_t size;\n  if (!DeSerialize(&size)) {\n    return false;\n  } else if (size > 50000000) {\n    // Arbitrarily limit the size to protect against bad data.\n    return false;\n  } else if (size > 0) {\n    // TODO: optimize.\n    data.resize(size);",
        ),
        (
            "thirdparty/tesseract/src/ccutil/serialis.cpp",
            "bool TFile::DeSerialize(std::vector<char> &data) {\n  uint32_t size;\n  if (!DeSerialize(&size)) {\n    return false;\n  } else if (size > 0) {\n    // TODO: optimize.\n    data.resize(size);",
            "bool TFile::DeSerialize(std::vector<char> &data) {\n  uint32_t size;\n  if (!DeSerialize(&size)) {\n    return false;\n  } else if (size > 50000000) {\n    // Arbitrarily limit the size to protect against bad data.\n    return false;\n  } else if (size > 0) {\n    // TODO: optimize.\n    data.resize(size);",
        ),
        (
            "thirdparty/tesseract/src/ccutil/tessdatamanager.cpp",
            "  }\n  for (unsigned i = 0; i < num_entries && i < TESSDATA_NUM_ENTRIES; ++i) {\n    if (offset_table[i] >= 0) {\n      int64_t entry_size = size - offset_table[i];\n      unsigned j = i + 1;\n      while (j < num_entries && offset_table[j] == -1) {\n        ++j;\n      }\n      if (j < num_entries) {\n        entry_size = offset_table[j] - offset_table[i];\n      }\n      entries_[i].resize(entry_size);\n      if (!fp.DeSerialize(&entries_[i][0], entry_size)) {\n        return false;",
            "  }\n  for (unsigned i = 0; i < num_entries && i < TESSDATA_NUM_ENTRIES; ++i) {\n    if (offset_table[i] >= 0) {\n      if (offset_table[i] > size) {\n        return false;\n      }\n      int64_t entry_size = size - offset_table[i];\n      unsigned j = i + 1;\n      while (j < num_entries && offset_table[j] == -1) {\n        ++j;\n      }\n      if (j < num_entries) {\n        if (offset_table[j] < 0 || offset_table[j] > size) {\n          return false;\n        }\n        entry_size = offset_table[j] - offset_table[i];\n      }\n      if (entry_size < 0) {\n        return false;\n      }\n      entries_[i].resize(entry_size);\n      if (!fp.DeSerialize(&entries_[i][0], entry_size)) {\n        return false;",
        ),
        (
            "thirdparty/tesseract/src/ccutil/unicharcompress.h",
            "    length_ = length;\n  }\n  // Sets the code value at the given index in the code.\n  void Set(int index, int value) {\n    code_[index] = value;\n    if (length_ <= index) {\n      length_ = index + 1;",
            "    length_ = length;\n  }\n  // Sets the code value at the given index in the code.\n  void Set(uint32_t index, int value) {\n    code_[index] = value;\n    if (length_ <= index) {\n      length_ = index + 1;",
        ),
        (
            "thirdparty/tesseract/src/ccutil/unicharcompress.h",
            "    return length_ == 0;\n  }\n  // Accessors\n  int length() const {\n    return length_;\n  }\n  int operator()(int index) const {",
            "    return length_ == 0;\n  }\n  // Accessors\n  uint32_t length() const {\n    return length_;\n  }\n  int operator()(int index) const {",
        ),
        (
            "thirdparty/tesseract/src/ccutil/unicharcompress.h",
            "  }\n  // Reads from the given file. Returns false in case of error.\n  bool DeSerialize(TFile *fp) {\n    return fp->DeSerialize(&self_normalized_) && fp->DeSerialize(&length_) &&\n           fp->DeSerialize(&code_[0], length_);\n  }\n  bool operator==(const RecodedCharID &other) const {\n    if (length_ != other.length_) {\n      return false;\n    }\n    for (int i = 0; i < length_; ++i) {\n      if (code_[i] != other.code_[i]) {\n        return false;\n      }",
            "  }\n  // Reads from the given file. Returns false in case of error.\n  bool DeSerialize(TFile *fp) {\n    if (!fp->DeSerialize(&self_normalized_) || !fp->DeSerialize(&length_)) {\n      return false;\n    }\n    if (length_ > kMaxCodeLen) {\n      return false;\n    }\n    return fp->DeSerialize(&code_[0], length_);\n  }\n  bool operator==(const RecodedCharID &other) const {\n    if (length_ != other.length_) {\n      return false;\n    }\n    for (uint32_t i = 0; i < length_; ++i) {\n      if (code_[i] != other.code_[i]) {\n        return false;\n      }",
        ),
        (
            "thirdparty/tesseract/src/ccutil/unicharcompress.h",
            "  struct RecodedCharIDHash {\n    uint64_t operator()(const RecodedCharID &code) const {\n      uint64_t result = 0;\n      for (int i = 0; i < code.length_; ++i) {\n        result ^= static_cast<uint64_t>(code(i)) << (7 * i);\n      }\n      return result;",
            "  struct RecodedCharIDHash {\n    uint64_t operator()(const RecodedCharID &code) const {\n      uint64_t result = 0;\n      for (uint32_t i = 0; i < code.length_; ++i) {\n        result ^= static_cast<uint64_t>(code(i)) << (7 * i);\n      }\n      return result;",
        ),
        (
            "thirdparty/tesseract/src/ccutil/unicharcompress.h",
            "  // that map to the same code. Has boolean value, but int8_t for serialization.\n  int8_t self_normalized_;\n  // The number of elements in use in code_;\n  int32_t length_;\n  // The re-encoded form of the unichar-id to which this RecodedCharID relates.\n  int32_t code_[kMaxCodeLen];\n};",
            "  // that map to the same code. Has boolean value, but int8_t for serialization.\n  int8_t self_normalized_;\n  // The number of elements in use in code_;\n  uint32_t length_;\n  // The re-encoded form of the unichar-id to which this RecodedCharID relates.\n  int32_t code_[kMaxCodeLen];\n};",
        ),
        (
            "thirdparty/tesseract/src/dict/dawg.cpp",
            "            (!word_end || end_of_word_from_edge_rec(edges_[edge]))) {\n          return (edge);\n        }\n      } while (!last_edge(edge++));\n    }\n  }\n  return (NO_EDGE); // not found",
            "            (!word_end || end_of_word_from_edge_rec(edges_[edge]))) {\n          return (edge);\n        }\n        if (last_edge(edge)) {\n          break;\n        }\n        ++edge;\n      } while (edge < num_edges_);\n    }\n  }\n  return (NO_EDGE); // not found",
        ),
        (
            "thirdparty/tesseract/src/dict/dawg.cpp",
            "  if (forward_edge(edge)) {\n    do {\n      num++;\n    } while (!last_edge(edge++));\n  }\n\n  return (num);",
            "  if (forward_edge(edge)) {\n    do {\n      num++;\n      if (last_edge(edge)) {\n        break;\n      }\n      ++edge;\n    } while (edge < num_edges_);\n  }\n\n  return (num);",
        ),
        (
            "thirdparty/tesseract/src/dict/dawg.cpp",
            "      if (edge - node > max_num_edges) {\n        return;\n      }\n    } while (!last_edge(edge++));\n\n    if (edge < num_edges_ && edge_occupied(edge) && backward_edge(edge)) {\n      do {",
            "      if (edge - node > max_num_edges) {\n        return;\n      }\n      if (last_edge(edge)) {\n        break;\n      }\n      ++edge;\n    } while (edge < num_edges_);\n\n    if (edge < num_edges_ && edge_occupied(edge) && backward_edge(edge)) {\n      do {",
        ),
        (
            "thirdparty/tesseract/src/dict/dawg.cpp",
            "        if (edge - node > MAX_NODE_EDGES_DISPLAY) {\n          return;\n        }\n      } while (!last_edge(edge++));\n    }\n  } else {\n    tprintf(REFFORMAT \" : no edges in this node\\n\", node);",
            "        if (edge - node > MAX_NODE_EDGES_DISPLAY) {\n          return;\n        }\n        if (last_edge(edge)) {\n          break;\n        }\n        ++edge;\n      } while (edge < num_edges_);\n    }\n  } else {\n    tprintf(REFFORMAT \" : no edges in this node\\n\", node);",
        ),
        (
            "thirdparty/tesseract/src/dict/dawg.cpp",
            "    return false;\n  }\n\n  int32_t unicharset_size;\n  if (!file->DeSerialize(&unicharset_size)) {\n    return false;\n  }\n  if (!file->DeSerialize(&num_edges_)) {\n    return false;\n  }\n  ASSERT_HOST(num_edges_ > 0); // DAWG should not be empty\n  Dawg::init(unicharset_size);\n\n  edges_ = new EDGE_RECORD[num_edges_];\n  if (!file->DeSerialize(&edges_[0], num_edges_)) {\n    return false;\n  }\n  if (debug_level_ > 2) {\n    tprintf(\"type: %d lang: %s perm: %d unicharset_size: %d num_edges: %d\\n\",\n            type_, lang_.c_str(), perm_, unicharset_size_, num_edges_);\n    for (EDGE_REF edge = 0; edge < num_edges_; ++edge) {\n      print_edge(edge);",
            "    return false;\n  }\n\n  uint32_t unicharset_size;\n  if (!file->DeSerialize(&unicharset_size)) {\n    return false;\n  }\n  if (!file->DeSerialize(&num_edges_)) {\n    return false;\n  }\n  if (num_edges_ == 0) {\n    tprintf(\"Empty dawg: num_edges is 0\\n\");\n    return false;\n  }\n  Dawg::init(unicharset_size);\n\n  edges_ = new EDGE_RECORD[num_edges_];\n  if (!file->DeSerialize(&edges_[0], num_edges_)) {\n    return false;\n  }\n  if (debug_level_ > 2) {\n    tprintf(\"type: %d lang: %s perm: %d unicharset_size: %d num_edges: %\" PRIu32 \"\\n\",\n            type_, lang_.c_str(), perm_, unicharset_size_, num_edges_);\n    for (EDGE_REF edge = 0; edge < num_edges_; ++edge) {\n      print_edge(edge);",
        ),
        (
            "thirdparty/tesseract/src/dict/dawg.cpp",
            "        break;\n      }\n      if (backward_edge(edge)) {\n        while (!last_edge(edge++)) {\n          ;\n        }\n      }\n      edge--;",
            "        break;\n      }\n      if (backward_edge(edge)) {\n        while (edge < num_edges_ && !last_edge(edge)) {\n          ++edge;\n        }\n        if (edge < num_edges_) {\n          ++edge; // Skip past the last backward edge.\n        }\n      }\n      edge--;",
        ),
        (
            "thirdparty/tesseract/src/dict/dawg.h",
            "    ASSERT_HOST(read_squished_dawg(&file));\n    num_forward_edges_in_node0 = num_forward_edges(0);\n  }\n  SquishedDawg(EDGE_ARRAY edges, int num_edges, DawgType type,\n               const std::string &lang, PermuterType perm, int unicharset_size,\n               int debug_level)\n      : Dawg(type, lang, perm, debug_level),",
            "    ASSERT_HOST(read_squished_dawg(&file));\n    num_forward_edges_in_node0 = num_forward_edges(0);\n  }\n  SquishedDawg(EDGE_ARRAY edges, uint32_t num_edges, DawgType type,\n               const std::string &lang, PermuterType perm, int unicharset_size,\n               int debug_level)\n      : Dawg(type, lang, perm, debug_level),",
        ),
        (
            "thirdparty/tesseract/src/dict/dawg.h",
            "    return true;\n  }\n\n  int NumEdges() {\n    return num_edges_;\n  }\n",
            "    return true;\n  }\n\n  uint32_t NumEdges() const {\n    return num_edges_;\n  }\n",
        ),
        (
            "thirdparty/tesseract/src/dict/dawg.h",
            "      if (!word_end || end_of_word_from_edge_rec(edges_[edge])) {\n        vec->push_back(NodeChild(unichar_id_from_edge_rec(edges_[edge]), edge));\n      }\n    } while (!last_edge(edge++));\n  }\n\n  /// Returns the next node visited by following the edge",
            "      if (!word_end || end_of_word_from_edge_rec(edges_[edge])) {\n        vec->push_back(NodeChild(unichar_id_from_edge_rec(edges_[edge]), edge));\n      }\n      if (last_edge(edge)) {\n        break;\n      }\n      ++edge;\n    } while (edge < num_edges_);\n  }\n\n  /// Returns the next node visited by following the edge",
        ),
        (
            "thirdparty/tesseract/src/dict/dawg.h",
            "  }\n  /// Goes through all the edges and clears each one out.\n  inline void clear_all_edges() {\n    for (int edge = 0; edge < num_edges_; edge++) {\n      set_empty_edge(edge);\n    }\n  }",
            "  }\n  /// Goes through all the edges and clears each one out.\n  inline void clear_all_edges() {\n    for (uint32_t edge = 0; edge < num_edges_; edge++) {\n      set_empty_edge(edge);\n    }\n  }",
        ),
        (
            "thirdparty/tesseract/src/dict/dawg.h",
            "  /// Prints the contents of the SquishedDawg.\n  void print_all(const char *msg) {\n    tprintf(\"\\n__________________________\\n%s\\n\", msg);\n    for (int i = 0; i < num_edges_; ++i) {\n      print_edge(i);\n    }\n    tprintf(\"__________________________\\n\");",
            "  /// Prints the contents of the SquishedDawg.\n  void print_all(const char *msg) {\n    tprintf(\"\\n__________________________\\n%s\\n\", msg);\n    for (uint32_t i = 0; i < num_edges_; ++i) {\n      print_edge(i);\n    }\n    tprintf(\"__________________________\\n\");",
        ),
        (
            "thirdparty/tesseract/src/dict/dawg.h",
            "\n  // Member variables.\n  EDGE_ARRAY edges_ = nullptr;\n  int32_t num_edges_ = 0;\n  int num_forward_edges_in_node0 = 0;\n};\n",
            "\n  // Member variables.\n  EDGE_ARRAY edges_ = nullptr;\n  uint32_t num_edges_ = 0;\n  int num_forward_edges_in_node0 = 0;\n};\n",
        ),
        (
            "thirdparty/tesseract/src/lstm/plumbing.cpp",
            "  if (!fp->DeSerialize(&size)) {\n    return false;\n  }\n  for (uint32_t i = 0; i < size; ++i) {\n    Network *network = CreateFromFile(fp);\n    if (network == nullptr) {",
            "  if (!fp->DeSerialize(&size)) {\n    return false;\n  }\n  // Reject unreasonably large network stacks.\n  if (size > 10000) {\n    return false;\n  }\n  for (uint32_t i = 0; i < size; ++i) {\n    Network *network = CreateFromFile(fp);\n    if (network == nullptr) {",
        ),
        (
            "thirdparty/tesseract/src/lstm/weightmatrix.cpp",
            "    if (!fp->DeSerialize(&size)) {\n      return false;\n    }\n#ifdef FAST_FLOAT\n    scales_.reserve(size);\n    for (auto n = size; n > 0; n--) {",
            "    if (!fp->DeSerialize(&size)) {\n      return false;\n    }\n    // Reject unreasonably large scale vectors.\n    if (size > 100000000) {\n      return false;\n    }\n#ifdef FAST_FLOAT\n    scales_.reserve(size);\n    for (auto n = size; n > 0; n--) {",
        ),
        (
            "thirdparty/tesseract/src/ccutil/serialis.h",
            "  void set_swap(bool value) {\n    swap_ = value;\n  }\n\n  // Deserialize data.\n  bool DeSerializeSize(int32_t *data);",
            "  void set_swap(bool value) {\n    swap_ = value;\n  }\n  // Returns the number of bytes remaining to be read.\n  size_t RemainingBytes() const {\n    return data_ != nullptr && offset_ < data_->size() ? data_->size() - offset_ : 0;\n  }\n\n  // Deserialize data.\n  bool DeSerializeSize(int32_t *data);",
        ),
        (
            "thirdparty/tesseract/src/dict/dawg.cpp",
            "    tprintf(\"Empty dawg: num_edges is 0\\n\");\n    return false;\n  }\n  Dawg::init(unicharset_size);\n\n  edges_ = new EDGE_RECORD[num_edges_];\n  if (!file->DeSerialize(&edges_[0], num_edges_)) {\n    return false;\n  }\n  if (debug_level_ > 2) {\n    tprintf(\"type: %d lang: %s perm: %d unicharset_size: %d num_edges: %\" PRIu32 \"\\n\",\n            type_, lang_.c_str(), perm_, unicharset_size_, num_edges_);",
            "    tprintf(\"Empty dawg: num_edges is 0\\n\");\n    return false;\n  }\n  // Reject if the declared edge count exceeds the remaining component bytes.\n  if (num_edges_ > file->RemainingBytes() / sizeof(EDGE_RECORD)) {\n    tprintf(\"Dawg num_edges %u exceeds remaining data\\n\", num_edges_);\n    return false;\n  }\n  Dawg::init(unicharset_size);\n\n  edges_ = new EDGE_RECORD[num_edges_];\n  if (!file->DeSerialize(&edges_[0], num_edges_)) {\n    return false;\n  }\n  // Validate the loaded edge structure: check that next_node values are in\n  // bounds and that forward edge runs are properly terminated.\n  for (uint32_t i = 0; i < num_edges_; ++i) {\n    if (edges_[i] == next_node_mask_) {\n      continue; // Empty slot.\n    }\n    NODE_REF next = next_node_from_edge_rec(edges_[i]);\n    if (next != 0 && static_cast<uint32_t>(next) >= num_edges_) {\n      tprintf(\"Dawg edge %u has out-of-bounds next_node\\n\", i);\n      return false;\n    }\n    if (forward_edge(i)) {\n      uint32_t j = i;\n      bool terminated = false;\n      do {\n        if (last_edge(j)) {\n          terminated = true;\n          break;\n        }\n        ++j;\n      } while (j < num_edges_);\n      if (!terminated) {\n        tprintf(\"Dawg forward edge run starting at %u is not terminated\\n\", i);\n        return false;\n      }\n    }\n  }\n  if (debug_level_ > 2) {\n    tprintf(\"type: %d lang: %s perm: %d unicharset_size: %d num_edges: %\" PRIu32 \"\\n\",\n            type_, lang_.c_str(), perm_, unicharset_size_, num_edges_);",
        ),
        (
            "thirdparty/tesseract/src/lstm/weightmatrix.h",
            "  int NumOutputs() const {\n    return int_mode_ ? wi_.dim1() : wf_.dim1();\n  }\n  // Provides one set of weights. Only used by peep weight maxpool.",
            "  int NumOutputs() const {\n    return int_mode_ ? wi_.dim1() : wf_.dim1();\n  }\n  // The dimensions of the active weight matrix (wi_ in int mode, else wf_).\n  int Dim1() const {\n    return int_mode_ ? wi_.dim1() : wf_.dim1();\n  }\n  int Dim2() const {\n    return int_mode_ ? wi_.dim2() : wf_.dim2();\n  }\n  // Provides one set of weights. Only used by peep weight maxpool.",
        ),
        (
            "thirdparty/tesseract/src/lstm/fullyconnected.cpp",
            "// Reads from the given file. Returns false in case of error.\nbool FullyConnected::DeSerialize(TFile *fp) {\n  return weights_.DeSerialize(IsTraining(), fp);\n}",
            "// Reads from the given file. Returns false in case of error.\nbool FullyConnected::DeSerialize(TFile *fp) {\n  if (!weights_.DeSerialize(IsTraining(), fp)) {\n    return false;\n  }\n  // The weight matrix must match the declared sizes (the second dimension\n  // includes the bias column); otherwise Forward would read or write\n  // outside the scratch buffers sized from ni_ and no_.\n  if (weights_.Dim1() != no_ || weights_.Dim2() != ni_ + 1) {\n    return false;\n  }\n  return true;\n}",
        ),
        (
            "thirdparty/tesseract/src/lstm/lstm.cpp",
            "      is_2d_ = na_ - nf_ == ni_ + 2 * ns_;\n    }\n  }\n  delete softmax_;\n  if (type_ == NT_LSTM_SOFTMAX || type_ == NT_LSTM_SOFTMAX_ENCODED) {\n    softmax_ = static_cast<FullyConnected *>(Network::CreateFromFile(fp));\n    if (softmax_ == nullptr) {\n      return false;\n    }\n  } else {\n    softmax_ = nullptr;\n  }",
            "      is_2d_ = na_ - nf_ == ni_ + 2 * ns_;\n    }\n  }\n  // The deserialized dimensions must be mutually consistent: the forward\n  // pass sizes its buffers from na_, no_ and ns_ while the gate matrices\n  // drive their own dimensions.\n  if (na_ != ni_ + nf_ + (is_2d_ ? 2 : 1) * ns_) {\n    return false;\n  }\n  for (int w = 0; w < WT_COUNT; ++w) {\n    if (w == GFS && !Is2D()) {\n      continue;\n    }\n    if (gate_weights_[w].Dim1() != ns_ || gate_weights_[w].Dim2() != na_ + 1) {\n      return false;\n    }\n  }\n  if ((type_ == NT_LSTM || type_ == NT_LSTM_SUMMARY) && ns_ != no_) {\n    return false;\n  }\n  delete softmax_;\n  if (type_ == NT_LSTM_SOFTMAX || type_ == NT_LSTM_SOFTMAX_ENCODED) {\n    softmax_ = static_cast<FullyConnected *>(Network::CreateFromFile(fp));\n    if (softmax_ == nullptr) {\n      return false;\n    }\n    if (softmax_->NumInputs() != ns_ || softmax_->NumOutputs() != no_) {\n      return false;\n    }\n  } else {\n    softmax_ = nullptr;\n  }",
        ),
        (
            "thirdparty/tesseract/src/lstm/networkio.cpp",
            "\n// Adds part of a single timestep to floats.\nvoid NetworkIO::AddTimeStepPart(int t, int offset, int num_features, float *inout) const {\n  if (int_mode_) {\n    const int8_t *line = i_[t] + offset;\n    for (int i = 0; i < num_features; ++i) {",
            "\n// Adds part of a single timestep to floats.\nvoid NetworkIO::AddTimeStepPart(int t, int offset, int num_features, float *inout) const {\n  ASSERT_HOST(offset + num_features <= NumFeatures());\n  if (int_mode_) {\n    const int8_t *line = i_[t] + offset;\n    for (int i = 0; i < num_features; ++i) {",
        ),
        (
            "thirdparty/tesseract/src/lstm/networkio.cpp",
            "// Writes a single timestep from floats in the range [-1, 1] writing only\n// num_features elements of input to (*this)[t], starting at offset.\nvoid NetworkIO::WriteTimeStepPart(int t, int offset, int num_features, const TFloat *input) {\n  if (int_mode_) {\n    int8_t *line = i_[t] + offset;\n    for (int i = 0; i < num_features; ++i) {",
            "// Writes a single timestep from floats in the range [-1, 1] writing only\n// num_features elements of input to (*this)[t], starting at offset.\nvoid NetworkIO::WriteTimeStepPart(int t, int offset, int num_features, const TFloat *input) {\n  ASSERT_HOST(offset + num_features <= NumFeatures());\n  if (int_mode_) {\n    int8_t *line = i_[t] + offset;\n    for (int i = 0; i < num_features; ++i) {",
        ),
        (
            "thirdparty/tesseract/src/ccutil/unicharcompress.cpp",
            "  for (unsigned c = 0; c < encoder_.size(); ++c) {\n    const RecodedCharID &code = encoder_[c];\n    decoder_[code] = c;\n    is_valid_start_[code(0)] = true;\n    RecodedCharID prefix = code;\n    int len = code.length() - 1;",
            "  for (unsigned c = 0; c < encoder_.size(); ++c) {\n    const RecodedCharID &code = encoder_[c];\n    decoder_[code] = c;\n    ASSERT_HOST(code(0) >= 0 && code(0) < code_range_);\n    is_valid_start_[code(0)] = true;\n    RecodedCharID prefix = code;\n    int len = code.length() - 1;",
        ),
        (
            "thirdparty/tesseract/src/ccutil/unicharcompress.h",
            "    if (length_ > kMaxCodeLen) {\n      return false;\n    }\n    return fp->DeSerialize(&code_[0], length_);\n  }\n  bool operator==(const RecodedCharID &other) const {\n    if (length_ != other.length_) {",
            "    if (length_ > kMaxCodeLen) {\n      return false;\n    }\n    if (!fp->DeSerialize(&code_[0], length_)) {\n      return false;\n    }\n    // Code values index arrays sized from the maximum code; reject values\n    // that are out of the sane range for a recoded alphabet.\n    for (uint32_t i = 0; i < length_; ++i) {\n      if (code_[i] < 0 || code_[i] >= static_cast<int32_t>(UINT16_MAX)) {\n        return false;\n      }\n    }\n    return true;\n  }\n  bool operator==(const RecodedCharID &other) const {\n    if (length_ != other.length_) {",
        ),
        (
            "thirdparty/tesseract/src/ccutil/unicharcompress.h",
            "  int DecodeUnichar(const RecodedCharID &code) const;\n  // Returns true if the given code is a valid start or single code.\n  bool IsValidFirstCode(int code) const {\n    return is_valid_start_[code];\n  }\n  // Returns a list of valid non-final next codes for a given prefix code,",
            "  int DecodeUnichar(const RecodedCharID &code) const;\n  // Returns true if the given code is a valid start or single code.\n  bool IsValidFirstCode(int code) const {\n    ASSERT_HOST(code >= 0 && code < code_range_);\n    return is_valid_start_[code];\n  }\n  // Returns a list of valid non-final next codes for a given prefix code,",
        ),
        // Additional upstream Tesseract language-data validation checks from
        // 5.5.3 and 5.5.4 (#577). Applied in order:
        // - 6db4e78940bb4faa2d5f445064720faca35a4b47
        // - 3a3fe8d7f4c10b3d159939d2a3cbb8b6e680ab84
        // - db9866a4bb16e88cb9354861862dabee841af628
        // - 7e61e3457de8809fbc60400d82690b8e9e46be2e
        // - 230b6132926e5b774de1d2ba5f66013076f14a79
        // - 289f72664cb51a1476a0869cb027fe53be1f3e01
        // - 2da83fd880b508c9f7e667b43f4aa230effd045b
        // - d2d7dcbe8eaed4889a3f4711a97c65d650aa4caa
        // - 2f4d2f4bf45c363785d7bf1da29b6628f8939a72
        // - 2d04d640db2e8c7e3bab2369d599343b5a8b8443
        // - 552771236b0d80cbdb0c7dd856120fa21a4672e5
        // - 76865fa087c15f5ea2f75f697993fc3b05eabaf2
        // - 309f963f50bd3e6732654a114550d7d32da6c278
        // - 8bde475d4792ba0d4d5bd7b7259b5405613770a5
        // - 048e92378f1a34dcdaf94bc515c7552ac01f08ef
        // - e00c98a35c3bf829b7a8a9d0147ab00e44f11b3b
        // - 416fa815095f383b9113f7a436fa3a385b488b8e
        // - ac0f85c2ee6a242c169a404978f0be8877bc05b7
        // - 0000341a396c1cfb6cec414a6732dd4ce68531c3
        // - 15879257e1afc87ccf21845a417bfccd3effee83
        // - cd3647052ec511ea9ab6ef9116977a014b46bc73
        // - ef24f875c3e306211137d5e7de76691c5faac8ea
        // Except for the hunk in 7e61e345 in SetupDecoder (where the context includes
        // the ASSERT_HOST line added by c94a5532, producing results identical to 5.5.4),
        // these are verbatim upstream changes.
        // Four upstream commits are intentionally omitted because they only touch code
        // that MuPDF does not build:
        // - 1bda5079b1c8a7e25f523486837426903d29ce84 (normmatch.cpp)
        // - 8b0574680f3b22f246ade6a4c8e3029104255c63 (legacy engine in src/classify/)
        // - 56e09ca12e751623fe796ce1554ce704bffd2ef0 (GenericVector::read, only called by intproto.cpp)
        // - c053424327352b9d5180aa7540c1f9e67c64f27e (FAST_FLOAT branches)
        (
            "thirdparty/tesseract/src/ccmain/tessedit.cpp",
            "      lstm_recognizer_ = new LSTMRecognizer(language_data_path_prefix.c_str());\n      ASSERT_HOST(lstm_recognizer_->Load(this->params(), lstm_use_matrix ? language : \"\", mgr));\n    } else {\n      tprintf(\"Error: LSTM requested, but not present!! Loading tesseract.\\n\");\n      tessedit_ocr_engine_mode.set_value(OEM_TESSERACT_ONLY);\n    }\n  }\n\n",
            "      lstm_recognizer_ = new LSTMRecognizer(language_data_path_prefix.c_str());\n      ASSERT_HOST(lstm_recognizer_->Load(this->params(), lstm_use_matrix ? language : \"\", mgr));\n    } else {\n#ifdef DISABLED_LEGACY_ENGINE\n      tprintf(\n          \"Error: LSTM requested, but not present in %s; the legacy \"\n          \"engine is disabled in this build, so there is no fallback.\\n\",\n          tessdata_path.c_str());\n      return false;\n#else\n      tprintf(\"Error: LSTM requested, but not present!! Loading tesseract.\\n\");\n      tessedit_ocr_engine_mode.set_value(OEM_TESSERACT_ONLY);\n#endif // DISABLED_LEGACY_ENGINE\n    }\n  }\n\n",
        ),
        (
            "thirdparty/tesseract/src/ccmain/tessedit.cpp",
            "      ASSERT_HOST(lstm_recognizer_->Load(this->params(), lstm_use_matrix ? language : \"\", mgr));\n    } else {\n#ifdef DISABLED_LEGACY_ENGINE\n      tprintf(\n          \"Error: LSTM requested, but not present in %s; the legacy \"\n          \"engine is disabled in this build, so there is no fallback.\\n\",\n",
            "      ASSERT_HOST(lstm_recognizer_->Load(this->params(), lstm_use_matrix ? language : \"\", mgr));\n    } else {\n#ifdef DISABLED_LEGACY_ENGINE\n      // The legacy engine is compiled out, so we cannot fall back to it.\n      // Returning false here propagates a load failure to init_tesseract,\n      // which logs \"Failed loading language '%s'\" and skips this language.\n      tprintf(\n          \"Error: LSTM requested, but not present in %s; the legacy \"\n          \"engine is disabled in this build, so there is no fallback.\\n\",\n",
        ),
        (
            "thirdparty/tesseract/src/ccstruct/fontinfo.cpp",
            "  if (vec_size == 0) {\n    return true;\n  }\n  fi->init_spacing(vec_size);\n  for (uint32_t i = 0; i < vec_size; ++i) {\n    auto *fs = new FontSpacingInfo();\n",
            "  if (vec_size == 0) {\n    return true;\n  }\n  // Reject unreasonably large spacing vectors.\n  if (vec_size > 1000000) {\n    return false;\n  }\n  fi->init_spacing(vec_size);\n  for (uint32_t i = 0; i < vec_size; ++i) {\n    auto *fs = new FontSpacingInfo();\n",
        ),
        (
            "thirdparty/tesseract/src/dict/dawg.cpp",
            "#include \"helpers.h\"\n#include \"tprintf.h\"\n\n#include <memory>\n\n/*----------------------------------------------------------------------\n",
            "#include \"helpers.h\"\n#include \"tprintf.h\"\n\n#include <climits>\n#include <memory>\n\n/*----------------------------------------------------------------------\n",
        ),
        (
            "thirdparty/tesseract/src/dict/dawg.cpp",
            "        return;\n      }\n      if (last_edge(edge)) {\n        break;\n      }\n      ++edge;\n",
            "        return;\n      }\n      if (last_edge(edge)) {\n        ++edge; // Advance past the last forward edge.\n        break;\n      }\n      ++edge;\n",
        ),
        (
            "thirdparty/tesseract/src/dict/dawg.cpp",
            "  if (!file->DeSerialize(&unicharset_size)) {\n    return false;\n  }\n  if (!file->DeSerialize(&num_edges_)) {\n    return false;\n  }\n",
            "  if (!file->DeSerialize(&unicharset_size)) {\n    return false;\n  }\n  // Dawg::init takes int; reject values that would overflow.\n  if (unicharset_size == 0 || unicharset_size > INT_MAX) {\n    tprintf(\"Bad dawg unicharset_size %u\\n\", unicharset_size);\n    return false;\n  }\n  if (!file->DeSerialize(&num_edges_)) {\n    return false;\n  }\n",
        ),
        (
            "thirdparty/tesseract/src/ccutil/serialis.cpp",
            "}\n\nbool TFile::Skip(size_t count) {\n  offset_ += count;\n  return true;\n}\n",
            "}\n\nbool TFile::Skip(size_t count) {\n  if (data_ != nullptr && offset_ + count > data_->size()) {\n    offset_ = data_->size();\n    return false;\n  }\n  offset_ += count;\n  return true;\n}\n",
        ),
        (
            "thirdparty/tesseract/src/ccutil/unicharcompress.cpp",
            "        // Add the direct_set unichar-ids of the unicodes in sequence to the\n        // code.\n        for (int uni : unicodes) {\n          int position = code.length();\n          if (position >= RecodedCharID::kMaxCodeLen) {\n            tprintf(\"Unichar %d=%s is too long to encode!!\\n\", u, unicharset.id_to_unichar(u));\n            return false;\n",
            "        // Add the direct_set unichar-ids of the unicodes in sequence to the\n        // code.\n        for (int uni : unicodes) {\n          uint32_t position = code.length();\n          if (position >= RecodedCharID::kMaxCodeLen) {\n            tprintf(\"Unichar %d=%s is too long to encode!!\\n\", u, unicharset.id_to_unichar(u));\n            return false;\n",
        ),
        (
            "thirdparty/tesseract/src/ccutil/unicharcompress.cpp",
            "  std::vector<int> offsets(code_range_);\n  // Find which codes are used\n  for (auto &code : encoder_) {\n    for (int i = 0; i < code.length(); ++i) {\n      offsets[code(i)] = 1;\n    }\n  }\n",
            "  std::vector<int> offsets(code_range_);\n  // Find which codes are used\n  for (auto &code : encoder_) {\n    for (uint32_t i = 0; i < code.length(); ++i) {\n      offsets[code(i)] = 1;\n    }\n  }\n",
        ),
        (
            "thirdparty/tesseract/src/ccutil/unicharcompress.cpp",
            "// Decodes code, returning the original unichar-id, or\n// INVALID_UNICHAR_ID if the input is invalid.\nint UnicharCompress::DecodeUnichar(const RecodedCharID &code) const {\n  int len = code.length();\n  if (len <= 0 || len > RecodedCharID::kMaxCodeLen) {\n    return INVALID_UNICHAR_ID;\n  }\n  auto it = decoder_.find(code);\n",
            "// Decodes code, returning the original unichar-id, or\n// INVALID_UNICHAR_ID if the input is invalid.\nint UnicharCompress::DecodeUnichar(const RecodedCharID &code) const {\n  uint32_t len = code.length();\n  if (len == 0 || len > RecodedCharID::kMaxCodeLen) {\n    return INVALID_UNICHAR_ID;\n  }\n  auto it = decoder_.find(code);\n",
        ),
        (
            "thirdparty/tesseract/src/ccutil/unicharcompress.cpp",
            "      continue;\n    }\n    encoding += std::to_string(code(0));\n    for (int i = 1; i < code.length(); ++i) {\n      encoding += \",\" + std::to_string(code(i));\n    }\n    encoding += \"\\t\";\n",
            "      continue;\n    }\n    encoding += std::to_string(code(0));\n    for (uint32_t i = 1; i < code.length(); ++i) {\n      encoding += \",\" + std::to_string(code(i));\n    }\n    encoding += \"\\t\";\n",
        ),
        (
            "thirdparty/tesseract/src/ccutil/unicharcompress.cpp",
            "void UnicharCompress::ComputeCodeRange() {\n  code_range_ = -1;\n  for (auto &code : encoder_) {\n    for (int i = 0; i < code.length(); ++i) {\n      if (code(i) > code_range_) {\n        code_range_ = code(i);\n      }\n",
            "void UnicharCompress::ComputeCodeRange() {\n  code_range_ = -1;\n  for (auto &code : encoder_) {\n    for (uint32_t i = 0; i < code.length(); ++i) {\n      if (code(i) > code_range_) {\n        code_range_ = code(i);\n      }\n",
        ),
        (
            "thirdparty/tesseract/src/ccutil/unicharcompress.cpp",
            "    ASSERT_HOST(code(0) >= 0 && code(0) < code_range_);\n    is_valid_start_[code(0)] = true;\n    RecodedCharID prefix = code;\n    int len = code.length() - 1;\n    prefix.Truncate(len);\n    auto final_it = final_codes_.find(prefix);\n    if (final_it == final_codes_.end()) {\n      auto *code_list = new std::vector<int>;\n      code_list->push_back(code(len));\n      final_codes_[prefix] = code_list;\n      while (--len >= 0) {\n        prefix.Truncate(len);\n        auto next_it = next_codes_.find(prefix);\n        if (next_it == next_codes_.end()) {\n",
            "    ASSERT_HOST(code(0) >= 0 && code(0) < code_range_);\n    is_valid_start_[code(0)] = true;\n    RecodedCharID prefix = code;\n    uint32_t len = code.length();\n    if (len == 0) {\n      continue;\n    }\n    --len;\n    prefix.Truncate(len);\n    auto final_it = final_codes_.find(prefix);\n    if (final_it == final_codes_.end()) {\n      auto *code_list = new std::vector<int>;\n      code_list->push_back(code(len));\n      final_codes_[prefix] = code_list;\n      while (len > 0) {\n        --len;\n        prefix.Truncate(len);\n        auto next_it = next_codes_.find(prefix);\n        if (next_it == next_codes_.end()) {\n",
        ),
        (
            "thirdparty/tesseract/src/ccutil/tessdatamanager.cpp",
            "        return false;\n      }\n      entries_[i].resize(entry_size);\n      if (!fp.DeSerialize(&entries_[i][0], entry_size)) {\n        return false;\n      }\n    }\n",
            "        return false;\n      }\n      entries_[i].resize(entry_size);\n      if (entry_size > 0 && !fp.DeSerialize(&entries_[i][0], entry_size)) {\n        return false;\n      }\n    }\n",
        ),
        (
            "thirdparty/tesseract/src/dict/dawg.cpp",
            "    tprintf(\"Empty dawg: num_edges is 0\\n\");\n    return false;\n  }\n  // Reject if the declared edge count exceeds the remaining component bytes.\n  if (num_edges_ > file->RemainingBytes() / sizeof(EDGE_RECORD)) {\n    tprintf(\"Dawg num_edges %u exceeds remaining data\\n\", num_edges_);\n",
            "    tprintf(\"Empty dawg: num_edges is 0\\n\");\n    return false;\n  }\n  // Hard upper bound to prevent resource-exhaustion DoS.\n  if (num_edges_ > 50000000) {\n    tprintf(\"Dawg num_edges %u exceeds hard limit\\n\", num_edges_);\n    return false;\n  }\n  // Reject if the declared edge count exceeds the remaining component bytes.\n  if (num_edges_ > file->RemainingBytes() / sizeof(EDGE_RECORD)) {\n    tprintf(\"Dawg num_edges %u exceeds remaining data\\n\", num_edges_);\n",
        ),
        (
            "thirdparty/tesseract/src/ccutil/serialis.cpp",
            "}\n\nbool TFile::Skip(size_t count) {\n  if (data_ != nullptr && offset_ + count > data_->size()) {\n    offset_ = data_->size();\n    return false;\n  }\n  offset_ += count;\n  return true;\n",
            "}\n\nbool TFile::Skip(size_t count) {\n  if (data_ != nullptr) {\n    size_t data_size = data_->size();\n    // Subtraction-based check to avoid overflow in offset_ + count.\n    if (offset_ >= data_size || count > data_size - offset_) {\n      offset_ = data_size > UINT_MAX ? UINT_MAX : static_cast<unsigned>(data_size);\n      return false;\n    }\n  }\n  offset_ += count;\n  return true;\n",
        ),
        (
            "thirdparty/tesseract/src/dict/dawg.cpp",
            "    if (edges_[i] == next_node_mask_) {\n      continue; // Empty slot.\n    }\n    NODE_REF next = next_node_from_edge_rec(edges_[i]);\n    if (next != 0 && static_cast<uint32_t>(next) >= num_edges_) {\n      tprintf(\"Dawg edge %u has out-of-bounds next_node\\n\", i);\n      return false;\n    }\n    if (forward_edge(i)) {\n      uint32_t j = i;\n      bool terminated = false;\n      do {\n        if (last_edge(j)) {\n          terminated = true;\n          break;\n        }\n        ++j;\n",
            "    if (edges_[i] == next_node_mask_) {\n      continue; // Empty slot.\n    }\n    if (forward_edge(i)) {\n      // Validate the entire forward-edge run and skip to its terminator.\n      bool terminated = false;\n      uint32_t j = i;\n      do {\n        NODE_REF nj = next_node_from_edge_rec(edges_[j]);\n        if (nj != 0 && static_cast<uint32_t>(nj) >= num_edges_) {\n          tprintf(\"Dawg edge %u has out-of-bounds next_node\\n\", j);\n          return false;\n        }\n        if (last_edge(j)) {\n          terminated = true;\n          i = j; // Advance outer loop past the terminator.\n          break;\n        }\n        ++j;\n",
        ),
        (
            "thirdparty/tesseract/src/dict/dawg.cpp",
            "        tprintf(\"Dawg forward edge run starting at %u is not terminated\\n\", i);\n        return false;\n      }\n    }\n  }\n  if (debug_level_ > 2) {\n",
            "        tprintf(\"Dawg forward edge run starting at %u is not terminated\\n\", i);\n        return false;\n      }\n    } else {\n      NODE_REF next = next_node_from_edge_rec(edges_[i]);\n      if (next != 0 && static_cast<uint32_t>(next) >= num_edges_) {\n        tprintf(\"Dawg edge %u has out-of-bounds next_node\\n\", i);\n        return false;\n      }\n    }\n  }\n  if (debug_level_ > 2) {\n",
        ),
        (
            "thirdparty/tesseract/src/dict/dawg.cpp",
            "  }\n  Dawg::init(unicharset_size);\n\n  edges_ = new EDGE_RECORD[num_edges_];\n  if (!file->DeSerialize(&edges_[0], num_edges_)) {\n    return false;\n  }\n  // Validate the loaded edge structure: check that next_node values are in\n",
            "  }\n  Dawg::init(unicharset_size);\n\n  delete[] edges_;\n  edges_ = new EDGE_RECORD[num_edges_];\n  if (!file->DeSerialize(&edges_[0], num_edges_)) {\n    delete[] edges_;\n    edges_ = nullptr;\n    num_edges_ = 0;\n    return false;\n  }\n  // Validate the loaded edge structure: check that next_node values are in\n",
        ),
        (
            "thirdparty/tesseract/src/dict/dawg.cpp",
            "        NODE_REF nj = next_node_from_edge_rec(edges_[j]);\n        if (nj != 0 && static_cast<uint32_t>(nj) >= num_edges_) {\n          tprintf(\"Dawg edge %u has out-of-bounds next_node\\n\", j);\n          return false;\n        }\n        if (last_edge(j)) {\n",
            "        NODE_REF nj = next_node_from_edge_rec(edges_[j]);\n        if (nj != 0 && static_cast<uint32_t>(nj) >= num_edges_) {\n          tprintf(\"Dawg edge %u has out-of-bounds next_node\\n\", j);\n          delete[] edges_;\n          edges_ = nullptr;\n          num_edges_ = 0;\n          return false;\n        }\n        if (last_edge(j)) {\n",
        ),
        (
            "thirdparty/tesseract/src/dict/dawg.cpp",
            "      } while (j < num_edges_);\n      if (!terminated) {\n        tprintf(\"Dawg forward edge run starting at %u is not terminated\\n\", i);\n        return false;\n      }\n    } else {\n      NODE_REF next = next_node_from_edge_rec(edges_[i]);\n      if (next != 0 && static_cast<uint32_t>(next) >= num_edges_) {\n        tprintf(\"Dawg edge %u has out-of-bounds next_node\\n\", i);\n        return false;\n      }\n    }\n",
            "      } while (j < num_edges_);\n      if (!terminated) {\n        tprintf(\"Dawg forward edge run starting at %u is not terminated\\n\", i);\n        delete[] edges_;\n        edges_ = nullptr;\n        num_edges_ = 0;\n        return false;\n      }\n    } else {\n      NODE_REF next = next_node_from_edge_rec(edges_[i]);\n      if (next != 0 && static_cast<uint32_t>(next) >= num_edges_) {\n        tprintf(\"Dawg edge %u has out-of-bounds next_node\\n\", i);\n        delete[] edges_;\n        edges_ = nullptr;\n        num_edges_ = 0;\n        return false;\n      }\n    }\n",
        ),
        (
            "thirdparty/tesseract/src/ccstruct/fontinfo.cpp",
            "    return false;\n  }\n  char *font_name = new char[size + 1];\n  fi->name = font_name;\n  if (!f->DeSerialize(font_name, size)) {\n    return false;\n  }\n  font_name[size] = '\\0';\n  return f->DeSerialize(&fi->properties);\n}\n",
            "    return false;\n  }\n  char *font_name = new char[size + 1];\n  if (!f->DeSerialize(font_name, size)) {\n    delete[] font_name;\n    return false;\n  }\n  fi->name = font_name;\n  font_name[size] = '\\0';\n  return f->DeSerialize(&fi->properties);\n}\n",
        ),
        (
            "thirdparty/tesseract/src/ccutil/tessdatamanager.cpp",
            "// Loads from the given memory buffer as if a file.\nbool TessdataManager::LoadMemBuffer(const char *name, const char *data, int size) {\n  // TODO: This method supports only the proprietary file format.\n  Clear();\n  data_file_name_ = name;\n  TFile fp;\n",
            "// Loads from the given memory buffer as if a file.\nbool TessdataManager::LoadMemBuffer(const char *name, const char *data, int size) {\n  // TODO: This method supports only the proprietary file format.\n  if (size < 0) {\n    return false;\n  }\n  Clear();\n  data_file_name_ = name;\n  TFile fp;\n",
        ),
        (
            "thirdparty/tesseract/src/ccutil/unicharcompress.h",
            "  }\n  // Sets the code value at the given index in the code.\n  void Set(uint32_t index, int value) {\n    code_[index] = value;\n    if (length_ <= index) {\n      length_ = index + 1;\n",
            "  }\n  // Sets the code value at the given index in the code.\n  void Set(uint32_t index, int value) {\n    if (index >= kMaxCodeLen) {\n      return;\n    }\n    code_[index] = value;\n    if (length_ <= index) {\n      length_ = index + 1;\n",
        ),
        (
            "thirdparty/tesseract/src/lstm/convolve.cpp",
            "\n#include \"convolve.h\"\n\n#include \"networkscratch.h\"\n#include \"serialis.h\"\n\nnamespace tesseract {\n\n",
            "\n#include \"convolve.h\"\n\n#include <cstdint>\n\n#include \"networkscratch.h\"\n#include \"serialis.h\"\n#include \"tprintf.h\"\n\nnamespace tesseract {\n\n",
        ),
        (
            "thirdparty/tesseract/src/lstm/convolve.cpp",
            "  if (!fp->DeSerialize(&half_y_)) {\n    return false;\n  }\n  no_ = ni_ * (2 * half_x_ + 1) * (2 * half_y_ + 1);\n  return true;\n}\n\n",
            "  if (!fp->DeSerialize(&half_y_)) {\n    return false;\n  }\n  if (half_x_ < 0 || half_y_ < 0 || ni_ <= 0) {\n    tprintf(\"Error: invalid Convolve parameters: ni=%d half_x=%d half_y=%d\\n\", ni_, half_x_,\n            half_y_);\n    return false;\n  }\n  int64_t kx = 2LL * half_x_ + 1;\n  int64_t ky = 2LL * half_y_ + 1;\n  // Stepwise overflow check: ni_ * kx * ky must fit in int.\n  if (kx > INT_MAX / ky) {\n    tprintf(\"Error: Convolve output-channel count overflows: ni=%d half_x=%d half_y=%d\\n\", ni_,\n            half_x_, half_y_);\n    return false;\n  }\n  int64_t kxky = kx * ky;\n  if (static_cast<int64_t>(ni_) > INT_MAX / kxky) {\n    tprintf(\"Error: Convolve output-channel count overflows: ni=%d half_x=%d half_y=%d\\n\", ni_,\n            half_x_, half_y_);\n    return false;\n  }\n  no_ = static_cast<int>(static_cast<int64_t>(ni_) * kxky);\n  return true;\n}\n\n",
        ),
        (
            "thirdparty/tesseract/src/lstm/network.cpp",
            "  if (!fp->DeSerialize(name)) {\n    return nullptr;\n  }\n\n  switch (type) {\n    case NT_CONVOLVE:\n      network = new Convolve(name, ni, 0, 0);\n",
            "  if (!fp->DeSerialize(name)) {\n    return nullptr;\n  }\n\n  if (ni < 0 || no < 0 || num_weights < 0) {\n    tprintf(\"Error: invalid network layer parameters: type=%d ni=%d no=%d num_weights=%d\\n\", type,\n            ni, no, num_weights);\n    return nullptr;\n  }\n\n  switch (type) {\n    case NT_CONVOLVE:\n      network = new Convolve(name, ni, 0, 0);\n",
        ),
        (
            "thirdparty/tesseract/src/lstm/networkio.cpp",
            "void NetworkIO::CopyTimeStepGeneral(int dest_t, int dest_offset, int num_features,\n                                    const NetworkIO &src, int src_t, int src_offset) {\n  ASSERT_HOST(int_mode_ == src.int_mode_);\n  if (int_mode_) {\n    memcpy(i_[dest_t] + dest_offset, src.i_[src_t] + src_offset, num_features * sizeof(i_[0][0]));\n  } else {\n",
            "void NetworkIO::CopyTimeStepGeneral(int dest_t, int dest_offset, int num_features,\n                                    const NetworkIO &src, int src_t, int src_offset) {\n  ASSERT_HOST(int_mode_ == src.int_mode_);\n  ASSERT_HOST(dest_offset + num_features <= NumFeatures());\n  if (int_mode_) {\n    memcpy(i_[dest_t] + dest_offset, src.i_[src_t] + src_offset, num_features * sizeof(i_[0][0]));\n  } else {\n",
        ),
        (
            "thirdparty/tesseract/src/lstm/networkio.cpp",
            "\n// Sets the given range to random values.\nvoid NetworkIO::Randomize(int t, int offset, int num_features, TRand *randomizer) {\n  if (int_mode_) {\n    int8_t *line = i_[t] + offset;\n    for (int i = 0; i < num_features; ++i) {\n",
            "\n// Sets the given range to random values.\nvoid NetworkIO::Randomize(int t, int offset, int num_features, TRand *randomizer) {\n  ASSERT_HOST(offset + num_features <= NumFeatures());\n  if (int_mode_) {\n    int8_t *line = i_[t] + offset;\n    for (int i = 0; i < num_features; ++i) {\n",
        ),
        (
            "thirdparty/tesseract/src/lstm/reconfig.cpp",
            "\n#include \"reconfig.h\"\n\nnamespace tesseract {\n\nReconfig::Reconfig(const std::string &name, int ni, int x_scale, int y_scale)\n",
            "\n#include \"reconfig.h\"\n\n#include <cstdint>\n\n#include \"tprintf.h\"\n\nnamespace tesseract {\n\nReconfig::Reconfig(const std::string &name, int ni, int x_scale, int y_scale)\n",
        ),
        (
            "thirdparty/tesseract/src/lstm/reconfig.cpp",
            "  if (!fp->DeSerialize(&y_scale_)) {\n    return false;\n  }\n  no_ = ni_ * x_scale_ * y_scale_;\n  return true;\n}\n\n",
            "  if (!fp->DeSerialize(&y_scale_)) {\n    return false;\n  }\n  if (x_scale_ <= 0 || y_scale_ <= 0 || ni_ <= 0) {\n    tprintf(\"Error: invalid Reconfig parameters: ni=%d x_scale=%d y_scale=%d\\n\", ni_, x_scale_,\n            y_scale_);\n    return false;\n  }\n  int64_t xs = x_scale_;\n  int64_t ys = y_scale_;\n  // Stepwise overflow check: ni_ * x_scale_ * y_scale_ must fit in int.\n  int64_t xsys = xs * ys;\n  if (static_cast<int64_t>(ni_) > INT_MAX / xsys) {\n    tprintf(\"Error: Reconfig output-channel count overflows: ni=%d x_scale=%d y_scale=%d\\n\", ni_,\n            x_scale_, y_scale_);\n    return false;\n  }\n  no_ = static_cast<int>(static_cast<int64_t>(ni_) * xsys);\n  return true;\n}\n\n",
        ),
        (
            "thirdparty/tesseract/src/ccutil/unicharset.cpp",
            "      sscanf(buffer, \"%d\", &unicharset_size) != 1) {\n    return false;\n  }\n  for (UNICHAR_ID id = 0; id < unicharset_size; ++id) {\n    char unichar[256];\n    unsigned int properties;\n",
            "      sscanf(buffer, \"%d\", &unicharset_size) != 1) {\n    return false;\n  }\n  if (unicharset_size <= 0) {\n    return false;\n  }\n  for (UNICHAR_ID id = 0; id < unicharset_size; ++id) {\n    char unichar[256];\n    unsigned int properties;\n",
        ),
        (
            "thirdparty/tesseract/src/ccutil/unicharset.cpp",
            "    } else {\n      this->unichar_insert_backwards_compatible(unichar);\n    }\n\n    this->set_isalpha(id, properties & ISALPHA_MASK);\n    this->set_islower(id, properties & ISLOWER_MASK);\n",
            "    } else {\n      this->unichar_insert_backwards_compatible(unichar);\n    }\n    // A duplicate or empty representation makes the insert a no-op,\n    // desynchronizing id from the unichars vector; the set_* calls and\n    // unichars[id] below would then write out of bounds. The file is\n    // malformed, so reject it.\n    if (size() != static_cast<size_t>(id) + 1) {\n      fprintf(stderr, \"%s:%d unichar %d has a duplicate or empty representation\\n\",\n              __FILE__, __LINE__, id);\n      return false;\n    }\n\n    this->set_isalpha(id, properties & ISALPHA_MASK);\n    this->set_islower(id, properties & ISLOWER_MASK);\n",
        ),
        (
            "thirdparty/tesseract/src/ccmain/tessedit.cpp",
            "#endif // ndef DISABLED_LEGACY_ENGINE\n    if (mgr->IsComponentAvailable(TESSDATA_LSTM)) {\n      lstm_recognizer_ = new LSTMRecognizer(language_data_path_prefix.c_str());\n      ASSERT_HOST(lstm_recognizer_->Load(this->params(), lstm_use_matrix ? language : \"\", mgr));\n    } else {\n#ifdef DISABLED_LEGACY_ENGINE\n      // The legacy engine is compiled out, so we cannot fall back to it.\n",
            "#endif // ndef DISABLED_LEGACY_ENGINE\n    if (mgr->IsComponentAvailable(TESSDATA_LSTM)) {\n      lstm_recognizer_ = new LSTMRecognizer(language_data_path_prefix.c_str());\n      if (!lstm_recognizer_->Load(this->params(), lstm_use_matrix ? language : \"\", mgr)) {\n        delete lstm_recognizer_;\n        lstm_recognizer_ = nullptr;\n        tprintf(\"Error: Failed to load the LSTM model from %s\\n\", tessdata_path.c_str());\n        return false;\n      }\n    } else {\n#ifdef DISABLED_LEGACY_ENGINE\n      // The legacy engine is compiled out, so we cannot fall back to it.\n",
        ),
        (
            "thirdparty/tesseract/src/lstm/plumbing.cpp",
            "  if (size > 10000) {\n    return false;\n  }\n  for (uint32_t i = 0; i < size; ++i) {\n    Network *network = CreateFromFile(fp);\n    if (network == nullptr) {\n",
            "  if (size > 10000) {\n    return false;\n  }\n  // Reject empty stacks: XScaleFactor, CacheXScaleFactor and other methods\n  // unconditionally dereference stack_[0] during network initialization.\n  // A Series needs at least two networks (see Series::Forward).\n  if (size == 0 || (type() == NT_SERIES && size == 1)) {\n    return false;\n  }\n  for (uint32_t i = 0; i < size; ++i) {\n    Network *network = CreateFromFile(fp);\n    if (network == nullptr) {\n",
        ),
        (
            "thirdparty/tesseract/src/ccutil/unicharset.h",
            "  // SpecialUnicharCodes available. If false then there are normal unichars\n  // at these codes and they should not be used.\n  bool has_special_codes() const {\n    return get_fragment(UNICHAR_BROKEN) != nullptr &&\n           strcmp(id_to_unichar(UNICHAR_BROKEN),\n                  kSpecialUnicharCodes[UNICHAR_BROKEN]) == 0;\n  }\n",
            "  // SpecialUnicharCodes available. If false then there are normal unichars\n  // at these codes and they should not be used.\n  bool has_special_codes() const {\n    return contains_unichar_id(UNICHAR_BROKEN) &&\n           get_fragment(UNICHAR_BROKEN) != nullptr &&\n           strcmp(id_to_unichar(UNICHAR_BROKEN),\n                  kSpecialUnicharCodes[UNICHAR_BROKEN]) == 0;\n  }\n",
        ),
        (
            "thirdparty/tesseract/src/ccutil/unichar.cpp",
            "  unicodes.reserve(utf8_length);\n  const_iterator end_it(end(utf8_str, utf8_length));\n  for (const_iterator it(begin(utf8_str, utf8_length)); it != end_it; ++it) {\n    if (it.is_legal()) {\n      unicodes.push_back(*it);\n    } else {\n      unicodes.clear();\n      return unicodes;\n    }\n  }\n  return unicodes;\n}\n",
            "  unicodes.reserve(utf8_length);\n  const_iterator end_it(end(utf8_str, utf8_length));\n  for (const_iterator it(begin(utf8_str, utf8_length)); it != end_it; ++it) {\n    // utf8_step() reports the width from the leading byte alone; reject a\n    // truncated trailing sequence rather than let the iterator run past the\n    // end of the string (issue #4495).\n    const int remaining = end_it.utf8_data() - it.utf8_data();\n    const int step = utf8_step(it.utf8_data());\n    if (step <= 0 || step > remaining) {\n      unicodes.clear();\n      return unicodes;\n    }\n    unicodes.push_back(*it);\n  }\n  return unicodes;\n}\n",
        ),
        (
            "thirdparty/tesseract/src/lstm/weightmatrix.cpp",
            "    if (!wi_.DeSerialize(fp)) {\n      return false;\n    }\n    uint32_t size;\n    if (!fp->DeSerialize(&size)) {\n      return false;\n",
            "    if (!wi_.DeSerialize(fp)) {\n      return false;\n    }\n    // The second dimension is the number of inputs plus the bias column, so\n    // it must be at least 1. A zero dimension would make IntSimdMatrix::Init\n    // compute num_in = dim2 - 1 = -1 and read the bias from array_[-1] on an\n    // unallocated (null) array, so reject the corrupt model instead of\n    // crashing.\n    if (wi_.dim2() < 1) {\n      return false;\n    }\n    uint32_t size;\n    if (!fp->DeSerialize(&size)) {\n      return false;\n",
        ),
        (
            "thirdparty/tesseract/src/ccstruct/matrix.h",
            "    }\n  }\n\n  // Reallocates the array to the given size. Does not keep old data, but does\n  // not initialize the array either.\n  // The allocated memory is expanded on the end by pad, allowing deliberate\n  // access beyond the bounds of the array.\n  void ResizeNoInit(int size1, int size2, int pad = 0) {\n    int new_size = size1 * size2 + pad;\n    if (new_size > size_allocated_) {\n      delete[] array_;\n      array_ = new T[new_size];\n",
            "    }\n  }\n\n  // DoS protection: the maximum number of elements (including the\n  // padding) a single array may hold. Legitimate matrices in tesseract\n  // are far smaller than this; the cap is consistent with the\n  // \"unreasonably large\" bound applied to the scale vector in\n  // WeightMatrix::DeSerialize.\n  static const int64_t kMaxElements = 100000000;\n\n  // Reallocates the array to the given size. Does not keep old data, but does\n  // not initialize the array either.\n  // The allocated memory is expanded on the end by pad, allowing deliberate\n  // access beyond the bounds of the array.\n  void ResizeNoInit(int size1, int size2, int pad = 0) {\n    // Compute the total element count in 64 bits so it cannot overflow\n    // (in particular (-1) * (-1) must not pass as 1), then reject\n    // corrupted values (a crafted .traineddata file could produce\n    // negative or oversized dimensions) and bound the allocation against\n    // denial of service.\n    const int64_t total = static_cast<int64_t>(size1) * size2 + pad;\n    if (size1 < 0 || size2 < 0 || pad < 0 || total > kMaxElements) {\n      return;\n    }\n    const int new_size = static_cast<int>(total);\n    if (new_size > size_allocated_) {\n      delete[] array_;\n      array_ = new T[new_size];\n",
        ),
        (
            "thirdparty/tesseract/src/ccstruct/matrix.h",
            "    dim1_ = size1;\n    dim2_ = size2;\n    // Fill the padding data so it isn't uninitialized.\n    for (int i = size1 * size2; i < new_size; ++i) {\n      array_[i] = empty_;\n    }\n  }\n",
            "    dim1_ = size1;\n    dim2_ = size2;\n    // Fill the padding data so it isn't uninitialized.\n    for (int i = new_size - pad; i < new_size; ++i) {\n      array_[i] = empty_;\n    }\n  }\n",
        ),
        (
            "thirdparty/tesseract/src/ccstruct/matrix.h",
            "    return true;\n  }\n  bool DeSerializeSize(TFile *fp) {\n    int32_t size1, size2;\n    if (!fp->DeSerialize(&size1)) {\n      return false;\n    }\n",
            "    return true;\n  }\n  bool DeSerializeSize(TFile *fp) {\n    // Read the dimensions as uint32_t, like the FILE * variant above.\n    // The previous int32_t read compared the signed value against the\n    // signed upper bound, so a negative dimension (e.g. 0xFFFFFFFF = -1)\n    // passed the check, later causing signed integer overflow in\n    // ResizeNoInit (size1 * size2) and out-of-bounds accesses in\n    // IntSimdMatrix. Read as unsigned, the same bytes are 0xFFFFFFFF\n    // and fail the bound check below.\n    uint32_t size1, size2;\n    if (!fp->DeSerialize(&size1)) {\n      return false;\n    }\n",
        ),
        (
            "thirdparty/tesseract/src/ccstruct/matrix.h",
            "    if (!fp->DeSerialize(&size2)) {\n      return false;\n    }\n    // Arbitrarily limit the number of elements to protect against bad data.\n    if (size1 > UINT16_MAX) {\n      return false;\n    }\n    if (size2 > UINT16_MAX) {\n      return false;\n    }\n    Resize(size1, size2, empty_);\n    return true;\n  }\n",
            "    if (!fp->DeSerialize(&size2)) {\n      return false;\n    }\n    // Arbitrarily limit the number of elements to protect against bad data.\n    if (size1 > UINT16_MAX) {\n      return false;\n    }\n    if (size2 > UINT16_MAX) {\n      return false;\n    }\n    // The caller allocates before reading the elements, so the declared\n    // size must be backed by the bytes that actually remain in the stream\n    // (empty_ plus the element data). Without this check a crafted file\n    // could declare gigabytes of data while carrying a few hundred bytes,\n    // forcing a huge allocation (denial of service) from a tiny input.\n    const uint64_t required = (static_cast<uint64_t>(size1) * size2 + 1) *\n                              static_cast<uint64_t>(sizeof(T));\n    if (required > fp->RemainingBytes()) {\n      return false;\n    }\n    Resize(size1, size2, empty_);\n    return true;\n  }\n",
        ),
        (
            "thirdparty/tesseract/src/lstm/lstmrecognizer.cpp",
            "  if (!fp->DeSerialize(&null_char_)) {\n    return false;\n  }\n  if (!fp->DeSerialize(&adam_beta_)) {\n    return false;\n  }\n",
            "  if (!fp->DeSerialize(&null_char_)) {\n    return false;\n  }\n  // null_char_ is used as an index into the network softmax output\n  // (top_n_flags_/outputs), so it must lie within the output range. A\n  // corrupt or malicious model may store a negative or oversized value,\n  // which would index out of bounds during recognition, so reject it here.\n  if (null_char_ < 0 || null_char_ >= network_->NumOutputs()) {\n    tprintf(\"Error: null_char %d out of range for network with %d outputs\\n\",\n            null_char_, network_->NumOutputs());\n    return false;\n  }\n  if (!fp->DeSerialize(&adam_beta_)) {\n    return false;\n  }\n",
        ),
        (
            "thirdparty/tesseract/src/lstm/weightmatrix.cpp",
            "    if (size > 100000000) {\n      return false;\n    }\n#ifdef FAST_FLOAT\n    scales_.reserve(size);\n    for (auto n = size; n > 0; n--) {\n",
            "    if (size > 100000000) {\n      return false;\n    }\n    // A valid model carries one scale per output row: the generic\n    // (non-SIMD) MatrixDotVector reads scales_[i] for i < wi_.dim1(), and the\n    // SIMD path resizes to a rounded size no smaller than that. So the\n    // serialized scale vector must have at least wi_.dim1() entries. Check\n    // this here, before resizing below, where a zero size would otherwise\n    // make the &scales_[0] read index an empty vector (undefined behaviour).\n    if (static_cast<size_t>(size) < static_cast<size_t>(wi_.dim1())) {\n      return false;\n    }\n#ifdef FAST_FLOAT\n    scales_.reserve(size);\n    for (auto n = size; n > 0; n--) {\n",
        ),
        (
            "thirdparty/tesseract/src/lstm/weightmatrix.cpp",
            "    }\n#else\n    scales_.resize(size);\n    if (!fp->DeSerialize(&scales_[0], size)) {\n      return false;\n    }\n    for (auto &scale : scales_) {\n",
            "    }\n#else\n    scales_.resize(size);\n    if (size > 0 && !fp->DeSerialize(&scales_[0], size)) {\n      return false;\n    }\n    for (auto &scale : scales_) {\n",
        ),
        (
            "thirdparty/tesseract/src/lstm/weightmatrix.cpp",
            "    if (!fp->DeSerialize(old_scales)) {\n      return false;\n    }\n    scales_.reserve(old_scales.size());\n    for (float old_scale : old_scales) {\n      scales_.push_back(old_scale);\n",
            "    if (!fp->DeSerialize(old_scales)) {\n      return false;\n    }\n    // The generic (non-SIMD) IntSimdMatrix::MatrixDotVector reads\n    // scales_[i] for i < wi_.dim1(), so the old format carries at least\n    // one scale per output row as well.\n    if (old_scales.size() < static_cast<size_t>(wi_.dim1())) {\n      return false;\n    }\n    scales_.reserve(old_scales.size());\n    for (float old_scale : old_scales) {\n      scales_.push_back(old_scale);\n",
        ),
        (
            "thirdparty/tesseract/src/lstm/weightmatrix.cpp",
            "    if (size > 100000000) {\n      return false;\n    }\n    // A valid model carries one scale per output row: the generic\n    // (non-SIMD) MatrixDotVector reads scales_[i] for i < wi_.dim1(), and the\n    // SIMD path resizes to a rounded size no smaller than that. So the\n",
            "    if (size > 100000000) {\n      return false;\n    }\n    // The scales are stored as doubles, so the file must contain that many\n    // bytes. Checking this before the allocation below stops a tiny crafted\n    // file from forcing a huge allocation.\n    if (static_cast<uint64_t>(size) * sizeof(double) > fp->RemainingBytes()) {\n      return false;\n    }\n    // A valid model carries one scale per output row: the generic\n    // (non-SIMD) MatrixDotVector reads scales_[i] for i < wi_.dim1(), and the\n    // SIMD path resizes to a rounded size no smaller than that. So the\n",
        ),
        (
            "thirdparty/tesseract/src/lstm/weightmatrix.cpp",
            "    if (!wi_.DeSerialize(fp)) {\n      return false;\n    }\n    std::vector<float> old_scales;\n    if (!fp->DeSerialize(old_scales)) {\n      return false;\n    }\n    // The generic (non-SIMD) IntSimdMatrix::MatrixDotVector reads\n",
            "    if (!wi_.DeSerialize(fp)) {\n      return false;\n    }\n    uint32_t num_scales;\n    if (!fp->DeSerialize(&num_scales)) {\n      return false;\n    }\n    // Reject unreasonably large scale vectors.\n    if (num_scales > 100000000) {\n      return false;\n    }\n    // The scales are stored as floats, so the file must contain that many\n    // bytes. Checking this before the allocation below stops a tiny crafted\n    // file from forcing a huge allocation.\n    if (static_cast<uint64_t>(num_scales) * sizeof(float) > fp->RemainingBytes()) {\n      return false;\n    }\n    std::vector<float> old_scales(num_scales);\n    if (num_scales > 0 && !fp->DeSerialize(&old_scales[0], num_scales)) {\n      return false;\n    }\n    // The generic (non-SIMD) IntSimdMatrix::MatrixDotVector reads\n",
        ),
        (
            "thirdparty/tesseract/src/lstm/weightmatrix.cpp",
            "    if (!wi_.DeSerialize(fp)) {\n      return false;\n    }\n    // The second dimension is the number of inputs plus the bias column, so\n    // it must be at least 1. A zero dimension would make IntSimdMatrix::Init\n    // compute num_in = dim2 - 1 = -1 and read the bias from array_[-1] on an\n",
            "    if (!wi_.DeSerialize(fp)) {\n      return false;\n    }\n    // The first dimension is the number of output rows, so it must be at\n    // least 1. A zero dimension would leave the reshaped weight matrix and\n    // the scale vector empty, so reject the corrupt model.\n    if (wi_.dim1() < 1) {\n      return false;\n    }\n    // The second dimension is the number of inputs plus the bias column, so\n    // it must be at least 1. A zero dimension would make IntSimdMatrix::Init\n    // compute num_in = dim2 - 1 = -1 and read the bias from array_[-1] on an\n",
        ),
        (
            "thirdparty/tesseract/src/lstm/lstmrecognizer.cpp",
            "  if (!DeSerialize(mgr, &fp)) {\n    return false;\n  }\n  if (lang.empty()) {\n    return true;\n  }\n",
            "  if (!DeSerialize(mgr, &fp)) {\n    return false;\n  }\n  // Recoded codes are used as indices into the network softmax output\n  // (top_n_flags_/outputs), sized to the network output count. The recoder is\n  // loaded independently of the network, so tie it to that count: a corrupt\n  // model with a code range wider than the softmax would index out of bounds\n  // during beam search. This covers both the recoding and the pass-through\n  // recoder.\n  //\n  // The check belongs in Load, not in the recoder-loading routines: the\n  // recognition path (LSTMRecognizer::Load) always has the final network and\n  // recoder, whereas the training path (LSTMTrainer::TryLoadingCheckpoint)\n  // deserializes an old network together with a target-traineddata recoder\n  // that may legitimately have a wider code range; that path reconciles the\n  // two with RemapOutputs, which must be allowed to run.\n  //\n  // It applies only when the network actually has a softmax to index into;\n  // a zero-output network has no softmax array, so the recoder's code range\n  // cannot point out of bounds and the check is skipped.\n  if (network_->NumOutputs() > 0 && recoder_.code_range() > network_->NumOutputs()) {\n    tprintf(\"Error: recoder code range %d exceeds network output count %d\\n\",\n            recoder_.code_range(), network_->NumOutputs());\n    return false;\n  }\n  if (lang.empty()) {\n    return true;\n  }\n",
        ),
        (
            "thirdparty/tesseract/src/ccutil/unicharset.cpp",
            "  }\n  if (script_table_size_reserved == 0) {\n    script_table_size_reserved = 8;\n    script_table = new char *[script_table_size_reserved];\n  } else if (script_table_size_used >= script_table_size_reserved) {\n    assert(script_table_size_used == script_table_size_reserved);\n    script_table_size_reserved += script_table_size_reserved;\n    char **new_script_table = new char *[script_table_size_reserved];\n    memcpy(new_script_table, script_table,\n           script_table_size_used * sizeof(char *));\n    delete[] script_table;\n    script_table = new_script_table;\n  }\n  script_table[script_table_size_used] = new char[strlen(script) + 1];\n  strcpy(script_table[script_table_size_used], script);\n",
            "  }\n  if (script_table_size_reserved == 0) {\n    script_table_size_reserved = 8;\n    script_table = new char *[script_table_size_reserved]();\n  } else if (script_table_size_used >= script_table_size_reserved) {\n    assert(script_table_size_used == script_table_size_reserved);\n    script_table_size_reserved += script_table_size_reserved;\n    char **old_script_table = script_table;\n    script_table = new char *[script_table_size_reserved]();\n    memcpy(script_table, old_script_table,\n           script_table_size_used * sizeof(char *));\n    delete[] old_script_table;\n  }\n  script_table[script_table_size_used] = new char[strlen(script) + 1];\n  strcpy(script_table[script_table_size_used], script);\n",
        ),

        // Upstream Tesseract fixes from 5.5.3 and 5.5.4 in code that recognition
        // runs (#580): LSTM beam search, layout analysis and the result iterator.
        // Applied in order:
        // - f4afb2cc9545f622a07812b09e1b72fc78f85a64
        // - 6ce521c74844af44c7c8d8bcb5733d8a81adc10f
        // - ef4b5095e9c8ce95b08b8b4d8452e845505dcaa3
        // - 810854166ed6382fcf0efe1a2e87b9333e41b930
        // - e13b2d79456de1091e57766a0298427fa3102187
        // - b77e37b5fcbb223cdbae4a46598b9f0b672adfb5
        // - 9983e9c69ac49e0ee2a6d05204879ae5ec3481b6
        // - 2cd9955ef1c6d2aa9974dfdbb5ff5c4e44603b26
        // - 5ca34b6b386ae09c8a6571b750fdac116cccbe40
        // - fb87a84b6e3384b424b2169c76de9031046861e9
        // These are verbatim upstream changes.
        // Other upstream changes from 5.5.3 and 5.5.4 are left out: they touch code
        // that MuPDF does not build (legacy engine, chopper, OSD, ScrollView) or
        // features this project does not use; see winner0988/Pdf-reader#580.
        (
            "thirdparty/tesseract/src/lstm/recodebeam.h",
            "    ASSERT_HOST(src.dawgs == nullptr);\n  }\n  RecodeNode &operator=(const RecodeNode &src) {\n    delete dawgs;\n    memcpy(this, &src, sizeof(src));\n    ((RecodeNode &)src).dawgs = nullptr;\n    return *this;\n  }\n  ~RecodeNode() {\n",
            "    ASSERT_HOST(src.dawgs == nullptr);\n  }\n  RecodeNode &operator=(const RecodeNode &src) {\n    if (this != &src) {\n      delete dawgs;\n      code = src.code;\n      unichar_id = src.unichar_id;\n      permuter = src.permuter;\n      start_of_dawg = src.start_of_dawg;\n      start_of_word = src.start_of_word;\n      end_of_word = src.end_of_word;\n      duplicate = src.duplicate;\n      certainty = src.certainty;\n      score = src.score;\n      prev = src.prev;\n      dawgs = src.dawgs;\n      code_hash = src.code_hash;\n      const_cast<RecodeNode &>(src).dawgs = nullptr;\n    }\n    return *this;\n  }\n  ~RecodeNode() {\n",
        ),
        (
            "thirdparty/tesseract/src/lstm/recodebeam.cpp",
            "  for (int p = length - 1; p >= 0 && previous != nullptr; --p) {\n    while (previous->duplicate || previous->code == null_char_) {\n      previous = previous->prev;\n    }\n    prefix.Set(p, previous->code);\n    full_code.Set(p, previous->code);\n",
            "  for (int p = length - 1; p >= 0 && previous != nullptr; --p) {\n    while (previous->duplicate || previous->code == null_char_) {\n      previous = previous->prev;\n      if (previous == nullptr) {\n        break;\n      }\n    }\n    if (previous == nullptr) {\n      // The beam entry has fewer than length codes in its history, so the\n      // prefix cannot be reconstructed reliably. Drop this continuation\n      // rather than using partially initialized prefix/full_code entries.\n      return;\n    }\n    prefix.Set(p, previous->code);\n    full_code.Set(p, previous->code);\n",
        ),
        (
            "thirdparty/tesseract/src/lstm/recodebeam.cpp",
            "    dawg_args.active_dawgs = uni_prev->dawgs;\n    word_start = uni_prev->start_of_dawg;\n  } else {\n    return; // Can't continue if not a dict word.\n  }\n  auto permuter = static_cast<PermuterType>(dict_->def_letter_is_okay(\n",
            "    dawg_args.active_dawgs = uni_prev->dawgs;\n    word_start = uni_prev->start_of_dawg;\n  } else {\n    delete updated_dawgs;\n    return; // Can't continue if not a dict word.\n  }\n  auto permuter = static_cast<PermuterType>(dict_->def_letter_is_okay(\n",
        ),
        (
            "thirdparty/tesseract/src/ccmain/paragraphs.cpp",
            "      continue;\n    }\n    // rows[start, end) should be a paragraph.\n    PARA *p = new PARA();\n    if (model == kCrownLeft || model == kCrownRight) {\n      p->is_very_first_or_continuation = true;\n      // Crown paragraph.\n",
            "      continue;\n    }\n    // rows[start, end) should be a paragraph.\n    std::unique_ptr<PARA> p(new PARA());\n    if (model == kCrownLeft || model == kCrownRight) {\n      p->is_very_first_or_continuation = true;\n      // Crown paragraph.\n",
        ),
        (
            "thirdparty/tesseract/src/ccmain/paragraphs.cpp",
            "    p->is_list_item = model->justification() == JUSTIFICATION_RIGHT\n                          ? rows[start].ri_->rword_indicates_list_item\n                          : rows[start].ri_->lword_indicates_list_item;\n    for (int row = start; row < end; row++) {\n      if ((*row_owners)[row] != nullptr) {\n        tprintf(\n",
            "    p->is_list_item = model->justification() == JUSTIFICATION_RIGHT\n                          ? rows[start].ri_->rword_indicates_list_item\n                          : rows[start].ri_->lword_indicates_list_item;\n    PARA *para = p.release();\n    for (int row = start; row < end; row++) {\n      if ((*row_owners)[row] != nullptr) {\n        tprintf(\n",
        ),
        (
            "thirdparty/tesseract/src/ccmain/paragraphs.cpp",
            "            \"more than once!\\n\");\n        delete (*row_owners)[row];\n      }\n      (*row_owners)[row] = p;\n    }\n  }\n}\n",
            "            \"more than once!\\n\");\n        delete (*row_owners)[row];\n      }\n      (*row_owners)[row] = para;\n    }\n  }\n}\n",
        ),
        (
            "thirdparty/tesseract/src/textord/cjkpitch.cpp",
            "#include \"topitch.h\"\n#include \"tovars.h\"\n\n#include <algorithm> // for std::sort\n#include <cmath>\n#include <vector>    // for std::vector\n\nnamespace tesseract {\n",
            "#include \"topitch.h\"\n#include \"tovars.h\"\n\n#include <algorithm> // for std::clamp, std::sort\n#include <cmath>\n#include <limits>    // for std::numeric_limits\n#include <vector>    // for std::vector\n\nnamespace tesseract {\n",
        ),
        (
            "thirdparty/tesseract/src/textord/cjkpitch.cpp",
            "// to be fixed-pitch.\nstatic const float kFixedPitchThreshold = 0.35f;\n\n// rank statistics for a small collection of float values.\nclass SimpleStats {\npublic:\n",
            "// to be fixed-pitch.\nstatic const float kFixedPitchThreshold = 0.35f;\n\n// Convert a float coordinate to TDimension, clamping out-of-range values\n// so that the conversion is well-defined even for corrupt or unusual input.\nstatic TDimension ToTDimension(float value) {\n  return static_cast<TDimension>(\n      std::clamp(static_cast<double>(value),\n                 static_cast<double>(std::numeric_limits<TDimension>::lowest()),\n                 static_cast<double>(std::numeric_limits<TDimension>::max())));\n}\n\n// rank statistics for a small collection of float values.\nclass SimpleStats {\npublic:\n",
        ),
        (
            "thirdparty/tesseract/src/textord/cjkpitch.cpp",
            "        skipped_whitespaces = true;\n        c1 -= estimated_pitch_;\n      }\n      TBOX ibody(c1, box(i).bottom(), c1 + estimated_pitch_, box(i).top());\n\n      // Collect all characters that mostly fit in the region.\n      // Also, their union height shouldn't be too big.\n",
            "        skipped_whitespaces = true;\n        c1 -= estimated_pitch_;\n      }\n      TBOX ibody(ToTDimension(c1), box(i).bottom(),\n                 ToTDimension(c1 + estimated_pitch_), box(i).top());\n\n      // Collect all characters that mostly fit in the region.\n      // Also, their union height shouldn't be too big.\n",
        ),
        (
            "thirdparty/tesseract/src/textord/cjkpitch.cpp",
            "        skipped_whitespaces = true;\n        c1 += estimated_pitch_;\n      }\n      TBOX ibody(c1 - estimated_pitch_, box(i).bottom(), c1, box(i).top());\n\n      size_t j = i;\n      TBOX merged;\n",
            "        skipped_whitespaces = true;\n        c1 += estimated_pitch_;\n      }\n      TBOX ibody(ToTDimension(c1 - estimated_pitch_), box(i).bottom(),\n                 ToTDimension(c1), box(i).top());\n\n      size_t j = i;\n      TBOX merged;\n",
        ),
        (
            "thirdparty/tesseract/src/textord/colpartitiongrid.cpp",
            "        BLOBNBOX *blob = blob_it.data();\n        if (blob->cblob()->area() == 0) {\n          // Any blob with zero area is a fake image blob and should be deleted.\n          delete blob->cblob();\n          delete blob;\n        }\n      }\n",
            "        BLOBNBOX *blob = blob_it.data();\n        if (blob->cblob()->area() == 0) {\n          // Any blob with zero area is a fake image blob and should be deleted.\n          // Remove it from the list first to avoid a double delete.\n          delete blob->remove_cblob();\n          blob_it.extract();\n          delete blob;\n        }\n      }\n",
        ),
        (
            "thirdparty/tesseract/src/textord/tospace.cpp",
            "  int16_t current_within_xht_gap = INT16_MAX;\n  int16_t next_within_xht_gap = INT16_MAX;\n  int16_t word_count = 0;\n\n  // repeated char words\n  WERD_IT rep_char_it(&(row->rep_words));\n",
            "  int16_t current_within_xht_gap = INT16_MAX;\n  int16_t next_within_xht_gap = INT16_MAX;\n  int16_t word_count = 0;\n  // Degenerate rows may have space_size 0, so guard against division by zero.\n  // A fallback larger than any int16_t gap yields floor(gap / space_size) == 0,\n  // so the blank count is raised to 1 by the checks below.\n  const float space_size = row->space_size > 0.0f\n                               ? row->space_size\n                               : static_cast<float>(INT16_MAX) + 1.0f;\n\n  // repeated char words\n  WERD_IT rep_char_it(&(row->rep_words));\n",
        ),
        (
            "thirdparty/tesseract/src/textord/tospace.cpp",
            "      current_gap = box_it.data()->bounding_box().left() - next_rep_char_word_right;\n      current_within_xht_gap = current_gap;\n      if (current_gap > tosp_rep_space * repetition_spacing) {\n        prev_blanks = static_cast<uint8_t>(std::floor(current_gap / row->space_size));\n        if (prev_blanks < 1) {\n          prev_blanks = 1;\n        }\n",
            "      current_gap = box_it.data()->bounding_box().left() - next_rep_char_word_right;\n      current_within_xht_gap = current_gap;\n      if (current_gap > tosp_rep_space * repetition_spacing) {\n        prev_blanks = static_cast<uint8_t>(std::floor(current_gap / space_size));\n        if (prev_blanks < 1) {\n          prev_blanks = 1;\n        }\n",
        ),
        (
            "thirdparty/tesseract/src/textord/tospace.cpp",
            "            current_gap = word->bounding_box().left() - prev_x;\n            current_within_xht_gap = current_gap;\n            if (current_gap > tosp_rep_space * repetition_spacing) {\n              blanks = static_cast<uint8_t>(std::floor(current_gap / row->space_size));\n              if (blanks < 1) {\n                blanks = 1;\n              }\n",
            "            current_gap = word->bounding_box().left() - prev_x;\n            current_within_xht_gap = current_gap;\n            if (current_gap > tosp_rep_space * repetition_spacing) {\n              blanks = static_cast<uint8_t>(std::floor(current_gap / space_size));\n              if (blanks < 1) {\n                blanks = 1;\n              }\n",
        ),
        (
            "thirdparty/tesseract/src/textord/tospace.cpp",
            "             */\n            current_gap = blob_box.left() - next_rep_char_word_right;\n            if (current_gap > tosp_rep_space * repetition_spacing) {\n              blanks = static_cast<uint8_t>(current_gap / row->space_size);\n              if (blanks < 1) {\n                blanks = 1;\n              }\n",
            "             */\n            current_gap = blob_box.left() - next_rep_char_word_right;\n            if (current_gap > tosp_rep_space * repetition_spacing) {\n              blanks = static_cast<uint8_t>(current_gap / space_size);\n              if (blanks < 1) {\n                blanks = 1;\n              }\n",
        ),
        (
            "thirdparty/tesseract/src/textord/tospace.cpp",
            "      repetition_spacing = find_mean_blob_spacing(word);\n      current_gap = word->bounding_box().left() - prev_x;\n      if (current_gap > tosp_rep_space * repetition_spacing) {\n        blanks = static_cast<uint8_t>(std::floor(current_gap / row->space_size));\n        if (blanks < 1) {\n          blanks = 1;\n        }\n",
            "      repetition_spacing = find_mean_blob_spacing(word);\n      current_gap = word->bounding_box().left() - prev_x;\n      if (current_gap > tosp_rep_space * repetition_spacing) {\n        blanks = static_cast<uint8_t>(std::floor(current_gap / space_size));\n        if (blanks < 1) {\n          blanks = 1;\n        }\n",
        ),
        (
            "thirdparty/tesseract/src/ccmain/pageiterator.cpp",
            "  if (it_->block() == nullptr) {\n    return; // At end of the document.\n  }\n  PAGE_RES_IT para(page_res_);\n  PAGE_RES_IT next_para(para);\n  next_para.forward_paragraph();\n  while (next_para.cmp(*it_) <= 0) {\n    para = next_para;\n    next_para.forward_paragraph();\n  }\n  *it_ = para;\n  BeginWord(0);\n}\n\nbool PageIterator::IsWithinFirstTextlineOfParagraph() const {\n",
            "  if (it_->block() == nullptr) {\n    return; // At end of the document.\n  }\n  const BLOCK_RES *target_block = it_->block();\n  const PARA *target_para = it_->row() == nullptr ? nullptr : it_->row()->row->para();\n  PAGE_RES_IT para(page_res_);\n  // Paragraph and block identities are stable within PAGE_RES. Avoid cmp(),\n  // which searches the page and makes this forward scan quadratic.\n  while (para.block() != nullptr &&\n         (para.block() != target_block ||\n          (para.row() != nullptr && para.row()->row->para() != target_para))) {\n    para.forward_paragraph();\n  }\n  if (para.block() != nullptr) {\n    *it_ = para;\n    BeginWord(0);\n  }\n}\n\nbool PageIterator::IsWithinFirstTextlineOfParagraph() const {\n",
        ),
        (
            "thirdparty/tesseract/src/ccmain/paragraphs.cpp",
            "    p->is_list_item = model->justification() == JUSTIFICATION_RIGHT\n                          ? rows[start].ri_->rword_indicates_list_item\n                          : rows[start].ri_->lword_indicates_list_item;\n    PARA *para = p.release();\n    for (int row = start; row < end; row++) {\n      if ((*row_owners)[row] != nullptr) {\n        tprintf(\n            \"Memory leak! ConvertHypothesizeModelRunsToParagraphs() called \"\n            \"more than once!\\n\");\n        delete (*row_owners)[row];\n      }\n      (*row_owners)[row] = para;\n    }\n  }\n}\n",
            "    p->is_list_item = model->justification() == JUSTIFICATION_RIGHT\n                          ? rows[start].ri_->rword_indicates_list_item\n                          : rows[start].ri_->lword_indicates_list_item;\n    // Free any stale owners of the rows of the run, then give all rows of\n    // the run a reference to the new paragraph, whose ownership is\n    // transferred directly to the first row of the run.\n    for (int row = start; row < end; row++) {\n      if ((*row_owners)[row] != nullptr) {\n        tprintf(\n            \"Memory leak! ConvertHypothesizedModelRunsToParagraphs() called \"\n            \"more than once!\\n\");\n        delete (*row_owners)[row];\n      }\n    }\n    (*row_owners)[start] = p.release();\n    for (int row = start + 1; row < end; row++) {\n      (*row_owners)[row] = (*row_owners)[start];\n    }\n  }\n}\n",
        ),
        (
            "thirdparty/tesseract/src/ccmain/resultiterator.cpp",
            "    return true; // doesn't matter.\n  }\n  LTRResultIterator it(*this);\n  it.RestartParagraph();\n  // Try to figure out the ltr-ness of the paragraph.  The rules below\n  // make more sense in the context of a difficult paragraph example.\n  // Here we denote {ltr characters, RTL CHARACTERS}:\n",
            "    return true; // doesn't matter.\n  }\n  LTRResultIterator it(*this);\n  if (!it.PageIterator::IsAtBeginningOf(RIL_PARA)) {\n    it.RestartParagraph();\n  }\n  // Try to figure out the ltr-ness of the paragraph.  The rules below\n  // make more sense in the context of a difficult paragraph example.\n  // Here we denote {ltr characters, RTL CHARACTERS}:\n",
        ),
        (
            "thirdparty/tesseract/src/ccmain/resultiterator.cpp",
            "\n  // A LTRResultIterator goes strictly left-to-right word order.\n  LTRResultIterator ltr_it(resit);\n  ltr_it.RestartRow();\n  if (ltr_it.Empty(RIL_WORD)) {\n    return;\n  }\n",
            "\n  // A LTRResultIterator goes strictly left-to-right word order.\n  LTRResultIterator ltr_it(resit);\n  if (!ltr_it.PageIterator::IsAtBeginningOf(RIL_TEXTLINE)) {\n    ltr_it.RestartRow();\n  }\n  if (ltr_it.Empty(RIL_WORD)) {\n    return;\n  }\n",
        ),
        (
            "thirdparty/tesseract/src/ccmain/resultiterator.cpp",
            "\nvoid ResultIterator::MoveToLogicalStartOfTextline() {\n  std::vector<int> word_indices;\n  RestartRow();\n  CalculateTextlineOrder(current_paragraph_is_ltr_, dynamic_cast<const LTRResultIterator &>(*this),\n                         &word_indices);\n  unsigned i = 0;\n",
            "\nvoid ResultIterator::MoveToLogicalStartOfTextline() {\n  std::vector<int> word_indices;\n  if (!PageIterator::IsAtBeginningOf(RIL_TEXTLINE)) {\n    RestartRow();\n  }\n  CalculateTextlineOrder(current_paragraph_is_ltr_, dynamic_cast<const LTRResultIterator &>(*this),\n                         &word_indices);\n  unsigned i = 0;\n",
        ),
        (
            "thirdparty/tesseract/src/ccmain/resultiterator.cpp",
            "      if (!PageIterator::Next(level)) {\n        return false;\n      }\n      if (IsWithinFirstTextlineOfParagraph()) {\n        // if we've advanced to a new paragraph,\n        // recalculate current_paragraph_is_ltr_\n        current_paragraph_is_ltr_ = CurrentParagraphIsLtr();\n",
            "      if (!PageIterator::Next(level)) {\n        return false;\n      }\n      if (PageIterator::IsAtBeginningOf(RIL_PARA)) {\n        // if we've advanced to a new paragraph,\n        // recalculate current_paragraph_is_ltr_\n        current_paragraph_is_ltr_ = CurrentParagraphIsLtr();\n",
        ),
    ];

    // FreeType 2.14.3, bundled with MuPDF 1.28.6, lacks upstream fixes from
    // FreeType 2.15.0 in code that MuPDF builds: the TrueType bytecode
    // interpreter, CID and CFF charstring loading, and fixed-point helpers
    // (winner0988/Pdf-reader#626). Applied in order:
    // - 7974be74d8b5a2fbf99aa88f0461d1f80af51cee
    // - 1803559c4ee407d0bcbf2a67dbe96690cee869d2
    // - 7d600a022e1d813e85a8c94ffd395f6135872267
    // - 287206a96b200f9ab048bb0d83a4dbab8766b9f8
    // - f3ca71c9900fe860849b3163a6e2c1e765b291d9
    // - a98c7c223ab274767f41467fe57795d86f60eedc
    // - c30faba496fdb36d40e290b0b9818fb57d68476a
    // - 1f705ffb716c7a35efe13d3b58f62ef7c4c6415d
    // - 968af86becaae66f571feec0c8ad11c1bc39ddd9
    // - df2fb115bf874a242a9fd49aa99c3c2fd41d151c
    // - 79d80cadfc3d992e307c15a3f7d3a2a1555235a5
    // - 1ec3406aef4f19a3808f06317280d011500ee0b5
    // 1803559c and 7d600a02 go together (the second fixes the first for zones
    // with fewer than four points), and so do a98c7c22 and c30faba4 (the second
    // fixes the first where FT_ULong has 32 bits, as on Windows). These are
    // verbatim upstream changes, applied like tesseract_patches. Other FreeType
    // 2.15.0 changes are left out: they touch code that 2.14.3 does not have or
    // that MuPDF does not build, or they change how valid fonts render; see
    // winner0988/Pdf-reader#626.
    let freetype_patches: &[(&str, &str, &str)] = &[
        (
            "thirdparty/freetype/src/truetype/ttinterp.c",
            "    IUP_WorkerRec  V;\n    FT_Byte        mask;\n\n    FT_UInt   first_point;   /* first point of contour        */\n    FT_UInt   end_point;     /* end point (last+1) of contour */\n\n    FT_UInt   first_touched; /* first touched point in contour   */\n    FT_UInt   cur_touched;   /* current touched point in contour */\n\n    FT_UInt   point;         /* current point   */\n    FT_Short  contour;       /* current contour */\n\n\n#ifdef TT_SUPPORT_SUBPIXEL_HINTING_MINIMAL\n",
            "    IUP_WorkerRec  V;\n    FT_Byte        mask;\n\n    FT_UInt  first_point;   /* first point of contour        */\n    FT_UInt  end_point;     /* end point (last+1) of contour */\n\n    FT_UInt  first_touched; /* first touched point in contour   */\n    FT_UInt  cur_touched;   /* current touched point in contour */\n\n    FT_UInt  point;         /* current point   */\n    FT_UInt  contour;       /* current contour */\n\n\n#ifdef TT_SUPPORT_SUBPIXEL_HINTING_MINIMAL\n",
        ),
        (
            "thirdparty/freetype/src/truetype/ttinterp.c",
            "    /* XXX: UNDOCUMENTED! SHZ doesn't move the phantom points.     */\n    /*      Twilight zone has no real contours, so use `n_points'. */\n    /*      Normal zone's `n_points' includes phantoms, so must    */\n    /*      use end of last contour.                               */\n    if ( exc->GS.gep2 == 0 )\n      limit = exc->zp2.n_points;\n    else if ( exc->GS.gep2 == 1 && exc->zp2.n_contours > 0 )\n      limit = exc->zp2.contours[exc->zp2.n_contours - 1] + 1;\n    else\n      limit = 0;\n\n    /* XXX: UNDOCUMENTED! SHZ doesn't touch the points */\n    for ( i = 0; i < limit; i++ )\n",
            "    /* XXX: UNDOCUMENTED! SHZ doesn't move the phantom points.     */\n    /*      Twilight zone has no real contours, so use `n_points'. */\n    /*      Normal zone's `n_points' includes phantoms, so must    */\n    /*      subtract them.                                         */\n    if ( exc->GS.gep2 == 0 )\n      limit = exc->zp2.n_points;\n    else\n      limit = exc->zp2.n_points - 4U;\n\n    /* XXX: UNDOCUMENTED! SHZ doesn't touch the points */\n    for ( i = 0; i < limit; i++ )\n",
        ),
        (
            "thirdparty/freetype/src/truetype/ttinterp.c",
            "    /*      subtract them.                                         */\n    if ( exc->GS.gep2 == 0 )\n      limit = exc->zp2.n_points;\n    else\n      limit = exc->zp2.n_points - 4U;\n\n    /* XXX: UNDOCUMENTED! SHZ doesn't touch the points */\n    for ( i = 0; i < limit; i++ )\n",
            "    /*      subtract them.                                         */\n    if ( exc->GS.gep2 == 0 )\n      limit = exc->zp2.n_points;\n    else if ( exc->zp2.n_points > 4U )\n      limit = exc->zp2.n_points - 4U;\n    else\n      return;\n\n    /* XXX: UNDOCUMENTED! SHZ doesn't touch the points */\n    for ( i = 0; i < limit; i++ )\n",
        ),
        (
            "thirdparty/freetype/src/truetype/ttinterp.c",
            "        ARRAY_BOUND_ERROR;\n    }\n    else\n      exc->cvt[I] = FT_MulFix( args[1], exc->tt_metrics.scale );\n  }\n\n\n",
            "        ARRAY_BOUND_ERROR;\n    }\n    else\n    {\n      Modify_CVT_Check( exc );\n      if ( exc->error )\n        return;\n\n      exc->cvt[I] = FT_MulFix( args[1], exc->tt_metrics.scale );\n    }\n  }\n\n\n",
        ),
        (
            "thirdparty/freetype/src/cid/cidload.c",
            "    CID_Subrs      subr;\n    FT_UInt        max_offsets = 0;\n    FT_ULong*      offsets = NULL;\n    PSAux_Service  psaux = (PSAux_Service)face->psaux;\n\n\n",
            "    CID_Subrs      subr;\n    FT_UInt        max_offsets = 0;\n    FT_ULong*      offsets = NULL;\n    FT_ULong       cum_len = 0;\n    PSAux_Service  psaux = (PSAux_Service)face->psaux;\n\n\n",
        ),
        (
            "thirdparty/freetype/src/cid/cidload.c",
            "      /* now, compute the size of subrs charstrings, */\n      /* allocate, and read them                     */\n      data_len = offsets[num_subrs] - offsets[0];\n\n      if ( FT_QNEW_ARRAY( subr->code, num_subrs + 1 ) ||\n           FT_QALLOC( subr->code[0], data_len )       )\n",
            "      /* now, compute the size of subrs charstrings, */\n      /* allocate, and read them                     */\n      data_len = offsets[num_subrs] - offsets[0];\n      cum_len += data_len;\n\n      if ( cum_len / 4 > stream->size )\n      {\n        FT_ERROR(( \"cid_read_subrs: excessive overlaps\\n\" ));\n        error = FT_THROW( Invalid_File_Format );\n        goto Fail;\n      }\n\n      if ( FT_QNEW_ARRAY( subr->code, num_subrs + 1 ) ||\n           FT_QALLOC( subr->code[0], data_len )       )\n",
        ),
        (
            "thirdparty/freetype/include/freetype/internal/ftcalc.h",
            "#endif /* FT_INT64 */\n\n\n  /**************************************************************************\n   *\n   * FT_MulDiv() and FT_MulFix() are declared in freetype.h.\n",
            "#endif /* FT_INT64 */\n\n\n#define ULABS( a )  ( a >= 0 ? (FT_ULong)a : 0UL - (FT_ULong)a )\n#define  UABS( a )  ( a >= 0 ?  (FT_UInt)a : 0U  -  (FT_UInt)a )\n\n\n  /**************************************************************************\n   *\n   * FT_MulDiv() and FT_MulFix() are declared in freetype.h.\n",
        ),
        (
            "thirdparty/freetype/src/base/ftcalc.c",
            "  FT_Matrix_Check( const FT_Matrix*  matrix )\n  {\n    FT_Fixed  xx, xy, yx, yy;\n    FT_Fixed  val;\n    FT_Int    shift;\n    FT_ULong  temp1, temp2;\n\n\n    if ( !matrix )\n",
            "  FT_Matrix_Check( const FT_Matrix*  matrix )\n  {\n    FT_Fixed  xx, xy, yx, yy;\n    FT_ULong  val, abs_det, frob_sq;\n    FT_Int    shift;\n\n\n    if ( !matrix )\n",
        ),
        (
            "thirdparty/freetype/src/base/ftcalc.c",
            "    xy  = matrix->xy;\n    yx  = matrix->yx;\n    yy  = matrix->yy;\n    val = FT_ABS( xx ) | FT_ABS( xy ) | FT_ABS( yx ) | FT_ABS( yy );\n\n    /* we only handle non-zero 32-bit values */\n    if ( !val || val > 0x7FFFFFFFL )\n      return 0;\n\n    /* Scale matrix to avoid the temp1 overflow, which is */\n    /* more stringent than avoiding the temp2 overflow.   */\n\n    shift = FT_MSB( val ) - 12;\n\n    if ( shift > 0 )\n    {\n",
            "    xy  = matrix->xy;\n    yx  = matrix->yx;\n    yy  = matrix->yy;\n    val = ULABS( xx ) | ULABS( xy ) | ULABS( yx ) | ULABS( yy );\n\n    /* we only handle non-zero 32-bit values */\n    if ( !val || val > 0x7FFFFFFFUL )\n      return 0;\n\n    /* Scale matrix to avoid overflows */\n    shift = FT_MSB( val ) - 4 * sizeof ( FT_ULong ) - 2;\n\n    if ( shift > 0 )\n    {\n",
        ),
        (
            "thirdparty/freetype/src/base/ftcalc.c",
            "      yy >>= shift;\n    }\n\n    temp1 = 32U * (FT_ULong)FT_ABS( xx * yy - xy * yx );\n    temp2 = (FT_ULong)( xx * xx ) + (FT_ULong)( xy * xy ) +\n            (FT_ULong)( yx * yx ) + (FT_ULong)( yy * yy );\n\n    if ( temp1 <= temp2 )\n      return 0;\n\n    return 1;\n",
            "      yy >>= shift;\n    }\n\n    abs_det = ULABS( xx * yy - xy * yx );\n    frob_sq = (FT_ULong)( xx * xx ) + (FT_ULong)( xy * xy ) +\n              (FT_ULong)( yx * yx ) + (FT_ULong)( yy * yy );\n\n    if ( abs_det <= frob_sq / 32 )\n      return 0;\n\n    return 1;\n",
        ),
        (
            "thirdparty/freetype/src/base/ftcalc.c",
            "    FT_Int    result;\n\n\n    if ( (FT_ULong)FT_ABS( in_x ) + (FT_ULong)FT_ABS( out_y ) <= 92681UL )\n    {\n      z1.lo = (FT_UInt32)in_x * (FT_UInt32)out_y;\n      z1.hi = (FT_UInt32)( (FT_Int32)z1.lo >> 31 );  /* sign-expansion */\n",
            "    FT_Int    result;\n\n\n    if ( ULABS( in_x ) + ULABS( out_y ) <= 92681UL )\n    {\n      z1.lo = (FT_UInt32)in_x * (FT_UInt32)out_y;\n      z1.hi = (FT_UInt32)( (FT_Int32)z1.lo >> 31 );  /* sign-expansion */\n",
        ),
        (
            "thirdparty/freetype/src/base/ftcalc.c",
            "    else\n      ft_multo64( (FT_UInt32)in_x, (FT_UInt32)out_y, &z1 );\n\n    if ( (FT_ULong)FT_ABS( in_y ) + (FT_ULong)FT_ABS( out_x ) <= 92681UL )\n    {\n      z2.lo = (FT_UInt32)in_y * (FT_UInt32)out_x;\n      z2.hi = (FT_UInt32)( (FT_Int32)z2.lo >> 31 );  /* sign-expansion */\n",
            "    else\n      ft_multo64( (FT_UInt32)in_x, (FT_UInt32)out_y, &z1 );\n\n    if ( ULABS( in_y ) + ULABS( out_x ) <= 92681UL )\n    {\n      z2.lo = (FT_UInt32)in_y * (FT_UInt32)out_x;\n      z2.hi = (FT_UInt32)( (FT_Int32)z2.lo >> 31 );  /* sign-expansion */\n",
        ),
        (
            "thirdparty/freetype/src/base/ftcalc.c",
            "      return 0;\n\n    /* Scale matrix to avoid overflows */\n    shift = FT_MSB( val ) - 4 * sizeof ( FT_ULong ) - 2;\n\n    if ( shift > 0 )\n    {\n",
            "      return 0;\n\n    /* Scale matrix to avoid overflows */\n    shift = FT_MSB( val ) - 4 * sizeof ( FT_ULong ) + 2;\n\n    if ( shift > 0 )\n    {\n",
        ),
        (
            "thirdparty/freetype/src/psaux/psstack.c",
            "                     CF2_UInt   idx,\n                     CF2_Fixed  val )\n  {\n    if ( idx > cf2_stack_count( stack ) )\n    {\n      CF2_SET_ERROR( stack->error, Stack_Overflow );\n      return;\n",
            "                     CF2_UInt   idx,\n                     CF2_Fixed  val )\n  {\n    if ( idx >= cf2_stack_count( stack ) )\n    {\n      CF2_SET_ERROR( stack->error, Stack_Overflow );\n      return;\n",
        ),
        (
            "thirdparty/freetype/src/psaux/psintrp.c",
            "          FT_TRACE4(( \"%s\", op1 == cf2_cmdCALLGSUBR ? \" callgsubr\"\n                                                    : \" callsubr\" ));\n\n          if ( ( !font->isT1 && charstringIndex > CF2_MAX_SUBR )       ||\n               (  font->isT1 && charstringIndex > T1_MAX_SUBRS_CALLS ) )\n          {\n            /* max subr plus one for charstring */\n            lastError = FT_THROW( Invalid_Glyph_Format );\n",
            "          FT_TRACE4(( \"%s\", op1 == cf2_cmdCALLGSUBR ? \" callgsubr\"\n                                                    : \" callsubr\" ));\n\n          if ( ( !font->isT1 && charstringIndex >= CF2_MAX_SUBR )       ||\n               (  font->isT1 && charstringIndex >= T1_MAX_SUBRS_CALLS ) )\n          {\n            /* max subr plus one for charstring */\n            lastError = FT_THROW( Invalid_Glyph_Format );\n",
        ),
        (
            "thirdparty/freetype/src/psaux/psintrp.c",
            "  /* Blend numOperands on the stack,                */\n  /* store results into the first numBlends values, */\n  /* then pop remaining arguments.                  */\n  static void\n  cf2_doBlend( const CFF_Blend  blend,\n               CF2_Stack        opStack,\n               CF2_UInt         numBlends )\n",
            "  /* Blend numOperands on the stack,                */\n  /* store results into the first numBlends values, */\n  /* then pop remaining arguments.                  */\n  static FT_Error\n  cf2_doBlend( const CFF_Blend  blend,\n               CF2_Stack        opStack,\n               CF2_UInt         numBlends )\n",
        ),
        (
            "thirdparty/freetype/src/psaux/psintrp.c",
            "    CF2_UInt  base;\n    CF2_UInt  i, j;\n    CF2_UInt  numOperands = (CF2_UInt)( numBlends * blend->lenBV );\n\n\n    base  = cf2_stack_count( opStack ) - numOperands;\n    delta = base + numBlends;\n\n    FT_TRACE6(( \" (\" ));\n",
            "    CF2_UInt  base;\n    CF2_UInt  i, j;\n    CF2_UInt  numOperands = (CF2_UInt)( numBlends * blend->lenBV );\n    CF2_UInt  count       = cf2_stack_count( opStack );\n\n\n    if ( numOperands > count )\n      return FT_THROW( Stack_Underflow );\n\n    base  = count - numOperands;\n    delta = base + numBlends;\n\n    FT_TRACE6(( \" (\" ));\n",
        ),
        (
            "thirdparty/freetype/src/psaux/psintrp.c",
            "\n    /* leave only `numBlends' results on stack */\n    cf2_stack_pop( opStack, numOperands - numBlends );\n  }\n\n\n",
            "\n    /* leave only `numBlends' results on stack */\n    cf2_stack_pop( opStack, numOperands - numBlends );\n\n    return FT_Err_Ok;\n  }\n\n\n",
        ),
        (
            "thirdparty/freetype/src/psaux/psintrp.c",
            "\n          /* do the blend */\n          numBlends = (FT_UInt)cf2_stack_popInt( opStack );\n          if ( numBlends > stackSize )\n          {\n            lastError = FT_THROW( Invalid_Glyph_Format );\n            goto exit;\n          }\n\n          cf2_doBlend( &font->blend, opStack, numBlends );\n\n          font->blend.usedBV = TRUE;\n        }\n",
            "\n          /* do the blend */\n          numBlends = (FT_UInt)cf2_stack_popInt( opStack );\n\n          lastError = cf2_doBlend( &font->blend, opStack, numBlends );\n          if ( lastError )\n            goto exit;\n\n          font->blend.usedBV = TRUE;\n        }\n",
        ),
        (
            "thirdparty/freetype/src/psaux/psintrp.c",
            "    CF2_UInt  delta;\n    CF2_UInt  base;\n    CF2_UInt  i, j;\n    CF2_UInt  numOperands = (CF2_UInt)( numBlends * blend->lenBV );\n    CF2_UInt  count       = cf2_stack_count( opStack );\n\n\n    if ( numOperands > count )\n      return FT_THROW( Stack_Underflow );\n\n",
            "    CF2_UInt  delta;\n    CF2_UInt  base;\n    CF2_UInt  i, j;\n    CF2_UInt  numOperands;\n    CF2_UInt  count = cf2_stack_count( opStack );\n\n\n    if ( blend->lenBV == 0                ||\n         numBlends > count / blend->lenBV )\n      return FT_THROW( Stack_Underflow );\n\n    numOperands = numBlends * blend->lenBV;  /* cannot overflow here */\n    if ( numOperands > count )\n      return FT_THROW( Stack_Underflow );\n\n",
        ),
        (
            "thirdparty/freetype/src/psaux/psstack.c",
            "    CF2_StackNumber  last = { { 0 }, CF2_NumberInt };\n\n    CF2_Int  start_idx, idx, i;\n\n\n    if ( count < 2 )\n",
            "    CF2_StackNumber  last = { { 0 }, CF2_NumberInt };\n\n    CF2_Int  start_idx, idx, i;\n    CF2_Int  offset;\n\n\n    if ( count < 2 )\n",
        ),
        (
            "thirdparty/freetype/src/psaux/psstack.c",
            "      return;\n    }\n\n    /* before C99 it is implementation-defined whether    */\n    /* the result of `%' is negative if the first operand */\n    /* is negative                                        */\n",
            "      return;\n    }\n\n    offset = (CF2_Int)cf2_stack_count( stack ) - count;\n\n    /* before C99 it is implementation-defined whether    */\n    /* the result of `%' is negative if the first operand */\n    /* is negative                                        */\n",
        ),
        (
            "thirdparty/freetype/src/psaux/psstack.c",
            "      {\n        start_idx++;\n        idx  = start_idx;\n        last = stack->buffer[idx];\n      }\n\n      idx += shift;\n",
            "      {\n        start_idx++;\n        idx  = start_idx;\n        last = stack->buffer[idx + offset];\n      }\n\n      idx += shift;\n",
        ),
        (
            "thirdparty/freetype/src/psaux/psstack.c",
            "      else if ( idx < 0 )\n        idx += count;\n\n      tmp                = stack->buffer[idx];\n      stack->buffer[idx] = last;\n      last               = tmp;\n    }\n  }\n\n",
            "      else if ( idx < 0 )\n        idx += count;\n\n      tmp                         = stack->buffer[idx + offset];\n      stack->buffer[idx + offset] = last;\n      last                        = tmp;\n    }\n  }\n\n",
        ),
    ];

    // Some upstream Tesseract fixes only change code that MuPDF does not build:
    // the legacy engine (DISABLED_LEGACY_ENGINE; src/classify/intproto.cpp,
    // normmatch.cpp and adaptmatch.cpp are not in the source lists) and the
    // FAST_FLOAT branches. They are left out of tesseract_patches. Fail if MuPDF
    // starts building that code, so that they are looked at again.
    let makelists = build_dir.join("Makelists");
    let vcxproj = build_dir.join("platform/win32/libtesseract.vcxproj");
    let mut legacy_off = true;
    for path in [&makelists, &vcxproj] {
        let text = fs::read_to_string(path)
            .map_err(|e| format!("Unable to read {}: {e}", path.display()))?;
        if ["FAST_FLOAT", "intproto.cpp", "normmatch.cpp", "adaptmatch.cpp"]
            .iter()
            .any(|name| text.contains(name))
        {
            legacy_off = false;
        }
        if path == &makelists {
            legacy_off &= text.contains("TESSERACT_DEFINES += -DDISABLED_LEGACY_ENGINE");
        } else {
            let defines: Vec<&str> = text
                .lines()
                .filter(|line| line.contains("<PreprocessorDefinitions>"))
                .collect();
            legacy_off &= !defines.is_empty()
                && defines.iter().all(|line| line.contains("DISABLED_LEGACY_ENGINE"));
        }
    }
    if !legacy_off {
        Err("Unable to patch Tesseract: MuPDF now builds its legacy engine or FAST_FLOAT code; check the upstream fixes left out of tesseract_patches".to_string())?;
    }

    // MuPDF 1.28 text extraction fixes, not fixed upstream (1.28.x branch or master) as of
    // 2026-11-20:
    // - pdf-op-run.c: a glyph drawn with text render mode 4, 5 or 6 also goes into the
    //   clip that ET applies, and the structured text device took that copy as text a
    //   second time. The clip copy of a drawn glyph gets no Unicode value (-1, as for the
    //   extra glyphs of one character), which the text device skips; glyphs that are only
    //   clipped (mode 7) keep theirs.
    // - pdf-type3.c: 1.28 reads a g name that is not a standard glyph name (g0024, g41) as a
    //   hexadecimal TrueType glyph index, a guess for old Ghostscript TrueType subsets. Type 3
    //   fonts that name their glyphs after the character code (g32, g33, ...) then got wrong
    //   letters instead of the "character code as ASCII" fallback. Type 3 fonts skip that
    //   guess again, as in 1.27; other fonts keep it.
    let text_patches: &[(&str, &str, &str)] = &[
        (
            "source/pdf/pdf-op-run.c",
            "\t\tpdf_tos_accumulate_clip(ctx, &pr->tos);\n\t\tfz_show_glyph_aux(ctx, pr->tos.clip_text, fontdesc->font, trm, adv, gid, ucsbuf[0], cid, fontdesc->wmode, pr->bidi, FZ_BIDI_NEUTRAL, lang);\n",
            "\t\tpdf_tos_accumulate_clip(ctx, &pr->tos);\n\t\tfz_show_glyph_aux(ctx, pr->tos.clip_text, fontdesc->font, trm, adv, gid, pr->tos.text_mode == 7 ? ucsbuf[0] : -1, cid, fontdesc->wmode, pr->bidi, FZ_BIDI_NEUTRAL, lang);\n",
        ),
        (
            "source/pdf/pdf-type3.c",
            "\t\t\tfor (i = 32; i < 127; ++i)\n\t\t\t\tif (fontdesc->cid_to_ucs[i] == FZ_REPLACEMENT_CHARACTER)\n\t\t\t\t\tfontdesc->cid_to_ucs[i] = i;\n",
            "\t\t\tfor (i = 0; i < 256; ++i)\n\t\t\t{\n\t\t\t\tif (estrings[i] && estrings[i][0] == 'g')\n\t\t\t\t{\n\t\t\t\t\tchar gname[64];\n\t\t\t\t\tchar *end;\n\t\t\t\t\tfz_strlcpy(gname, estrings[i], sizeof gname);\n\t\t\t\t\tend = strchr(gname, '.');\n\t\t\t\t\tif (end)\n\t\t\t\t\t\t*end = 0;\n\t\t\t\t\tend = strchr(gname, '_');\n\t\t\t\t\tif (end)\n\t\t\t\t\t\t*end = 0;\n\t\t\t\t\tif (!fz_unicode_from_glyph_name_strict(gname))\n\t\t\t\t\t\tfontdesc->cid_to_ucs[i] = FZ_REPLACEMENT_CHARACTER;\n\t\t\t\t}\n\t\t\t\tif (i >= 32 && i < 127 && fontdesc->cid_to_ucs[i] == FZ_REPLACEMENT_CHARACTER)\n\t\t\t\t\tfontdesc->cid_to_ucs[i] = i;\n\t\t\t}\n",
        ),
    ];

    // Limits for tiling patterns that cannot finish painting (winner0988/Pdf-reader#550), not
    // in upstream (1.28.x branch or master) as of 2026-11-22. Such a page now fails at once with
    // FZ_ERROR_LIMIT instead of running into the worker's 30 s render timeout:
    // - draw-device.c: 1.28 doubles a step that moves less than 1/16 pixel, but a large tile
    //   with a small step is still painted once per repeat, about tile area * clip area /
    //   cell area pixels in all. A pattern whose painting would take far longer than the
    //   timeout is refused before the loop starts.
    // - pdf-op-run.c: a pattern that uses blending (or a device that asks for no tiling) runs
    //   the pattern's content once per repeat, with no guard at all against tiny steps. More
    //   repeats than could run within the timeout are refused.
    let tiling_patches: &[(&str, &str, &str)] = &[
        (
            "source/fitz/draw-device.c",
            "\t\tif (abandon)\n\t\t\tbreak;\n\n\t\tdest = fz_new_pixmap_from_pixmap(ctx, state[1].dest, NULL);\n",
            concat!(
                "\t\tif (abandon)\n\t\t\tbreak;\n\n",
                "\t\t/* pdf-reader #550: refuse a tiling pattern that would take far longer to paint\n",
                "\t\t * than any page should. Every repeat below paints the part of the tile that\n",
                "\t\t * falls inside the clip; summed over all repeats that is about tile area * clip\n",
                "\t\t * area / cell area pixels (the cell being what one step in x and one in y span\n",
                "\t\t * on the page), and never more than every repeat painting all of its overlap.\n",
                "\t\t * Rows and repeats cost a little more each. Each weight is below the cost\n",
                "\t\t * measured on the fastest case (a transparent tile), so a pattern refused here\n",
                "\t\t * could not have been painted within the worker's 30 s render timeout. */\n",
                "\t\t{\n",
                "\t\t\tfz_irect clip = fz_intersect_irect(state[0].scissor, fz_pixmap_bbox(ctx, state[0].dest));\n",
                "\t\t\tdouble cw = clip.x1 > clip.x0 ? (double)clip.x1 - clip.x0 : 0;\n",
                "\t\t\tdouble ch = clip.y1 > clip.y0 ? (double)clip.y1 - clip.y0 : 0;\n",
                "\t\t\tdouble tw = state[1].dest->w;\n",
                "\t\t\tdouble th = state[1].dest->h;\n",
                "\t\t\tdouble repeats = x1 > x0 && y1 > y0 ? ((double)x1 - x0) * ((double)y1 - y0) : 0;\n",
                "\t\t\tdouble cell = fabs((double)ctm.a * ctm.d - (double)ctm.b * ctm.c) * fabs((double)xstep * ystep);\n",
                "\t\t\tdouble pixels = repeats * (tw < cw ? tw : cw) * (th < ch ? th : ch);\n",
                "\t\t\tdouble rows = repeats * (th < ch ? th : ch);\n",
                "\t\t\tdouble work;\n\n",
                "\t\t\tif (cell > 0 && tw * th * cw * ch / cell < pixels)\n",
                "\t\t\t\tpixels = tw * th * cw * ch / cell;\n",
                "\t\t\tif (cell > 0 && (tw + cw) * th * ch / cell < rows)\n",
                "\t\t\t\trows = (tw + cw) * th * ch / cell;\n",
                "\t\t\twork = (1 + (state[1].shape != NULL) + (state[1].group_alpha != NULL)) * (pixels + 8 * rows) + 64 * repeats;\n",
                "\t\t\tif (work > 274877906944.0) /* 2^38 */\n",
                "\t\t\t\tfz_throw(ctx, FZ_ERROR_LIMIT, \"tiling pattern needs too much painting\");\n",
                "\t\t}\n\n",
                "\t\tdest = fz_new_pixmap_from_pixmap(ctx, state[1].dest, NULL);\n",
            ),
        ),
        (
            "source/pdf/pdf-op-run.c",
            "\t\tint x, y;\n\n\t\t/* When calculating the number of tiles required, we adjust by\n",
            concat!(
                "\t\tint x, y;\n\n",
                "\t\t/* pdf-reader #550: here the pattern's content runs once per repeat, with no guard\n",
                "\t\t * against tiny steps (this path is taken when the pattern uses blending, or the\n",
                "\t\t * device asks for no tiling). Even empty content costs about a microsecond a\n",
                "\t\t * repeat, so refuse more repeats than could run within the worker's 30 s render\n",
                "\t\t * timeout. The count comes from the unrounded values, before they become ints. */\n",
                "\t\tif (((double)fx1 - fx0 + 1) * ((double)fy1 - fy0 + 1) > 67108864.0) /* 2^26 */\n",
                "\t\t\tfz_throw(ctx, FZ_ERROR_LIMIT, \"tiling pattern needs too many repeats\");\n\n",
                "\t\t/* When calculating the number of tiles required, we adjust by\n",
            ),
        ),
    ];

    for &(rel_path, old, new) in tesseract_patches
        .iter()
        .chain(freetype_patches)
        .chain(text_patches)
        .chain(tiling_patches)
    {
        let path = build_dir.join(rel_path);
        let mut source = fs::read_to_string(&path)
            .map_err(|e| format!("Unable to read {}: {e}", path.display()))?;
        source = source.replace("\r\n", "\n");
        let count = source.matches(old).count();
        if count == 1 {
            source = source.replacen(old, new, 1);
            fs::write(&path, source)
                .map_err(|e| format!("Unable to write {}: {e}", path.display()))?;
        } else if count == 0 && source.contains(new) {
            // Already applied.
        } else {
            Err(format!(
                "Unable to patch {rel_path} in {}: expected 1 occurrence of unpatched code, found {count}",
                path.display()
            ))?;
        }
    }

    Ok(())
}

fn build_wrapper() -> Result<()> {
    let mut build = cc::Build::new();
    for entry in fs::read_dir("wrapper")? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "c") {
            build.file(&path);
        }
    }
    build.include("mupdf/include").include("wrapper");
    build.try_compile("mupdf-wrapper")?;
    Ok(())
}

fn find_clang_sysroot(target: &Target) -> Result<Option<String>> {
    if target.os == "emscripten" {
        let sdk = env::var("EMSDK").map_err(|e| match e {
            env::VarError::NotPresent => {
                "Using emscripten requires the EMSDK environment variable to be set".to_owned()
            }
            _ => {
                format!("Invalid EMSDK environment variable: {e}")
            }
        })?;

        let mut sysroot = PathBuf::from(sdk);
        sysroot.push("upstream/emscripten/cache/sysroot");
        let sysroot = sysroot.into_os_string().into_string().unwrap();
        return Ok(Some(sysroot));
    }

    Ok(None)
}

fn generate_bindings(target: &Target, path: &Path, sysroot: Option<String>) -> Result<()> {
    let mut builder = bindgen::builder();

    if let Some(sysroot) = sysroot {
        builder = builder.clang_arg("--sysroot").clang_arg(sysroot);
    }

    if target.os == "emscripten" {
        builder = builder.clang_arg("-fvisibility=default");
    }

    builder = builder
        .clang_arg("-Imupdf/include")
        .clang_arg("-Iwrapper")
        .header("wrapper/wrapper.h");

    for entry in fs::read_dir("wrapper")? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "c") {
            builder = builder.header(path.to_str().unwrap());
        }
    }

    builder = builder
        .allowlist_recursively(false)
        .allowlist_type("wchar_t")
        .allowlist_type("FILE")
        .opaque_type("FILE")
        .allowlist_item("max_align_t")
        .opaque_type("max_align_t");

    builder = builder
        .allowlist_item("fz_.*")
        .allowlist_item("FZ_.*")
        .allowlist_item("pdf_.*")
        .allowlist_item("PDF_.*")
        .allowlist_type("cmap_splay")
        .allowlist_item("ucdn_.*")
        .allowlist_item("UCDN_.*")
        .allowlist_item("Memento_.*")
        .allowlist_item("mupdf_.*");

    // remove va_list functions as for all of these versions using ... exist
    builder = builder
        .blocklist_function("Memento_vasprintf") // Memento_asprintf
        .blocklist_function("fz_vthrow") // fz_throw
        .blocklist_function("fz_vwarn") // fz_warn
        .blocklist_function("fz_vlog_error_printf") // fz_log_error_printf
        .blocklist_function("fz_append_vprintf") // fz_append_printf
        .blocklist_function("fz_write_vprintf") // fz_write_printf
        .blocklist_function("fz_vsnprintf") // fz_snprintf
        .blocklist_function("fz_format_string"); // mupdf_format_string

    // build config
    builder = builder
        .blocklist_var("FZ_VERSION.*")
        .blocklist_var("FZ_ENABLE_.*")
        .blocklist_var("FZ_PLOTTERS_.*");

    // internal implementation details, considered private
    builder = builder
        .blocklist_item("fz_jmp_buf")
        .blocklist_function("fz_var_imp")
        .blocklist_function("fz_push_try")
        .blocklist_function("fz_do_.*")
        .blocklist_var("FZ_JMPBUF_ALIGN")
        .blocklist_type("fz_error_stack_slot")
        .blocklist_type("fz_error_context")
        .blocklist_type("fz_warn_context")
        .blocklist_type("fz_aa_context")
        .blocklist_type("fz_activity_.*")
        .blocklist_function("fz_register_activity_logger")
        .opaque_type("fz_context")
        .blocklist_type("fz_new_context_imp")
        .blocklist_type("fz_lock")
        .blocklist_type("fz_unlock");

    builder = builder
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .parse_callbacks(Box::new(DocsCallbacks::default()));

    #[cfg(feature = "zerocopy")]
    {
        builder = builder.parse_callbacks(Box::new(ZerocopyDeriveCallbacks));
    }

    builder
        .prepend_enum_name(false)
        .use_core()
        .generate()?
        .write_to_file(path)?;

    Ok(())
}

// see https://github.com/ArtifexSoftware/mupdf/blob/master/source/fitz/noto.c
const FONTS: [&str; 6] = [
    "TOFU",
    "TOFU_CJK",
    "TOFU_NOTO",
    "TOFU_SYMBOL",
    "TOFU_EMOJI",
    "TOFU_SIL",
];

const BASE14_FONT_PRUNE_FLAG: &str = "TOFU_BASE14";

enum Build {
    Make(Make),
    Msbuild(Msbuild),
}

impl Build {
    fn new(target: &Target) -> Self {
        if target.env == "msvc" {
            Self::Msbuild(Msbuild::default())
        } else {
            Self::Make(Make::default())
        }
    }

    fn define(&mut self, var: &str, val: &str) {
        match self {
            Self::Make(m) => m.define(var, val),
            Self::Msbuild(m) => m.define(var, val),
        };
    }

    fn define_bool(&mut self, var: &str, val: bool) {
        self.define(var, if val { "1" } else { "0" });
    }

    fn make_bool(&mut self, var: &str, val: bool) {
        if let Self::Make(m) = self {
            m.make_bool(var, val);
        }
    }

    fn fz_enable(&mut self, name: &str, enable: bool) {
        self.define_bool(&format!("FZ_ENABLE_{name}"), enable);
    }

    fn run(mut self, target: &Target, build_dir: &str) -> Result<()> {
        let xps = cfg!(feature = "xps");
        let svg = cfg!(feature = "svg");
        let cbz = cfg!(feature = "cbz");
        let img = cfg!(feature = "img");
        let html = cfg!(feature = "html");
        let epub = cfg!(feature = "epub");
        let js = cfg!(feature = "js");
        let brotli = cfg!(feature = "brotli");
        let docx_output = cfg!(feature = "docx-output");
        let tesseract = cfg!(feature = "tesseract");
        let zxingcpp = cfg!(feature = "zxingcpp");
        let libarchive = cfg!(feature = "libarchive");

        // gates #ifdef
        self.fz_enable("XPS", xps);
        self.fz_enable("SVG", svg);
        self.fz_enable("CBZ", cbz);
        self.fz_enable("IMG", img);
        self.fz_enable("HTML", html);
        self.fz_enable("EPUB", epub);
        self.fz_enable("JS", js);
        self.fz_enable("BROTLI", brotli);
        self.fz_enable("DOCX_OUTPUT", docx_output);
        // MuPDF 1.28 adds a Markdown reader (FZ_ENABLE_MD, on by default) that also
        // guesses Markdown from file contents. No feature of this crate asks for it.
        self.fz_enable("MD", false);

        // gates which features get built
        self.make_bool("xps", xps);
        self.make_bool("svg", svg);
        self.make_bool("mujs", js);
        self.make_bool("html", html || epub);
        self.make_bool("brotli", brotli);
        self.make_bool("extract", docx_output);
        self.make_bool("tesseract", tesseract);
        self.make_bool("barcode", zxingcpp);
        self.make_bool("archive", libarchive);

        if cfg!(feature = "all-fonts") {
            println!(
                "cargo:warning=mupdf-sys/all-fonts is deprecated and no longer compiles large fonts into mupdf-sys; use mupdf's runtime bundled font features instead"
            );
        }

        for font in &FONTS {
            // TOFU flags skip fonts when set to 1. Keep non-URW fonts out of
            // mupdf-sys so the crate remains below crates.io's package limit.
            self.define_bool(font, true);
        }

        if !cfg!(feature = "base14-fonts") {
            // Skip the URW base14 fonts too (warning: makes plain PDFs render
            // without text unless replacement fonts are provided at runtime,
            // e.g. via mupdf's FontLoader hooks).
            self.define_bool(BASE14_FONT_PRUNE_FLAG, true);
        }

        match self {
            Self::Make(m) => m.build(target, build_dir),
            Self::Msbuild(m) => m.build(target, build_dir),
        }
    }
}

struct Target {
    debug: bool,
    opt_level: String,

    arch: String,
    os: String,
    env: String,

    features: Vec<String>,
}

impl Target {
    fn from_cargo() -> Result<Self> {
        Ok(Self {
            debug: env::var_os("DEBUG").is_some_and(|s| s != "0" && s != "false"),
            opt_level: env::var("OPT_LEVEL")?,

            arch: env::var("CARGO_CFG_TARGET_ARCH")?,
            os: env::var("CARGO_CFG_TARGET_OS")?,
            env: env::var("CARGO_CFG_TARGET_ENV")?,

            features: env::var("CARGO_CFG_TARGET_FEATURE")
                .unwrap_or_default()
                .split(',')
                .filter(|feature| !feature.is_empty())
                .map(str::to_owned)
                .collect(),
        })
    }

    fn small_profile(&self) -> bool {
        !self.debug && matches!(&*self.opt_level, "s" | "z")
    }

    fn debug_profile(&self) -> bool {
        self.debug && !matches!(&*self.opt_level, "2" | "3")
    }
}

#[cfg(feature = "zerocopy")]
#[derive(Debug)]
struct ZerocopyDeriveCallbacks;

#[cfg(feature = "zerocopy")]
impl bindgen::callbacks::ParseCallbacks for ZerocopyDeriveCallbacks {
    fn add_derives(&self, info: &bindgen::callbacks::DeriveInfo<'_>) -> Vec<String> {
        const TYPES: [&str; 5] = ["fz_irect", "fz_matrix", "fz_point", "fz_quad", "fz_rect"];

        if TYPES.contains(&info.name) {
            vec![
                "zerocopy::FromBytes".to_owned(),
                "zerocopy::IntoBytes".to_owned(),
                "zerocopy::Immutable".to_owned(),
            ]
        } else {
            vec![]
        }
    }
}
