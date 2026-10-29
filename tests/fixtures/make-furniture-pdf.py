#!/usr/bin/env python3
"""Writes furniture.pdf next to this script: four pages of made-up text, each
with a running header, a footer and a page number, for the PDF page
furniture test in src/extract.rs (#158). Plain PDF syntax, no dependencies:

    python3 tests/fixtures/make-furniture-pdf.py
"""

from pathlib import Path

TITLE = "Field Notes on Garden Soil"
BODY = [
    [
        "Clay holds water well but drains slowly after rain.",
        "Adding compost each autumn loosens it over a few years.",
    ],
    [
        "Sandy beds dry out within a day of watering.",
        "A mulch of leaves keeps the top layer damp in summer.",
    ],
    [
        "Earthworms were counted in a square of turned soil.",
        "The composted bed had twice as many as the bare one.",
    ],
    [
        "Test the pH before liming, since most vegetables like it near neutral.",
        "These notes will be updated after the next season.",
    ],
]


def text(x: int, y: int, s: str) -> str:
    """One line, as its own text object, the way most generators draw them."""
    s = s.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)")
    return f"BT /F1 10 Tf {x} {y} Td ({s}) Tj ET\n"


def page(n: int, total: int) -> str:
    out = text(72, 750, TITLE) + text(460, 750, "Spring 2026")
    y = 700
    for line in BODY[n - 1]:
        out += text(72, y, line)
        y -= 14
    return out + text(72, 50, "Example Press") + text(460, 50, f"Page {n} of {total}")


def main() -> None:
    total = len(BODY)
    # Objects 1 catalog, 2 pages, 3 font, then a page and its content each.
    objects = {
        1: "<< /Type /Catalog /Pages 2 0 R >>",
        3: "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>",
    }
    kids = []
    for n in range(1, total + 1):
        page_id, content_id = 2 + 2 * n, 3 + 2 * n
        kids.append(f"{page_id} 0 R")
        objects[page_id] = (
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
            f"/Resources << /Font << /F1 3 0 R >> >> /Contents {content_id} 0 R >>"
        )
        stream = page(n, total)
        objects[content_id] = f"<< /Length {len(stream)} >>\nstream\n{stream}endstream"
    objects[2] = f"<< /Type /Pages /Kids [{' '.join(kids)}] /Count {total} >>"

    out = b"%PDF-1.4\n"
    offsets = {}
    for i in sorted(objects):
        offsets[i] = len(out)
        out += f"{i} 0 obj\n{objects[i]}\nendobj\n".encode("latin-1")
    xref = len(out)
    size = max(objects) + 1
    out += f"xref\n0 {size}\n0000000000 65535 f \n".encode()
    for i in range(1, size):
        out += f"{offsets[i]:010d} 00000 n \n".encode()
    out += f"trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode()
    Path(__file__).with_name("furniture.pdf").write_bytes(out)


if __name__ == "__main__":
    main()
