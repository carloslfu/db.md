# corpus-c-formats

Extraction fixtures for `dbmd extract` (plan Block 3 / Block 7 eval). Each
binary document under `sources/docs/` has a known-good `.txt` sibling used to
diff extractor output. The fixtures live under `sources/docs/` (not a flat
folder) so they exercise the real store path: `sources/docs/` is where the
`pdf-source` type is canonically filed (SPEC § Recognized types).

This corpus is **not** a valid db.md store on its own — there is no `DB.md`,
no `index.md`, no frontmatter on the fixtures. It is a bag of raw documents
plus their expected extracted text. `dbmd extract <file>` reads a raw binary
and prints plain text; that is what these fixtures test. (The frontmatter'd
`pdf-source` markdown wrapper around an extracted document is a separate
concern, exercised by corpus-a / corpus-e.)

## Fixtures

| Fixture              | Format        | Purpose / what it exercises                                              |
|----------------------|---------------|--------------------------------------------------------------------------|
| `sample.html`        | HTML          | Plain HTML extraction; headings, ul/ol lists, links flattened to text; `<script>`/`<style>`/comments stripped. Target crate: `html2text`. |
| `text.pdf`           | PDF (text)    | Clean single-column text layer. The happy path. Target crate: `pdf-extract`. |
| `multi-column.pdf`   | PDF (text)    | Two text columns. Naive reading-order extraction **interleaves** the columns line-by-line — the known-good captures that interleaving, since it is the documented behavior of `pdf-extract` (and `pdftotext` without `-layout`). This fixture exists to pin that failure mode, not to assert column reconstruction. |
| `weird-fonts.pdf`    | PDF (text)    | Mixed base-14 fonts (Times, Courier, Helvetica-Oblique) on one page. Confirms font variety does not corrupt the text stream. |
| `image-only.pdf`     | PDF (scan)    | A full-page **raster image** of text with **no text layer**. Extraction yields nothing — the known-good `.txt` is **empty (0 bytes)**. This is the OCR-required case; OCR is explicitly out of scope (a future `dbmd-ocr`, see plan "Non-goals"). The fixture pins "empty in, empty out — do not hallucinate text." |
| `encrypted.pdf`      | PDF (RC4/AES) | Password-protected (same content as `text.pdf`). Without a password, extraction must **fail/refuse** (not crash, not emit garbage). With the user password it yields `text.pdf`'s text. **User password: `open-sesame`** (owner password: `owner-secret`). |
| `sample.docx`        | DOCX (OOXML)  | Minimal WordprocessingML: a Heading1 paragraph + four body paragraphs. Target crate: `docx-rs` or `quick-xml` over the unzipped `word/document.xml`. |
| `sample.xlsx`        | XLSX (OOXML)  | Minimal SpreadsheetML: one sheet `Expenses`, a header row + two data rows, text cells via `sharedStrings`, numeric cells inline. Target crate: `calamine`. |
| `sample.epub`        | EPUB 3        | Minimal reflowable EPUB: `mimetype` (stored) + `META-INF/container.xml` + OPF + nav + one XHTML chapter. Target crate: `epub`. |

**Nothing is missing.** Every format requested by the plan (line 493: text PDF,
multi-column, weird-fonts, image-only, encrypted PDF, `.docx`, `.xlsx`,
`.epub`, `.html`) is present with a real, well-formed binary fixture and a
known-good text sibling. `file(1)` identifies each as a genuine document of its
type; the PDFs, docx, xlsx, and epub were all confirmed extractable with
independent tooling (`pdftotext`/poppler, `pandoc`, `openpyxl`) before their
`.txt` expectations were written.

## Comparison convention (how to diff)

The Rust extractor crates are not wired up yet (plan Block 3 is downstream of
this corpus), so the `.txt` expectations are **content-faithful, normalized
plain text** — the words and their reading order — not a byte capture of a
specific crate's raw output. The expectations were authored so that every
fixture's text matches the output of independent extractors (poppler `pdftotext`,
`pandoc`, `openpyxl`) **at the token level** (whitespace-insensitive); they
were verified that way before being committed.

Because different extractors disagree on *layout* (line-wrap width, blank-line
spacing, bullet glyph) while agreeing on *content*, a robust extraction test
must normalize both sides before diffing. Two equally valid strictness levels:

**Token-level (recommended — layout-agnostic, what was used to verify):**
collapse every run of whitespace (including newlines) to a single space on
both sides, then compare. This asserts "same words, same order" and is immune
to how the chosen crate wraps lines or spaces paragraphs. The only fixture
needing pre-treatment is `sample.html` (see list-marker note below).

**Line-level (stricter — if you want layout pinned too):**
1. Normalize line endings to `\n`; strip trailing whitespace per line.
2. **Reflow soft-wraps:** join consecutive non-blank lines within a paragraph
   into one logical line before comparing (extractors hard-wrap paragraphs at
   different column widths — `pandoc` at ~72, `pdf-extract` not at all). Treat
   a blank line as the only paragraph separator.
3. Collapse 3+ consecutive blank lines to one; trim leading/trailing blanks.
4. **List markers:** the `.txt` uses `*` for unordered items and `N. ` (single
   space) for ordered items. Some extractors emit `-` for bullets and `N.  `
   (two spaces). Normalize the leading marker before comparing, or relax the
   expectation — the marker glyph is not content.

Fixture-specific notes:
- `sample.xlsx.txt`: rows are tab-separated (`\t`), one row per line,
  row-major; numeric cells render as their integer form (`1200`, `3450`).
  `calamine` exposes cells as typed values — render numbers without a trailing
  `.0`.
- `multi-column.pdf.txt`: compare against reading-order (interleaved) output —
  left line 1, right line 1, left line 2, right line 2, … This is the
  documented behavior of `pdf-extract` and `pdftotext` (no `-layout`). If the
  chosen crate instead reconstructs columns, update this expectation rather
  than the fixture, and note the change here.
- `image-only.pdf.txt`: empty (0 bytes). The extractor must return no text;
  any non-whitespace output is a bug (hallucinated text or an OCR path that
  should not be in scope).
- `encrypted.pdf`: the extractor must be handed the user password
  `open-sesame`. With no password it must fail cleanly (non-zero / typed error),
  not crash and not emit partial bytes.

If a chosen crate emits content (not just layout) that differs from an
expectation after normalization, treat it as a real finding: either the
normalizer is too strict, or the crate's output is the new truth — in which
case regenerate the `.txt` from the crate and record why in the plan's findings
log. The plan (line 411) already flags a possible swap to `pdfium-render` if
`pdf-extract` quality bites; such a swap would re-baseline the PDF `.txt` files.

## Regenerating the binary fixtures

`gen_fixtures.py` rebuilds every binary fixture deterministically (fixed zip
timestamps, no embedded build date). The `.html` and all `.txt` files are
hand-maintained and are **not** produced by the script.

```sh
cd tests/corpora/corpus-c-formats
python3 gen_fixtures.py
```

Toolchain used to author/verify (host tools, not runtime deps of dbmd):
`reportlab` (text PDFs), `pikepdf` (encryption), `Pillow` (image-only raster),
Python `zipfile` (docx/xlsx/epub as zip+xml). Verification only:
`pdftotext`/poppler, `pandoc`, `openpyxl`. None of these are dbmd
dependencies — `dbmd extract` uses the Rust crates named in the table above.
