//! Regression tests for `dbmd extract`, locking down the launch-readiness
//! fixes that the per-format `extract.rs`/`extract_e2e.rs` suites do not cover:
//!
//! - **#4** — a malicious spreadsheet (two cells at opposite grid corners)
//!   used to OOM/abort the process via calamine's dense-matrix allocation.
//!   `dbmd extract` must now refuse it cleanly with the stable
//!   `EXTRACT_PARSE_ERROR` code and a non-zero exit, never crash.
//! - **#26** — `dbmd extract big.pdf | head` (a closed downstream pipe) used to
//!   exit 1 with an `IO_ERROR` envelope; a broken pipe is a benign truncation
//!   and must exit 0 with nothing on stderr.
//!
//! Both drive the real `dbmd` binary so they assert the agent-visible CLI
//! contract (exit code, machine code, stderr), not library internals.

mod common;

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

use common::dbmd;

// ── #4: malicious spreadsheet refused, not OOM ────────────────────────────────

/// CRC-32 (IEEE, the zip polynomial), table-free. `dbmd-cli` has no `zip` /
/// `crc32` dev-dependency, so the stored-zip writer below computes the per-entry
/// CRC itself — a few lines, fully deterministic, no new crates.
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

/// Write a minimal STORED-only zip (no compression) from `(name, bytes)` entries
/// using only the standard library. STORED is the simplest zip encoding —
/// compressed size equals uncompressed size — so a correct archive needs only
/// the local headers, the raw bytes, the central directory, and the EOCD record.
fn write_stored_zip(dest: &Path, entries: &[(&str, &[u8])]) {
    fn u16le(v: u16) -> [u8; 2] {
        v.to_le_bytes()
    }
    fn u32le(v: u32) -> [u8; 4] {
        v.to_le_bytes()
    }

    let mut out = Vec::new();
    let mut central = Vec::new();

    for (name, data) in entries {
        let name = name.as_bytes();
        let crc = crc32(data);
        let off = out.len() as u32;

        // Local file header.
        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&u16le(20)); // version needed
        out.extend_from_slice(&u16le(0)); // flags
        out.extend_from_slice(&u16le(0)); // method 0 = stored
        out.extend_from_slice(&u16le(0)); // mod time
        out.extend_from_slice(&u16le(0)); // mod date
        out.extend_from_slice(&u32le(crc));
        out.extend_from_slice(&u32le(data.len() as u32)); // compressed size
        out.extend_from_slice(&u32le(data.len() as u32)); // uncompressed size
        out.extend_from_slice(&u16le(name.len() as u16));
        out.extend_from_slice(&u16le(0)); // extra len
        out.extend_from_slice(name);
        out.extend_from_slice(data);

        // Central directory record (built now, appended after all entries).
        central.extend_from_slice(b"PK\x01\x02");
        central.extend_from_slice(&u16le(20)); // version made by
        central.extend_from_slice(&u16le(20)); // version needed
        central.extend_from_slice(&u16le(0)); // flags
        central.extend_from_slice(&u16le(0)); // method
        central.extend_from_slice(&u16le(0)); // mod time
        central.extend_from_slice(&u16le(0)); // mod date
        central.extend_from_slice(&u32le(crc));
        central.extend_from_slice(&u32le(data.len() as u32));
        central.extend_from_slice(&u32le(data.len() as u32));
        central.extend_from_slice(&u16le(name.len() as u16));
        central.extend_from_slice(&u16le(0)); // extra len
        central.extend_from_slice(&u16le(0)); // comment len
        central.extend_from_slice(&u16le(0)); // disk number
        central.extend_from_slice(&u16le(0)); // internal attrs
        central.extend_from_slice(&u32le(0)); // external attrs
        central.extend_from_slice(&u32le(off)); // local header offset
        central.extend_from_slice(name);
    }

    let cd_offset = out.len() as u32;
    out.extend_from_slice(&central);

    // End of central directory.
    out.extend_from_slice(b"PK\x05\x06");
    out.extend_from_slice(&u16le(0)); // disk number
    out.extend_from_slice(&u16le(0)); // cd start disk
    out.extend_from_slice(&u16le(entries.len() as u16)); // entries on disk
    out.extend_from_slice(&u16le(entries.len() as u16)); // total entries
    out.extend_from_slice(&u32le(central.len() as u32)); // cd size
    out.extend_from_slice(&u32le(cd_offset)); // cd offset
    out.extend_from_slice(&u16le(0)); // comment len

    std::fs::write(dest, out).unwrap();
}

/// Build a VALID `.xlsx` whose one sheet places two real cells at the opposite
/// corners of Excel's grid (`A1` and `XFD1048576`). calamine sizes a sheet's
/// dense `Vec<Data>` from the MIN/MAX cell positions, so this two-cell sheet
/// would otherwise force a ~1.7e10-element (~400 GB) allocation and abort the
/// process. The surrounding workbook parts are the minimal set calamine needs to
/// open the file.
fn write_dense_bomb_xlsx(dest: &Path) {
    let content_types = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
</Types>"#;

    let root_rels = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#;

    let workbook = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets>
</workbook>"#;

    let workbook_rels = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"#;

    let bomb_sheet = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<sheetData>
<row r="1"><c r="A1"><v>1</v></c></row>
<row r="1048576"><c r="XFD1048576"><v>2</v></c></row>
</sheetData></worksheet>"#;

    write_stored_zip(
        dest,
        &[
            ("[Content_Types].xml", content_types),
            ("_rels/.rels", root_rels),
            ("xl/workbook.xml", workbook),
            ("xl/_rels/workbook.xml.rels", workbook_rels),
            ("xl/worksheets/sheet1.xml", bomb_sheet),
        ],
    );
}

#[test]
fn dense_grid_bomb_xlsx_refuses_cleanly_not_oom() {
    let tmp = tempfile::TempDir::new().unwrap();
    let bomb = tmp.path().join("invoice.xlsx");
    write_dense_bomb_xlsx(&bomb);

    // A few-KB file on disk — the danger is the in-memory dense expansion, which
    // the pre-fix code attempted (aborting the process). Post-fix, `extract`
    // bounds the grid first and refuses without allocating it.
    assert!(
        std::fs::metadata(&bomb).unwrap().len() < 10_000,
        "the bomb must be tiny on disk"
    );

    // Plain mode: non-zero exit, no partial bytes on stdout.
    let out = dbmd().arg("extract").arg(&bomb).assert().failure().code(1);
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.is_empty(),
        "an over-cap spreadsheet must emit nothing to stdout, got: {stdout:?}"
    );

    // JSON mode: the typed refusal carries the stable parse-error code.
    let out = dbmd()
        .arg("--json")
        .arg("extract")
        .arg(&bomb)
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8(out.get_output().stderr.clone()).unwrap();
    let parsed: serde_json::Value =
        serde_json::from_str(stderr.trim()).expect("JSON error object on stderr");
    assert_eq!(parsed["error"]["code"], "EXTRACT_PARSE_ERROR");
}

// ── #26: broken pipe is a clean exit 0, not IO_ERROR ──────────────────────────

/// A large `.html` whose flattened text far exceeds the OS pipe buffer (~64 KB),
/// so the extractor is still writing when a downstream reader closes the pipe —
/// the only way to deterministically provoke a `BrokenPipe` write error.
fn write_large_html(dest: &Path) {
    let mut body = String::with_capacity(2 * 1024 * 1024);
    body.push_str("<html><body>");
    for i in 0..40_000 {
        body.push_str(&format!(
            "<p>line number {i} with some filler words here</p>"
        ));
    }
    body.push_str("</body></html>");
    std::fs::write(dest, body).unwrap();
}

#[test]
fn broken_pipe_downstream_exits_zero_not_io_error() {
    let tmp = tempfile::TempDir::new().unwrap();
    let big = tmp.path().join("big.html");
    write_large_html(&big);

    // Spawn the real binary with stdout piped, read a little, then drop the read
    // end (closing the pipe) — like `dbmd extract big.html | head -c 64`.
    let bin = assert_cmd::cargo::cargo_bin("dbmd");
    let mut child = Command::new(bin)
        .arg("extract")
        .arg(&big)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn dbmd extract");

    {
        let mut stdout = child.stdout.take().expect("piped stdout");
        // Read a small prefix, far less than the output, then drop `stdout` at the
        // end of this scope — closing the read end while the child is still
        // writing, so its next write fails with BrokenPipe.
        let mut buf = [0u8; 64];
        let _ = stdout.read(&mut buf);
    }

    let output = child.wait_with_output().expect("wait for dbmd");

    // The fix: a broken pipe is benign — clean exit 0, no error envelope. Pre-fix
    // this exited 1 with an `IO_ERROR` object on stderr.
    assert!(
        output.status.success(),
        "broken pipe must exit 0, got status {:?} with stderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "broken pipe must not emit an error envelope, got stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
