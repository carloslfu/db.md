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
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Compressed/input bytes accepted by any in-process document adapter. The
/// individual ZIP-entry and extracted-output limits remain independent. A
/// source larger than this must be handled by an externally sandboxed importer,
/// not parsed in the toolkit process.
const MAX_DOCUMENT_INPUT_BYTES: u64 = 128 * 1024 * 1024;

/// The result of extracting one document: the plain text plus a small,
/// format-tagged metadata map.
///
/// This is the `--json` shape the CLI emits verbatim (`{text, metadata}`); in
/// plain mode the CLI prints [`Extracted::text`] and discards the metadata.
/// Metadata is intentionally minimal and best-effort — extraction never *fails*
/// for want of a title; it just omits the key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    let bytes =
        crate::fsx::read_bounded_nofollow(path, MAX_DOCUMENT_INPUT_BYTES).map_err(|error| {
            if error.kind() == std::io::ErrorKind::InvalidData {
                ExtractError::Parse {
                    format: format.tag(),
                    message: format!(
                        "input must be one regular file within the {} MiB extraction cap: {error}",
                        MAX_DOCUMENT_INPUT_BYTES / (1024 * 1024)
                    ),
                }
            } else {
                ExtractError::Io(error)
            }
        })?;

    match format {
        Format::Pdf => extract_pdf(&bytes),
        Format::Docx => extract_docx(&bytes),
        Format::Spreadsheet => extract_spreadsheet(
            &bytes,
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("ods")),
        ),
        Format::Epub => extract_epub(&bytes),
        Format::Html => extract_html(&bytes),
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
fn extract_pdf(bytes: &[u8]) -> Result<Extracted> {
    let text = match guard_pdf_panic(|| pdf_extract::extract_text_from_mem(bytes))? {
        Ok(t) => t,
        Err(e) => return Err(classify_pdf_error(e)),
    };

    let mut out = Extracted::new(text, Format::Pdf);

    // Page count is best-effort; derive it from the parsed document. A parse
    // failure OR an internal panic here is non-fatal — the text already
    // succeeded — so a contained panic (outer `Err`) and a load failure (inner
    // `Err`) are both silently skipped.
    if let Ok(Ok(doc)) = guard_pdf_panic(|| pdf_extract::Document::load_mem(bytes)) {
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
fn extract_docx(bytes: &[u8]) -> Result<Extracted> {
    let mut archive = open_zip(Cursor::new(bytes), "docx")?;
    let mut budget = ExtractionBudget::default();

    let xml = read_zip_entry(&mut archive, "word/document.xml", "docx", &mut budget)?;
    let text = wordprocessing_text(&xml, "docx")?;

    Ok(Extracted::new(text, Format::Docx))
}

/// Pull paragraph text out of a WordprocessingML / DrawingML XML body.
///
/// Shared by [`extract_docx`]. Walks the event stream collecting `<w:t>` text;
/// `<w:p>` ends a line, `<w:tab/>` is a tab, `<w:br>`/`<w:cr>` a newline.
///
/// Output-bounded for parity with the HTML/EPUB adapters. A docx is a zip, and
/// `word/document.xml` is attacker-controlled `sources/` input that can compress
/// enormously: a few-hundred-KB `.docx` whose `document.xml` inflates to hundreds
/// of MB of `<w:t>` runs would otherwise accumulate without bound. We cap the
/// running output at [`MAX_EXTRACT_OUTPUT_BYTES`] *during* accumulation — the
/// same ceiling EPUB enforces — so peak memory stays bounded rather than only
/// being checked after the full string is materialized.
fn wordprocessing_text(xml: &str, format: &'static str) -> Result<String> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out = String::new();
    let mut in_text_run = false;

    // Refuse once accumulated text crosses the cap. Checked after each append so a
    // single huge run can't blow past the ceiling before the next loop turn.
    macro_rules! bound_output {
        () => {
            if out.len() > MAX_EXTRACT_OUTPUT_BYTES {
                return Err(ExtractError::Parse {
                    format,
                    message: format!(
                        "extracted text exceeds the {MAX_EXTRACT_OUTPUT_BYTES} byte cap \
                         (malformed or hostile input)"
                    ),
                });
            }
        };
    }

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
                    b"p" => {
                        out.push('\n');
                        bound_output!();
                    }
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
                    bound_output!();
                }
            }
            // `Smith &amp; Co` arrives as Text("Smith ") + GeneralRef("amp") +
            // Text(" Co"); resolve the ref so `&`/`<`/`>`/numeric chars survive.
            Ok(Event::GeneralRef(r)) => {
                if in_text_run {
                    out.push_str(&resolve_entity_ref(&r));
                    bound_output!();
                }
            }
            // CDATA inside a `<w:t>` run is valid WordprocessingML; its payload
            // is literal text and must be appended like `Event::Text`.
            Ok(Event::CData(c)) => {
                if in_text_run {
                    out.push_str(&String::from_utf8_lossy(&c.into_inner()));
                    bound_output!();
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
/// Two million cells is still a large working sheet while bounding calamine's
/// dense `Vec<Data>` to roughly tens of MiB instead of the previous ~1.2 GiB.
const MAX_SPREADSHEET_CELLS: u64 = 2_000_000;

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
fn extract_spreadsheet(bytes: &[u8], is_ods: bool) -> Result<Extracted> {
    use calamine::{open_workbook_auto_from_rs, Reader};

    // ODS has no sparse-iterator pre-scan (see `spreadsheet_dense_cells`), so the
    // xlsx-family fail-fast on a truncated/unclosed `content.xml` does not protect
    // it: a `.ods` whose `content.xml` opens `<table:table>` then hits EOF makes
    // calamine's ODS reader spin forever (an UNBOUNDED loop, not a panic —
    // `catch_unwind` cannot recover it). The hang is reachable from the very first
    // calamine call (`open_workbook_auto` parses the ODS document on open), so the
    // structural validity gate has to run BEFORE we hand the file to calamine at
    // all — not merely before `worksheet_range`. Gate by extension (the `.ods`
    // backend is the only one with this unbounded shape; `.xls`/BIFF is
    // format-bounded and the xlsx-family is pre-scanned). A truncated/unclosed
    // document fails fast here with a typed Parse refusal — the same shape the
    // xlsx pre-scan produces on a truncated sheet.
    if is_ods {
        ods_content_xml_well_formed(bytes)?;
    }

    let mut workbook =
        open_workbook_auto_from_rs(Cursor::new(bytes)).map_err(|e| ExtractError::Parse {
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
            if text.len() > MAX_EXTRACT_OUTPUT_BYTES {
                return Err(ExtractError::Parse {
                    format: "spreadsheet",
                    message: format!(
                        "extracted text exceeds the {MAX_EXTRACT_OUTPUT_BYTES} byte cap \
                         (malformed or hostile spreadsheet)"
                    ),
                });
            }
        }
    }

    let mut out = Extracted::new(text, Format::Spreadsheet);
    out.put_num("sheets", sheet_names.len() as u64);
    if !sheet_names.is_empty() {
        out.put_str("sheet_names", sheet_names.join(", "));
    }
    Ok(out)
}

/// Structurally validate an `.ods` `content.xml` before the unbounded calamine
/// ODS reader touches it.
///
/// calamine's ODS backend exposes no sparse-cell iterator, so it gets none of the
/// streaming pre-scan that bounds (and fails fast on truncated input) the
/// xlsx/xlsb path in [`spreadsheet_dense_cells`]. On a `.ods` whose `content.xml`
/// opens `<table:table>` and then hits EOF before the matching `</table:table>`,
/// `worksheet_range` spins forever at full CPU — a resource-exhaustion DoS on
/// untrusted `sources/` input, and an *infinite loop* that [`catch_unwind`]
/// cannot recover (it catches panics, not hangs).
///
/// This gate reuses the shared zip helpers ([`open_zip`] / [`read_zip_entry`],
/// bounded by [`MAX_ZIP_ENTRY_BYTES`]) to read `content.xml`, then streams it
/// through `quick-xml` exactly like [`wordprocessing_text`] does for docx. A
/// truncated/unclosed document surfaces as a `quick-xml` error (e.g. "Unexpected
/// end of xml") or as an at-EOF tag-balance mismatch; either way we return a
/// typed [`ExtractError::Parse`] (format `"spreadsheet"`) in well under a second,
/// matching how a truncated `.xlsx` already fails — instead of letting calamine
/// hang. A well-formed `content.xml` passes through untouched, so valid `.ods`
/// extraction is unchanged. Peak memory stays bounded by the zip-entry cap; the
/// scan never densely materializes anything.
fn ods_content_xml_well_formed(bytes: &[u8]) -> Result<()> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut archive = open_zip(Cursor::new(bytes), "spreadsheet")?;
    let mut budget = ExtractionBudget::default();
    let xml = read_zip_entry(&mut archive, "content.xml", "spreadsheet", &mut budget)?;

    let mut reader = Reader::from_str(&xml);
    let mut depth: i64 = 0;
    let mut events = 0usize;
    let mut in_row = false;
    let mut row_cells = 0u64;
    let mut row_repeat = 1u64;
    let mut logical_rows = 0u64;
    let mut declared_cells = 0u64;
    loop {
        events += 1;
        if events > MAX_XML_EVENTS {
            return Err(ExtractError::Parse {
                format: "spreadsheet",
                message: format!(
                    "ODS content.xml exceeds the {MAX_XML_EVENTS}-event parser budget"
                ),
            });
        }
        match reader.read_event() {
            // Any structural malformation (including the unclosed `<table:table>`
            // at EOF, which quick-xml reports as "Unexpected end of xml") is a
            // typed refusal — never a hang.
            Err(e) => {
                return Err(ExtractError::Parse {
                    format: "spreadsheet",
                    message: format!("malformed ODS content.xml: {e}"),
                });
            }
            Ok(Event::Start(element)) => {
                depth += 1;
                match local_name(element.name().as_ref()) {
                    b"table-row" => {
                        in_row = true;
                        row_cells = 0;
                        row_repeat = ods_repeat(&element, b"number-rows-repeated")?;
                        logical_rows = logical_rows.checked_add(row_repeat).ok_or_else(|| {
                            ExtractError::Parse {
                                format: "spreadsheet",
                                message: "ODS repeated-row count overflow".to_string(),
                            }
                        })?;
                        if logical_rows > MAX_SPREADSHEET_CELLS {
                            return Err(ExtractError::Parse {
                                format: "spreadsheet",
                                message: format!(
                                    "ODS declares {logical_rows} logical rows, over the \
                                     {MAX_SPREADSHEET_CELLS}-row structural cap"
                                ),
                            });
                        }
                    }
                    b"table-cell" | b"covered-table-cell" if in_row => {
                        row_cells = row_cells
                            .checked_add(ods_repeat(&element, b"number-columns-repeated")?)
                            .ok_or_else(|| ExtractError::Parse {
                                format: "spreadsheet",
                                message: "ODS repeated-column count overflow".to_string(),
                            })?;
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(element)) => match local_name(element.name().as_ref()) {
                b"table-row" => {
                    let repeated = ods_repeat(&element, b"number-rows-repeated")?;
                    logical_rows =
                        logical_rows
                            .checked_add(repeated)
                            .ok_or_else(|| ExtractError::Parse {
                                format: "spreadsheet",
                                message: "ODS repeated-row count overflow".to_string(),
                            })?;
                    if logical_rows > MAX_SPREADSHEET_CELLS {
                        return Err(ExtractError::Parse {
                            format: "spreadsheet",
                            message: format!(
                                "ODS declares {logical_rows} logical rows, over the \
                                 {MAX_SPREADSHEET_CELLS}-row structural cap"
                            ),
                        });
                    }
                }
                b"table-cell" | b"covered-table-cell" if in_row => {
                    row_cells = row_cells
                        .checked_add(ods_repeat(&element, b"number-columns-repeated")?)
                        .ok_or_else(|| ExtractError::Parse {
                            format: "spreadsheet",
                            message: "ODS repeated-column count overflow".to_string(),
                        })?;
                }
                _ => {}
            },
            Ok(Event::End(element)) => {
                depth -= 1;
                if local_name(element.name().as_ref()) == b"table-row" && in_row {
                    let expanded =
                        row_cells
                            .checked_mul(row_repeat)
                            .ok_or_else(|| ExtractError::Parse {
                                format: "spreadsheet",
                                message: "ODS repeated-cell grid overflow".to_string(),
                            })?;
                    declared_cells = declared_cells.checked_add(expanded).ok_or_else(|| {
                        ExtractError::Parse {
                            format: "spreadsheet",
                            message: "ODS declared-cell count overflow".to_string(),
                        }
                    })?;
                    if declared_cells > MAX_SPREADSHEET_CELLS {
                        return Err(ExtractError::Parse {
                            format: "spreadsheet",
                            message: format!(
                                "ODS declares {declared_cells} expanded cells, over the \
                                 {MAX_SPREADSHEET_CELLS}-cell cap"
                            ),
                        });
                    }
                    in_row = false;
                }
            }
            Ok(Event::Eof) => break,
            _ => {}
        }
    }

    // Belt-and-suspenders: even if a quirk let the stream reach EOF with elements
    // still open, an unbalanced tree is not a document the ODS reader can finish.
    // Refuse rather than risk the unbounded path.
    if depth != 0 {
        return Err(ExtractError::Parse {
            format: "spreadsheet",
            message: "malformed ODS content.xml: unbalanced elements (truncated document)"
                .to_string(),
        });
    }

    Ok(())
}

fn ods_repeat(element: &quick_xml::events::BytesStart<'_>, key: &[u8]) -> Result<u64> {
    let Some(raw) = attr_value(element, key) else {
        return Ok(1);
    };
    let repeat = raw.parse::<u64>().map_err(|_| ExtractError::Parse {
        format: "spreadsheet",
        message: format!(
            "ODS attribute {} has an invalid repeat count",
            String::from_utf8_lossy(key)
        ),
    })?;
    if repeat == 0 {
        return Err(ExtractError::Parse {
            format: "spreadsheet",
            message: format!(
                "ODS attribute {} must be at least 1",
                String::from_utf8_lossy(key)
            ),
        });
    }
    Ok(repeat)
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
fn spreadsheet_dense_cells<RS>(
    workbook: &mut calamine::Sheets<RS>,
    name: &str,
) -> Result<Option<u64>>
where
    RS: std::io::Read + std::io::Seek + Clone,
{
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

/// Max spine itemrefs an `.epub` may declare before extraction refuses it. The
/// spine is attacker-controlled (`parse_opf` pushes every `<itemref>`), so a
/// few-KB file can declare millions; this bounds the read loop. Far above any
/// real book (which has well under a few hundred reading-order items).
const MAX_EPUB_SPINE_ITEMS: usize = 10_000;
const MAX_EPUB_MANIFEST_ITEMS: usize = 20_000;
const MAX_XML_EVENTS: usize = 1_000_000;

/// Hard cap on accumulated extracted-text bytes, shared by every adapter that
/// concatenates or materializes a large string from untrusted `sources/` input:
/// EPUB chapter concatenation, the HTML/XHTML flattener ([`html_to_text`]), and
/// the WordprocessingML run accumulator ([`wordprocessing_text`]). The common
/// backstop against output amplification — a long EPUB spine, a renderer
/// pathology, or a docx whose `document.xml` inflates to hundreds of MB — so
/// extracted text (and stdout) can't balloon without bound. Each adapter checks
/// it *during* accumulation, not only at the end, to keep peak memory bounded.
/// Far above any real document's flattened text; only hostile/corrupt input hits.
const MAX_EXTRACT_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

/// Extract an EPUB's reading-order text:
/// 1. read `META-INF/container.xml` → the OPF package path;
/// 2. parse the OPF `manifest` (id→href) and `spine` (ordered idref list);
/// 3. for each spine item, read its XHTML and flatten it with [`html_to_text`];
/// 4. join chapters with a blank line.
///
/// Bounded against spine amplification: the spine length is capped, each
/// distinct chapter is rendered at most once (memoized), and the total output is
/// capped — so a tiny crafted `.epub` can neither peg a core nor balloon memory.
///
/// Metadata carries `title` (the OPF `dc:title`) and `chapters` (spine length).
fn extract_epub(bytes: &[u8]) -> Result<Extracted> {
    let mut archive = open_zip(Cursor::new(bytes), "epub")?;
    let mut budget = ExtractionBudget::default();

    // 1. container.xml → OPF path.
    let container = read_zip_entry(&mut archive, "META-INF/container.xml", "epub", &mut budget)?;
    let opf_path = epub_opf_path(&container)?;

    // 2. OPF → base dir, manifest, spine, title.
    let opf = read_zip_entry(&mut archive, &opf_path, "epub", &mut budget)?;
    let parsed = parse_opf(&opf)?;
    let base = opf_base_dir(&opf_path);

    // Bound the spine length BEFORE the loop: `parse_opf` pushes every
    // attacker-controlled `<itemref idref>` verbatim, so a tiny crafted .epub can
    // declare millions of items. Even spine entries that render to empty text
    // still cost a zip read each, so the output cap below can't bound the loop on
    // its own — this guard does. Real books have well under a few hundred items.
    if parsed.spine.len() > MAX_EPUB_SPINE_ITEMS {
        return Err(ExtractError::Parse {
            format: "epub",
            message: format!(
                "spine declares {} items, exceeding the {} cap",
                parsed.spine.len(),
                MAX_EPUB_SPINE_ITEMS
            ),
        });
    }

    // 3. Spine items in order → flattened chapter text.
    let mut text = String::new();
    let mut chapters = 0u64;
    // Memoize rendered chapters by zip-entry path: a spine that references the
    // SAME manifest item repeatedly must re-render it in O(1), not re-decode the
    // zip entry and re-flatten its XHTML each time (the dominant CPU cost of the
    // spine-amplification DoS — a few-KB file could peg a core indefinitely).
    let mut rendered: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for idref in &parsed.spine {
        let Some(href) = parsed.manifest.get(idref) else {
            continue; // dangling spine ref; skip rather than fail
        };
        let entry = join_zip_path(&base, href);
        let chapter_text = match rendered.get(&entry) {
            Some(cached) => cached.clone(),
            None => {
                // A missing spine target is skipped (best-effort), not fatal.
                let Ok(chapter_xhtml) = read_zip_entry(&mut archive, &entry, "epub", &mut budget)
                else {
                    continue;
                };
                let t = html_to_text(chapter_xhtml.as_bytes())?;
                rendered.insert(entry.clone(), t.clone());
                t
            }
        };
        if !chapter_text.trim().is_empty() {
            if chapters > 0 {
                text.push('\n');
            }
            text.push_str(&chapter_text);
            text.push('\n');
            chapters += 1;
            // Hard output backstop: a long spine of DISTINCT items, or a near-cap
            // chapter referenced many times, must not balloon the extracted text
            // (and stdout) without bound.
            if text.len() > MAX_EXTRACT_OUTPUT_BYTES {
                return Err(ExtractError::Parse {
                    format: "epub",
                    message: format!(
                        "extracted text exceeds the {} byte cap",
                        MAX_EXTRACT_OUTPUT_BYTES
                    ),
                });
            }
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
    let mut events = 0usize;

    loop {
        events += 1;
        if events > MAX_XML_EVENTS {
            return Err(ExtractError::Parse {
                format: "epub",
                message: format!("OPF exceeds the {MAX_XML_EVENTS}-event parser budget"),
            });
        }
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match local_name(e.name().as_ref()) {
                b"item" => {
                    if let (Some(id), Some(href)) = (attr_value(&e, b"id"), attr_value(&e, b"href"))
                    {
                        if !manifest.contains_key(&id) && manifest.len() >= MAX_EPUB_MANIFEST_ITEMS
                        {
                            return Err(ExtractError::Parse {
                                format: "epub",
                                message: format!(
                                    "manifest exceeds the {MAX_EPUB_MANIFEST_ITEMS}-item cap"
                                ),
                            });
                        }
                        manifest.insert(id, href);
                    }
                }
                b"itemref" => {
                    if let Some(idref) = attr_value(&e, b"idref") {
                        if spine.len() >= MAX_EPUB_SPINE_ITEMS {
                            return Err(ExtractError::Parse {
                                format: "epub",
                                message: format!(
                                    "spine exceeds the {MAX_EPUB_SPINE_ITEMS}-item cap"
                                ),
                            });
                        }
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
                        if !manifest.contains_key(&id) && manifest.len() >= MAX_EPUB_MANIFEST_ITEMS
                        {
                            return Err(ExtractError::Parse {
                                format: "epub",
                                message: format!(
                                    "manifest exceeds the {MAX_EPUB_MANIFEST_ITEMS}-item cap"
                                ),
                            });
                        }
                        manifest.insert(id, href);
                    }
                }
                b"itemref" => {
                    if let Some(idref) = attr_value(&e, b"idref") {
                        if spine.len() >= MAX_EPUB_SPINE_ITEMS {
                            return Err(ExtractError::Parse {
                                format: "epub",
                                message: format!(
                                    "spine exceeds the {MAX_EPUB_SPINE_ITEMS}-item cap"
                                ),
                            });
                        }
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
                    if title_buf.len() > 1024 * 1024 {
                        return Err(ExtractError::Parse {
                            format: "epub",
                            message: "OPF title exceeds the 1 MiB metadata cap".to_string(),
                        });
                    }
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
fn extract_html(bytes: &[u8]) -> Result<Extracted> {
    let text = html_to_text(bytes)?;
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
    // Bound table size BEFORE html2text lays the table out. Depth alone misses
    // the *width* amplification: a flat `<table><tr><td>x</td>×200_000</tr>` is
    // only ~3 deep, so the nesting guard never fires — but html2text lays the row
    // out at the 10_000 wrap width and draws full-width U+2500 box rules per row
    // boundary, turning a ~2 MB input into multi-GB output and 9 GB+ peak RSS
    // (resource-exhaustion DoS on untrusted `sources/` input). The MAX_EXTRACT_
    // OUTPUT_BYTES backstop below cannot prevent that spike — html2text has
    // already materialized the giant string by the time it's measured. So we
    // refuse the layout BEFORE it happens, on the structural cause (table cell
    // counts — both single-row width and the overall total), mirroring the
    // refuse-before-allocate precedent of MAX_SPREADSHEET_CELLS / MAX_ZIP_ENTRY_
    // BYTES. EPUB/xhtml chapters route through here too, so this covers them.
    if let Some(bomb) =
        html_table_amplification(html, MAX_HTML_TABLE_ROW_CELLS, MAX_HTML_TABLE_CELLS)
    {
        let message = match bomb {
            TableBomb::RowTooWide(width) => format!(
                "a table row declares {width} cells, exceeding the \
                 {MAX_HTML_TABLE_ROW_CELLS}-cell-per-row cap (malformed or hostile input)"
            ),
            TableBomb::TooManyCells(total) => format!(
                "HTML declares over {total} table cells, exceeding the \
                 {MAX_HTML_TABLE_CELLS}-cell cap (malformed or hostile input)"
            ),
        };
        return Err(ExtractError::Parse {
            format: "html",
            message,
        });
    }
    let text = html2text::config::with_decorator(PlainContentDecorator)
        .string_from_read(html, 10_000)
        .map_err(|e| ExtractError::Parse {
            format: "html",
            message: e.to_string(),
        })?;
    // Hard output backstop. The structural pre-checks above stop the known
    // amplifier (wide tables) before the layout pass, but they cannot anticipate
    // every renderer pathology; this final byte cap guarantees the HTML path can
    // never return (or stream to stdout) more than the same ceiling EPUB enforces,
    // independent of *why* the output grew. A real document's flattened text is
    // far under 64 MB; only hostile or corrupt input reaches it.
    if text.len() > MAX_EXTRACT_OUTPUT_BYTES {
        return Err(ExtractError::Parse {
            format: "html",
            message: format!(
                "extracted text exceeds the {MAX_EXTRACT_OUTPUT_BYTES} byte cap \
                 (malformed or hostile input)"
            ),
        });
    }
    Ok(text)
}

/// The deepest block-element nesting `html_to_text` tolerates. No legitimate
/// document nests containers anywhere near this deep; the cap exists purely to
/// refuse the deeply-nested bomb that makes html2text's layout pass run for
/// minutes. Set with large headroom so it can only fire on pathological input.
const MAX_HTML_NESTING_DEPTH: usize = 4_096;

/// Ceiling on the number of cells (`<td>`/`<th>`) in any SINGLE table row before
/// extraction refuses the document. This is the primary structural guard against
/// the wide-table amplification DoS: html2text lays a table out at the 10_000
/// wrap width and draws full-width U+2500 box rules sized to the row, so a flat
/// `<td>`×N single row is the worst case — N=200_000 in a ~2 MB file balloons to
/// multi-GB output and 9 GB+ peak RSS. *Row width* is what drives the spike (a
/// tall narrow table of the same total cell count costs an order of magnitude
/// less), so we bound it directly and BEFORE html2text runs — the same
/// refuse-before-allocate precedent as MAX_SPREADSHEET_CELLS / MAX_ZIP_ENTRY_BYTES.
///
/// 4_096 columns is far beyond any real document's table width — a spreadsheet
/// export with thousands of columns is already unreadable as flattened text —
/// yet keeps the worst-case (all in one row) layout under ~16 MB peak, measured.
const MAX_HTML_TABLE_ROW_CELLS: usize = 4_096;

/// Ceiling on the TOTAL number of table cells (`<td>`/`<th>`) across the whole
/// document. The backstop to [`MAX_HTML_TABLE_ROW_CELLS`] for the *tall* shape:
/// even narrow rows, if there are enough of them, grow html2text's layout memory
/// roughly linearly in total cells (independent of output size). The row-width
/// cap alone wouldn't bound a million-row × few-column table, so this caps the
/// aggregate too. Checked in the same single scan, before html2text runs.
///
/// 200_000 cells is far above any real tabular document (a 20_000-row × 10-column
/// table) yet keeps the worst measured tall-table peak under ~450 MB. Set
/// generously so it can only fire on pathological input.
const MAX_HTML_TABLE_CELLS: usize = 200_000;

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
    let mut stack: Vec<&[u8]> = Vec::with_capacity(limit.min(256));
    let mut i = 0usize;
    while let Some(tag) = next_html_tag(html, &mut i) {
        if tag.closing {
            // An unmatched closing tag must not lower the security counter.
            // HTML5 ignores or repairs many mismatches; blindly decrementing on
            // any `</x>` let an attacker interleave bogus closes to hide a
            // deeply nested tree from this guard.
            if stack
                .last()
                .is_some_and(|open| open.eq_ignore_ascii_case(tag.name))
            {
                stack.pop();
            }
            continue;
        }
        let is_void = std::str::from_utf8(tag.name)
            .map(|name| {
                HTML_VOID_ELEMENTS
                    .iter()
                    .any(|void| name.eq_ignore_ascii_case(void))
            })
            .unwrap_or(false);
        if !tag.self_closing && !is_void {
            stack.push(tag.name);
            if stack.len() > limit {
                return Some(stack.len());
            }
            if skip_raw_text_element(html, &mut i, tag.name) {
                stack.pop();
            }
        }
    }
    None
}

/// Why a table-cell pre-check refused an HTML document, with the offending count.
/// Returned by [`html_table_amplification`] so the caller can name the exact
/// structural cause (row width vs. total cells) in the typed error.
enum TableBomb {
    /// A single row holds more than [`MAX_HTML_TABLE_ROW_CELLS`] cells — the wide
    /// shape that html2text amplifies into multi-GB output. Carries the row width.
    RowTooWide(usize),
    /// The document holds more than [`MAX_HTML_TABLE_CELLS`] cells in total — the
    /// tall shape whose aggregate grows html2text's layout memory. Carries the
    /// total count (at the moment the cap was crossed).
    TooManyCells(usize),
}

/// Scan an HTML byte stream once and return `Some(TableBomb)` if its table cells
/// would amplify html2text's layout past a safe bound, else `None`. Two bounds
/// are checked in the single pass: the max cells in any one `<tr>` (the *width*
/// amplifier, the dominant cost) against `row_limit`, and the total cell count
/// (the *tall* aggregate) against `total_limit`. Whichever trips first wins.
///
/// Like [`html_block_nesting_exceeds`] this is a crude, allocation-free tag
/// scanner — NOT a parser. It counts cell *opens* (`<td>`/`<th>`); closing tags
/// and self-closing forms add no cell. A `<tr>` open resets the per-row counter.
/// Comments/doctype/PI are skipped (so a `<td>` inside a comment isn't counted)
/// and a stray `<` in prose is ignored. The exact tally past a limit doesn't
/// matter, only whether the limit is crossed — so we can early-return.
fn html_table_amplification(
    html: &[u8],
    row_limit: usize,
    total_limit: usize,
) -> Option<TableBomb> {
    let mut total: usize = 0;
    let mut row_cells: usize = 0;
    let mut i = 0usize;
    while let Some(tag) = next_html_tag(html, &mut i) {
        if tag.closing {
            continue;
        }
        if tag.name.eq_ignore_ascii_case(b"tr") {
            // A new row resets the per-row width tally. (A `<td>` outside any row
            // still counts toward both totals; resetting only on `<tr>` is the
            // conservative choice — it can never under-count a real row's width.)
            row_cells = 0;
        } else if tag.name.eq_ignore_ascii_case(b"td") || tag.name.eq_ignore_ascii_case(b"th") {
            total += 1;
            row_cells += 1;
            if row_cells > row_limit {
                return Some(TableBomb::RowTooWide(row_cells));
            }
            if total > total_limit {
                return Some(TableBomb::TooManyCells(total));
            }
        }
        let _ = skip_raw_text_element(html, &mut i, tag.name);
    }
    None
}

#[derive(Clone, Copy)]
struct HtmlTag<'a> {
    name: &'a [u8],
    closing: bool,
    self_closing: bool,
}

/// Return the next real tag using an allocation-free lexical pass that honors
/// quoted attributes and full comment/CDATA bodies. The previous `first '>'`
/// scanner could be desynchronized by `data=\"></tr>\"` or `<!-- > ... -->`,
/// letting hostile table/depth markup reach html2text uncounted.
fn next_html_tag<'a>(html: &'a [u8], cursor: &mut usize) -> Option<HtmlTag<'a>> {
    while *cursor < html.len() {
        let start = html[*cursor..].iter().position(|byte| *byte == b'<')? + *cursor;
        if html[start..].starts_with(b"<!--") {
            *cursor = find_bytes(html, start + 4, b"-->").unwrap_or(html.len());
            if *cursor < html.len() {
                *cursor += 3;
            }
            continue;
        }
        if html[start..].starts_with(b"<![CDATA[") {
            *cursor = find_bytes(html, start + 9, b"]]>").unwrap_or(html.len());
            if *cursor < html.len() {
                *cursor += 3;
            }
            continue;
        }

        let mut pos = start + 1;
        let closing = html.get(pos) == Some(&b'/');
        if closing {
            pos += 1;
        }
        while html.get(pos).is_some_and(u8::is_ascii_whitespace) {
            pos += 1;
        }
        if !html.get(pos).is_some_and(u8::is_ascii_alphabetic) {
            // Declaration, processing instruction, or literal `<`: skip its
            // quote-aware terminator, then keep looking.
            *cursor = html_tag_end(html, pos).unwrap_or(html.len());
            continue;
        }
        let name_start = pos;
        while html
            .get(pos)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'-'))
        {
            pos += 1;
        }
        let end = html_tag_end(html, pos)?;
        let mut before_end = end.saturating_sub(1);
        while before_end > start && html[before_end - 1].is_ascii_whitespace() {
            before_end -= 1;
        }
        let self_closing = before_end > start && html[before_end - 1] == b'/';
        *cursor = end;
        return Some(HtmlTag {
            name: &html[name_start..pos],
            closing,
            self_closing,
        });
    }
    None
}

fn html_tag_end(html: &[u8], from: usize) -> Option<usize> {
    let mut quote: Option<u8> = None;
    let mut pos = from;
    while pos < html.len() {
        match (quote, html[pos]) {
            (Some(active), byte) if byte == active => quote = None,
            (None, byte @ (b'\'' | b'"')) => quote = Some(byte),
            (None, b'>') => return Some(pos + 1),
            _ => {}
        }
        pos += 1;
    }
    None
}

fn find_bytes(haystack: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| from + offset)
}

/// HTML raw-text/RCDATA elements do not tokenize `<tr>`/`<td>` strings in their
/// contents as markup. Skip directly to the matching closing tag so script or
/// style text cannot reset the row counter and bypass the table guard.
fn skip_raw_text_element(html: &[u8], cursor: &mut usize, name: &[u8]) -> bool {
    if ![b"script".as_slice(), b"style", b"textarea", b"title"]
        .iter()
        .any(|raw| name.eq_ignore_ascii_case(raw))
    {
        return false;
    }
    let mut pos = *cursor;
    while let Some(relative) = html[pos..].iter().position(|byte| *byte == b'<') {
        let start = pos + relative;
        let mut probe = start + 1;
        if html.get(probe) != Some(&b'/') {
            pos = start + 1;
            continue;
        }
        probe += 1;
        while html.get(probe).is_some_and(u8::is_ascii_whitespace) {
            probe += 1;
        }
        let end_name = probe.saturating_add(name.len());
        if html
            .get(probe..end_name)
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(name))
            && html
                .get(end_name)
                .is_some_and(|byte| byte.is_ascii_whitespace() || matches!(byte, b'>' | b'/'))
        {
            *cursor = html_tag_end(html, end_name).unwrap_or(html.len());
            return true;
        }
        pos = start + 1;
    }
    *cursor = html.len();
    true
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
    mut reader: R,
    format: &'static str,
) -> Result<zip::ZipArchive<R>> {
    preflight_zip_directory(&mut reader, format)?;
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|e| ExtractError::Parse {
            format,
            message: format!("rewinding zip container after preflight: {e}"),
        })?;
    zip::ZipArchive::new(reader).map_err(|e| ExtractError::Parse {
        format,
        message: format!("not a valid zip container: {e}"),
    })
}

/// Document ZIPs are small structured containers, not general-purpose backup
/// archives. Bound the attacker-controlled central directory before `zip`
/// materializes one entry record per member. Per-entry inflation caps alone do
/// not help a file with millions of empty members: parsing its central directory
/// can exhaust memory before any named document member is opened.
const MAX_ZIP_ENTRIES: u16 = 20_000;
const MAX_ZIP_CENTRAL_DIRECTORY_BYTES: u32 = 32 * 1024 * 1024;

/// Parse the classic EOCD from the bounded tail of a ZIP and reject oversized,
/// multi-disk, inconsistent, or ZIP64 containers before `ZipArchive::new`.
///
/// ZIP64 is deliberately refused for in-process document adapters. The complete
/// compressed document is already capped at 128 MiB and legitimate DOCX/XLSX/
/// ODS/EPUB files do not need 65k entries or 4-GiB offsets; a ZIP64 sentinel
/// here is therefore hostile/corrupt for this surface.
fn preflight_zip_directory<R: Read + Seek>(reader: &mut R, format: &'static str) -> Result<()> {
    const EOCD_LEN: usize = 22;
    const MAX_COMMENT: usize = u16::MAX as usize;

    let file_len = reader
        .seek(SeekFrom::End(0))
        .map_err(|e| ExtractError::Parse {
            format,
            message: format!("sizing zip container: {e}"),
        })?;
    let tail_len = usize::try_from(file_len.min((EOCD_LEN + MAX_COMMENT) as u64))
        .expect("bounded ZIP tail fits usize");
    if tail_len < EOCD_LEN {
        return Err(ExtractError::Parse {
            format,
            message: "not a valid zip container: missing end-of-central-directory".to_string(),
        });
    }
    reader
        .seek(SeekFrom::Start(file_len - tail_len as u64))
        .map_err(|e| ExtractError::Parse {
            format,
            message: format!("seeking to zip directory tail: {e}"),
        })?;
    let mut tail = vec![0u8; tail_len];
    reader
        .read_exact(&mut tail)
        .map_err(|e| ExtractError::Parse {
            format,
            message: format!("reading zip directory tail: {e}"),
        })?;

    let eocd = (0..=tail_len - EOCD_LEN).rev().find(|&offset| {
        tail[offset..].starts_with(b"PK\x05\x06")
            && offset
                + EOCD_LEN
                + usize::from(u16::from_le_bytes([tail[offset + 20], tail[offset + 21]]))
                == tail_len
    });
    let Some(offset) = eocd else {
        return Err(ExtractError::Parse {
            format,
            message: "not a valid zip container: missing end-of-central-directory".to_string(),
        });
    };
    let u16_at = |position: usize| u16::from_le_bytes([tail[position], tail[position + 1]]);
    let u32_at = |position: usize| {
        u32::from_le_bytes([
            tail[position],
            tail[position + 1],
            tail[position + 2],
            tail[position + 3],
        ])
    };
    let disk = u16_at(offset + 4);
    let central_disk = u16_at(offset + 6);
    let entries_on_disk = u16_at(offset + 8);
    let entries = u16_at(offset + 10);
    let central_size = u32_at(offset + 12);
    let central_offset = u32_at(offset + 16);

    if disk != 0 || central_disk != 0 || entries_on_disk != entries {
        return Err(ExtractError::Parse {
            format,
            message: "multi-disk zip containers are not accepted".to_string(),
        });
    }
    if entries == u16::MAX || central_size == u32::MAX || central_offset == u32::MAX {
        return Err(ExtractError::Parse {
            format,
            message: "ZIP64 document containers are not accepted".to_string(),
        });
    }
    if entries > MAX_ZIP_ENTRIES {
        return Err(ExtractError::Parse {
            format,
            message: format!(
                "zip central directory declares {entries} entries, over the {MAX_ZIP_ENTRIES}-entry cap"
            ),
        });
    }
    if central_size > MAX_ZIP_CENTRAL_DIRECTORY_BYTES {
        return Err(ExtractError::Parse {
            format,
            message: format!(
                "zip central directory declares {central_size} bytes, over the \
                 {MAX_ZIP_CENTRAL_DIRECTORY_BYTES}-byte cap"
            ),
        });
    }
    let eocd_absolute = file_len - tail_len as u64 + offset as u64;
    let central_end = u64::from(central_offset)
        .checked_add(u64::from(central_size))
        .ok_or_else(|| ExtractError::Parse {
            format,
            message: "zip central-directory bounds overflow".to_string(),
        })?;
    if central_end > eocd_absolute {
        return Err(ExtractError::Parse {
            format,
            message: "zip central directory extends beyond its end record".to_string(),
        });
    }
    Ok(())
}

/// Cap on a single decompressed zip entry. docx/epub members are XML text — a
/// member that inflates past this ceiling is a decompression bomb or corruption,
/// not real evidence. `sources/` is untrusted input, so bound the read rather
/// than let `read_to_end` follow a hostile DEFLATE stream until OOM.
const MAX_ZIP_ENTRY_BYTES: u64 = 32 * 1024 * 1024;
const MAX_ZIP_INFLATED_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Default)]
struct ExtractionBudget {
    inflated_bytes: u64,
}

impl ExtractionBudget {
    fn charge_inflated(&mut self, bytes: u64, format: &'static str) -> Result<()> {
        self.inflated_bytes =
            self.inflated_bytes
                .checked_add(bytes)
                .ok_or_else(|| ExtractError::Parse {
                    format,
                    message: "aggregate inflated-byte budget overflow".to_string(),
                })?;
        if self.inflated_bytes > MAX_ZIP_INFLATED_BYTES {
            return Err(ExtractError::Parse {
                format,
                message: format!(
                    "document inflates to over the {MAX_ZIP_INFLATED_BYTES}-byte aggregate cap"
                ),
            });
        }
        Ok(())
    }
}

/// Read a single zip entry to a UTF-8 string, bounded by [`MAX_ZIP_ENTRY_BYTES`]
/// so a zip-bomb member cannot exhaust memory. A missing entry, an over-cap
/// entry, or a read failure is a typed [`ExtractError::Parse`]; invalid UTF-8 is
/// lossily decoded (OOXML / XHTML are declared UTF-8, but we never panic on a
/// stray byte).
fn read_zip_entry<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &str,
    format: &'static str,
    budget: &mut ExtractionBudget,
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
    budget.charge_inflated(declared, format)?;
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
    // A lying header may declare fewer bytes than the stream produces. Charge
    // the delta after the bounded read so the aggregate cap follows actual
    // inflation without double-counting the declared portion.
    if bytes.len() as u64 > declared {
        budget.charge_inflated(bytes.len() as u64 - declared, format)?;
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Look up a start/empty element's attribute value by local name, returning it
/// unescaped as an owned `String`. Prefix-agnostic on the attribute key.
fn attr_value(elem: &quick_xml::events::BytesStart<'_>, key: &[u8]) -> Option<String> {
    elem.attributes().flatten().find_map(|attr| {
        if local_name(attr.key.as_ref()) == key {
            let encoded = std::str::from_utf8(attr.value.as_ref()).ok()?;
            quick_xml::escape::unescape(encoded)
                .ok()
                .map(|cow| cow.into_owned())
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn extract_refuses_oversized_sparse_input_before_adapter_allocation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hostile.pdf");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_DOCUMENT_INPUT_BYTES + 1).unwrap();

        let err = extract(&path).unwrap_err();
        assert!(
            matches!(err, ExtractError::Parse { format: "pdf", .. }),
            "oversized document must fail at the metadata gate: {err:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn extract_refuses_symlink_input_instead_of_reopening_its_target() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("secret.pdf");
        std::fs::write(&secret, b"not actually a pdf; still private").unwrap();
        let selected = dir.path().join("selected.pdf");
        symlink(&secret, &selected).unwrap();

        let error = extract(&selected).expect_err("document input symlinks must fail closed");
        assert!(matches!(error, ExtractError::Io(_)), "got {error:?}");
    }

    fn classic_eocd(entries: u16, central_size: u32, central_offset: u32) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(22);
        bytes.extend_from_slice(b"PK\x05\x06");
        bytes.extend_from_slice(&0u16.to_le_bytes()); // this disk
        bytes.extend_from_slice(&0u16.to_le_bytes()); // central directory disk
        bytes.extend_from_slice(&entries.to_le_bytes());
        bytes.extend_from_slice(&entries.to_le_bytes());
        bytes.extend_from_slice(&central_size.to_le_bytes());
        bytes.extend_from_slice(&central_offset.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes()); // comment length
        bytes
    }

    #[test]
    fn zip_preflight_accepts_a_bounded_classic_directory() {
        let mut archive = Cursor::new(classic_eocd(0, 0, 0));
        preflight_zip_directory(&mut archive, "docx").unwrap();
    }

    #[test]
    fn zip_preflight_rejects_entry_count_before_zip_allocates_records() {
        let mut archive = Cursor::new(classic_eocd(MAX_ZIP_ENTRIES + 1, 0, 0));
        let error = preflight_zip_directory(&mut archive, "docx")
            .expect_err("hostile central-directory count must be refused");
        assert!(
            matches!(error, ExtractError::Parse { format: "docx", ref message }
                if message.contains("entry cap")),
            "got {error:?}"
        );
    }

    #[test]
    fn zip_preflight_rejects_declared_central_directory_size_before_allocation() {
        let mut archive = Cursor::new(classic_eocd(1, MAX_ZIP_CENTRAL_DIRECTORY_BYTES + 1, 0));
        let error = preflight_zip_directory(&mut archive, "epub")
            .expect_err("hostile central-directory size must be refused");
        assert!(
            matches!(error, ExtractError::Parse { format: "epub", ref message }
                if message.contains("byte cap")),
            "got {error:?}"
        );
    }

    #[test]
    fn zip_preflight_rejects_zip64_sentinels() {
        let mut archive = Cursor::new(classic_eocd(u16::MAX, u32::MAX, u32::MAX));
        let error = preflight_zip_directory(&mut archive, "spreadsheet")
            .expect_err("ZIP64 documents are outside the bounded adapter contract");
        assert!(
            matches!(error, ExtractError::Parse { format: "spreadsheet", ref message }
                if message.contains("ZIP64")),
            "got {error:?}"
        );
    }

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

    #[test]
    fn regression_html_self_closing_non_void_is_flat_not_deep() {
        // Adversarial review #17: a self-closing NON-void element (`<div/>`,
        // `<section />`) is flat, not a nesting increment. The off-by-one read the
        // `>` byte (always present) instead of the `/` (at end-2), so the
        // self-closing check was dead and N such elements miscounted as depth N,
        // falsely tripping the cap on a valid, flat document (XHTML/EPUB chapters
        // commonly self-close).
        let flat = "<div/>".repeat(MAX_HTML_NESTING_DEPTH + 1000);
        assert!(
            html_block_nesting_exceeds(flat.as_bytes(), MAX_HTML_NESTING_DEPTH).is_none(),
            "a flat run of self-closing <div/> must not trip the nesting cap"
        );
        let spaced = "<section />".repeat(MAX_HTML_NESTING_DEPTH + 1000);
        assert!(
            html_block_nesting_exceeds(spaced.as_bytes(), MAX_HTML_NESTING_DEPTH).is_none(),
            "`<section />` (space before slash) is self-closing too"
        );
        // Defense intact: genuine deep nesting of the SAME tag still trips it.
        let deep = "<div>".repeat(MAX_HTML_NESTING_DEPTH + 1);
        assert!(
            html_block_nesting_exceeds(deep.as_bytes(), MAX_HTML_NESTING_DEPTH).is_some(),
            "real deep nesting must still trip the cap"
        );
    }

    /// The table scanner counts `<td>`/`<th>` opens, ignores closing and
    /// commented-out cells, resets the per-row tally on `<tr>`, and reports the
    /// right bomb variant (row-too-wide vs. too-many-cells). Small-limit probes
    /// keep the test fast.
    #[test]
    fn html_table_scanner_counts_cells_and_classifies_shape() {
        // 5 real cells (td + th, case-insensitive) in ONE row; the commented cell
        // and the closing tags must NOT be counted.
        let one_row = b"<table><tr><td>a</td><TH>b</TH><td>c</td>\
<!-- <td>x</td> --><td>d</td><td>e</td></tr></table>";
        // Row-width cap of 4 trips on the 5-wide row.
        assert!(
            matches!(
                html_table_amplification(one_row, 4, 1000),
                Some(TableBomb::RowTooWide(w)) if w == 5
            ),
            "a 5-wide row must trip the row-width cap as RowTooWide(5)"
        );
        // Generous row cap, generous total → no bomb (commented cell not counted).
        assert!(
            html_table_amplification(one_row, 100, 100).is_none(),
            "5 cells under both caps must not fire"
        );

        // Many narrow rows: width stays at 1, total accumulates → TooManyCells.
        let tall: String = "<table>".to_string() + &"<tr><td>x</td></tr>".repeat(20) + "</table>";
        assert!(
            matches!(
                html_table_amplification(tall.as_bytes(), 100, 10),
                Some(TableBomb::TooManyCells(t)) if t == 11
            ),
            "20 single-cell rows must trip the total cap at 11 (width stays under)"
        );

        // A document with no tables never trips it.
        assert!(
            html_table_amplification(b"<p>plain prose, a < b</p>", 0, 0).is_none(),
            "no table cells means the scanner never fires"
        );
    }

    #[test]
    fn html_guards_cannot_be_desynchronized_by_quoted_gt_or_comment_markup() {
        // `>` and a fake closing row inside an attribute are data, not tokens.
        // The old first-`>` scanner ended the `<td ...>` early and then treated
        // `</tr>` inside the quoted value as markup, resetting/undercounting the
        // real row that follows.
        let quoted = br#"<table><tr><td data="></tr>">a</td><td>b</td><td>c</td></tr></table>"#;
        assert!(matches!(
            html_table_amplification(quoted, 2, 100),
            Some(TableBomb::RowTooWide(3))
        ));

        // A `>` does not terminate an HTML comment; fake tags after it remain
        // commented out until `-->`.
        let commented = b"<table><tr><!-- > <tr><td>fake</td> --><td>a</td><td>b</td></tr></table>";
        assert!(matches!(
            html_table_amplification(commented, 1, 100),
            Some(TableBomb::RowTooWide(2))
        ));

        // Raw script text is not parsed as table markup by HTML5. It therefore
        // cannot inject fake `<tr>` resets between real cells.
        let script =
            b"<table><tr><td>a</td><script>\"<tr><td>fake</td>\"</script><td>b</td></tr></table>";
        assert!(matches!(
            html_table_amplification(script, 1, 100),
            Some(TableBomb::RowTooWide(2))
        ));

        // Bogus closing tags must not lower the nesting counter.
        let mut depth_bypass = String::new();
        for _ in 0..=MAX_HTML_NESTING_DEPTH {
            depth_bypass.push_str("<div></bogus>");
        }
        assert!(
            html_block_nesting_exceeds(depth_bypass.as_bytes(), MAX_HTML_NESTING_DEPTH).is_some()
        );
    }

    /// The wide-table amplification bomb (HIGH DoS): a tiny flat `<td>`×N row
    /// makes html2text emit gigantic U+2500 box rules (multi-GB output, 9 GB+
    /// RSS) from a ~MB input. The row-width pre-check refuses it BEFORE the
    /// layout pass — fast, typed, never materializing the giant string — while a
    /// normal small table still extracts intact (no regression).
    #[test]
    fn regression_html_wide_table_bomb_is_refused_small_table_ok() {
        // Just over the per-row width cap in a single row — the exact shape of the
        // real exploit (a flat `<td>`×N row), kept small enough that the test is
        // fast precisely BECAUSE the pre-check refuses before html2text runs.
        let cells = MAX_HTML_TABLE_ROW_CELLS + 10;
        let bomb = format!(
            "<html><body><table><tr>{}</tr></table></body></html>",
            "<td>x</td>".repeat(cells)
        );
        // The pre-check fires; html2text is never reached, so no giant string is
        // materialized (the test would OOM/hang otherwise).
        assert!(
            matches!(
                html_table_amplification(
                    bomb.as_bytes(),
                    MAX_HTML_TABLE_ROW_CELLS,
                    MAX_HTML_TABLE_CELLS
                ),
                Some(TableBomb::RowTooWide(_))
            ),
            "an over-cap wide row must trip the scanner as RowTooWide"
        );
        let err = html_to_text(bomb.as_bytes()).unwrap_err();
        assert!(
            matches!(&err, ExtractError::Parse { format, message }
                if *format == "html" && message.contains("cell-per-row")),
            "the wide-table bomb must be refused with a typed row-width error; got {err:?}"
        );
        assert_eq!(err.code(), "EXTRACT_PARSE_ERROR");

        // A tall table whose TOTAL cells exceed the aggregate cap is also refused
        // (narrow rows, but too many of them) — bounding the other shape.
        let rows = MAX_HTML_TABLE_CELLS / 2 + 5; // 2 cells/row, just over the total cap
        let tall = format!(
            "<html><body><table>{}</table></body></html>",
            "<tr><td>a</td><td>b</td></tr>".repeat(rows)
        );
        let err = html_to_text(tall.as_bytes()).unwrap_err();
        assert!(
            matches!(&err, ExtractError::Parse { message, .. } if message.contains("table cells")),
            "an over-cap tall table must be refused with the total-cell error; got {err:?}"
        );

        // A normal small table still extracts its cell content cleanly.
        let ok = "<html><body><table>\
<tr><td>Name</td><td>Amount</td></tr>\
<tr><td>Acme</td><td>1200</td></tr></table></body></html>";
        let out = html_to_text(ok.as_bytes()).unwrap();
        for token in ["Name", "Amount", "Acme", "1200"] {
            assert!(
                out.contains(token),
                "small table must keep {token:?}, got {out:?}"
            );
        }
        // And the output is far under the byte cap.
        assert!(
            out.len() < MAX_EXTRACT_OUTPUT_BYTES,
            "a 2x2 table must not approach the output cap (got {} bytes)",
            out.len()
        );
    }

    /// Build an `.epub` whose single chapter body is `chapter_body` (spliced
    /// inside `<body>…</body>`). Lets a test exercise a hostile chapter shape
    /// (e.g. a wide table) through the real EPUB → html_to_text path.
    fn write_epub_with_chapter_body(dest: &Path, chapter_body: &str) {
        use std::io::Write;
        let container = "<?xml version=\"1.0\"?>\
<container version=\"1.0\" xmlns=\"urn:oasis:names:tc:opendocument:xmlns:container\">\
<rootfiles><rootfile full-path=\"OEBPS/content.opf\" \
media-type=\"application/oebps-package+xml\"/></rootfiles></container>";
        let opf = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<package xmlns=\"http://www.idpf.org/2007/opf\" version=\"3.0\" unique-identifier=\"id\">\
<metadata xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><dc:title>Wide</dc:title></metadata>\
<manifest><item id=\"c1\" href=\"chapter.xhtml\" media-type=\"application/xhtml+xml\"/></manifest>\
<spine><itemref idref=\"c1\"/></spine></package>";
        let chapter = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<html xmlns=\"http://www.w3.org/1999/xhtml\"><body>{chapter_body}</body></html>"
        );
        let file = std::fs::File::create(dest).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer.start_file("mimetype", stored).unwrap();
        writer.write_all(b"application/epub+zip").unwrap();
        writer.start_file("META-INF/container.xml", stored).unwrap();
        writer.write_all(container.as_bytes()).unwrap();
        writer.start_file("OEBPS/content.opf", stored).unwrap();
        writer.write_all(opf.as_bytes()).unwrap();
        writer.start_file("OEBPS/chapter.xhtml", stored).unwrap();
        writer.write_all(chapter.as_bytes()).unwrap();
        writer.finish().unwrap();
    }

    /// An EPUB chapter that is itself a wide-table bomb routes through
    /// `html_to_text` and must be refused with the same typed table-cell error,
    /// before any giant chapter string is materialized — so EPUB peak memory
    /// stays bounded per chapter, not just at the final concatenation check.
    #[test]
    fn regression_epub_wide_table_chapter_is_refused() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bomb = tmp.path().join("wide.epub");
        let body = format!(
            "<table><tr>{}</tr></table>",
            "<td>x</td>".repeat(MAX_HTML_TABLE_ROW_CELLS + 10)
        );
        write_epub_with_chapter_body(&bomb, &body);
        let err = extract(&bomb).unwrap_err();
        assert!(
            matches!(&err, ExtractError::Parse { message, .. } if message.contains("cell-per-row")),
            "a wide-table EPUB chapter must be refused with the row-width error; got {err:?}"
        );

        // A normal EPUB chapter with a small table still extracts.
        let ok = tmp.path().join("ok.epub");
        write_epub_with_chapter_body(
            &ok,
            "<p>Chapter one.</p><table><tr><td>Cell A</td><td>Cell B</td></tr></table>",
        );
        let got = extract(&ok).unwrap();
        assert_eq!(got.metadata["chapters"], MetaValue::Num(1));
        assert!(
            got.text.contains("Cell A") && got.text.contains("Cell B"),
            "small EPUB table must extract, got {:?}",
            got.text
        );
    }

    /// A `.docx` whose `word/document.xml` expands to an enormous run of `<w:t>`
    /// text must be refused by the output-byte cap during accumulation (docx
    /// parity with HTML/EPUB), while a normal docx extracts unchanged.
    #[test]
    fn regression_docx_oversized_text_is_bounded() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bomb = tmp.path().join("huge.docx");
        // One paragraph whose single run holds > MAX_EXTRACT_OUTPUT_BYTES of text.
        // (Built as one big string so the body XML itself is the amplified input;
        // a real exploit relies on zip deflate to ship this compactly.)
        let big = "A".repeat(MAX_EXTRACT_OUTPUT_BYTES + 1024);
        let body = format!("<w:p><w:r><w:t>{big}</w:t></w:r></w:p>");
        write_docx(&bomb, &body);
        let err = extract(&bomb).unwrap_err();
        assert!(
            matches!(&err, ExtractError::Parse { format, message }
                if *format == "docx" && message.contains("byte cap")),
            "an oversized docx must be refused with the output-cap error; got {err:?}"
        );

        // A normal docx still extracts intact (no regression).
        let ok = tmp.path().join("ok.docx");
        write_docx(
            &ok,
            "<w:p><w:r><w:t>Quarterly report total 1200.</w:t></w:r></w:p>",
        );
        let got = extract(&ok).unwrap();
        assert_eq!(got.text, "Quarterly report total 1200.\n");
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

    /// Build a minimal `.ods` (OpenDocument Spreadsheet) whose `content.xml`
    /// body is exactly `content_xml`, written to `dest`. Lets a test inject a
    /// truncated/unclosed document XML and drive it through the real
    /// `extract_spreadsheet` ODS path. The mimetype + manifest members make
    /// calamine's auto-detector recognize the package as ODS.
    fn write_ods_with_content(dest: &Path, content_xml: &str) {
        use std::io::Write;
        let manifest = "<?xml version=\"1.0\"?>\
<manifest:manifest xmlns:manifest=\"urn:oasis:names:tc:opendocument:xmlns:manifest:1.0\">\
<manifest:file-entry manifest:full-path=\"/\" \
manifest:media-type=\"application/vnd.oasis.opendocument.spreadsheet\"/></manifest:manifest>";
        let file = std::fs::File::create(dest).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        // The mimetype member must be the first, STORED entry for OpenDocument.
        writer.start_file("mimetype", stored).unwrap();
        writer
            .write_all(b"application/vnd.oasis.opendocument.spreadsheet")
            .unwrap();
        writer.start_file("META-INF/manifest.xml", stored).unwrap();
        writer.write_all(manifest.as_bytes()).unwrap();
        writer.start_file("content.xml", stored).unwrap();
        writer.write_all(content_xml.as_bytes()).unwrap();
        writer.finish().unwrap();
    }

    /// A truncated `.ods` — `content.xml` opens `<table:table>` then hits EOF
    /// before the matching `</table:table>` — must be REFUSED fast with a typed
    /// Parse error, not spin forever inside calamine's unbounded ODS reader
    /// (resource-exhaustion DoS on untrusted `sources/` input). Pre-fix this test
    /// hangs (calamine's `worksheet_range` never returns); post-fix the structural
    /// pre-scan refuses it in microseconds. A well-formed `.ods` still extracts.
    #[test]
    fn regression_truncated_ods_is_refused_not_hung() {
        let tmp = tempfile::TempDir::new().unwrap();

        // Truncated: the spreadsheet opens `<table:table>` and the document ends
        // there — exactly the EOF-mid-table shape that hangs the ODS reader.
        let trunc = tmp.path().join("trunc.ods");
        let truncated_content = "<?xml version=\"1.0\"?>\
<office:document-content \
xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" \
xmlns:table=\"urn:oasis:names:tc:opendocument:xmlns:table:1.0\">\
<office:body><office:spreadsheet><table:table table:name=\"S\">";
        write_ods_with_content(&trunc, truncated_content);

        let start = std::time::Instant::now();
        let err = extract(&trunc).unwrap_err();
        let elapsed = start.elapsed();
        assert!(
            matches!(&err, ExtractError::Parse { format, .. } if *format == "spreadsheet"),
            "a truncated .ods must be a typed spreadsheet Parse refusal, got {err:?}"
        );
        assert_eq!(err.code(), "EXTRACT_PARSE_ERROR");
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "the truncated .ods must fail fast (<1s); took {elapsed:?} (would-be hang)"
        );

        // A well-formed `.ods` with a single 1-row, 2-cell table still extracts
        // its cell text — the pre-scan must not regress valid spreadsheets.
        let ok = tmp.path().join("ok.ods");
        let valid_content = "<?xml version=\"1.0\"?>\
<office:document-content \
xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" \
xmlns:table=\"urn:oasis:names:tc:opendocument:xmlns:table:1.0\" \
xmlns:text=\"urn:oasis:names:tc:opendocument:xmlns:text:1.0\">\
<office:body><office:spreadsheet>\
<table:table table:name=\"S\">\
<table:table-row>\
<table:table-cell office:value-type=\"string\"><text:p>Alpha</text:p></table:table-cell>\
<table:table-cell office:value-type=\"string\"><text:p>Beta</text:p></table:table-cell>\
</table:table-row>\
</table:table>\
</office:spreadsheet></office:body></office:document-content>";
        write_ods_with_content(&ok, valid_content);
        let got = extract(&ok).unwrap();
        assert!(
            got.text.contains("Alpha") && got.text.contains("Beta"),
            "a valid .ods must still extract its cell text, got {:?}",
            got.text
        );
    }

    #[test]
    fn ods_repeat_attributes_are_bounded_before_calamine_allocates() {
        let tmp = tempfile::TempDir::new().unwrap();
        let hostile = tmp.path().join("repeat-bomb.ods");
        let content = format!(
            "<?xml version=\"1.0\"?>\
<office:document-content \
xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" \
xmlns:table=\"urn:oasis:names:tc:opendocument:xmlns:table:1.0\">\
<office:body><office:spreadsheet><table:table table:name=\"S\">\
<table:table-row table:number-rows-repeated=\"{}\">\
<table:table-cell table:number-columns-repeated=\"2\"/>\
</table:table-row></table:table></office:spreadsheet></office:body>\
</office:document-content>",
            MAX_SPREADSHEET_CELLS
        );
        write_ods_with_content(&hostile, &content);
        let error = extract(&hostile)
            .expect_err("expanded ODS cells must be refused before dense materialization");
        assert!(
            matches!(&error, ExtractError::Parse { format, message }
                if *format == "spreadsheet" && message.contains("expanded cells")),
            "got {error:?}"
        );
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

    /// Build an `.epub` whose spine references the single chapter `spine_count`
    /// times — the spine-amplification shape.
    fn write_epub_with_spine(dest: &Path, spine_count: usize) {
        use std::io::Write;
        let container = "<?xml version=\"1.0\"?>\
<container version=\"1.0\" xmlns=\"urn:oasis:names:tc:opendocument:xmlns:container\">\
<rootfiles><rootfile full-path=\"OEBPS/content.opf\" \
media-type=\"application/oebps-package+xml\"/></rootfiles></container>";
        let itemrefs = "<itemref idref=\"c1\"/>".repeat(spine_count);
        let opf = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<package xmlns=\"http://www.idpf.org/2007/opf\" version=\"3.0\" unique-identifier=\"id\">\
<metadata xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><dc:title>Bomb</dc:title></metadata>\
<manifest><item id=\"c1\" href=\"chapter.xhtml\" media-type=\"application/xhtml+xml\"/></manifest>\
<spine>{itemrefs}</spine></package>"
        );
        let chapter = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><p>Repeated chapter body.</p></body></html>";
        let file = std::fs::File::create(dest).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer.start_file("mimetype", stored).unwrap();
        writer.write_all(b"application/epub+zip").unwrap();
        writer.start_file("META-INF/container.xml", stored).unwrap();
        writer.write_all(container.as_bytes()).unwrap();
        writer.start_file("OEBPS/content.opf", stored).unwrap();
        writer.write_all(opf.as_bytes()).unwrap();
        writer.start_file("OEBPS/chapter.xhtml", stored).unwrap();
        writer.write_all(chapter.as_bytes()).unwrap();
        writer.finish().unwrap();
    }

    #[test]
    fn regression_epub_spine_amplification_is_bounded() {
        // Adversarial review #8: a tiny .epub whose spine references the same
        // chapter a huge number of times pegged a CPU core (re-decoding +
        // re-rendering the chapter each time) and ballooned output. The spine
        // length is now capped, so an over-cap spine is REFUSED — fast, never
        // hung.
        let tmp = tempfile::TempDir::new().unwrap();
        let bomb = tmp.path().join("bomb.epub");
        write_epub_with_spine(&bomb, MAX_EPUB_SPINE_ITEMS + 1);
        let err = extract(&bomb).unwrap_err();
        assert!(
            matches!(&err, ExtractError::Parse { message, .. } if message.contains("spine")),
            "an over-cap spine must be refused with a spine error; got {err:?}"
        );

        // A legitimate small repeat-spine still extracts: memoization renders the
        // shared chapter once, but each reading-order reference is still counted.
        let ok = tmp.path().join("ok.epub");
        write_epub_with_spine(&ok, 5);
        let got = extract(&ok).unwrap();
        assert_eq!(got.metadata["chapters"], MetaValue::Num(5));
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
