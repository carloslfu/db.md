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
    #[error("unsupported document format: {0:?} (supported: pdf, docx, xlsx, epub, html)")]
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

    let mut lines: Vec<&str> = unix.lines().map(|l| l.trim_end()).collect();

    // Trim leading blank lines.
    while lines.first().is_some_and(|l| l.is_empty()) {
        lines.remove(0);
    }
    // Trim trailing blank lines.
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }

    if lines.is_empty() {
        return String::new();
    }

    // Collapse runs of 2+ blank lines down to a single blank line.
    let mut out = String::new();
    let mut blank_run = 0usize;
    for line in lines {
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
fn extract_pdf(path: &Path) -> Result<Extracted> {
    // Read the bytes ourselves so a missing/unreadable file is a clean
    // `ExtractError::Io` (via `?`) before we hand anything to the PDF parser.
    let bytes = std::fs::read(path)?;

    let text = match pdf_extract::extract_text_from_mem(&bytes) {
        Ok(t) => t,
        Err(e) => return Err(classify_pdf_error(e)),
    };

    let mut out = Extracted::new(text, Format::Pdf);

    // Page count is cheap and useful; derive it from the parsed document. A
    // failure here is non-fatal — the text already succeeded.
    if let Ok(doc) = pdf_extract::Document::load_mem(&bytes) {
        let pages = doc.get_pages().len() as u64;
        out.put_num("pages", pages);
    }

    Ok(out)
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
            // quick-xml 0.40 yields already-unescaped text in `Event::Text`.
            Ok(Event::Text(t)) => {
                if in_text_run {
                    out.push_str(&String::from_utf8_lossy(&t.into_inner()));
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

// ─────────────────────────────────────────────────────────────────────────────
// Spreadsheet — calamine (xlsx / xlsm / xlsb / ods)
// ─────────────────────────────────────────────────────────────────────────────

/// Extract every sheet of a spreadsheet via `calamine`, rendering each row as
/// tab-separated cells, one row per line, sheets in workbook order separated by
/// a blank line.
///
/// Cell rendering: text verbatim; integers and whole-valued floats without a
/// trailing `.0` (`1200`, not `1200.0`); other floats via their default
/// formatting; booleans as `TRUE`/`FALSE`; empty/error cells as the empty
/// string. Metadata carries the sheet count and the joined sheet-name list.
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
        Data::DateTime(dt) => dt.to_string(),
        Data::DateTimeIso(s) => s.clone(),
        Data::DurationIso(s) => s.clone(),
        Data::Error(e) => format!("{e:?}"),
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
    let mut in_title = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local_name(e.name().as_ref()) {
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
                b"title" => in_title = true,
                _ => {}
            },
            Ok(Event::End(e)) => {
                if local_name(e.name().as_ref()) == b"title" {
                    in_title = false;
                }
            }
            Ok(Event::Text(t)) => {
                if in_title && title.is_none() {
                    let s = String::from_utf8_lossy(&t.into_inner()).trim().to_string();
                    if !s.is_empty() {
                        title = Some(s);
                    }
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
fn join_zip_path(base: &str, href: &str) -> String {
    let href = href.trim_start_matches("./");
    if base.is_empty() {
        href.to_string()
    } else {
        format!("{base}/{href}")
    }
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
/// Uses `html2text`'s non-decorating plain renderer (which already drops
/// `<script>`/`<style>`/comments and flattens lists), then strips the two
/// markdown-ish decorations that renderer still emits — leading `#` heading
/// markers and `[text]` link brackets — so headings and link text read as plain
/// prose. Unordered list items keep their `*` marker and ordered items their
/// `N.` marker (those are content-faithful and match the corpus convention).
///
/// A very wide wrap width (10_000) is used so paragraphs are not hard-wrapped by
/// the renderer; paragraph structure comes from the source's block elements, and
/// final layout is canonicalized by [`normalize_text`].
fn html_to_text(html: &[u8]) -> Result<String> {
    let rendered = html2text::config::plain_no_decorate()
        .string_from_read(html, 10_000)
        .map_err(|e| ExtractError::Parse {
            format: "html",
            message: e.to_string(),
        })?;

    Ok(strip_markdown_decorations(&rendered))
}

/// Strip the residual markdown decorations `html2text`'s plain renderer emits:
/// leading run of `#` (ATX heading markers) at the start of a line, and `[...]`
/// brackets around link/anchor text (the reference-style `[n]` suffix is already
/// gone under `plain_no_decorate`). Bullet (`*`) and ordered (`N.`) markers are
/// left intact — they are content, not decoration.
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

/// Read a single zip entry to a UTF-8 string. A missing entry or a read failure
/// is a typed [`ExtractError::Parse`]; invalid UTF-8 is lossily decoded (OOXML /
/// XHTML are declared UTF-8, but we never panic on a stray byte).
fn read_zip_entry<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &str,
    format: &'static str,
) -> Result<String> {
    let mut entry = archive.by_name(name).map_err(|e| ExtractError::Parse {
        format,
        message: format!("missing zip entry {name:?}: {e}"),
    })?;
    let mut bytes = Vec::new();
    entry
        .read_to_end(&mut bytes)
        .map_err(|e| ExtractError::Parse {
            format,
            message: format!("reading {name:?}: {e}"),
        })?;
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
}
