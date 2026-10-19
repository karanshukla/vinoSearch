#!/usr/bin/env python3
"""Draws the README's animated demos as SVG: a simulated Plasma desktop.

    python3 docs/media/demos.py   # writes docs/media/krunner.svg and settings.svg

Nothing here is a screen capture. The layouts follow KRunner (Breeze Dark)
and the settings window's Index page; file names, snippets and counts are
made up, so no real index or photo ends up in the repo.
"""

from html import escape
from pathlib import Path

W, H = 960, 540
FONT = "'Noto Sans','Inter','Segoe UI',system-ui,sans-serif"


class Timeline:
    """One looping clock. Each element gets keyframes that show it from t0 to t1."""

    def __init__(self, length: float):
        self.length = length
        self.frames: list[str] = []

    def pct(self, t: float) -> str:
        return f"{max(0.0, min(100.0, t / self.length * 100)):.3f}%"

    def show(self, t0: float, t1: float, fade: float = 0.0, rise: float = 0.0) -> str:
        """A style attribute that shows the element from t0 to t1, fading over `fade` s."""
        name = f"k{len(self.frames)}"
        off = f"opacity:0;transform:translateY({rise}px)" if rise else "opacity:0"
        on = "opacity:1;transform:translateY(0)" if rise else "opacity:1"
        f = max(fade, 0.001)
        steps = [(0, off), (t0, off), (t0 + f, on), (t1, on), (t1 + f, off), (self.length, off)]
        body = "".join(f"{self.pct(t)}{{{s}}}" for t, s in steps)
        self.frames.append(f"@keyframes {name}{{{body}}}")
        return f'style="animation-name:{name}"'

    def css(self, extra: str = "") -> str:
        return (
            f"<style>.a{{animation-duration:{self.length}s;animation-iteration-count:infinite;"
            "animation-timing-function:linear;opacity:0}"
            ".caret{animation:blink 1s steps(1) infinite}@keyframes blink{50%{opacity:0}}"
            ".spin{transform-box:fill-box;transform-origin:center;animation:spin .9s linear infinite}"
            "@keyframes spin{to{transform:rotate(360deg)}}"
            f"text{{font-family:{FONT}}}{extra}{''.join(self.frames)}</style>"
        )


def text(x, y, s, size=14, fill="#fcfcfc", weight=400, anchor="start", extra=""):
    return (
        f'<text x="{x}" y="{y}" font-size="{size}" fill="{fill}" font-weight="{weight}" '
        f'text-anchor="{anchor}" {extra}>{escape(s)}</text>'
    )


def desktop() -> str:
    """Wallpaper and a bottom panel."""
    return (
        '<defs><linearGradient id="wall" x1="0" y1="0" x2="1" y2="1">'
        '<stop offset="0" stop-color="#16233a"/><stop offset=".55" stop-color="#26304f"/>'
        '<stop offset="1" stop-color="#3a2a4f"/></linearGradient>'
        '<radialGradient id="glow" cx=".72" cy=".7" r=".5"><stop offset="0" stop-color="#3daee9" stop-opacity=".22"/>'
        '<stop offset="1" stop-color="#3daee9" stop-opacity="0"/></radialGradient></defs>'
        f'<rect width="{W}" height="{H}" fill="url(#wall)"/><rect width="{W}" height="{H}" fill="url(#glow)"/>'
        f'<rect y="{H - 36}" width="{W}" height="36" fill="#1b1e20" fill-opacity=".92"/>'
        + "".join(
            f'<rect x="{14 + i * 40}" y="{H - 28}" width="20" height="20" rx="5" fill="{c}"/>'
            for i, c in enumerate(["#3daee9", "#e8a23a", "#56cc78", "#a0a4ac"])
        )
        + text(W - 16, H - 13, "17:13", 13, "#dfe1e3", anchor="end")
    )


def icon_text(x, y, label="MD", color="#3daee9") -> str:
    """A document icon with a type label, 32 px."""
    return (
        f'<g transform="translate({x},{y})"><path d="M4 0h17l9 9v23H4z" fill="#eff0f1"/>'
        '<path d="M21 0v9h9z" fill="#bdc3c7"/>'
        f'<rect x="4" y="18" width="26" height="10" fill="{color}"/>'
        f'<text x="17" y="26.3" font-size="8" font-weight="700" fill="#fff" text-anchor="middle">{label}</text>'
        '<rect x="8" y="7" width="10" height="2" fill="#9aa0a6"/><rect x="8" y="12" width="16" height="2" fill="#9aa0a6"/></g>'
    )


def icon_image(x, y) -> str:
    """An image file icon, 32 px."""
    return (
        f'<g transform="translate({x},{y})"><rect x="1" y="3" width="30" height="26" rx="3" fill="#2c9e6a"/>'
        '<circle cx="10" cy="11" r="3.5" fill="#f6d25c"/>'
        '<path d="M3 27l9-10 6 6 4-4 8 8z" fill="#e8f5ee"/></g>'
    )


def row(y, icon, title, sub, selected_from=None, tl=None, t1=None) -> str:
    """One KRunner match, 56 px tall, at y."""
    out = ""
    if selected_from is not None:
        out += (
            f'<rect class="a" {tl.show(selected_from, t1, .15)} x="168" y="{y + 2}" width="624" height="52" '
            'rx="4" fill="#3daee9" fill-opacity=".22" stroke="#3daee9" stroke-opacity=".7"/>'
        )
    return (
        out + icon(178, y + 12)
        + text(222, y + 25, title, 15, "#fcfcfc")
        + text(222, y + 44, sub, 12.5, "#a1a9b1")
    )


def dog_beach(x, y, w, h) -> str:
    """The photo the image query finds: a dog on a beach at sunset."""
    return (
        f'<g transform="translate({x},{y})"><defs><linearGradient id="sky" x1="0" y1="0" x2="0" y2="1">'
        '<stop offset="0" stop-color="#2b3a6b"/><stop offset=".55" stop-color="#e2725b"/>'
        '<stop offset="1" stop-color="#f5b867"/></linearGradient>'
        f'<clipPath id="photo"><rect width="{w}" height="{h}"/></clipPath></defs>'
        '<g clip-path="url(#photo)">'
        f'<rect width="{w}" height="{h}" fill="url(#sky)"/>'
        f'<circle cx="{w * .62}" cy="{h * .56}" r="{h * .13}" fill="#ffd98a"/>'
        f'<rect y="{h * .58}" width="{w}" height="{h * .17}" fill="#3d5a80"/>'
        + "".join(
            f'<rect x="{w * .62 - h * (.16 - i * .03)}" y="{h * (.6 + i * .035)}" width="{h * (.32 - i * .06)}" '
            f'height="2.5" fill="#ffd98a" fill-opacity="{.7 - i * .15}"/>'
            for i in range(4)
        )
        + f'<path d="M0 {h * .75} Q{w * .5} {h * .7} {w} {h * .76} V{h} H0z" fill="#d9a066"/>'
        f'<path d="M0 {h * .82} Q{w * .4} {h * .78} {w} {h * .84} V{h} H0z" fill="#c78a52"/>'
        # The dog, in silhouette, mid-run.
        f'<g transform="translate({w * .3},{h * .66}) scale({h / 300})" fill="#2a1d1a">'
        '<ellipse cx="0" cy="0" rx="46" ry="17"/><circle cx="50" cy="-20" r="15"/>'
        '<path d="M58 -16 l22 4 -2 9 -22 -1z"/><path d="M42 -32 l6 -14 8 12z"/>'
        '<path d="M-44 -6 q-24 -22 -30 -30 q10 4 32 22z"/>'
        '<path d="M28 6 l16 30 -7 3 -17 -28z"/><path d="M14 8 l-4 34 7 1 6 -32z"/>'
        '<path d="M-30 6 l-24 26 5 5 26 -24z"/><path d="M-20 8 l2 34 7 -1 -1 -33z"/></g>'
        f'<ellipse cx="{w * .3}" cy="{h * .82}" rx="{h * .2}" ry="4" fill="#000" fill-opacity=".18"/>'
        "</g></g>"
    )


def krunner() -> str:
    tl = Timeline(17)
    q1 = "how do I rotate the signing key"
    q2 = "dog on a beach at sunset"
    open_at, viewer_at, end = 1.0, 12.4, 16.4
    t1_start = 1.4
    t1_end = t1_start + len(q1) * .055
    clear_at = 7.0
    t2_start = clear_at + .4
    t2_end = t2_start + len(q2) * .065
    res1, res2 = t1_end + .35, t2_end + .4
    out = [desktop()]

    # Alt+Space hint.
    out.append(
        f'<g class="a" {tl.show(.2, open_at + .3, .2)}><rect x="24" y="{H - 82}" width="104" height="32" rx="6" '
        f'fill="#1b1e20" stroke="#4d5359"/>{text(76, H - 61, "Alt+Space", 14, "#dfe1e3", 600, "middle")}</g>'
    )

    # Panel backgrounds, one height per phase.
    def panel(t0, t1, height):
        return (
            f'<g class="a" {tl.show(t0, t1, .15)}><rect x="160" y="-8" width="640" height="{height + 8}" rx="8" '
            f'fill="#2a2e32" stroke="#4d5359"/></g>'
        )

    text_rows, image_rows = 4, 4
    out.append(panel(open_at, res1, 60))
    out.append(panel(res1, clear_at, 60 + 30 + text_rows * 56 + 8))
    out.append(panel(clear_at, res2, 60))
    out.append(panel(res2, viewer_at, 60 + 30 + image_rows * 56 + 8))

    # The search field.
    field = (
        '<rect x="172" y="10" width="616" height="40" rx="5" fill="#1b1e20" stroke="#3daee9"/>'
        '<circle cx="193" cy="29" r="6.5" fill="none" stroke="#a1a9b1" stroke-width="2"/>'
        '<path d="M198 34l5 5" stroke="#a1a9b1" stroke-width="2.2" stroke-linecap="round"/>'
    )
    out.append(f'<g class="a" {tl.show(open_at, viewer_at, .15)}>{field}</g>')

    def typed(query, start, step, until):
        for i in range(len(query) + 1):
            t0 = start + i * step
            t_next = start + (i + 1) * step if i < len(query) else until
            caret = '<tspan class="caret" fill="#3daee9">|</tspan>'
            out.append(
                f'<text class="a" {tl.show(t0, t_next)} x="214" y="35" font-size="17" fill="#fcfcfc">'
                f"{escape(query[:i])}{caret}</text>"
            )

    out.append(
        f'<text class="a" {tl.show(open_at, t1_start)} x="214" y="35" font-size="17" fill="#fcfcfc">'
        '<tspan class="caret" fill="#3daee9">|</tspan></text>'
    )
    typed(q1, t1_start, .055, clear_at)
    typed(q2, t2_start, .065, viewer_at)

    # Text results: found by meaning, most never say "rotate".
    texts = [
        ("signing-keys.md", "~/Documents/runbooks:12  Every 90 days, generate a new release key and revoke the old…", "MD", "#3daee9"),
        ("release-checklist.md", "~/Documents/notes:40  Re-sign with the new key before tagging; the old one stays valid…", "MD", "#3daee9"),
        ("gpg.md", "~/Development/dotfiles/docs:7  Subkeys expire yearly. Extend them with gpg --quick-set-expire…", "MD", "#3daee9"),
        ("deploy.sh", "~/Development/site/scripts:88  # swap keys: keep the previous key trusted for 24h so in-flight…", "SH", "#56cc78"),
    ]
    head = 60
    out.append(f'<g class="a" {tl.show(res1, clear_at, .2)}>{text(178, head + 21, "vinoSearch Text", 12, "#a1a9b1", 700)}</g>')
    for i, (name, sub, label, color) in enumerate(texts):
        y = head + 30 + i * 56
        r = row(y, lambda x, yy, l=label, c=color: icon_text(x, yy, l, c), name, sub,
                res1 + .1 if i == 0 else None, tl, clear_at)
        out.append(f'<g class="a" {tl.show(res1 + i * .08, clear_at, .25, 6)}>{r}</g>')

    # Image results: the names say nothing, the pixels matched.
    images = [
        ("IMG_2041.jpg", "~/Pictures/2025/Tofino"),
        ("IMG_2038.jpg", "~/Pictures/2025/Tofino"),
        ("PXL_20240812_201544.jpg", "~/Pictures/Phone/Camera"),
        ("DSC00417.jpg", "~/Pictures/2023/Outer Banks"),
    ]
    out.append(f'<g class="a" {tl.show(res2, viewer_at, .2)}>{text(178, head + 21, "vinoSearch Images", 12, "#a1a9b1", 700)}</g>')
    for i, (name, sub) in enumerate(images):
        y = head + 30 + i * 56
        r = row(y, icon_image, name, sub, res2 + .1 if i == 0 else None, tl, viewer_at)
        out.append(f'<g class="a" {tl.show(res2 + i * .08, viewer_at, .25, 6)}>{r}</g>')

    # Enter: the hint, then the photo opens.
    out.append(
        f'<g class="a" {tl.show(viewer_at - 1.0, viewer_at, .2)}><rect x="24" y="{H - 82}" width="72" height="32" rx="6" '
        f'fill="#1b1e20" stroke="#4d5359"/>{text(60, H - 61, "Enter", 14, "#dfe1e3", 600, "middle")}</g>'
    )
    vx, vy, vw, vh = 170, 44, 620, 420
    viewer = (
        f'<rect x="{vx}" y="{vy}" width="{vw}" height="{vh}" rx="8" fill="#202326" stroke="#4d5359"/>'
        f'<rect x="{vx}" y="{vy}" width="{vw}" height="34" rx="8" fill="#2a2e32"/>'
        f'<rect x="{vx}" y="{vy + 26}" width="{vw}" height="8" fill="#2a2e32"/>'
        + text(vx + vw / 2, vy + 22, "IMG_2041.jpg  ·  Image Viewer", 13, "#dfe1e3", 600, "middle")
        + f'<circle cx="{vx + vw - 18}" cy="{vy + 17}" r="7" fill="#4d5359"/>'
        + dog_beach(vx + 10, vy + 44, vw - 20, vh - 54)
    )
    out.append(f'<g class="a" {tl.show(viewer_at, end, .3)}>{viewer}</g>')

    return svg(tl, out, "vinoSearch in KRunner: a text query finds a runbook by meaning, then an image query finds a photo by what is in it")


def settings() -> str:
    tl = Timeline(14)
    out = [desktop()]
    x, y, w, h = 110, 40, 740, 440
    green, text_c, muted, rule, bg = "#7bdc97", "#e4e5e8", "#9a9ea6", "#2a2c30", "#121315"
    logo = Path(__file__).resolve().parents[2] / "data" / "dev.vinosearch.svg"
    icon = logo.read_text().split(">", 1)[1].rsplit("</svg>", 1)[0]

    out.append(
        f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="10" fill="{bg}" stroke="#3a3d42"/>'
        f'<path d="M{x} {y + 10} a10 10 0 0 1 10 -10 h{w - 20} a10 10 0 0 1 10 10 v46 h-{w} z" fill="#1b1c1f"/>'
        f'<svg x="{x + 16}" y="{y + 12}" width="32" height="32" viewBox="2 2 60 60">{icon}</svg>'
        + text(x + 58, y + 34, "vinoSearch", 17, text_c, 700)
        + "".join(
            f'<circle cx="{x + w - 22 - i * 24}" cy="{y + 28}" r="7" fill="#3a3d42"/>' for i in range(3)
        )
        + f'<rect x="{x}" y="{y + 56}" width="{w}" height="1" fill="{rule}"/>'
    )
    # Sidebar.
    pages = ["Index", "Folders", "Search", "Model", "Hardware", "Integration"]
    for i, page in enumerate(pages):
        py = y + 78 + i * 36
        if i == 0:
            out.append(f'<rect x="{x + 10}" y="{py - 18}" width="150" height="30" rx="6" fill="{green}" fill-opacity=".12"/>')
            out.append(f'<rect x="{x + 10}" y="{py - 14}" width="3" height="22" rx="1.5" fill="{green}"/>')
        out.append(text(x + 26, py + 2, page, 14, green if i == 0 else muted, 600 if i == 0 else 400))
    out.append(f'<rect x="{x + 170}" y="{y + 57}" width="1" height="{h - 57}" fill="{rule}"/>')

    cx = x + 196
    lx = cx + 110  # Value column.
    out.append(text(cx, y + 92, "Index", 20, text_c, 700))
    labels = ["Now:", "Contents:", "Last change:", "Checked:"]
    for i, label in enumerate(labels):
        out.append(text(lx - 12, y + 132 + i * 34, label, 14, muted, anchor="end"))
    out.append(text(cx, y + 288, "Largest folders", 14, text_c, 700))

    folders = ["Development/linux-drivers", "Development/kwin", "Documents", "Pictures"]
    base_files, base_chunks, base_mb = 9763, 113341, 470.0
    images = 649
    steps = 12
    start, step = 1.0, .7
    done_at = start + steps * step

    def button(bx, by, label, wid):
        return (
            f'<rect x="{bx}" y="{by - 17}" width="{wid}" height="24" rx="5" fill="#26282c" stroke="#3a3d42"/>'
            + text(bx + wid / 2, by, label, 13, text_c, anchor="middle")
        )

    def frame(n: int, t0: float, t1: float):
        indexing = n < steps
        done = images if not indexing else round(images * n / steps)
        files, chunks = base_files + done, base_chunks + done
        mb = base_mb + done * 0.013
        g = []
        if indexing:
            g.append(
                f'<circle class="spin" cx="{lx + 8}" cy="{y + 127}" r="7" fill="none" stroke="{green}" '
                'stroke-width="2.4" stroke-dasharray="30 14" stroke-linecap="round"/>'
            )
            g.append(text(lx + 24, y + 132, f"Indexing, {done:,} files so far", 14, text_c))
            g.append(button(lx + 238, y + 132, "Stop", 52))
        else:
            g.append(text(lx, y + 132, "Up to date", 14, text_c))
        g.append(text(lx, y + 166, f"{files:,} files, {chunks:,} chunks, {mb:.0f} MB on disk", 14, text_c))
        g.append(text(lx, y + 200, "Indexed just now" if done else "Indexed 3 minutes ago", 14, text_c))
        g.append(text(lx, y + 234, "2 minutes ago", 14, text_c))
        g.append(button(lx + 112, y + 234, "Check now", 88))
        shares = [38, 27, 12, 2 + 7 * done / images]
        total = sum(shares)
        for i, (folder, share) in enumerate(zip(folders, shares)):
            fy = y + 318 + i * 28
            pct = share / total * 100
            g.append(text(cx, fy, folder, 13, text_c))
            g.append(f'<rect x="{cx + 200}" y="{fy - 10}" width="250" height="8" rx="4" fill="{rule}"/>')
            g.append(f'<rect x="{cx + 200}" y="{fy - 10}" width="{2.5 * pct:.1f}" height="8" rx="4" fill="{green}"/>')
            g.append(text(cx + 462, fy, f"{pct:.0f}%", 13, muted))
            g.append(f'<rect x="{cx + 494}" y="{fy - 15}" width="18" height="18" rx="4" fill="none" stroke="#3a3d42"/>')
            g.append(f'<path d="M{cx + 499} {fy - 6}h8" stroke="{muted}" stroke-width="1.5"/>')
        out.append(f'<g class="a" {tl.show(t0, t1)}>{"".join(g)}</g>')

    frame(0, 0, start)
    for n in range(1, steps + 1):
        frame(n, start + (n - 1) * step, start + n * step if n < steps else tl.length)
    out.append(
        f'<g class="a" {tl.show(.3, start + 1.5, .3)}><rect x="{cx + 300}" y="{y + 70}" width="208" height="30" rx="6" '
        f'fill="{green}" fill-opacity=".14" stroke="{green}" stroke-opacity=".5"/>'
        + text(cx + 404, y + 90, "Images turned on", 13, green, 600, "middle") + "</g>"
    )
    return svg(tl, out, "The vinoSearch settings window indexing photos: the file count rises and the Pictures folder grows")


def svg(tl: Timeline, body: list[str], title: str) -> str:
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" '
        f'role="img" aria-label="{escape(title)}"><title>{escape(title)}</title>'
        f'{tl.css()}{"".join(body)}</svg>\n'
    )


if __name__ == "__main__":
    here = Path(__file__).parent
    (here / "krunner.svg").write_text(krunner())
    (here / "settings.svg").write_text(settings())
