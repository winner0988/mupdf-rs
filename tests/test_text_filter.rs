//! `PdfPage::filter_text`: MuPDF's content filter with a callback per glyph
//! (winner0988/Pdf-reader#748).

use mupdf::pdf::{
    PdfDocument, PdfPage, PdfRedactImageMethod, PdfRedactLineArtMethod, PdfRedactOptions,
    PdfRedactTextMethod, TextFilterAction, TextFilterOptions, TextRenderMode,
};
use mupdf::{Colorspace, Error, Matrix, Quad, Rect, TextPageFlags};

/// A PDF of the objects (object n is `objects[n - 1]`, object 1 the catalog), with a correct
/// cross-reference table.
fn pdf(objects: &[String]) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (index, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", index + 1).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

fn stream(dict: &str, content: &str) -> String {
    format!(
        "<< {dict} /Length {} >>\nstream\n{content}\nendstream",
        content.len()
    )
}

const HELVETICA: &str = "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>";

/// One Letter page with `content`, Helvetica as `/F1`, `extra_resources` and `annots`.
fn one_page(content: &str, extra_resources: &str, annots: &str, more: &[String]) -> PdfDocument {
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R \
             /Resources << /Font << /F1 5 0 R >> {extra_resources} >> {annots} >>"
        ),
        stream("", content),
        HELVETICA.to_owned(),
    ];
    objects.extend_from_slice(more);
    PdfDocument::from_bytes(&pdf(&objects)).unwrap()
}

fn text_of(page: &PdfPage) -> String {
    page.to_text_page(TextPageFlags::empty())
        .unwrap()
        .to_text()
        .unwrap()
}

/// Every character of the page with its quad, in reading order.
fn chars_of(page: &PdfPage) -> Vec<(char, Quad)> {
    let text = page.to_text_page(TextPageFlags::empty()).unwrap();
    let mut chars = Vec::new();
    for block in text.blocks() {
        for line in block.lines() {
            for ch in line.chars() {
                if let Some(c) = ch.char() {
                    chars.push((c, ch.quad()));
                }
            }
        }
    }
    chars
}

/// The box of the one hit of `needle` on the page.
fn hit(page: &PdfPage, needle: &str) -> Rect {
    let hits = page
        .to_text_page(TextPageFlags::empty())
        .unwrap()
        .search(needle)
        .unwrap();
    assert_eq!(hits.len(), 1, "{needle} once");
    Rect::from(hits[0].clone())
}

fn center(rect: Rect) -> (f32, f32) {
    ((rect.x0 + rect.x1) / 2.0, (rect.y0 + rect.y1) / 2.0)
}

fn inside(rect: Rect, (x, y): (f32, f32)) -> bool {
    rect.x0 <= x && x <= rect.x1 && rect.y0 <= y && y <= rect.y1
}

fn near(a: Rect, b: Rect, tolerance: f32) -> bool {
    (a.x0 - b.x0).abs() <= tolerance
        && (a.y0 - b.y0).abs() <= tolerance
        && (a.x1 - b.x1).abs() <= tolerance
        && (a.y1 - b.y1).abs() <= tolerance
}

/// The quad of the first `c` at or after character `from` of the page.
fn quad_of(chars: &[(char, Quad)], c: char, from: usize) -> Rect {
    let (_, quad) = chars[from..]
        .iter()
        .find(|(found, _)| *found == c)
        .unwrap_or_else(|| panic!("{c} on the page"));
    Rect::from(quad.clone())
}

fn render(page: &PdfPage) -> Vec<u8> {
    page.to_pixmap(&Matrix::IDENTITY, &Colorspace::device_rgb(), false, false)
        .unwrap()
        .samples()
        .to_vec()
}

#[test]
fn one_word_goes_and_the_others_stay_where_they_were() {
    let doc = one_page(
        "BT /F1 24 Tf 72 700 Td (Hello brave world) Tj ET\n\
         BT /F1 24 Tf 72 650 Td [(Keep) -500 (drop) -300 (this)] TJ ET",
        "",
        "",
        &[],
    );
    let mut page = doc.load_pdf_page(0).unwrap();
    let before = chars_of(&page);
    let brave = hit(&page, "brave");
    let drop_word = hit(&page, "drop");

    let mut removed = String::new();
    page.filter_text(TextFilterOptions::default(), |glyph| {
        let middle = center(glyph.bounds());
        if inside(brave, middle) || inside(drop_word, middle) {
            removed.extend(glyph.unicode());
            TextFilterAction::Remove
        } else {
            TextFilterAction::Keep
        }
    })
    .unwrap();

    assert_eq!(removed, "bravedrop");
    let text = text_of(&page);
    assert!(!text.contains("brave") && !text.contains("drop"), "{text}");
    assert!(text.contains("Hello") && text.contains("world"), "{text}");
    assert!(text.contains("Keep") && text.contains("this"), "{text}");
    // The words after the removed ones did not move, in a `Tj` string and in a `TJ` array.
    let after = chars_of(&page);
    for (c, from) in [('w', 6), ('t', 15)] {
        let old = quad_of(&before, c, from);
        let new = quad_of(&after, c, 0);
        assert!(near(old, new, 0.01), "{c}: {old:?} -> {new:?}");
    }
}

/// A word with a link and a text box (a FreeText annotation) over it.
fn word_under_a_link() -> PdfDocument {
    one_page(
        "BT /F1 24 Tf 72 700 Td (Hello brave world) Tj ET",
        "",
        "/Annots [6 0 R 7 0 R]",
        &[
            "<< /Type /Annot /Subtype /Link /Rect [130 690 210 725] /Border [0 0 0] \
             /A << /S /URI /URI (https://example.com/) >> >>"
                .to_owned(),
            "<< /Type /Annot /Subtype /FreeText /Rect [125 685 215 730] /F 4              /DA (/Helv 12 Tf 0 g) /Contents (A note) >>"
                .to_owned(),
        ],
    )
}

#[test]
fn links_and_annotations_over_a_removed_word_stay() {
    let doc = word_under_a_link();
    let mut page = doc.load_pdf_page(0).unwrap();
    let brave = hit(&page, "brave");
    assert_eq!(page.links().unwrap().count(), 1);
    assert_eq!(page.annotations().count(), 1);

    page.filter_text(TextFilterOptions::default(), |glyph| {
        if inside(brave, center(glyph.bounds())) {
            TextFilterAction::Remove
        } else {
            TextFilterAction::Keep
        }
    })
    .unwrap();

    assert!(!text_of(&page).contains("brave"));
    let links: Vec<_> = page.links().unwrap().collect();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].uri, "https://example.com/");
    assert_eq!(page.annotations().count(), 1);
}

/// The control for the test above: redaction removes the same word, and the link and the text
/// box with it (MuPDF 1.28 deletes the links and FreeText annotations an area touches).
#[test]
fn redacting_the_same_word_deletes_the_link_and_the_annotation() {
    let doc = word_under_a_link();
    let mut page = doc.load_pdf_page(0).unwrap();
    let brave = hit(&page, "brave");
    page.add_redact_annotation(brave).unwrap();
    page.redact_with_options(PdfRedactOptions {
        black_boxes: false,
        image_method: PdfRedactImageMethod::None,
        line_art: PdfRedactLineArtMethod::None,
        text: PdfRedactTextMethod::Remove,
    })
    .unwrap();

    assert!(!text_of(&page).contains("brave"));
    assert_eq!(page.links().unwrap().count(), 0);
    assert_eq!(page.annotations().count(), 0);
}

#[test]
fn text_in_form_xobjects_is_filtered_on_this_page_only() {
    // Page 1 draws text of its own and form 7; form 7 draws text and form 8. Page 2 draws form 7
    // too.
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".to_owned(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 5 0 R \
         /Resources << /Font << /F1 9 0 R >> /XObject << /Fm1 7 0 R >> >> >>"
            .to_owned(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 6 0 R \
         /Resources << /XObject << /Fm1 7 0 R >> >> >>"
            .to_owned(),
        stream(
            "",
            "BT /F1 18 Tf 72 700 Td (Page) Tj ET q 1 0 0 1 0 -100 cm /Fm1 Do Q",
        ),
        stream("", "/Fm1 Do"),
        stream(
            "/Type /XObject /Subtype /Form /BBox [0 -50 400 50] /Matrix [1 0 0 1 72 600] \
             /Resources << /Font << /F1 9 0 R >> /XObject << /Fm2 8 0 R >> >>",
            "BT /F1 18 Tf 0 0 Td (Outer) Tj ET /Fm2 Do",
        ),
        stream(
            "/Type /XObject /Subtype /Form /BBox [0 -50 400 50] \
             /Resources << /Font << /F1 9 0 R >> >>",
            "BT /F1 18 Tf 0 -40 Td (Inner) Tj ET",
        ),
        HELVETICA.to_owned(),
    ];
    let doc = PdfDocument::from_bytes(&pdf(&objects)).unwrap();
    let mut page = doc.load_pdf_page(0).unwrap();
    let before = chars_of(&page);
    assert_eq!(
        text_of(&page).split_whitespace().collect::<Vec<_>>(),
        ["Page", "Outer", "Inner"]
    );

    let mut seen = Vec::new();
    page.filter_text(TextFilterOptions::default(), |glyph| {
        seen.push((glyph.unicode()[0], glyph.in_form_xobject(), glyph.bounds()));
        if glyph.in_form_xobject() {
            TextFilterAction::Remove
        } else {
            TextFilterAction::Keep
        }
    })
    .unwrap();

    // "Page" is the page's own; "Outer" and "Inner" are in forms.
    assert_eq!(seen.len(), 14);
    assert!(seen[..4].iter().all(|(_, in_form, _)| !in_form));
    assert!(seen[4..].iter().all(|(_, in_form, _)| *in_form));
    assert_eq!(
        seen.iter().map(|(c, ..)| *c).collect::<String>(),
        "PageOuterInner"
    );
    // The boxes are in the space of the text page: through the form's matrix, the `cm` before
    // `Do` and the page's transformation.
    for (index, (c, _, bounds)) in seen.iter().enumerate() {
        let expected = Rect::from(before[index].1.clone());
        assert_eq!(before[index].0, *c);
        assert!(near(*bounds, expected, 0.5), "{c}: {bounds:?} {expected:?}");
    }

    assert_eq!(text_of(&page).trim(), "Page");
    // Page 2 draws the original form, which the filter did not change.
    let other = doc.load_pdf_page(1).unwrap();
    assert_eq!(
        text_of(&other).split_whitespace().collect::<Vec<_>>(),
        ["Outer", "Inner"]
    );
}

#[test]
fn invisible_clipping_and_visible_text_are_told_apart() {
    // Mode 0, mode 3, mode 7 (clipping a blue fill), mode 4, and mode 0 with fill opacity 0. `Tr`
    // stays in effect after `ET`, so each mode is set inside `q` and `Q`.
    let content = "BT /F1 24 Tf 72 700 Td (Shown) Tj ET\n\
                   q BT /F1 24 Tf 3 Tr 72 650 Td (Hidden) Tj ET Q\n\
                   q BT /F1 24 Tf 7 Tr 72 600 Td (Clipping) Tj ET 0 0 1 rg 0 0 612 792 re f Q\n\
                   q BT /F1 24 Tf 4 Tr 72 550 Td (Drawn) Tj ET Q\n\
                   q /GS0 gs BT /F1 24 Tf 72 500 Td (Clear) Tj ET Q";
    let sample = || {
        one_page(
            content,
            "/ExtGState << /GS0 6 0 R >>",
            "",
            &["<< /Type /ExtGState /ca 0 >>".to_owned()],
        )
    };

    let doc = sample();
    let mut page = doc.load_pdf_page(0).unwrap();
    let picture = render(&page);
    let mut modes = Vec::new();
    page.filter_text(TextFilterOptions::default(), |glyph| {
        modes.push((
            glyph.unicode()[0],
            glyph.render_mode(),
            glyph.is_invisible(),
        ));
        // What a fix of recognized text (Pdf-reader#749) removes: the invisible text layer.
        if glyph.render_mode() == Some(TextRenderMode::Invisible) {
            TextFilterAction::Remove
        } else {
            TextFilterAction::Keep
        }
    })
    .unwrap();

    let first = |c: char| {
        *modes
            .iter()
            .find(|(found, ..)| *found == c)
            .unwrap_or_else(|| panic!("{c}"))
    };
    assert_eq!(first('S'), ('S', Some(TextRenderMode::Fill), false));
    assert_eq!(first('H'), ('H', Some(TextRenderMode::Invisible), true));
    assert_eq!(first('C'), ('C', Some(TextRenderMode::Clip), false));
    assert_eq!(first('D'), ('D', Some(TextRenderMode::FillClip), false));
    // Fill opacity 0 counts as invisible by MuPDF's rule, but its mode is still 0.
    assert_eq!(
        modes.iter().filter(|(.., invisible)| *invisible).count(),
        6 + 5
    );
    assert!(TextRenderMode::Clip.clips() && !TextRenderMode::Clip.paints());
    assert!(TextRenderMode::FillClip.clips() && TextRenderMode::FillClip.paints());

    // Only the mode 3 text went, and the picture is the same to the byte.
    let text = text_of(&page);
    assert!(!text.contains("Hidden"), "{text}");
    for word in ["Shown", "Clipping", "Drawn"] {
        assert!(text.contains(word), "{word} in {text}");
    }
    assert_eq!(render(&page), picture);
    // What the content has left, text with opacity 0 too (which text extraction leaves out).
    let mut left = String::new();
    let look = TextFilterOptions {
        no_update: true,
        ..TextFilterOptions::default()
    };
    page.filter_text(look, |glyph| {
        left.extend(glyph.unicode());
        TextFilterAction::Keep
    })
    .unwrap();
    assert_eq!(left, "ShownClippingDrawnClear");

    // The control: removing the text that only clips changes the picture (the blue fill it
    // clipped is gone), so it must never be removed as if it were invisible.
    let doc = sample();
    let mut page = doc.load_pdf_page(0).unwrap();
    page.filter_text(TextFilterOptions::default(), |glyph| {
        if glyph.render_mode() == Some(TextRenderMode::Clip) {
            TextFilterAction::Remove
        } else {
            TextFilterAction::Keep
        }
    })
    .unwrap();
    assert_ne!(render(&page), picture);
}

#[test]
fn a_panicking_callback_is_an_error_and_leaves_the_page_as_it_was() {
    let doc = one_page("BT /F1 24 Tf 72 700 Td (Hello world) Tj ET", "", "", &[]);
    let mut page = doc.load_pdf_page(0).unwrap();
    let mut calls = 0;
    let result = page.filter_text(TextFilterOptions::default(), |_| {
        calls += 1;
        if calls == 4 {
            panic!("the callback fails at the fourth glyph");
        }
        TextFilterAction::Remove
    });

    match result {
        Err(Error::MuPdf(err)) => assert!(err.message.contains("panicked"), "{err}"),
        other => panic!("expected an error, got {other:?}"),
    }
    // MuPDF stopped at the panic: the callback was not called again.
    assert_eq!(calls, 4);
    // The three glyphs removed before the panic are still there: the page did not change.
    assert_eq!(text_of(&page).trim(), "Hello world");

    // The page can be filtered again.
    page.filter_text(TextFilterOptions::default(), |glyph| {
        if glyph.unicode() == ['H'] {
            TextFilterAction::Remove
        } else {
            TextFilterAction::Keep
        }
    })
    .unwrap();
    assert_eq!(text_of(&page).trim(), "ello world");
}

#[test]
fn no_update_only_looks() {
    let doc = one_page("BT /F1 24 Tf 72 700 Td (Hello world) Tj ET", "", "", &[]);
    let mut page = doc.load_pdf_page(0).unwrap();
    let options = TextFilterOptions {
        no_update: true,
        ..TextFilterOptions::default()
    };
    let mut seen = String::new();
    page.filter_text(options, |glyph| {
        seen.extend(glyph.unicode());
        TextFilterAction::Remove
    })
    .unwrap();
    assert_eq!(seen, "Hello world");
    assert_eq!(text_of(&page).trim(), "Hello world");
}

#[test]
fn recurse_and_instance_forms_together_are_refused() {
    let doc = one_page("BT /F1 24 Tf 72 700 Td (Hello) Tj ET", "", "", &[]);
    let mut page = doc.load_pdf_page(0).unwrap();
    let options = TextFilterOptions {
        recurse: true,
        instance_forms: true,
        ..TextFilterOptions::default()
    };
    let result = page.filter_text(options, |_| TextFilterAction::Remove);
    assert!(
        matches!(result, Err(Error::InvalidArgument(_))),
        "{result:?}"
    );
    assert_eq!(text_of(&page).trim(), "Hello");
}
