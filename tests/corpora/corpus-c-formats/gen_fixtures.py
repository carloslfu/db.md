#!/usr/bin/env python3
"""Generator for corpus-c-formats extraction fixtures.

Builds the binary document fixtures under sources/docs/ deterministically so
they can be regenerated and reviewed. The .html fixture is hand-authored (no
generator). PDFs use reportlab (text) + pikepdf (encryption). The .docx, .xlsx,
and .epub are constructed as zip+xml by hand so there is no opaque toolchain
between the source XML and the fixture on disk.

Run from the corpus-c-formats directory:

    python3 gen_fixtures.py

Each fixture has a known-good ".txt" sibling (hand-maintained, not produced by
this script) used to diff `dbmd extract` output. This script only produces the
binary inputs; it never writes the .txt expectations.

Determinism: all zip members are written with a fixed timestamp and fixed order
so the archives are byte-stable across runs (no embedded build date).
"""

import os
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))
DOCS = os.path.join(HERE, "sources", "docs")

# Fixed zip member timestamp (Y, M, D, H, M, S) for reproducible archives.
ZIP_TS = (2026, 4, 1, 0, 0, 0)


def write_zip(path, members):
    """Write a zip with deterministic ordering and timestamps.

    members: list of (arcname, bytes). Order is preserved as given.
    """
    if os.path.exists(path):
        os.remove(path)
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as zf:
        for arcname, data in members:
            if isinstance(data, str):
                data = data.encode("utf-8")
            info = zipfile.ZipInfo(arcname, date_time=ZIP_TS)
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o644 << 16
            zf.writestr(info, data)


# --------------------------------------------------------------------------
# .docx  (Office Open XML, WordprocessingML) — minimal but valid
# --------------------------------------------------------------------------

def build_docx():
    content_types = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>
"""

    root_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>
"""

    # A document.xml with a heading paragraph, two body paragraphs, and a
    # bulleted-style line. Each <w:p> is a paragraph; <w:t> holds the run text.
    document = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p>
      <w:pPr><w:pStyle w:val="Heading1"/></w:pPr>
      <w:r><w:t>Vendor Security Review</w:t></w:r>
    </w:p>
    <w:p>
      <w:r><w:t>NorthStar Logistics completed the annual security review on 2026 March 18.</w:t></w:r>
    </w:p>
    <w:p>
      <w:r><w:t>The review covered access controls, data retention, and incident response.</w:t></w:r>
    </w:p>
    <w:p>
      <w:r><w:t>No critical findings were reported. Two low-severity items remain open.</w:t></w:r>
    </w:p>
    <w:p>
      <w:r><w:t>Next review is scheduled for March 2027.</w:t></w:r>
    </w:p>
  </w:body>
</w:document>
"""

    members = [
        ("[Content_Types].xml", content_types),
        ("_rels/.rels", root_rels),
        ("word/document.xml", document),
    ]
    write_zip(os.path.join(DOCS, "sample.docx"), members)


# --------------------------------------------------------------------------
# .xlsx  (Office Open XML, SpreadsheetML) — minimal, one sheet, shared strings
# --------------------------------------------------------------------------

def build_xlsx():
    content_types = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
  <Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
  <Override PartName="/xl/sharedStrings.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml"/>
</Types>
"""

    root_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>
"""

    workbook = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets>
    <sheet name="Expenses" sheetId="1" r:id="rId1"/>
  </sheets>
</workbook>
"""

    workbook_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/>
</Relationships>
"""

    # Shared strings: index 0..4 are the text cells. Numbers are inline (t="n").
    shared_strings = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="7" uniqueCount="7">
  <si><t>Vendor</t></si>
  <si><t>Category</t></si>
  <si><t>Amount</t></si>
  <si><t>Acme Cloud</t></si>
  <si><t>Hosting</t></si>
  <si><t>NorthStar</t></si>
  <si><t>Logistics</t></si>
</sst>
"""

    # Sheet: header row (A1:C1) from shared strings (t="s"), then two data rows.
    # Numeric cells carry the value directly with no t attribute.
    sheet = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>
    <row r="1">
      <c r="A1" t="s"><v>0</v></c>
      <c r="B1" t="s"><v>1</v></c>
      <c r="C1" t="s"><v>2</v></c>
    </row>
    <row r="2">
      <c r="A2" t="s"><v>3</v></c>
      <c r="B2" t="s"><v>4</v></c>
      <c r="C2"><v>1200</v></c>
    </row>
    <row r="3">
      <c r="A3" t="s"><v>5</v></c>
      <c r="B3" t="s"><v>6</v></c>
      <c r="C3"><v>3450</v></c>
    </row>
  </sheetData>
</worksheet>
"""

    members = [
        ("[Content_Types].xml", content_types),
        ("_rels/.rels", root_rels),
        ("xl/workbook.xml", workbook),
        ("xl/_rels/workbook.xml.rels", workbook_rels),
        ("xl/sharedStrings.xml", shared_strings),
        ("xl/worksheets/sheet1.xml", sheet),
    ]
    write_zip(os.path.join(DOCS, "sample.xlsx"), members)


# --------------------------------------------------------------------------
# .epub  (EPUB 3, reflowable) — minimal, one XHTML chapter
# --------------------------------------------------------------------------

def build_epub():
    # mimetype MUST be the first entry and stored (uncompressed) per the spec.
    mimetype = "application/epub+zip"

    container = """<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>
"""

    content_opf = """<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="bookid">urn:uuid:db-md-corpus-c-epub</dc:identifier>
    <dc:title>Operations Playbook</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="ch1" href="chapter1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="ch1"/>
  </spine>
</package>
"""

    nav = """<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
  <head><title>Contents</title></head>
  <body>
    <nav epub:type="toc">
      <ol><li><a href="chapter1.xhtml">Onboarding</a></li></ol>
    </nav>
  </body>
</html>
"""

    chapter1 = """<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
  <head><title>Onboarding</title></head>
  <body>
    <h1>Onboarding</h1>
    <p>Every new customer starts with a kickoff call within two business days.</p>
    <p>The operations lead records the call notes and files them under sources.</p>
    <p>A welcome packet is sent before the first invoice is issued.</p>
  </body>
</html>
"""

    if os.path.exists(os.path.join(DOCS, "sample.epub")):
        os.remove(os.path.join(DOCS, "sample.epub"))

    with zipfile.ZipFile(os.path.join(DOCS, "sample.epub"), "w") as zf:
        # mimetype: first, STORED, no extra fields.
        mi = zipfile.ZipInfo("mimetype", date_time=ZIP_TS)
        mi.compress_type = zipfile.ZIP_STORED
        zf.writestr(mi, mimetype)
        for arcname, data in [
            ("META-INF/container.xml", container),
            ("OEBPS/content.opf", content_opf),
            ("OEBPS/nav.xhtml", nav),
            ("OEBPS/chapter1.xhtml", chapter1),
        ]:
            info = zipfile.ZipInfo(arcname, date_time=ZIP_TS)
            info.compress_type = zipfile.ZIP_DEFLATED
            zf.writestr(info, data)


# --------------------------------------------------------------------------
# PDFs — text, multi-column, weird-fonts via reportlab; encrypted via pikepdf
# --------------------------------------------------------------------------

def build_pdfs():
    from reportlab.lib.pagesizes import letter
    from reportlab.lib.units import inch
    from reportlab.pdfgen import canvas

    # ---- text PDF: single column, plain Helvetica ----
    text_pdf = os.path.join(DOCS, "text.pdf")
    c = canvas.Canvas(text_pdf, pagesize=letter)
    c.setTitle("Invoice 2026-0042")
    y = 10.5 * inch
    lines = [
        "Invoice 2026-0042",
        "",
        "Bill to: NorthStar Logistics",
        "Date: 2026-04-15",
        "Due: 2026-05-15",
        "",
        "Description: Managed hosting, April 2026",
        "Amount: USD 3450.00",
        "",
        "Payment is due within 30 days. Thank you for your business.",
    ]
    c.setFont("Helvetica", 12)
    for line in lines:
        c.drawString(1 * inch, y, line)
        y -= 0.3 * inch
    c.showPage()
    c.save()

    # ---- multi-column PDF: two text columns, left then right ----
    mc_pdf = os.path.join(DOCS, "multi-column.pdf")
    c = canvas.Canvas(mc_pdf, pagesize=letter)
    c.setTitle("Two Column Report")
    c.setFont("Helvetica-Bold", 14)
    c.drawString(1 * inch, 10.5 * inch, "Field Report")
    c.setFont("Helvetica", 11)
    left = [
        "Left column paragraph one.",
        "It describes the morning route",
        "and the first three stops on",
        "the north side of the city.",
    ]
    right = [
        "Right column paragraph one.",
        "It describes the afternoon route",
        "and the final two stops near",
        "the river crossing downtown.",
    ]
    y0 = 10.0 * inch
    y = y0
    for line in left:
        c.drawString(1 * inch, y, line)
        y -= 0.25 * inch
    y = y0
    for line in right:
        c.drawString(4.5 * inch, y, line)
        y -= 0.25 * inch
    c.showPage()
    c.save()

    # ---- weird-fonts PDF: Times + Courier + Helvetica-Oblique mixed ----
    wf_pdf = os.path.join(DOCS, "weird-fonts.pdf")
    c = canvas.Canvas(wf_pdf, pagesize=letter)
    c.setTitle("Mixed Fonts")
    y = 10.5 * inch
    c.setFont("Times-Roman", 13)
    c.drawString(1 * inch, y, "Times Roman heading line.")
    y -= 0.4 * inch
    c.setFont("Courier", 11)
    c.drawString(1 * inch, y, "Courier monospaced body line 0123456789.")
    y -= 0.4 * inch
    c.setFont("Helvetica-Oblique", 12)
    c.drawString(1 * inch, y, "Helvetica oblique closing line.")
    c.showPage()
    c.save()

    # ---- image-only PDF: a full-page raster of text with NO text layer ----
    # The page is a PNG of text drawn with Pillow, embedded as an image. There
    # are no PDF text operators, so a text extractor yields nothing — this is
    # the OCR-required case (OCR is explicitly out of scope; see plan).
    from PIL import Image, ImageDraw, ImageFont
    from reportlab.lib.utils import ImageReader

    img = Image.new("RGB", (1700, 2200), "white")
    draw = ImageDraw.Draw(img)
    try:
        font = ImageFont.truetype("/System/Library/Fonts/Supplemental/Arial.ttf", 48)
    except Exception:
        font = ImageFont.load_default()
    img_lines = [
        "Scanned Memo",
        "",
        "This page is a raster image with no text layer.",
        "A text extractor returns nothing; OCR is required.",
        "Quarter end inventory was confirmed on 2026-03-31.",
    ]
    yy = 200
    for line in img_lines:
        draw.text((180, yy), line, fill="black", font=font)
        yy += 90
    img_only_pdf = os.path.join(DOCS, "image-only.pdf")
    c = canvas.Canvas(img_only_pdf, pagesize=letter)
    c.setTitle("Scanned Memo")
    c.drawImage(ImageReader(img), 0, 0, width=letter[0], height=letter[1])
    c.showPage()
    c.save()

    # ---- encrypted PDF: take text.pdf and add a user password via pikepdf ----
    import pikepdf

    enc_pdf = os.path.join(DOCS, "encrypted.pdf")
    if os.path.exists(enc_pdf):
        os.remove(enc_pdf)
    with pikepdf.open(text_pdf) as pdf:
        pdf.save(
            enc_pdf,
            encryption=pikepdf.Encryption(owner="owner-secret", user="open-sesame", R=4),
        )


def main():
    os.makedirs(DOCS, exist_ok=True)
    build_docx()
    build_xlsx()
    build_epub()
    build_pdfs()
    print("Fixtures written to:", DOCS)
    for name in sorted(os.listdir(DOCS)):
        full = os.path.join(DOCS, name)
        print(f"  {name}  ({os.path.getsize(full)} bytes)")


if __name__ == "__main__":
    main()
