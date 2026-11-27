//! Removing single glyphs from a page's content with MuPDF's content filter:
//! [`pdf_filter_page_contents`] with a [`pdf_new_sanitize_filter`] whose `text_filter`
//! (in [`pdf_sanitize_filter_options`]) asks a Rust callback about every glyph.
//!
//! Unlike [`PdfPage::redact`], which runs the same filter, nothing else on the page changes:
//! `pdf_redact_page` also deletes every link and annotation that reaches into a redaction area
//! (`pdf-clean.c`, `pdf_apply_redaction_imp`), and removes any glyph that overlaps an area,
//! whatever lies above or below it. Here the callback decides, glyph by glyph, from the
//! characters, the position and how the glyph is painted (the text rendering mode, `Tr`).
//!
//! The glue that MuPDF calls is C (`mupdf-sys/wrapper/pdf_text_filter.c`): it creates the
//! sanitize filter for each content stream and turns a failed callback into a MuPDF exception,
//! so the exception's `longjmp` never crosses a Rust frame.
//!
//! [`pdf_filter_page_contents`]: mupdf_sys::pdf_filter_page_contents
//! [`pdf_new_sanitize_filter`]: mupdf_sys::pdf_new_sanitize_filter
//! [`pdf_sanitize_filter_options`]: mupdf_sys::pdf_sanitize_filter_options

use std::ffi::{c_int, c_void};
use std::panic::{self, AssertUnwindSafe};
use std::slice;

use mupdf_sys::*;

use crate::pdf::PdfPage;
use crate::{context, Error, FFIWrapper, Matrix, Point, Quad, Rect};

/// The most characters MuPDF gives one glyph (`PDF_MRANGE_CAP`, the size of the buffer that
/// `filter_show_char` in `pdf-op-filter.c` passes).
const MAX_CHARS: usize = PDF_MRANGE_CAP as usize;

/// How a glyph is painted: the PDF text rendering mode (`Tr`, ISO 32000-2, 9.3.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TextRenderMode {
    /// `0 Tr`: filled.
    Fill = 0,
    /// `1 Tr`: stroked.
    Stroke = 1,
    /// `2 Tr`: filled, then stroked.
    FillStroke = 2,
    /// `3 Tr`: neither filled nor stroked, as in the text layer of a scanned page.
    Invisible = 3,
    /// `4 Tr`: filled and added to the clip path.
    FillClip = 4,
    /// `5 Tr`: stroked and added to the clip path.
    StrokeClip = 5,
    /// `6 Tr`: filled, stroked and added to the clip path.
    FillStrokeClip = 6,
    /// `7 Tr`: only added to the clip path. Not painted, but it decides what later content in
    /// the same clip shows, so removing it changes the page.
    Clip = 7,
}

impl TextRenderMode {
    /// The mode for a `Tr` operand; `None` outside 0 to 7.
    pub fn from_raw(value: i32) -> Option<Self> {
        Some(match value {
            0 => Self::Fill,
            1 => Self::Stroke,
            2 => Self::FillStroke,
            3 => Self::Invisible,
            4 => Self::FillClip,
            5 => Self::StrokeClip,
            6 => Self::FillStrokeClip,
            7 => Self::Clip,
            _ => return None,
        })
    }

    /// Whether the glyph is filled, stroked or both (modes 0 to 2 and 4 to 6).
    pub fn paints(self) -> bool {
        !matches!(self, Self::Invisible | Self::Clip)
    }

    /// Whether the glyph is added to the clip path (modes 4 to 7).
    pub fn clips(self) -> bool {
        matches!(
            self,
            Self::FillClip | Self::StrokeClip | Self::FillStrokeClip | Self::Clip
        )
    }
}

/// What the filter does with a glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TextFilterAction {
    /// The glyph stays where it is.
    Keep,
    /// The glyph is taken out of the content stream. The glyphs after it keep their positions.
    Remove,
}

/// The options of `pdf_filter_options` (`include/mupdf/pdf/interpret.h`) for
/// [`PdfPage::filter_text`].
///
/// The default instances forms and nothing else, as MuPDF's own redaction does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TextFilterOptions {
    /// Also filter the resources the page's content uses, after the page's content: patterns,
    /// soft mask groups and Type 3 glyph procedures, and Form XObjects when `instance_forms` is
    /// off. Each of them is changed in place, for every page that uses it, and its glyphs come
    /// to the callback in the stream's own space, not the page's. Cannot be combined with
    /// `instance_forms` (MuPDF asserts against it for Type 3 fonts).
    pub recurse: bool,
    /// Filter each Form XObject the page draws as a new copy made for this page: other pages that
    /// use the same form keep it as it was, and its glyphs come to the callback in page space.
    /// Without it (and without `recurse`), text in forms is not filtered at all.
    pub instance_forms: bool,
    /// Write binary data in the new content (such as inline images) as ASCII hex.
    pub ascii: bool,
    /// Do not change the document: the filter runs and the callback sees every glyph, but the
    /// result is dropped. Copies of forms made by `instance_forms` are still added to the
    /// document, referenced by nothing; saving with garbage collection drops them.
    pub no_update: bool,
    /// Write a newline after each operator in the new content.
    pub newlines: bool,
}

impl Default for TextFilterOptions {
    fn default() -> Self {
        Self {
            recurse: false,
            instance_forms: true,
            ascii: false,
            no_update: false,
            newlines: false,
        }
    }
}

/// One glyph the filter asks about.
///
/// Positions: [`bbox`](Self::bbox) is in glyph space; [`trm`](Self::trm) takes it to text space
/// and [`ctm`](Self::ctm) on to the page's PDF user space (origin at the bottom left of the
/// MediaBox, y up, before `/Rotate`). [`quad`](Self::quad) goes one step further, to the space of
/// [`TextPage`](crate::TextPage) and [`TextChar::quad`](crate::TextChar::quad).
#[derive(Debug)]
pub struct TextFilterGlyph<'a> {
    unicode: &'a [char],
    trm: Matrix,
    ctm: Matrix,
    bbox: Rect,
    page_ctm: &'a Matrix,
    render_mode: i32,
    fill_alpha: f32,
    stroke_alpha: f32,
    in_form_xobject: bool,
}

impl TextFilterGlyph<'_> {
    /// The characters the glyph stands for, from the font's `ToUnicode` or encoding: usually one,
    /// several for a ligature, U+FFFD when the font says nothing.
    pub fn unicode(&self) -> &[char] {
        self.unicode
    }

    /// The text rendering matrix: font size, horizontal scaling, rise and the text matrix.
    pub fn trm(&self) -> Matrix {
        self.trm.clone()
    }

    /// The current transformation matrix, from text space to the page's PDF user space. For a
    /// glyph in a form that `instance_forms` copied, it includes the form's `/Matrix` and the
    /// transformation where the page draws the form.
    pub fn ctm(&self) -> Matrix {
        self.ctm.clone()
    }

    /// The glyph's box in glyph space: from the origin to the advance, from the font's descender
    /// to its ascender (for horizontal writing).
    pub fn bbox(&self) -> Rect {
        self.bbox
    }

    /// The glyph's box in the space of [`TextPage`](crate::TextPage): `bbox` through `trm`, `ctm`
    /// and the page's transformation ([`PdfPage::ctm`]), corners named as in
    /// [`TextChar::quad`](crate::TextChar::quad) (upper = ascender side).
    pub fn quad(&self) -> Quad {
        let to_page = &(&self.trm * &self.ctm) * self.page_ctm;
        let corner = |x: f32, y: f32| {
            let (x, y) = to_page.transform_xy(x, y);
            Point::new(x, y)
        };
        let Rect { x0, y0, x1, y1 } = self.bbox;
        Quad::new(
            corner(x0, y1),
            corner(x1, y1),
            corner(x0, y0),
            corner(x1, y0),
        )
    }

    /// The smallest rectangle around [`quad`](Self::quad).
    pub fn bounds(&self) -> Rect {
        Rect::from(self.quad())
    }

    /// The text rendering mode in effect for the glyph; `None` for a `Tr` operand outside 0 to 7.
    pub fn render_mode(&self) -> Option<TextRenderMode> {
        TextRenderMode::from_raw(self.render_mode)
    }

    /// The `Tr` operand as the content stream gave it.
    pub fn raw_render_mode(&self) -> i32 {
        self.render_mode
    }

    /// The fill opacity (`ca` of the graphics state).
    pub fn fill_alpha(&self) -> f32 {
        self.fill_alpha
    }

    /// The stroke opacity (`CA` of the graphics state).
    pub fn stroke_alpha(&self) -> f32 {
        self.stroke_alpha
    }

    /// Whether the glyph leaves no mark, by MuPDF's own rule for redaction
    /// (`pdf_redact_invisible_text_filter` in `pdf-clean.c`): mode 3, or a fill or stroke with
    /// opacity 0. Modes 4 to 7 are never invisible: they clip. Opacity only covers the graphics
    /// state's `ca` and `CA`, not soft masks, blend modes or what is drawn over the glyph.
    pub fn is_invisible(&self) -> bool {
        match self.render_mode() {
            Some(TextRenderMode::Fill) => self.fill_alpha == 0.0,
            Some(TextRenderMode::Stroke) => self.stroke_alpha == 0.0,
            Some(TextRenderMode::FillStroke) => self.fill_alpha == 0.0 && self.stroke_alpha == 0.0,
            Some(TextRenderMode::Invisible) => true,
            _ => false,
        }
    }

    /// Whether the glyph is outside the page's own content stream: in a Form XObject (also one
    /// nested in another form) and, with [`TextFilterOptions::recurse`], in a pattern, a soft
    /// mask group or a Type 3 glyph procedure.
    pub fn in_form_xobject(&self) -> bool {
        self.in_form_xobject
    }
}

/// What the C glue's callback reaches through its `opaque` pointer.
struct FilterState<'f> {
    filter: &'f mut dyn FnMut(&TextFilterGlyph<'_>) -> TextFilterAction,
    page_ctm: Matrix,
    panicked: bool,
}

/// The callback for `mupdf_pdf_filter_page_text`: 0 keeps the glyph, 1 removes it, -1 says the
/// Rust callback panicked, and the C glue then aborts the filtering.
///
/// # Safety
///
/// `opaque` must point to a live `FilterState` that nothing else uses during the call, and
/// `ucsbuf` must hold `ucslen` values (or be null).
#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn filter_glyph(
    opaque: *mut c_void,
    ucsbuf: *const c_int,
    ucslen: c_int,
    trm: fz_matrix,
    ctm: fz_matrix,
    bbox: fz_rect,
    tr: c_int,
    ca: f32,
    stroke_ca: f32,
    in_form: c_int,
) -> c_int {
    // SAFETY: `opaque` is the `FilterState` that `PdfPage::filter_text` passes to
    // `mupdf_pdf_filter_page_text`. The glue calls back only during that call, on the same
    // thread, and `filter_text` does not touch the state until the call returns, so this is the
    // only reference to it.
    let state = unsafe { &mut *opaque.cast::<FilterState<'_>>() };
    if state.panicked {
        return -1;
    }

    let len = usize::try_from(ucslen).unwrap_or(0).min(MAX_CHARS);
    let mut chars = [char::REPLACEMENT_CHARACTER; MAX_CHARS];
    let len = if ucsbuf.is_null() {
        0
    } else {
        // SAFETY: `filter_show_char` (`pdf-op-filter.c`) passes its local `int
        // ucsbuf[PDF_MRANGE_CAP]` with `ucslen` values filled in; `len` is at most both.
        let codes = unsafe { slice::from_raw_parts(ucsbuf, len) };
        for (char, &code) in chars.iter_mut().zip(codes) {
            *char = u32::try_from(code)
                .ok()
                .and_then(char::from_u32)
                .unwrap_or(char::REPLACEMENT_CHARACTER);
        }
        len
    };

    let glyph = TextFilterGlyph {
        unicode: &chars[..len],
        trm: trm.into(),
        ctm: ctm.into(),
        bbox: bbox.into(),
        page_ctm: &state.page_ctm,
        render_mode: tr,
        fill_alpha: ca,
        stroke_alpha: stroke_ca,
        in_form_xobject: in_form != 0,
    };
    // A panic must not unwind into MuPDF's C code (that aborts the process): it becomes an error.
    let filter = &mut state.filter;
    match panic::catch_unwind(AssertUnwindSafe(|| filter(&glyph))) {
        Ok(TextFilterAction::Keep) => 0,
        Ok(TextFilterAction::Remove) => 1,
        Err(payload) => {
            state.panicked = true;
            // Dropping the payload runs its destructor, which could panic too.
            let _ = panic::catch_unwind(AssertUnwindSafe(move || drop(payload)));
            -1
        }
    }
}

impl PdfPage {
    /// Filters the page's content with MuPDF's sanitize filter, asking `filter` about every glyph
    /// in the order the content draws them, and removes the glyphs it answers
    /// [`TextFilterAction::Remove`] for. The other glyphs keep their positions; links,
    /// annotations, form fields, pictures and vector graphics are not touched. The content is
    /// rewritten even when nothing is removed (the filter normalizes the operators, with the
    /// same picture).
    ///
    /// `filter` must not change the document while the filter runs.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidArgument`] for `recurse` together with `instance_forms`.
    /// - [`Error::MuPdf`] when MuPDF fails, and when `filter` panics: the panic is caught, the
    ///   filtering stops and the page's content stays as it was. Copies of forms made by then stay
    ///   in the document, referenced by nothing; with `recurse`, resources already filtered stay
    ///   changed.
    pub fn filter_text<F>(&mut self, options: TextFilterOptions, mut filter: F) -> Result<(), Error>
    where
        F: FnMut(&TextFilterGlyph<'_>) -> TextFilterAction,
    {
        if options.recurse && options.instance_forms {
            return Err(Error::InvalidArgument(
                "recurse cannot be combined with instance_forms".to_owned(),
            ));
        }
        let mut state = FilterState {
            filter: &mut filter,
            page_ctm: self.ctm()?,
            panicked: false,
        };
        // SAFETY: the page pointer is valid for the lifetime of `self`, and `&mut self` keeps
        // other Rust code of this crate from using the page meanwhile. `state` outlives the call,
        // which is the only time the glue uses the pointer to it, and `filter_glyph` matches the
        // glue's `mupdf_text_filter_fn`. MuPDF's exceptions are caught by the glue and come back
        // as `err` through `ffi_try!`.
        let result = unsafe {
            ffi_try!(mupdf_pdf_filter_page_text(
                context(),
                self.as_mut_ptr(),
                c_int::from(options.recurse),
                c_int::from(options.instance_forms),
                c_int::from(options.ascii),
                c_int::from(options.no_update),
                c_int::from(options.newlines),
                Some(filter_glyph),
                (&raw mut state).cast::<c_void>()
            ))
        };
        if state.panicked {
            return Err(Error::MuPdf(crate::error::MuPdfError {
                code: FZ_ERROR_ABORT as i32,
                message: "text filter callback panicked".to_owned(),
            }));
        }
        result
    }
}
