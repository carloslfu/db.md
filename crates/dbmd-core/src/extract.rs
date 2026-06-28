//! Document text extraction — the `dbmd extract` engine.
//!
//! `sources/` is where raw evidence lands: invoices, contracts, reports,
//! exports. Most of it arrives as binary documents (PDF, Word, Excel, EPUB) or
//! HTML, not markdown. Before an agent can reason over that evidence — wiki-link
//! it, summarize it into the wiki layer, file a typed record that cites it — the
//! text has to come out. This module is that step: a binary document in, plain
//! UTF-8 text out, format chosen by file extension.
//!
//! # What this is, and is not
//!
//! - **Deterministic decoders only.** Every adapter is a format parser
//!   (`pdf-extract`, `calamine`, `html2text`, `quick-xml`+`zip`). There is **no
//!   AI, no OCR, no embeddings** here — consistent with the crate-wide invariant
//!   (`lib.rs`). The agent driving `dbmd` is the semantic layer; this is plumbing.
//! - **Text layer, not pixels.** A scanned PDF with no text layer yields the
//!   empty string — *empty in, empty out, never hallucinated text.* OCR is an
//!   explicit non-goal (a future `dbmd-ocr`).
//! - **Single document, single call.** [`extract`] handles one file. Walking a
//!   store and extracting every document is the caller's loop, not this module's.
//!
//! # Format dispatch
//!
//! [`Format::from_path`] maps the file extension to an adapter; [`extract`]
//! dispatches:
//!
//! | Extension                | Format            | Adapter                          |
//! |--------------------------|-------------------|----------------------------------|
//! | `.pdf`                   | [`Format::Pdf`]   | `pdf-extract`                    |
//! | `.docx`                  | [`Format::Docx`]  | `zip` + `quick-xml` (`w:t` runs) |
//! | `.xlsx` / `.xlsm` / `.xlsb` / `.ods` | [`Format::Spreadsheet`] | `calamine` |
//! | `.epub`                  | [`Format::Epub`]  | `zip` + `quick-xml` + `html2text`|
//! | `.html` / `.htm` / `.xhtml` | [`Format::Html`] | `html2text`                    |
//!
//! Anything else is [`ExtractError::UnsupportedFormat`] — a typed refusal the
//! CLI surfaces with a stable code, never a panic.

use std::collections::BTreeMap;
use std::io::Read;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;

use serde::Serialize;

/// The result of extracting one document: the plain text plus a small,
/// format-tagged metadata map.
///
/// This is the `--json` shape the CLI emits verbatim (`{text, metadata}`); in
/// plain mode the CLI prints [`Extracted::text`] and discards the metadata.
/// Metadata is intentionally minimal and best-effort — extraction never *fails*
/// for want of a title; it just omits the key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Extracted {
    /// The extracted plain text (UTF-8), normalized to `\n` line endings with
    /// trailing whitespace trimmed per line and a single trailing newline. For
    /// a document with no recoverable text layer (e.g. a scanned, image-only
    /// PDF) this is the empty string — the contract is "empty in, empty out."
    pub text: String,

    /// Best-effort key/value metadata. Always carries `format` (the adapter
    /// that ran, e.g. `"pdf"`). Adapters add what they cheaply know:
    /// `pages`/`sheets`/`sheet_names` (counts), `title` (when the container
    /// declares one). A `BTreeMap` so `--json` output is key-ordered and stable.
    pub metadata: BTreeMap<String, MetaValue>,
}

impl Extracted {
    /// Build an [`Extracted`] from raw adapter text + the detected format,
    /// applying the canonical text normalization ([`normalize_text`]) and
    /// seeding the `format` metadata key.
    fn new(raw_text: String, format: Format) -> Self {
        let mut metadata = BTreeMap::new();
        metadata.insert(
            "format".to_string(),
            MetaValue::Str(format.tag().to_string()),
        );
        Extracted {
            text: normalize_text(&raw_text),
            metadata,
        }
    }

    /// Insert a string metadata key only when the value is non-empty (keeps the
    /// map free of empty `title: ""` noise).
    fn put_str(&mut self, key: &str, value: impl Into<String>) {
        let v = value.into();
        if !v.trim().is_empty() {
            self.metadata.insert(key.to_string(), MetaValue::Str(v));
        }
    }

    /// Insert a numeric (count) metadata key.
    fn put_num(&mut self, key: &str, value: u64) {
        self.metadata.insert(key.to_string(), MetaValue::Num(value));
    }
}

/// A metadata value: a string (title, format tag, sheet name list joined) or a
/// non-negative count (pages, sheets). Serializes to a bare JSON string or
/// number — no wrapper object — so `{text, metadata}` stays flat and readable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum MetaValue {
    /// A textual value (e.g. document title, the `format` tag).
    Str(String),
    /// A non-negative count (e.g. page count, sheet count).
    Num(u64),
}

/// The document formats `dbmd extract` understands, one per adapter. Detected
/// from the file extension by [`Format::from_path`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Portable Document Format (`.pdf`) — text layer via `pdf-extract`.
    Pdf,
    /// Office Open XML WordprocessingML (`.docx`) — `w:t` runs via `quick-xml`.
    Docx,
    /// A spreadsheet (`.xlsx`/`.xlsm`/`.xlsb`/`.ods`) — cells via `calamine`.
    Spreadsheet,
    /// EPUB e-book (`.epub`) — spine XHTML via `zip` + `quick-xml` + `html2text`.
    Epub,
    /// HTML (`.html`/`.htm`/`.xhtml`) — plain text via `html2text`.
    Html,
}

impl Format {
    /// Detect the format from a path's extension (case-insensitive). Returns
    /// `None` for an unrecognized or missing extension; [`extract`] turns that
    /// into [`ExtractError::UnsupportedFormat`] with the offending extension.
    pub fn from_path(path: &Path) -> Option<Format> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "pdf" => Format::Pdf,
            "docx" => Format::Docx,
            "xlsx" | "xlsm" | "xlsb" | "ods" => Format::Spreadsheet,
            "epub" => Format::Epub,
            "html" | "htm" | "xhtml" => Format::Html,
            _ => return None,
        })
    }

    /// The short, stable tag recorded in `metadata.format` and used in error
    /// messages. Distinct from the file extension (one tag can cover several
    /// extensions, e.g. `spreadsheet`).
    pub fn tag(self) -> &'static str {
        match self {
            Format::Pdf => "pdf",
            Format::Docx => "docx",
            Format::Spreadsheet => "spreadsheet",
            Format::Epub => "epub",
            Format::Html => "html",
        }
    }
}

/// Errors from document extraction. Every variant is a typed refusal the CLI
/// maps to a stable machine code — extraction never panics on a bad or
/// encrypted input.
#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    /// The file extension is missing or not one of the supported document
    /// formats. Carries the offending extension (or `""` when absent).
    #[error("unsupported document format: {0:?} (supported: pdf, docx, xlsx/xlsm/xlsb/ods, epub, html/htm/xhtml)")]
    UnsupportedFormat(String),

    /// The document is encrypted/password-protected and could not be opened
    /// without a password (or with the wrong one). A clean refusal — the
    /// extractor must never emit partial/garbled bytes for a locked file.
    #[error("document is encrypted or password-protected: {0}")]
    Encrypted(String),

    /// A format adapter failed to parse a structurally invalid or corrupt
    /// document. Carries the adapter's diagnostic.
    #[error("failed to parse {format} document: {message}")]
    Parse {
        /// The format tag whose adapter failed (e.g. `"pdf"`, `"docx"`).
        format: &'static str,
        /// The underlying parser diagnostic.
        message: String,
    },

    /// An underlying I/O failure (file missing, unreadable, etc.).
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl ExtractError {
    /// A short, stable machine code for this error, mirrored at the CLI
    /// boundary for `--json` output and exit-code mapping.
    pub fn code(&self) -> &'static str {
        match self {
            ExtractError::UnsupportedFormat(_) => "UNSUPPORTED_FORMAT",
            ExtractError::Encrypted(_) => "DOCUMENT_ENCRYPTED",
            ExtractError::Parse { .. } => "EXTRACT_PARSE_ERROR",
            ExtractError::Io(_) => "IO_ERROR",
        }
    }
}

/// Result alias for extraction operations.
pub type Result<T> = std::result::Result<T, ExtractError>;

/// Extract plain text (and best-effort metadata) from a document, choosing the
/// adapter by the file's extension.
///
/// This is the single entry point the CLI calls. It reads exactly one file and
/// returns one [`Extracted`]; there is no whole-store walk here (per the
/// crate-wide O(changed) invariant — a store-wide extraction is the caller's
/// loop). An unsupported extension is [`ExtractError::UnsupportedFormat`]; an
/// encrypted PDF is [`ExtractError::Encrypted`]; neither panics.
///
/// # Examples
///
/// ```no_run
/// use std::path::Path;
/// let out = dbmd_core::extract::extract(Path::new("sources/docs/invoice.pdf"))?;
/// println!("{}", out.text);
/// # Ok::<(), dbmd_core::extract::ExtractError>(())
/// ```
pub fn extract(path: &Path) -> Result<Extracted> {
    let format = Format::from_path(path).ok_or_else(|| {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_string();
        ExtractError::UnsupportedFormat(ext)
    })?;

    match format {
        Format::Pdf => extract_pdf(path),
        Format::Docx => extract_docx(path),
        Format::Spreadsheet => extract_spreadsheet(path),
        Format::Epub => extract_epub(path),
        Format::Html => extract_html(path),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Text normalization
// ─────────────────────────────────────────────────────────────────────────────

/// Canonicalize extracted text so output is stable across adapters:
///
/// 1. Normalize line endings to `\n` (drop `\r`).
/// 2. Trim trailing whitespace on each line.
/// 3. Collapse three-or-more consecutive blank lines to a single blank line.
/// 4. Trim leading/trailing blank lines, then append exactly one `\n` (unless
///    the whole text is empty, which stays empty — the image-only-PDF contract).
///
/// This is *layout* tid-up only; it never reorders or drops words. Word-level
/// content is whatever the adapter recovered.
pub fn normalize_text(raw: &str) -> String {
    let unix = raw.replace("\r\n", "\n").replace('\r', "\n");

    let lines: Vec<&str> = unix.lines().map(|l| l.trim_end()).collect();

    // Trim leading/trailing blank lines by locating the first and last
    // non-blank line ONCE, then slicing. The previous `while … lines.remove(0)`
    // shifted every remaining element on each removal — O(n²) when the document
    // is dominated by leading blanks (e.g. an adapter that emits millions of
    // empty paragraphs), letting a few-hundred-KB document hang extraction for
    // minutes. Index-and-slice is O(n) regardless of how many blanks lead.
    let Some(first) = lines.iter().position(|l| !l.is_empty()) else {
        return String::new();
    };
    // `first` exists, so a last non-blank line exists too (rposition can't be None).
    let last = lines
        .iter()
        .rposition(|l| !l.is_empty())
        .expect("a non-blank line exists once `first` is found");
    let lines = &lines[first..=last];

    // Collapse runs of 2+ blank lines down to a single blank line.
    let mut out = String::new();
    let mut blank_run = 0usize;
    for &line in lines {
        if line.is_empty() {
            blank_run += 1;
            if blank_run >= 2 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// PDF — pdf-extract
// ─────────────────────────────────────────────────────────────────────────────

/// Extract a PDF's text layer via `pdf-extract`.
///
/// A PDF with no text layer (a scanned image) yields the empty string — that is
/// correct, not an error (OCR is out of scope). A password-protected PDF that
/// cannot be opened is mapped to [`ExtractError::Encrypted`] rather than a raw
/// parse error so the caller can branch on it. Metadata carries the page count
/// when the document tree exposes it.
///
/// `pdf-extract`/`lopdf` `panic!` internally on some malformed-but-openable
/// PDFs (e.g. an out-of-set base `/Encoding` name), so both parser calls are
/// wrapped in [`std::panic::catch_unwind`]: an internal abort is contained and
/// surfaced as [`ExtractError::Parse`], upholding this module's "never panics"
/// contract on untrusted `sources/` input.
fn extract_pdf(path: &Path) -> Result<Extracted> {
    // Read the bytes ourselves so a missing/unreadable file is a clean
    // `ExtractError::Io` (via `?`) before we hand anything to the PDF parser.
    let bytes = std::fs::read(path)?;

    let text = match guard_pdf_panic(|| pdf_extract::extract_text_from_mem(&bytes))? {
        Ok(t) => t,
        Err(e) => return Err(classify_pdf_error(e)),
    };

    let mut out = Extracted::new(text, Format::Pdf);

    // Page count is best-effort; derive it from the parsed document. A parse
    // failure OR an internal panic here is non-fatal — the text already
    // succeeded — so a contained panic (outer `Err`) and a load failure (inner
    // `Err`) are both silently skipped.
    if let Ok(Ok(doc)) = guard_pdf_panic(|| pdf_extract::Document::load_mem(&bytes)) {
        out.put_num("pages", doc.get_pages().len() as u64);
    }

    Ok(out)
}

/// Run a panic-prone `pdf-extract`/`lopdf` call, converting an internal unwind
/// into a typed [`ExtractError::Parse`] tagged `pdf` so the module's "never
/// panics" contract holds on adversarial PDFs. `AssertUnwindSafe` is sound: the
/// closure borrows only `&[u8]`, and on a caught unwind we discard any partial
/// state and return an owned error. The default panic hook still writes the
/// panic line to stderr — library code must not mutate the process-global hook.
fn guard_pdf_panic<T>(f: impl FnOnce() -> T) -> Result<T> {
    catch_unwind(AssertUnwindSafe(f)).map_err(|_| ExtractError::Parse {
        format: "pdf",
        message: "pdf parser aborted on malformed input".to_string(),
    })
}

/// Map a `pdf-extract` error onto the right [`ExtractError`] variant.
/// Decryption failures become [`ExtractError::Encrypted`]; everything else is a
/// [`ExtractError::Parse`] tagged `pdf`.
fn classify_pdf_error(err: pdf_extract::OutputError) -> ExtractError {
    let msg = err.to_string();
    let lower = msg.to_ascii_lowercase();
    if lower.contains("password") || lower.contains("decrypt") || lower.contains("encrypt") {
        ExtractError::Encrypted(msg)
    } else {
        ExtractError::Parse {
            format: "pdf",
            message: msg,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// DOCX — zip + quick-xml (no docx-rs dependency; quick-xml is already needed
// for epub, so docx, xlsx-via-calamine, and epub share one XML/zip surface)
// ─────────────────────────────────────────────────────────────────────────────

/// Extract a `.docx` (WordprocessingML) by unzipping `word/document.xml` and
/// concatenating the `<w:t>` run text, one logical line per `<w:p>` paragraph.
///
/// `<w:tab/>` becomes a tab and `<w:br/>` / `<w:cr>` a newline so table-ish and
/// line-broken content keeps its shape; everything else is structural and
/// ignored. This is the same minimal-but-faithful path `docx-rs` takes for text
/// extraction, without pulling in a second XML/zip stack.
fn extract_docx(path: &Path) -> Result<Extracted> {
    let file = std::fs::File::open(path)?;
    let mut archive = open_zip(file, "docx")?;

    let xml = read_zip_entry(&mut archive, "word/document.xml", "docx")?;
    let text = wordprocessing_text(&xml, "docx")?;

    Ok(Extracted::new(text, Format::Docx))
}

/// Pull paragraph text out of a WordprocessingML / DrawingML XML body.
///
/// Shared by [`extract_docx`]. Walks the event stream collecting `<w:t>` text;
/// `<w:p>` ends a line, `<w:tab/>` is a tab, `<w:br>`/`<w:cr>` a newline.
fn wordprocessing_text(xml: &str, format: &'static str) -> Result<String> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out = String::new();
    let mut in_text_run = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                if local_name(e.name().as_ref()) == b"t" {
                    in_text_run = true;
                }
            }
            Ok(Event::End(e)) => {
                let name = e.name();
                match local_name(name.as_ref()) {
                    b"t" => in_text_run = false,
                    b"p" => out.push('\n'),
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) => {
                // Self-closing run-level breaks inside a paragraph.
                match local_name(e.name().as_ref()) {
                    b"tab" => out.push('\t'),
                    b"br" | b"cr" => out.push('\n'),
                    _ => {}
                }
            }
            // quick-xml 0.40 surfaces text verbatim in `Event::Text` but routes
            // every entity reference to a separate `Event::GeneralRef` and CDATA
            // to `Event::CData` — all three carry run content.
            Ok(Event::Text(t)) => {
                if in_text_run {
                    out.push_str(&String::from_utf8_lossy(&t.into_inner()));
                }
            }
            // `Smith &amp; Co` arrives as Text("Smith ") + GeneralRef("amp") +
            // Text(" Co"); resolve the ref so `&`/`<`/`>`/numeric chars survive.
            Ok(Event::GeneralRef(r)) => {
                if in_text_run {
                    out.push_str(&resolve_entity_ref(&r));
                }
            }
            // CDATA inside a `<w:t>` run is valid WordprocessingML; its payload
            // is literal text and must be appended like `Event::Text`.
            Ok(Event::CData(c)) => {
                if in_text_run {
                    out.push_str(&String::from_utf8_lossy(&c.into_inner()));
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(ExtractError::Parse {
                    format,
                    message: format!("malformed XML: {e}"),
                });
            }
            _ => {}
        }
        buf.clear();
    }

    Ok(out)
}

/// The local part of a possibly-namespaced XML name: `w:t` → `t`, `t` → `t`.
/// docx/epub XML uses prefixes (`w:`, `dc:`) the writer chose; matching the
/// local name is prefix-agnostic and robust to that choice.
fn local_name(qname: &[u8]) -> &[u8] {
    match qname.iter().rposition(|&b| b == b':') {
        Some(i) => &qname[i + 1..],
        None => qname,
    }
}

/// Resolve a `quick_xml` general-entity / character reference to its literal
/// text. quick-xml 0.40 does NOT inline-resolve entity references inside
/// `Event::Text`; instead it surfaces each `&name;` / `&#nnn;` as a separate
/// `Event::GeneralRef`. Routing those to a `_ => {}` arm silently drops `&`,
/// `<`, `>`, numeric refs, etc. from extracted text — corrupting any title,
/// company name, or amount that contains them. This resolves the five
/// XML-predefined named entities and any numeric character reference; an
/// unknown named entity falls back to its bare name (best-effort, never a
/// panic), matching the "recover what we can" stance of `sources/` extraction.
fn resolve_entity_ref(reference: &quick_xml::events::BytesRef<'_>) -> String {
    // Numeric character reference (`&#8212;`, `&#x2014;`): resolve to the char.
    if let Ok(Some(ch)) = reference.resolve_char_ref() {
        return ch.to_string();
    }
    // Named entity: map the five XML-predefined names; fall back to the bare
    // name for anything else (custom DTD entities are out of scope here).
    match reference.decode().as_deref() {
        Ok("amp") => "&".to_string(),
        Ok("lt") => "<".to_string(),
        Ok("gt") => ">".to_string(),
        Ok("quot") => "\"".to_string(),
        Ok("apos") => "'".to_string(),
        Ok(other) => other.to_string(),
        Err(_) => String::new(),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Spreadsheet — calamine (xlsx / xlsm / xlsb / ods)
// ─────────────────────────────────────────────────────────────────────────────

/// Ceiling on a single sheet's dense cell grid (`rows × cols`). `calamine`
/// materializes a worksheet as a DENSE `Vec<Data>` sized from the MIN/MAX cell
/// positions (`Range::from_sparse`), so two cells at `A1` and `XFD1048576` in a
/// few-hundred-byte file force a ~1.7e10-element (~400 GB) allocation that
/// **aborts** the process — bypassing the docx/epub zip-entry cap and the
/// PDF panic guard (an allocation failure aborts, it does not unwind, so
/// `catch_unwind` cannot contain it). `sources/` is untrusted input, so we
/// bound the read the same way docx/epub do: refuse before the allocation.
///
/// 50M cells is ~1.2 GB worst-case dense (`Data` ≈ 24 bytes) — far above any
/// real spreadsheet's used range, far below the weaponizable extreme.
const MAX_SPREADSHEET_CELLS: u64 = 50_000_000;

/// Extract every sheet of a spreadsheet via `calamine`, rendering each row as
/// tab-separated cells, one row per line, sheets in workbook order separated by
/// a blank line.
///
/// Cell rendering: text verbatim; integers and whole-valued floats without a
/// trailing `.0` (`1200`, not `1200.0`); other floats via their default
/// formatting; booleans as `TRUE`/`FALSE`; empty/error cells as the empty
/// string. Metadata carries the sheet count and the joined sheet-name list.
///
/// Before materializing each sheet, [`spreadsheet_dense_cells`] bounds the
/// would-be dense grid against [`MAX_SPREADSHEET_CELLS`] and returns a typed
/// [`ExtractError::Parse`] refusal rather than letting an attacker-supplied
/// sheet OOM/abort the process — upholding the module's "never panics on
/// untrusted `sources/` input" contract for the spreadsheet adapter.
fn extract_spreadsheet(path: &Path) -> Result<Extracted> {
    use calamine::{open_workbook_auto, Reader};

    let mut workbook = open_workbook_auto(path).map_err(|e| ExtractError::Parse {
        format: "spreadsheet",
        message: e.to_string(),
    })?;

    let sheet_names = workbook.sheet_names().to_vec();
    let mut text = String::new();

    for (idx, name) in sheet_names.iter().enumerate() {
        if idx > 0 {
            text.push('\n'); // blank line between sheets
        }

        // Bound the dense grid BEFORE calamine allocates it. For the zip-XML /
        // record backends that expose a sparse cell iterator (xlsx-family,
        // xlsb) this never densely allocates; over-cap sheets refuse cleanly.
        if let Some(cells) = spreadsheet_dense_cells(&mut workbook, name)? {
            if cells > MAX_SPREADSHEET_CELLS {
                return Err(ExtractError::Parse {
                    format: "spreadsheet",
                    message: format!(
                        "sheet {name:?} declares a {cells}-cell grid, over the \
                         {MAX_SPREADSHEET_CELLS}-cell cap (malformed or hostile spreadsheet)"
                    ),
                });
            }
        }

        let range = workbook
            .worksheet_range(name)
            .map_err(|e| ExtractError::Parse {
                format: "spreadsheet",
                message: format!("sheet {name:?}: {e}"),
            })?;

        for row in range.rows() {
            let cells: Vec<String> = row.iter().map(render_cell).collect();
            text.push_str(&cells.join("\t"));
            text.push('\n');
        }
    }

    let mut out = Extracted::new(text, Format::Spreadsheet);
    out.put_num("sheets", sheet_names.len() as u64);
    if !sheet_names.is_empty() {
        out.put_str("sheet_names", sheet_names.join(", "));
    }
    Ok(out)
}

/// Compute the would-be dense cell count (`rows × cols`) of one sheet WITHOUT
/// the dense allocation, by streaming the sheet's sparse cells and tracking the
/// MIN/MAX non-empty position — exactly the bounds `Range::from_sparse` uses.
///
/// Returns `Some(rows * cols)` for the formats that expose a sparse cell
/// iterator (`.xlsx`/`.xlsm`/`.xlsb`/`.xlam`), which are the realistic
/// decompression/dimension-bomb vectors (an OOXML/record sheet can place two
/// cells 1e10 apart in a few hundred bytes). Returns `None` for `.xls` (BIFF,
/// format-bounded to ≤ 65 536 × 256 ≈ 1.7e7 cells) and `.ods`, neither of which
/// exposes a sparse iterator on the auto-detected reader; those fall through to
/// the normal materialization path. A row/col delta is saturated into `u64` so
/// the multiply cannot overflow.
fn spreadsheet_dense_cells(
    workbook: &mut calamine::Sheets<std::io::BufReader<std::fs::File>>,
    name: &str,
) -> Result<Option<u64>> {
    use calamine::{DataRef, Sheets};

    // Stream cells, tracking the non-empty MIN/MAX extent that `from_sparse`
    // would allocate. Empty cells are excluded (calamine drops them before
    // computing the dense bounds), matching the dense grid exactly.
    fn extent<E: std::fmt::Display>(
        mut next: impl FnMut() -> std::result::Result<Option<((u32, u32), bool)>, E>,
    ) -> Result<Option<u64>> {
        let (mut r0, mut r1, mut c0, mut c1) = (u32::MAX, 0u32, u32::MAX, 0u32);
        let mut any = false;
        loop {
            match next() {
                Ok(Some(((r, c), is_empty))) => {
                    if is_empty {
                        continue;
                    }
                    any = true;
                    r0 = r0.min(r);
                    r1 = r1.max(r);
                    c0 = c0.min(c);
                    c1 = c1.max(c);
                }
                Ok(None) => break,
                Err(e) => {
                    return Err(ExtractError::Parse {
                        format: "spreadsheet",
                        message: format!("scanning sheet dimensions: {e}"),
                    })
                }
            }
        }
        if !any {
            return Ok(Some(0));
        }
        let rows = u64::from(r1 - r0) + 1;
        let cols = u64::from(c1 - c0) + 1;
        Ok(Some(rows.saturating_mul(cols)))
    }

    match workbook {
        Sheets::Xlsx(xlsx) => {
            let mut reader =
                xlsx.worksheet_cells_reader(name)
                    .map_err(|e| ExtractError::Parse {
                        format: "spreadsheet",
                        message: format!("sheet {name:?}: {e}"),
                    })?;
            extent(|| {
                reader.next_cell().map(|opt| {
                    opt.map(|c| (c.get_position(), matches!(c.get_value(), DataRef::Empty)))
                })
            })
        }
        Sheets::Xlsb(xlsb) => {
            let mut reader =
                xlsb.worksheet_cells_reader(name)
                    .map_err(|e| ExtractError::Parse {
                        format: "spreadsheet",
                        message: format!("sheet {name:?}: {e}"),
                    })?;
            extent(|| {
                reader.next_cell().map(|opt| {
                    opt.map(|c| (c.get_position(), matches!(c.get_value(), DataRef::Empty)))
                })
            })
        }
        // `.xls` (BIFF, format-bounded) and `.ods` expose no sparse iterator on
        // the auto reader; let them materialize normally.
        Sheets::Xls(_) | Sheets::Ods(_) => Ok(None),
    }
}

/// Render one spreadsheet cell to its text form. Whole-valued floats drop the
/// `.0` (so `3450.0` → `3450`), matching how spreadsheet apps display an
/// integer-typed amount.
fn render_cell(cell: &calamine::Data) -> String {
    use calamine::Data;
    match cell {
        Data::Empty => String::new(),
        Data::String(s) => s.clone(),
        Data::Int(i) => i.to_string(),
        Data::Float(f) => {
            if f.fract() == 0.0 && f.is_finite() && f.abs() < 1e15 {
                format!("{}", *f as i64)
            } else {
                f.to_string()
            }
        }
        Data::Bool(b) => {
            if *b {
                "TRUE".to_string()
            } else {
                "FALSE".to_string()
            }
        }
        // A date/datetime cell is an Excel SERIAL number (days since the 1900
        // epoch, fractional part = time of day). `ExcelDateTime`'s `Display`
        // writes the raw serial (`46188`, `46143.5`), which is meaningless to an
        // agent filing the value into a record, so render the calendar date
        // instead. `to_ymd_hms_milli` is available without the `chrono` feature.
        Data::DateTime(dt) => render_excel_datetime(dt),
        Data::DateTimeIso(s) => s.clone(),
        Data::DurationIso(s) => s.clone(),
        Data::Error(e) => format!("{e:?}"),
    }
}

/// Render an Excel serial date/datetime to an ISO calendar string. A pure date
/// (midnight, no sub-day component) renders `YYYY-MM-DD`; a datetime with a time
/// component renders `YYYY-MM-DD HH:MM:SS`. A duration (Excel `[hh]:mm:ss`
/// elapsed-time format) is not a calendar date, so it keeps its raw serial form
/// (the prior behavior) rather than being misrendered as a date.
fn render_excel_datetime(dt: &calamine::ExcelDateTime) -> String {
    // Guard the serial BEFORE calling `to_ymd_hms_milli`. A date cell carries an
    // arbitrary (attacker-controlled in `sources/`) f64; calamine's conversion is
    // only defined over its calendar window (~1899-12-31..9999-12-31, i.e. serial
    // 0..=2_958_465). Outside it, calamine saturates `floor() as u64` and then
    // overflows on `days += 109_571` — a panic in debug (abort, exit 101) and a
    // fabricated far-past date in release (`1e308` → `1899-12-29`), both of which
    // violate the module contract ("never panics on untrusted input, never
    // hallucinated text"). A duration is likewise not a calendar point. In every
    // such case keep the raw serial, exactly as the duration branch always did.
    let serial = dt.as_f64();
    if dt.is_duration() || !(0.0..=2_958_465.0).contains(&serial) {
        return serial.to_string();
    }
    let (y, mo, d, h, mi, s, _ms) = dt.to_ymd_hms_milli();
    if h == 0 && mi == 0 && s == 0 {
        format!("{y:04}-{mo:02}-{d:02}")
    } else {
        format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// EPUB — zip + quick-xml (spine order) + html2text (per-chapter)
// ─────────────────────────────────────────────────────────────────────────────
//
// We do NOT use the `epub` crate: it is GPL-3.0, which violates the toolkit's
// permissive-only license rule. An EPUB is a zip whose OPF package declares a
// reading-order `spine`; each spine item is an XHTML document. zip + quick-xml
// (already dependencies) read the container/OPF, and html2text (already a
// dependency for `.html`) flattens each chapter. Same machinery, no GPL.

/// Extract an EPUB's reading-order text:
/// 1. read `META-INF/container.xml` → the OPF package path;
/// 2. parse the OPF `manifest` (id→href) and `spine` (ordered idref list);
/// 3. for each spine item, read its XHTML and flatten it with [`html_to_text`];
/// 4. join chapters with a blank line.
///
/// Metadata carries `title` (the OPF `dc:title`) and `chapters` (spine length).
fn extract_epub(path: &Path) -> Result<Extracted> {
    let file = std::fs::File::open(path)?;
    let mut archive = open_zip(file, "epub")?;

    // 1. container.xml → OPF path.
    let container = read_zip_entry(&mut archive, "META-INF/container.xml", "epub")?;
    let opf_path = epub_opf_path(&container)?;

    // 2. OPF → base dir, manifest, spine, title.
    let opf = read_zip_entry(&mut archive, &opf_path, "epub")?;
    let parsed = parse_opf(&opf)?;
    let base = opf_base_dir(&opf_path);

    // 3. Spine items in order → flattened chapter text.
    let mut text = String::new();
    let mut chapters = 0u64;
    for idref in &parsed.spine {
        let Some(href) = parsed.manifest.get(idref) else {
            continue; // dangling spine ref; skip rather than fail
        };
        let entry = join_zip_path(&base, href);
        // A missing spine target is skipped (best-effort), not fatal.
        let Ok(chapter_xhtml) = read_zip_entry(&mut archive, &entry, "epub") else {
            continue;
        };
        let chapter_text = html_to_text(chapter_xhtml.as_bytes())?;
        if !chapter_text.trim().is_empty() {
            if chapters > 0 {
                text.push('\n');
            }
            text.push_str(&chapter_text);
            text.push('\n');
            chapters += 1;
        }
    }

    let mut out = Extracted::new(text, Format::Epub);
    out.put_num("chapters", chapters);
    if let Some(title) = parsed.title {
        out.put_str("title", title);
    }
    Ok(out)
}

/// The full-path of the OPF package file, read from `META-INF/container.xml`'s
/// first `<rootfile full-path="…">`.
fn epub_opf_path(container_xml: &str) -> Result<String> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_str(container_xml);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                if local_name(e.name().as_ref()) == b"rootfile" {
                    if let Some(p) = attr_value(&e, b"full-path") {
                        return Ok(p);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(ExtractError::Parse {
                    format: "epub",
                    message: format!("container.xml: {e}"),
                })
            }
            _ => {}
        }
        buf.clear();
    }
    Err(ExtractError::Parse {
        format: "epub",
        message: "container.xml has no <rootfile full-path>".to_string(),
    })
}

/// The parsed-out pieces of an OPF package we need for reading-order text.
struct OpfParsed {
    /// Manifest: item id → href (relative to the OPF's directory).
    manifest: BTreeMap<String, String>,
    /// Spine: ordered list of manifest item ids (the reading order).
    spine: Vec<String>,
    /// `dc:title`, if present.
    title: Option<String>,
}

/// Parse an OPF package document into its manifest, spine, and title.
fn parse_opf(opf_xml: &str) -> Result<OpfParsed> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_str(opf_xml);
    let mut buf = Vec::new();

    let mut manifest = BTreeMap::new();
    let mut spine = Vec::new();
    let mut title: Option<String> = None;
    // Whether we are inside the FIRST `<dc:title>` element, and the text we have
    // accumulated for it. We accumulate across every Text/GeneralRef/CData event
    // until the matching End so an entity, comment, or nested element inside the
    // title does not truncate it.
    let mut in_title = false;
    let mut title_buf = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match local_name(e.name().as_ref()) {
                b"item" => {
                    if let (Some(id), Some(href)) = (attr_value(&e, b"id"), attr_value(&e, b"href"))
                    {
                        manifest.insert(id, href);
                    }
                }
                b"itemref" => {
                    if let Some(idref) = attr_value(&e, b"idref") {
                        spine.push(idref);
                    }
                }
                // Only a Start (not a self-closing Empty) opens the title: an
                // Empty `<dc:title/>` has no content and produces no End event,
                // so latching `in_title` on it would wrongly capture the next
                // text node (e.g. the author) as the title.
                b"title" if title.is_none() => in_title = true,
                _ => {}
            },
            // Self-closing manifest/spine entries are Empty events; the title is
            // never captured from Empty (see the Start arm's note).
            Ok(Event::Empty(e)) => match local_name(e.name().as_ref()) {
                b"item" => {
                    if let (Some(id), Some(href)) = (attr_value(&e, b"id"), attr_value(&e, b"href"))
                    {
                        manifest.insert(id, href);
                    }
                }
                b"itemref" => {
                    if let Some(idref) = attr_value(&e, b"idref") {
                        spine.push(idref);
                    }
                }
                _ => {}
            },
            Ok(Event::End(e)) => {
                if in_title && local_name(e.name().as_ref()) == b"title" {
                    in_title = false;
                    let s = title_buf.trim();
                    if !s.is_empty() {
                        title = Some(s.to_string());
                    }
                }
            }
            Ok(Event::Text(t)) => {
                if in_title {
                    title_buf.push_str(&String::from_utf8_lossy(&t.into_inner()));
                }
            }
            // An entity (`&amp;`) or numeric ref inside the title resolves into
            // the accumulated value rather than truncating it.
            Ok(Event::GeneralRef(r)) => {
                if in_title {
                    title_buf.push_str(&resolve_entity_ref(&r));
                }
            }
            // CDATA inside `<dc:title>` is literal title text.
            Ok(Event::CData(c)) => {
                if in_title {
                    title_buf.push_str(&String::from_utf8_lossy(&c.into_inner()));
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(ExtractError::Parse {
                    format: "epub",
                    message: format!("OPF: {e}"),
                })
            }
            _ => {}
        }
        buf.clear();
    }

    Ok(OpfParsed {
        manifest,
        spine,
        title,
    })
}

/// The directory portion of an OPF path (`"OEBPS/content.opf"` → `"OEBPS"`,
/// `"content.opf"` → `""`), used to resolve manifest hrefs against the OPF's own
/// location inside the zip.
fn opf_base_dir(opf_path: &str) -> String {
    match opf_path.rfind('/') {
        Some(i) => opf_path[..i].to_string(),
        None => String::new(),
    }
}

/// Join an OPF base dir with a (possibly `./`-prefixed) manifest href into a zip
/// entry name. Forward-slash only — zip paths are always `/`-separated.
///
/// OPF manifest hrefs are URLs: the EPUB spec requires reserved characters
/// (spaces, non-ASCII) to be percent-encoded, but zip entry NAMES are raw. So an
/// href `my%20chapter.xhtml` must be percent-decoded to `my chapter.xhtml`
/// before it can match the zip entry, or the chapter is silently dropped. We
/// percent-decode the href and then normalize `.`/`..` segments so a relative
/// href like `../text/ch1.xhtml` resolves against the OPF's directory.
fn join_zip_path(base: &str, href: &str) -> String {
    let decoded = percent_decode(href);
    let combined = if base.is_empty() {
        decoded
    } else {
        format!("{base}/{decoded}")
    };
    normalize_zip_path(&combined)
}

/// Percent-decode a URL path component (`%20` → space, `%C3%A9` → `é`).
/// Decodes byte-by-byte then UTF-8-lossy-reinterprets, so a multi-byte
/// percent-encoded codepoint (`%C3%A9`) round-trips. A stray `%` not followed by
/// two hex digits is emitted verbatim (best-effort, never a panic).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Resolve `.` and `..` segments in a `/`-separated zip path so a manifest href
/// like `../text/ch1.xhtml` (relative to the OPF's directory) maps to the real
/// entry name. A leading `..` that would escape the archive root is dropped
/// (zip entries have no parent of the root).
fn normalize_zip_path(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out.join("/")
}

// ─────────────────────────────────────────────────────────────────────────────
// HTML — html2text + light markdown-decoration cleanup
// ─────────────────────────────────────────────────────────────────────────────

/// Extract plain text from an `.html` file.
fn extract_html(path: &Path) -> Result<Extracted> {
    let bytes = std::fs::read(path)?;
    let text = html_to_text(&bytes)?;
    Ok(Extracted::new(text, Format::Html))
}

/// Flatten an HTML/XHTML byte stream to clean plain text.
///
/// Renders with [`PlainContentDecorator`] — `html2text`'s plain renderer driven
/// by a decorator that emits **no** link brackets and **no** `#` heading
/// markers, while keeping list-item markers (`*` / `N.`). This removes the two
/// decorations at the source instead of post-stripping them: the previous
/// approach blindly deleted every `[bracketed]` substring and every leading `#`
/// run from the rendered text, which also destroyed *literal* content —
/// citation markers (`[1]`, `[sic]`), code subscripts (`x[i]`), and ranking
/// prose (`#1 in sales`). The renderer knows which `[`/`#` it produced; literal
/// brackets and hashes in the source now survive untouched.
///
/// A very wide wrap width (10_000) is used so paragraphs are not hard-wrapped by
/// the renderer; paragraph structure comes from the source's block elements, and
/// final layout is canonicalized by [`normalize_text`].
fn html_to_text(html: &[u8]) -> Result<String> {
    // Bound block-element nesting BEFORE handing the bytes to html2text. The
    // layout engine is super-linear in nesting depth (O(depth^2) observed), so a
    // tiny crafted file (`<div>`×40_000 …`</div>`×40_000`, ~440 KB) hangs
    // extraction for tens of seconds. `sources/` is untrusted, and every other
    // adapter bounds its untrusted input (MAX_ZIP_ENTRY_BYTES, MAX_SPREADSHEET_
    // CELLS); the HTML path is the lone unbounded one. This is the missing bound.
    // A pure byte cap can't distinguish a 440 KB bomb from a 440 KB legitimate
    // article, so we bound the structural cause (depth) rather than size. EPUB
    // chapters route through here too, so the guard covers them as well.
    if let Some(depth) = html_block_nesting_exceeds(html, MAX_HTML_NESTING_DEPTH) {
        return Err(ExtractError::Parse {
            format: "html",
            message: format!(
                "HTML block nesting depth exceeds the {MAX_HTML_NESTING_DEPTH} cap (reached {depth}; \
                 malformed or hostile input)"
            ),
        });
    }
    html2text::config::with_decorator(PlainContentDecorator)
        .string_from_read(html, 10_000)
        .map_err(|e| ExtractError::Parse {
            format: "html",
            message: e.to_string(),
        })
}

/// The deepest block-element nesting `html_to_text` tolerates. No legitimate
/// document nests containers anywhere near this deep; the cap exists purely to
/// refuse the deeply-nested bomb that makes html2text's layout pass run for
/// minutes. Set with large headroom so it can only fire on pathological input.
const MAX_HTML_NESTING_DEPTH: usize = 4_096;

/// HTML5 void elements — they have no closing tag, so they must NOT increment
/// the nesting depth (a document of many sibling `<br>`/`<img>` is flat, not
/// deep). Kept lowercase; the scan lowercases the tag name before matching.
const HTML_VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// Scan an HTML byte stream once and return `Some(depth)` if open-tag nesting
/// ever exceeds `limit`, else `None`. This is a deliberately crude, allocation-
/// free tag scanner — NOT a parser. It tracks only nesting *depth* to bound
/// html2text's super-linear layout cost; correctness of the depth count past the
/// limit does not matter (we only care whether it is exceeded). Closing tags
/// decrement (saturating at 0), void/self-closing tags and comments/doctype/PI
/// are ignored, and a `<` not followed by a tag-ish character is treated as
/// literal text rather than a tag open (so `a < b` in prose does not inflate it).
fn html_block_nesting_exceeds(html: &[u8], limit: usize) -> Option<usize> {
    let mut depth: usize = 0;
    let mut i = 0usize;
    let n = html.len();
    while i < n {
        if html[i] != b'<' {
            i += 1;
            continue;
        }
        // Look at the byte after `<` to classify the tag.
        let Some(&c) = html.get(i + 1) else { break };
        if c == b'!' || c == b'?' {
            // Comment, doctype, CDATA, or processing instruction — skip to `>`.
            i = memchr_gt(html, i + 1);
            continue;
        }
        if c == b'/' {
            depth = depth.saturating_sub(1);
            i = memchr_gt(html, i + 1);
            continue;
        }
        if !c.is_ascii_alphabetic() {
            // A stray `<` in text (`a < b`) — not a tag open.
            i += 1;
            continue;
        }
        // Find the tag's end `>` and whether it self-closes (`... />`).
        let end = memchr_gt(html, i + 1);
        let self_closing = end > 0 && end <= n && html.get(end - 1) == Some(&b'/');
        // Extract the tag name (letters/digits after `<`).
        let name_end = (i + 1..end.min(n))
            .find(|&j| !html[j].is_ascii_alphanumeric())
            .unwrap_or(end.min(n));
        let name = html[i + 1..name_end].to_ascii_lowercase();
        let is_void = std::str::from_utf8(&name)
            .map(|s| HTML_VOID_ELEMENTS.contains(&s))
            .unwrap_or(false);
        if !self_closing && !is_void {
            depth += 1;
            if depth > limit {
                return Some(depth);
            }
        }
        i = end;
    }
    None
}

/// Index just past the next `>` at or after `from` (or `len` if none). Small
/// helper so [`html_block_nesting_exceeds`] always makes forward progress.
fn memchr_gt(hay: &[u8], from: usize) -> usize {
    let mut j = from;
    while j < hay.len() {
        if hay[j] == b'>' {
            return j + 1;
        }
        j += 1;
    }
    hay.len()
}

/// A `html2text` decorator that flattens HTML to plain text WITHOUT emitting the
/// markup that would otherwise have to be post-stripped: no `[`/`]` around link
/// text, no `#` heading prefix, no `^{…}` superscript braces. List-item markers
/// (`* ` for unordered, `N. ` for ordered) ARE emitted — they are content-
/// faithful and match the corpus convention. Quote prefixes are kept as in the
/// stock plain decorator. This is the fix for the literal-content corruption the
/// old `strip_markdown_decorations`/`unwrap_brackets` post-pass caused.
#[derive(Clone, Debug)]
struct PlainContentDecorator;

impl html2text::render::TextDecorator for PlainContentDecorator {
    type Annotation = ();

    fn decorate_link_start(&mut self, _url: &str) -> (String, Self::Annotation) {
        (String::new(), ())
    }
    fn decorate_link_end(&mut self) -> String {
        String::new()
    }
    fn decorate_em_start(&self) -> (String, Self::Annotation) {
        (String::new(), ())
    }
    fn decorate_em_end(&self) -> String {
        String::new()
    }
    fn decorate_strong_start(&self) -> (String, Self::Annotation) {
        (String::new(), ())
    }
    fn decorate_strong_end(&self) -> String {
        String::new()
    }
    fn decorate_strikeout_start(&self) -> (String, Self::Annotation) {
        (String::new(), ())
    }
    fn decorate_strikeout_end(&self) -> String {
        String::new()
    }
    fn decorate_code_start(&self) -> (String, Self::Annotation) {
        (String::new(), ())
    }
    fn decorate_code_end(&self) -> String {
        String::new()
    }
    fn decorate_preformat_first(&self) -> Self::Annotation {}
    fn decorate_preformat_cont(&self) -> Self::Annotation {}
    fn decorate_image(&mut self, _src: &str, title: &str) -> (String, Self::Annotation) {
        // Alt/title text only — no surrounding brackets (the stock plain
        // decorator wraps it in `[...]`, which would read as literal content).
        (title.to_string(), ())
    }
    fn header_prefix(&self, _level: usize) -> String {
        // No `#` heading marker — heading text reads as plain prose.
        String::new()
    }
    fn quote_prefix(&self) -> String {
        "> ".to_string()
    }
    fn unordered_item_prefix(&self) -> String {
        "* ".to_string()
    }
    fn ordered_item_prefix(&self, i: i64) -> String {
        format!("{i}. ")
    }
    fn decorate_superscript_start(&self) -> (String, Self::Annotation) {
        // Plain text: no `^{…}` braces (which would corrupt literal content).
        (String::new(), ())
    }
    fn decorate_superscript_end(&self) -> String {
        String::new()
    }
    fn make_subblock_decorator(&self) -> Self {
        PlainContentDecorator
    }
}

/// Strip the residual markdown decorations `html2text`'s plain renderer emits:
/// leading run of `#` (ATX heading markers) at the start of a line, and `[...]`
/// brackets around link/anchor text (the reference-style `[n]` suffix is already
/// gone under `plain_no_decorate`). Bullet (`*`) and ordered (`N.`) markers are
/// left intact — they are content, not decoration.
///
/// No longer used by [`html_to_text`] (the [`PlainContentDecorator`] now removes
/// these decorations at the source so literal `[brackets]`/`#hashes` survive);
/// retained only for its unit test documenting the old renderer's behavior.
#[allow(dead_code)]
fn strip_markdown_decorations(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        // Strip a leading "#"-run + the single space after it (ATX heading).
        let trimmed = line.trim_start();
        let after_hashes = trimmed.trim_start_matches('#');
        let line = if after_hashes.len() != trimmed.len() {
            // It was a heading line: keep indentation-free heading text.
            after_hashes.trim_start()
        } else {
            line
        };
        out.push_str(&unwrap_brackets(line));
        out.push('\n');
    }
    out
}

/// Replace every `[inner]` with `inner` (one pass, non-nested). `html2text`'s
/// plain renderer wraps link/anchor text in single brackets; unwrapping yields
/// the bare text. Escaped or unmatched brackets are left as-is.
///
/// No longer used by [`html_to_text`] (see [`strip_markdown_decorations`]);
/// retained only for its unit test.
#[allow(dead_code)]
fn unwrap_brackets(line: &str) -> String {
    if !line.contains('[') {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '[' {
            // Collect until the matching ']'; if none, emit the '[' literally.
            let mut inner = String::new();
            let mut closed = false;
            for d in chars.by_ref() {
                if d == ']' {
                    closed = true;
                    break;
                }
                inner.push(d);
            }
            if closed {
                out.push_str(&inner);
            } else {
                out.push('[');
                out.push_str(&inner);
            }
        } else {
            out.push(c);
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// Shared zip helpers (docx + epub)
// ─────────────────────────────────────────────────────────────────────────────

/// Open a zip archive from a reader, mapping any failure to a typed
/// [`ExtractError::Parse`] tagged with the calling format.
fn open_zip<R: Read + std::io::Seek>(
    reader: R,
    format: &'static str,
) -> Result<zip::ZipArchive<R>> {
    zip::ZipArchive::new(reader).map_err(|e| ExtractError::Parse {
        format,
        message: format!("not a valid zip container: {e}"),
    })
}

/// Cap on a single decompressed zip entry. docx/epub members are XML text — a
/// member that inflates past this ceiling is a decompression bomb or corruption,
/// not real evidence. `sources/` is untrusted input, so bound the read rather
/// than let `read_to_end` follow a hostile DEFLATE stream until OOM.
const MAX_ZIP_ENTRY_BYTES: u64 = 256 * 1024 * 1024;

/// Read a single zip entry to a UTF-8 string, bounded by [`MAX_ZIP_ENTRY_BYTES`]
/// so a zip-bomb member cannot exhaust memory. A missing entry, an over-cap
/// entry, or a read failure is a typed [`ExtractError::Parse`]; invalid UTF-8 is
/// lossily decoded (OOXML / XHTML are declared UTF-8, but we never panic on a
/// stray byte).
fn read_zip_entry<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &str,
    format: &'static str,
) -> Result<String> {
    let entry = archive.by_name(name).map_err(|e| ExtractError::Parse {
        format,
        message: format!("missing zip entry {name:?}: {e}"),
    })?;
    // Reject up front when the central directory declares an over-cap size...
    let declared = entry.size();
    if declared > MAX_ZIP_ENTRY_BYTES {
        return Err(ExtractError::Parse {
            format,
            message: format!(
                "zip entry {name:?} declares {declared} bytes, over the {MAX_ZIP_ENTRY_BYTES}-byte cap"
            ),
        });
    }
    // ...and bound the actual decompressed read so a lying header (a bomb that
    // understates its uncompressed size) still cannot allocate past the cap.
    let mut bytes = Vec::new();
    entry
        .take(MAX_ZIP_ENTRY_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| ExtractError::Parse {
            format,
            message: format!("reading {name:?}: {e}"),
        })?;
    if bytes.len() as u64 > MAX_ZIP_ENTRY_BYTES {
        return Err(ExtractError::Parse {
            format,
            message: format!(
                "zip entry {name:?} exceeds the {MAX_ZIP_ENTRY_BYTES}-byte cap (decompression bomb?)"
            ),
        });
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Look up a start/empty element's attribute value by local name, returning it
/// unescaped as an owned `String`. Prefix-agnostic on the attribute key.
fn attr_value(elem: &quick_xml::events::BytesStart<'_>, key: &[u8]) -> Option<String> {
    elem.attributes().flatten().find_map(|attr| {
        if local_name(attr.key.as_ref()) == key {
            // `unescape_value` returns an XML-unescaped `Cow<str>` — exactly the
            // owned attribute text we want. It is soft-deprecated in quick-xml
            // 0.40 in favor of `normalized_value(XmlVersion)`, whose extra
            // version arg and byte-Cow return buy us nothing here; the simple
            // form is correct for the UTF-8 OOXML/OPF attributes we read.
            #[allow(deprecated)]
            attr.unescape_value().ok().map(|cow| cow.into_owned())
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Absolute path to a corpus-c-formats fixture under `sources/docs/`.
    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/corpora/corpus-c-formats/sources/docs")
            .join(name)
    }

    /// Read the known-good `.txt` sibling of a fixture.
    fn expected(name: &str) -> String {
        std::fs::read_to_string(fixture(&format!("{name}.txt"))).unwrap()
    }

    /// Token-level normalization: collapse every run of whitespace (incl.
    /// newlines) to one space and trim. This is the corpus's recommended,
    /// layout-agnostic comparison ("same words, same order").
    fn tokens(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// The sorted set of non-blank, token-normalized lines — order-agnostic
    /// content comparison (used where extractor reading-order legitimately
    /// differs, e.g. multi-column PDF).
    fn line_set(s: &str) -> Vec<String> {
        let mut v: Vec<String> = s.lines().map(tokens).filter(|l| !l.is_empty()).collect();
        v.sort();
        v
    }

    // ── untrusted-input guards (adversarial review) ──────────────────────────

    /// A crafted spreadsheet date cell carries an arbitrary f64 serial. An
    /// out-of-range serial must NOT panic (debug `attempt to add with overflow`)
    /// and must NOT fabricate a calendar date (release `1e308` → `1899-12-29`);
    /// it keeps the raw serial, exactly like the duration fallback.
    #[test]
    fn excel_datetime_out_of_range_serial_stays_raw_and_never_panics() {
        use calamine::{ExcelDateTime, ExcelDateTimeType};
        // In-range serial → a real calendar date (contains a `-`).
        let in_range = render_excel_datetime(&ExcelDateTime::new(
            46_188.0,
            ExcelDateTimeType::DateTime,
            false,
        ));
        assert!(
            in_range.contains('-'),
            "an in-range serial should render a calendar date, got {in_range}"
        );
        // Out-of-range / hostile serials keep the raw serial string, no panic.
        for serial in [1e308_f64, 3_000_000.0, 9e18, -5.0] {
            let out = render_excel_datetime(&ExcelDateTime::new(
                serial,
                ExcelDateTimeType::DateTime,
                false,
            ));
            assert_eq!(
                out,
                serial.to_string(),
                "out-of-range serial {serial} must stay raw, got {out}"
            );
        }
    }

    /// The HTML adapter's block-nesting guard refuses a deeply-nested bomb (the
    /// O(depth^2) html2text blowup) while passing flat documents — including ones
    /// with tens of thousands of sibling VOID elements (which must not count as
    /// depth) and prose containing a literal `<`.
    #[test]
    fn html_nesting_guard_refuses_deep_bomb_passes_flat() {
        let deep = format!(
            "<html><body>{}x{}</body></html>",
            "<div>".repeat(8_000),
            "</div>".repeat(8_000)
        );
        assert!(
            html_block_nesting_exceeds(deep.as_bytes(), MAX_HTML_NESTING_DEPTH).is_some(),
            "an 8000-deep nest must trip the guard"
        );
        assert!(
            html_to_text(deep.as_bytes()).is_err(),
            "html_to_text must refuse the bomb (typed error), not hang"
        );

        let flat = format!("<html><body>{}</body></html>", "<br>".repeat(50_000));
        assert!(
            html_block_nesting_exceeds(flat.as_bytes(), MAX_HTML_NESTING_DEPTH).is_none(),
            "50k sibling void <br> are flat, not deep — must pass"
        );

        let normal =
            "<html><body><div><p>hi <a href=\"u\">link</a>; a < b in prose</p></div></body></html>";
        assert!(
            html_block_nesting_exceeds(normal.as_bytes(), MAX_HTML_NESTING_DEPTH).is_none(),
            "ordinary nesting (and a stray `<`) must pass"
        );
        assert!(
            html_to_text(normal.as_bytes()).is_ok(),
            "a normal document must still flatten fine"
        );
    }

    // ── format detection ────────────────────────────────────────────────────

    #[test]
    fn detects_format_by_extension_case_insensitively() {
        assert_eq!(Format::from_path(Path::new("a.pdf")), Some(Format::Pdf));
        assert_eq!(Format::from_path(Path::new("a.PDF")), Some(Format::Pdf));
        assert_eq!(Format::from_path(Path::new("a.docx")), Some(Format::Docx));
        assert_eq!(
            Format::from_path(Path::new("a.xlsx")),
            Some(Format::Spreadsheet)
        );
        assert_eq!(
            Format::from_path(Path::new("a.ods")),
            Some(Format::Spreadsheet)
        );
        assert_eq!(Format::from_path(Path::new("a.epub")), Some(Format::Epub));
        assert_eq!(Format::from_path(Path::new("a.html")), Some(Format::Html));
        assert_eq!(Format::from_path(Path::new("a.htm")), Some(Format::Html));
        assert_eq!(Format::from_path(Path::new("a.txt")), None);
        assert_eq!(Format::from_path(Path::new("noext")), None);
    }

    #[test]
    fn unsupported_extension_is_typed_error() {
        let err = extract(Path::new("/tmp/whatever.txt")).unwrap_err();
        assert!(matches!(err, ExtractError::UnsupportedFormat(ref e) if e == "txt"));
        assert_eq!(err.code(), "UNSUPPORTED_FORMAT");
    }

    #[test]
    fn missing_extension_is_unsupported() {
        let err = extract(Path::new("/tmp/noext")).unwrap_err();
        assert!(matches!(err, ExtractError::UnsupportedFormat(ref e) if e.is_empty()));
    }

    // ── normalization ─────────────────────────────────────────────────────────

    #[test]
    fn normalize_collapses_blanks_and_trims() {
        let raw = "\r\n\r\nHeading\r\n\r\n\r\n\r\nBody line   \r\n\r\n";
        assert_eq!(normalize_text(raw), "Heading\n\nBody line\n");
    }

    #[test]
    fn normalize_empty_stays_empty() {
        assert_eq!(normalize_text(""), "");
        assert_eq!(normalize_text("   \n\n  \n"), "");
    }

    // ── per-format extraction against corpus-c fixtures ───────────────────────

    #[test]
    fn extract_text_pdf_matches_known_good() {
        let got = extract(&fixture("text.pdf")).unwrap();
        assert_eq!(got.metadata["format"], MetaValue::Str("pdf".into()));
        assert_eq!(got.metadata["pages"], MetaValue::Num(1));
        assert_eq!(tokens(&got.text), tokens(&expected("text.pdf")));
    }

    #[test]
    fn extract_weird_fonts_pdf_matches_known_good() {
        let got = extract(&fixture("weird-fonts.pdf")).unwrap();
        assert_eq!(tokens(&got.text), tokens(&expected("weird-fonts.pdf")));
    }

    #[test]
    fn extract_multi_column_pdf_matches_content_order_agnostic() {
        // pdf-extract reads column-by-column; the known-good `.txt` captures the
        // interleaved (pdftotext) order. Both carry identical content — assert
        // the line SET, not the order. (README § multi-column.)
        let got = extract(&fixture("multi-column.pdf")).unwrap();
        assert_eq!(line_set(&got.text), line_set(&expected("multi-column.pdf")));
    }

    #[test]
    fn extract_image_only_pdf_yields_empty() {
        // No text layer → empty out, never hallucinated text. OCR out of scope.
        let got = extract(&fixture("image-only.pdf")).unwrap();
        assert_eq!(got.text, "");
        assert!(expected("image-only.pdf").trim().is_empty());
    }

    #[test]
    fn extract_encrypted_pdf_without_password_refuses_cleanly() {
        let err = extract(&fixture("encrypted.pdf")).unwrap_err();
        assert!(
            matches!(err, ExtractError::Encrypted(_)),
            "expected Encrypted, got {err:?}"
        );
        assert_eq!(err.code(), "DOCUMENT_ENCRYPTED");
    }

    #[test]
    fn guard_pdf_panic_contains_unwind_as_parse_error() {
        // The "never panics" contract: an internal pdf-extract/lopdf panic must
        // surface as a typed ExtractError::Parse, not abort the process. (cargo
        // captures the unwind's stderr line for a passing test.)
        let contained: Result<()> = guard_pdf_panic(|| panic!("simulated pdf-extract abort"));
        assert!(
            matches!(contained, Err(ExtractError::Parse { format: "pdf", .. })),
            "panic must be contained as a pdf Parse error, got {contained:?}"
        );
        // The success path is transparent — the value passes straight through.
        let ok: Result<u32> = guard_pdf_panic(|| 42);
        assert_eq!(ok.unwrap(), 42);
    }

    #[test]
    fn extract_docx_matches_known_good() {
        let got = extract(&fixture("sample.docx")).unwrap();
        assert_eq!(got.metadata["format"], MetaValue::Str("docx".into()));
        assert_eq!(tokens(&got.text), tokens(&expected("sample.docx")));
    }

    #[test]
    fn extract_xlsx_matches_known_good() {
        let got = extract(&fixture("sample.xlsx")).unwrap();
        assert_eq!(got.metadata["format"], MetaValue::Str("spreadsheet".into()));
        assert_eq!(got.metadata["sheets"], MetaValue::Num(1));
        assert_eq!(
            got.metadata["sheet_names"],
            MetaValue::Str("Expenses".into())
        );
        // Tab-separated, integers without `.0` — exact match (no soft-wrap risk).
        assert_eq!(got.text.trim_end(), expected("sample.xlsx").trim_end());
    }

    #[test]
    fn extract_epub_matches_known_good() {
        let got = extract(&fixture("sample.epub")).unwrap();
        assert_eq!(got.metadata["format"], MetaValue::Str("epub".into()));
        assert_eq!(got.metadata["chapters"], MetaValue::Num(1));
        assert_eq!(
            got.metadata["title"],
            MetaValue::Str("Operations Playbook".into())
        );
        assert_eq!(tokens(&got.text), tokens(&expected("sample.epub")));
    }

    #[test]
    fn extract_html_matches_known_good() {
        let got = extract(&fixture("sample.html")).unwrap();
        assert_eq!(got.metadata["format"], MetaValue::Str("html".into()));
        assert_eq!(tokens(&got.text), tokens(&expected("sample.html")));
    }

    // ── helper-level unit tests ───────────────────────────────────────────────

    #[test]
    fn unwrap_brackets_flattens_link_text() {
        assert_eq!(
            unwrap_brackets("contact [ops@acme.example] or the [handbook]."),
            "contact ops@acme.example or the handbook."
        );
        // Unmatched '[' is preserved.
        assert_eq!(unwrap_brackets("a [b c"), "a [b c");
        // No brackets → untouched.
        assert_eq!(unwrap_brackets("plain text"), "plain text");
    }

    #[test]
    fn strip_markdown_decorations_drops_heading_hashes() {
        let input = "# Title\n## Section\n* bullet\n1. ordered\nplain\n";
        let out = strip_markdown_decorations(input);
        assert_eq!(out, "Title\nSection\n* bullet\n1. ordered\nplain\n");
    }

    #[test]
    fn local_name_strips_prefix() {
        assert_eq!(local_name(b"w:t"), b"t");
        assert_eq!(local_name(b"t"), b"t");
        assert_eq!(local_name(b"dc:title"), b"title");
    }

    #[test]
    fn extracted_serializes_to_text_metadata_json() {
        let got = extract(&fixture("sample.xlsx")).unwrap();
        let json = serde_json::to_value(&got).unwrap();
        assert!(json.get("text").is_some());
        assert_eq!(json["metadata"]["format"], "spreadsheet");
        assert_eq!(json["metadata"]["sheets"], 1);
        // MetaValue::Num serializes as a bare JSON number, Str as a bare string.
        assert!(json["metadata"]["sheets"].is_number());
        assert!(json["metadata"]["format"].is_string());
    }

    // ── regression: leading-blank normalization is linear (finding #13) ────────

    /// `normalize_text` must trim leading blank lines in O(n), not O(n²). The
    /// pre-fix loop used `lines.remove(0)` per blank line — O(n) shift each, so a
    /// document dominated by leading blanks took O(n²) and hung extraction.
    ///
    /// 500_000 leading blank lines is ~2.5e11 element shifts under the old code
    /// (minutes-to-hours, effectively a hang) but instant under the index-and-
    /// slice path; the test reconstructs the finding's trigger (an adapter output
    /// that is mostly leading blanks then one line of text) and asserts the
    /// correct, fully-trimmed result. Against the pre-fix code this test does not
    /// complete in a reasonable time — encoding the quadratic regression.
    #[test]
    fn regression_normalize_text_leading_blanks_is_linear() {
        let blanks = "\n".repeat(500_000);
        let raw = format!("{blanks}only real line\n");
        // Leading blanks fully trimmed; single trailing newline; body intact.
        assert_eq!(normalize_text(&raw), "only real line\n");

        // A wholly-blank giant input still collapses to empty (the other branch).
        assert_eq!(normalize_text(&"   \n".repeat(500_000)), "");
    }

    // ── regression: spreadsheet dense-grid bomb is refused (finding #4) ────────

    /// Build a VALID `.xlsx` whose single sheet declares two real cells at the
    /// opposite corners of Excel's grid (`A1` and `XFD1048576`). `calamine`
    /// materializes a sheet as a DENSE `Vec<Data>` sized from the MIN/MAX cell
    /// positions, so this two-cell sheet would force a ~1.7e10-element (~400 GB)
    /// allocation and abort the process. We reuse the corpus `sample.xlsx`
    /// container verbatim and swap ONLY `xl/worksheets/sheet1.xml`, so every
    /// other part (workbook, rels, content-types) is a real, openable workbook.
    fn write_dense_bomb_xlsx(dest: &Path) {
        use std::io::Write;

        let base = std::fs::read(fixture("sample.xlsx")).expect("corpus sample.xlsx exists");
        let mut archive =
            zip::ZipArchive::new(std::io::Cursor::new(base)).expect("sample.xlsx is a valid zip");

        let bomb_sheet = b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">\
<sheetData>\
<row r=\"1\"><c r=\"A1\"><v>1</v></c></row>\
<row r=\"1048576\"><c r=\"XFD1048576\"><v>2</v></c></row>\
</sheetData></worksheet>";

        let out = std::fs::File::create(dest).unwrap();
        let mut writer = zip::ZipWriter::new(out);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        for i in 0..archive.len() {
            let entry = archive.by_index(i).unwrap();
            let name = entry.name().to_string();
            if name == "xl/worksheets/sheet1.xml" {
                writer.start_file(name, opts).unwrap();
                writer.write_all(bomb_sheet).unwrap();
            } else {
                // Copy every other entry's already-compressed bytes verbatim.
                writer.raw_copy_file(entry).unwrap();
            }
        }
        writer.finish().unwrap();
    }

    /// A spreadsheet whose declared dense grid exceeds [`MAX_SPREADSHEET_CELLS`]
    /// is refused with a typed [`ExtractError::Parse`] BEFORE calamine allocates
    /// the dense matrix — never an OOM/abort. Pre-fix, `extract_spreadsheet`
    /// called `worksheet_range` directly and the process aborted on the
    /// allocation; this test would not return (it would kill the test runner),
    /// so it encodes the resource-exhaustion regression.
    #[test]
    fn regression_spreadsheet_dense_bomb_refused_not_oom() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bomb = tmp.path().join("invoice.xlsx");
        write_dense_bomb_xlsx(&bomb);

        // A few-hundred-byte file on disk — the whole point of the bomb.
        assert!(
            std::fs::metadata(&bomb).unwrap().len() < 10_000,
            "the bomb must be tiny on disk; the danger is the in-memory expansion"
        );

        let err = extract(&bomb).unwrap_err();
        assert!(
            matches!(
                err,
                ExtractError::Parse {
                    format: "spreadsheet",
                    ..
                }
            ),
            "an over-cap dense grid must be a typed spreadsheet Parse refusal, got {err:?}"
        );
        assert_eq!(err.code(), "EXTRACT_PARSE_ERROR");
    }

    /// The cap is a guard, not a wall: a normal spreadsheet still extracts. Locks
    /// down that the preflight bound does not regress the legitimate path (the
    /// corpus `sample.xlsx` is a 3×3 grid, far under the cap).
    #[test]
    fn regression_spreadsheet_cap_allows_real_workbook() {
        let got = extract(&fixture("sample.xlsx")).unwrap();
        assert_eq!(got.metadata["sheets"], MetaValue::Num(1));
        assert!(!got.text.is_empty());
    }

    // ── regression: entity-ref / CDATA fidelity (findings #34, #1011) ──────────

    /// Build a minimal valid `.docx` whose `word/document.xml` body is the given
    /// run XML, written to `dest`. Only the three OOXML members `extract_docx`
    /// touches need to be real; the rest of a Word package is optional for text
    /// extraction.
    fn write_docx(dest: &Path, body_runs: &str) {
        use std::io::Write;
        let document = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
<w:body>{body_runs}</w:body></w:document>"
        );
        let file = std::fs::File::create(dest).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer.start_file("word/document.xml", opts).unwrap();
        writer.write_all(document.as_bytes()).unwrap();
        writer.finish().unwrap();
    }

    #[test]
    fn regression_docx_resolves_entity_refs() {
        // quick-xml 0.40 surfaces `&amp;`/`&lt;`/`&gt;`/`&#8212;` as separate
        // GeneralRef events; pre-fix they were routed to `_ => {}` and dropped,
        // corrupting `Smith & Co invoice <final> total — 100`.
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("entity.docx");
        write_docx(
            &f,
            "<w:p><w:r><w:t>Smith &amp; Co invoice &lt;final&gt; total &#8212; 100</w:t></w:r></w:p>",
        );
        let got = extract(&f).unwrap();
        assert_eq!(got.text, "Smith & Co invoice <final> total — 100\n");
    }

    #[test]
    fn regression_docx_preserves_cdata_run_text() {
        // CDATA inside `<w:t>` is valid and literal; pre-fix it fell through the
        // wildcard arm and the payload vanished.
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("cdata.docx");
        write_docx(
            &f,
            "<w:p><w:r><w:t>Line A.</w:t></w:r></w:p>\
<w:p><w:r><w:t><![CDATA[IMPORTANT CDATA CONTENT]]></w:t></w:r></w:p>\
<w:p><w:r><w:t>Line C.</w:t></w:r></w:p>",
        );
        let got = extract(&f).unwrap();
        assert_eq!(got.text, "Line A.\nIMPORTANT CDATA CONTENT\nLine C.\n");
    }

    #[test]
    fn resolve_entity_ref_maps_named_and_numeric() {
        use quick_xml::events::BytesRef;
        let r = |s: &'static str| resolve_entity_ref(&BytesRef::new(s));
        assert_eq!(r("amp"), "&");
        assert_eq!(r("lt"), "<");
        assert_eq!(r("gt"), ">");
        assert_eq!(r("quot"), "\"");
        assert_eq!(r("apos"), "'");
        assert_eq!(r("#8212"), "—");
        assert_eq!(r("#x2014"), "—");
        // Unknown named entity → bare name (best-effort, never a panic).
        assert_eq!(r("nbsp"), "nbsp");
    }

    // ── regression: EPUB OPF parsing (findings #35, #37, #1012) ────────────────

    /// Build a minimal valid EPUB at `dest`. `opf_metadata` is spliced verbatim
    /// inside `<metadata>`; `manifest_href` is the chapter item's href; the
    /// chapter XHTML is stored under the literal zip entry `chapter_entry`. The
    /// mimetype member is written first and stored (per the EPUB OCF spec).
    fn write_epub(dest: &Path, opf_metadata: &str, manifest_href: &str, chapter_entry: &str) {
        use std::io::Write;
        let container = "<?xml version=\"1.0\"?>\
<container version=\"1.0\" xmlns=\"urn:oasis:names:tc:opendocument:xmlns:container\">\
<rootfiles><rootfile full-path=\"OEBPS/content.opf\" \
media-type=\"application/oebps-package+xml\"/></rootfiles></container>";
        let opf = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<package xmlns=\"http://www.idpf.org/2007/opf\" version=\"3.0\" unique-identifier=\"id\">\
<metadata xmlns:dc=\"http://purl.org/dc/elements/1.1/\">{opf_metadata}</metadata>\
<manifest><item id=\"c1\" href=\"{manifest_href}\" media-type=\"application/xhtml+xml\"/></manifest>\
<spine><itemref idref=\"c1\"/></spine></package>"
        );
        let chapter = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<html xmlns=\"http://www.w3.org/1999/xhtml\"><body>\
<p>Hello world body text.</p></body></html>";

        let file = std::fs::File::create(dest).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        // mimetype must be the first member and stored uncompressed.
        writer.start_file("mimetype", stored).unwrap();
        writer.write_all(b"application/epub+zip").unwrap();
        writer.start_file("META-INF/container.xml", stored).unwrap();
        writer.write_all(container.as_bytes()).unwrap();
        writer.start_file("OEBPS/content.opf", stored).unwrap();
        writer.write_all(opf.as_bytes()).unwrap();
        writer.start_file(chapter_entry, stored).unwrap();
        writer.write_all(chapter.as_bytes()).unwrap();
        writer.finish().unwrap();
    }

    #[test]
    fn regression_epub_title_accumulates_entities_and_nested_events() {
        // Pre-fix the title was cut at the first Text node, so an entity or a
        // comment inside `<dc:title>` truncated it.
        let tmp = tempfile::TempDir::new().unwrap();

        let f1 = tmp.path().join("entity.epub");
        write_epub(
            &f1,
            "<dc:title>Smith &amp; Jones: A &lt;Tale&gt;</dc:title>",
            "chapter.xhtml",
            "OEBPS/chapter.xhtml",
        );
        let got = extract(&f1).unwrap();
        assert_eq!(
            got.metadata["title"],
            MetaValue::Str("Smith & Jones: A <Tale>".into())
        );

        let f2 = tmp.path().join("comment.epub");
        write_epub(
            &f2,
            "<dc:title>Part One<!-- editorial --> and Part Two</dc:title>",
            "chapter.xhtml",
            "OEBPS/chapter.xhtml",
        );
        let got = extract(&f2).unwrap();
        assert_eq!(
            got.metadata["title"],
            MetaValue::Str("Part One and Part Two".into())
        );
    }

    #[test]
    fn regression_epub_self_closing_title_does_not_capture_author() {
        // A self-closing `<dc:title/>` (an untitled book) must NOT latch the next
        // text node (the author) as the title.
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("empty-title.epub");
        write_epub(
            &f,
            "<dc:title/><dc:creator>John Doe</dc:creator>",
            "chapter.xhtml",
            "OEBPS/chapter.xhtml",
        );
        let got = extract(&f).unwrap();
        // No (or empty) title — never the author. `put_str` omits empty values.
        assert!(
            !got.metadata.contains_key("title"),
            "self-closing title must not capture the author, got {:?}",
            got.metadata.get("title")
        );
        // The chapter still extracts.
        assert_eq!(got.metadata["chapters"], MetaValue::Num(1));
    }

    #[test]
    fn regression_epub_percent_encoded_href_resolves() {
        // An href `my%20chapter.xhtml` must match the zip entry
        // `OEBPS/my chapter.xhtml`; pre-fix the lookup failed and the chapter was
        // silently dropped (empty text, 0 chapters).
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("spaced.epub");
        write_epub(
            &f,
            "<dc:title>Spaced</dc:title>",
            "my%20chapter.xhtml",
            "OEBPS/my chapter.xhtml",
        );
        let got = extract(&f).unwrap();
        assert_eq!(got.metadata["chapters"], MetaValue::Num(1));
        assert!(
            got.text.contains("Hello world body text."),
            "percent-encoded-href chapter must extract, got {:?}",
            got.text
        );
    }

    #[test]
    fn percent_decode_handles_spaces_and_unicode_and_stray_percent() {
        assert_eq!(percent_decode("my%20chapter.xhtml"), "my chapter.xhtml");
        // `%C3%A9` is UTF-8 for `é`.
        assert_eq!(percent_decode("caf%C3%A9.xhtml"), "café.xhtml");
        // A stray `%` not followed by two hex digits is emitted verbatim.
        assert_eq!(percent_decode("100%done"), "100%done");
        assert_eq!(percent_decode("plain.xhtml"), "plain.xhtml");
    }

    #[test]
    fn normalize_zip_path_resolves_dot_segments() {
        assert_eq!(
            normalize_zip_path("OEBPS/../text/ch1.xhtml"),
            "text/ch1.xhtml"
        );
        assert_eq!(normalize_zip_path("OEBPS/./ch1.xhtml"), "OEBPS/ch1.xhtml");
        assert_eq!(normalize_zip_path("OEBPS/ch1.xhtml"), "OEBPS/ch1.xhtml");
    }

    // ── regression: spreadsheet date rendering (finding #1013) ─────────────────

    #[test]
    fn render_excel_datetime_renders_iso_not_serial() {
        use calamine::{ExcelDateTime, ExcelDateTimeType};
        // 46188 → 2026-06-15 (date only, midnight → no time component).
        let date = ExcelDateTime::new(46188.0, ExcelDateTimeType::DateTime, false);
        assert_eq!(render_excel_datetime(&date), "2026-06-15");
        // 46143.5 → 2026-05-01 12:00:00 (has a time component).
        let dt = ExcelDateTime::new(46143.5, ExcelDateTimeType::DateTime, false);
        assert_eq!(render_excel_datetime(&dt), "2026-05-01 12:00:00");
        // A duration is elapsed time, not a calendar date → keep the serial form.
        let dur = ExcelDateTime::new(1.5, ExcelDateTimeType::TimeDelta, false);
        assert_eq!(render_excel_datetime(&dur), "1.5");
    }

    #[test]
    fn render_cell_dates_are_iso() {
        use calamine::{Data, ExcelDateTime, ExcelDateTimeType};
        assert_eq!(
            render_cell(&Data::DateTime(ExcelDateTime::new(
                46188.0,
                ExcelDateTimeType::DateTime,
                false
            ))),
            "2026-06-15"
        );
        // The integer/float/string paths are unchanged by the date fix.
        assert_eq!(render_cell(&Data::Float(3450.0)), "3450");
        assert_eq!(render_cell(&Data::Int(7)), "7");
    }

    // ── regression: HTML/EPUB literal-content fidelity (finding #36) ───────────

    /// Render an HTML body string through the production extract path.
    fn html_text(body: &str) -> String {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("doc.html");
        std::fs::write(&f, format!("<html><body>{body}</body></html>")).unwrap();
        extract(&f).unwrap().text
    }

    #[test]
    fn regression_html_keeps_literal_brackets_and_hashes() {
        // Pre-fix every `[bracketed]` substring and every leading-`#` run was
        // stripped from real prose, fusing `total[net]` into `totalnet` and
        // deleting the `#` from `#1 in sales`.
        let out = html_text(
            "<p>#1 in sales this quarter</p>\
<p>see chart[3] for data, array[0] = total[net]</p>",
        );
        assert!(out.contains("#1 in sales this quarter"), "got {out:?}");
        assert!(
            out.contains("see chart[3] for data, array[0] = total[net]"),
            "got {out:?}"
        );

        // Citation markers and subscripts survive intact.
        let out = html_text("<p>See note [1] and [sic] here.</p><p>x[i] + y[j]</p>");
        assert!(out.contains("See note [1] and [sic] here."), "got {out:?}");
        assert!(out.contains("x[i] + y[j]"), "got {out:?}");
    }

    #[test]
    fn html_headings_render_as_plain_prose_no_hash() {
        // A real `<h1>` heading still renders WITHOUT a `#` marker (the renderer
        // emits no heading prefix now), so headings read as prose.
        let out = html_text("<h1>Launch Plan</h1><p>Body prose.</p>");
        assert!(out.contains("Launch Plan"), "got {out:?}");
        assert!(
            !out.contains('#'),
            "no heading marker expected, got {out:?}"
        );
    }

    #[test]
    fn html_links_render_as_bare_text_no_brackets() {
        // Link display text renders bare; the surrounding `[...]` the stock plain
        // decorator would add is gone.
        let out = html_text("<p>See the <a href=\"https://x.example\">handbook</a>.</p>");
        assert!(out.contains("See the handbook."), "got {out:?}");
    }
}
