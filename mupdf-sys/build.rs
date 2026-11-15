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

    // OpenJPEG 與 lcms2 的修正已經在 MuPDF 1.28.6 內附的版本裡。
    // Tesseract 5.5.0 lacks bounds checking on count and length fields read
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
    ];

    for &(rel_path, old, new) in tesseract_patches {
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
