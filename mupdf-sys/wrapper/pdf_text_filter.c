#include "internal.h"

/*
 * Glue for pdf_filter_page_contents with a sanitize filter whose text_filter
 * asks a callback about every glyph (winner0988/Pdf-reader#748).
 *
 * Nothing in MuPDF is changed: the filter factory below is the one MuPDF calls
 * for each content stream it filters (the page's first, then each Form XObject
 * it instances, or with recurse the resources it filters afterwards), so it can
 * tell the callback whether a glyph is outside the page's own content stream.
 */

/*
 * The callback: returns 0 to keep the glyph, 1 to remove it, and a negative
 * value when it failed (a panic caught on the Rust side). A failure aborts the
 * filtering with an FZ_ERROR_ABORT exception, thrown here in C, so the
 * longjmp never crosses a Rust frame.
 */
typedef int (*mupdf_text_filter_fn)(void *opaque, const int *ucsbuf, int ucslen, fz_matrix trm, fz_matrix ctm, fz_rect bbox, int tr, float ca, float CA, int in_form);

typedef struct text_filter_stream text_filter_stream;

/* The options of one sanitize filter, alive until the filtering ends. */
struct text_filter_stream
{
    pdf_sanitize_filter_options sopts;
    mupdf_text_filter_fn callback;
    void *opaque;
    int in_form;
    text_filter_stream *next;
};

typedef struct
{
    mupdf_text_filter_fn callback;
    void *opaque;
    int streams;
    text_filter_stream *head;
} text_filter_state;

static int text_filter_glyph(fz_context *ctx, void *opaque, int *ucsbuf, int ucslen, fz_matrix trm, fz_matrix ctm, fz_rect bbox, int tr, float ca, float CA)
{
    text_filter_stream *stream = opaque;
    int action = stream->callback(stream->opaque, ucsbuf, ucslen, trm, ctm, bbox, tr, ca, CA, stream->in_form);
    if (action < 0)
        fz_throw(ctx, FZ_ERROR_ABORT, "text filter callback panicked");
    return action != 0;
}

static pdf_processor *text_filter_factory(fz_context *ctx, pdf_document *doc, pdf_processor *chain, int struct_parents, fz_matrix transform, pdf_filter_options *options, void *factory_options)
{
    text_filter_state *state = factory_options;
    text_filter_stream *stream = fz_malloc_struct(ctx, text_filter_stream);

    /* Owned by the state from here on, so it is freed whatever happens next. */
    stream->next = state->head;
    state->head = stream;

    stream->sopts.opaque = stream;
    stream->sopts.text_filter = text_filter_glyph;
    stream->callback = state->callback;
    stream->opaque = state->opaque;
    /* MuPDF creates the page's filter first, before it reads the page's
     * content; every later filter is for another content stream. */
    stream->in_form = state->streams > 0;
    state->streams++;

    return pdf_new_sanitize_filter(ctx, doc, chain, struct_parents, transform, options, &stream->sopts);
}

void mupdf_pdf_filter_page_text(fz_context *ctx, pdf_page *page, int recurse, int instance_forms, int ascii, int no_update, int newlines, mupdf_text_filter_fn callback, void *opaque, mupdf_error_t **errptr)
{
    text_filter_state *state = NULL;
    pdf_filter_factory filters[2];
    pdf_filter_options options;

    if (callback == NULL)
    {
        *errptr = mupdf_new_error_from_str("text filter callback is null");
        return;
    }
    /* pdf_filter_type3 asserts this combination away (pdf-clean.c). */
    if (recurse && instance_forms)
    {
        *errptr = mupdf_new_error_from_str("recurse cannot be combined with instance_forms");
        return;
    }

    memset(&options, 0, sizeof options);
    options.recurse = recurse;
    options.instance_forms = instance_forms;
    options.ascii = ascii;
    options.no_update = no_update;
    options.newlines = newlines;
    options.filters = filters;

    /* The state lives on the heap, so the filters can change it while only
     * the pointer is a local; fz_try is setjmp, and fz_var keeps that local
     * valid in fz_always after a longjmp. */
    fz_var(state);
    fz_try(ctx)
    {
        state = fz_malloc_struct(ctx, text_filter_state);
        state->callback = callback;
        state->opaque = opaque;
        filters[0].filter = text_filter_factory;
        filters[0].options = state;
        filters[1].filter = NULL;
        filters[1].options = NULL;
        pdf_filter_page_contents(ctx, page->doc, page, &options);
    }
    fz_always(ctx)
    {
        if (state != NULL)
        {
            /* Every filter is dropped by now (pdf_filter_content_stream). */
            text_filter_stream *stream = state->head;
            while (stream != NULL)
            {
                text_filter_stream *next = stream->next;
                fz_free(ctx, stream);
                stream = next;
            }
            fz_free(ctx, state);
        }
    }
    fz_catch(ctx)
    {
        mupdf_save_error(ctx, errptr);
    }
}
