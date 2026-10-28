"""Near-duplicate images (#78): how alike EmbeddingGemma 2's vision vectors
score for copies, edits and bursts of one photo, against different photos,
and for screenshots of one UI. Orchard Photos' thresholds are SigLIP 2's.

    ENV="uv run --python 3.13 --no-project --with pillow --with numpy python -I"
    $ENV bench/near_duplicates.py make <flickr images dir> <bench dir>
    XDG_DATA_HOME=<bench dir>/data XDG_CONFIG_HOME=<bench dir>/config \
        vinosearch index -y <bench dir>/img
    $ENV bench/near_duplicates.py score <bench dir>

`make` takes the first 200 Flickr30k test images (sorted by name) as
originals and makes six variants of the first 100: resized to half
(`-800x400`), saved again at JPEG quality 35 (` (1)`), cropped to the middle
85% and to the middle half, brightened, and shifted 6% (a burst's next
frame, roughly). It also draws screenshots of 12 made-up app windows, each in
three states with different text and one with only the clock changed
(1280x800 PNG, PIL's default font). Link
`<bench dir>/data/vinosearch/models` to the real models first. `score` reads
the vectors from the bench index (no runner has it open) and prints
percentiles. Keep <bench dir> on disk, not tmpfs.
"""
import os, random, sqlite3, sys
import numpy as np
from PIL import Image, ImageDraw, ImageEnhance

ORIGINALS, VARIED = 200, 100
VARIANTS = {
    "-800x400": lambda im: im.resize((im.width // 2, im.height // 2), Image.LANCZOS),
    " (1)": lambda im: im,  # saved again at low quality below
    "-crop85": lambda im: centre(im, 0.85),
    "-crop50": lambda im: centre(im, 0.5),
    "-edit": lambda im: ImageEnhance.Contrast(ImageEnhance.Brightness(im).enhance(1.15)).enhance(1.1),
    "-burst": lambda im: im.crop((int(im.width * 0.06), int(im.height * 0.03), im.width, im.height)).resize(im.size),
}

def centre(im, f):
    w, h = int(im.width * f), int(im.height * f)
    x, y = (im.width - w) // 2, (im.height - h) // 2
    return im.crop((x, y, x + w, y + h))

WORDS = ("invoice budget meeting deploy report payroll draft review backup kernel "
         "update calendar notes travel receipt photo export settings network").split()

def screenshot(ui, state, clock):
    """A made-up app window: `ui` fixes its layout and colours, `state` its text."""
    r = random.Random(ui)
    bg = tuple(r.randrange(200, 256) for _ in range(3))
    bar = tuple(r.randrange(20, 120) for _ in range(3))
    side = r.randrange(160, 320)
    im = Image.new("RGB", (1280, 800), bg)
    d = ImageDraw.Draw(im)
    d.rectangle((0, 0, 1280, 40), fill=bar)
    d.text((12, 12), f"App {ui}", fill="white")
    d.text((1200, 12), f"{9 + clock}:4{clock}", fill="white")
    d.rectangle((0, 40, side, 800), fill=tuple(max(c - 30, 0) for c in bg))
    for i in range(8):
        d.text((16, 60 + 30 * i), r.choice(WORDS).title(), fill=bar)
    s = random.Random(f"{ui}/{state}")
    for i in range(22):
        line = " ".join(s.choice(WORDS) for _ in range(s.randrange(3, 12)))
        d.text((side + 24, 64 + 30 * i), line, fill=(30, 30, 30))
    for i in range(3):
        d.rounded_rectangle((side + 24 + 140 * i, 740, side + 144 + 140 * i, 776), 6, fill=bar)
    return im

def make(src, out):
    img = os.path.join(out, "img")
    for d in ("orig", "var", "shots"):
        os.makedirs(os.path.join(img, d), exist_ok=True)
    names = sorted(os.listdir(src))[:ORIGINALS]
    for i, n in enumerate(names):
        im = Image.open(os.path.join(src, n)).convert("RGB")
        im.save(os.path.join(img, "orig", n), quality=92)
        if i >= VARIED:
            continue
        stem = os.path.splitext(n)[0]
        for suffix, f in VARIANTS.items():
            f(im).save(os.path.join(img, "var", f"{stem}{suffix}.jpg"), quality=35 if suffix == " (1)" else 92)
    for ui in range(12):
        for state in range(3):
            screenshot(ui, state, 0).save(os.path.join(img, "shots", f"ui{ui:02}-s{state}.png"))
        screenshot(ui, 0, 1).save(os.path.join(img, "shots", f"ui{ui:02}-s0-clock.png"))
    for d in ("data/vinosearch", "config/vinosearch"):
        os.makedirs(os.path.join(out, d), exist_ok=True)
    with open(os.path.join(out, "config/vinosearch/config.toml"), "w") as f:
        f.write("index_busy_percent = 100\n\n[formats.images]\nenabled = true\n")

def vectors(out):
    db = sqlite3.connect(f"file:{os.path.join(out, 'data/vinosearch/index.sqlite')}?mode=ro", uri=True)
    rows = db.execute("SELECT f.path, c.embedding FROM chunks c JOIN files f ON f.id = c.file_id WHERE c.kind = 1")
    return {os.path.relpath(p, os.path.join(out, "img")): np.frombuffer(e, np.float32) for p, e in rows}

def grey(path, n):
    return np.asarray(Image.open(path).convert("L").resize((n, n), Image.BILINEAR), np.int16)

def differing(a, b, n, step):
    """Share of n x n grey pixels more than step/255 apart."""
    return float((np.abs(grey(a, n) - grey(b, n)) > step).mean())

# Orchard Photos' pixel rule, and `dupes::THUMB` / `PIXEL_STEP`.
PIXEL_RULES = ((64, 32), (256, 16))

def pct(xs):
    xs = np.sort(np.array(xs))
    q = lambda p: xs[min(int(p * len(xs)), len(xs) - 1)]
    return f"n={len(xs):3}  min {xs[0]:.4f}  p5 {q(0.05):.4f}  p50 {q(0.5):.4f}  p95 {q(0.95):.4f}  max {xs[-1]:.4f}"

def score(out):
    v = vectors(out)
    cos = lambda a, b: float(v[a] @ v[b])
    origs = sorted(k for k in v if k.startswith("orig/"))
    print(f"{len(v)} image vectors")
    print("\nVariant to its original:")
    for suffix in VARIANTS:
        xs = []
        for o in origs[:VARIED]:
            stem = os.path.splitext(os.path.basename(o))[0]
            k = f"var/{stem}{suffix}.jpg"
            if k in v:
                xs.append(cos(o, k))
        print(f"  {suffix:10} {pct(xs)}")
    print("\nVariant to its group's centre (original and the variants but -crop50):")
    xs = []
    for o in origs[:VARIED]:
        stem = os.path.splitext(os.path.basename(o))[0]
        group = [o] + [f"var/{stem}{s}.jpg" for s in VARIANTS if s != "-crop50"]
        group = [g for g in group if g in v]
        c = sum(v[g] for g in group); c /= np.linalg.norm(c)
        xs += [float(v[g] @ c) for g in group]
    print(f"  {'':10} {pct(xs)}")
    print("\nDifferent photos, each original's closest other original:")
    M = np.stack([v[o] for o in origs]); S = M @ M.T; np.fill_diagonal(S, -1)
    print(f"  {'':10} {pct(S.max(1))}")
    print(f"  pairs over 0.90: {int((np.triu(S, 1) > 0.90).sum())}, over 0.95: {int((np.triu(S, 1) > 0.95).sum())}")
    shots = sorted(k for k in v if k.startswith("shots/"))
    if shots:
        same_ui, other_ui, clock = [], [], []
        px = {r: ([], [], []) for r in PIXEL_RULES}
        p = lambda k: os.path.join(out, "img", k)
        for a in shots:
            for b in shots:
                if a >= b:
                    continue
                ua, ub = a[6:10], b[6:10]
                if ua != ub:
                    i, xs = 2, other_ui
                elif "clock" in a + b and a.replace("-clock", "") == b.replace("-clock", ""):
                    i, xs = 0, clock
                else:
                    i, xs = 1, same_ui
                xs.append(cos(a, b))
                for r in PIXEL_RULES:
                    px[r][i].append(differing(p(a), p(b), *r))
        print("\nScreenshots, cosine:")
        print(f"  {'clock only':16} {pct(clock)}")
        print(f"  {'same UI':16} {pct(same_ui)}")
        print(f"  {'other UI':16} {pct(other_ui)}")
        for (n, step), (c, s_, o) in px.items():
            print(f"Screenshots, share of {n}x{n} grey pixels over {step}/255 apart:")
            print(f"  {'clock only':16} {pct(c)}")
            print(f"  {'same UI':16} {pct(s_)}")
            print(f"  {'other UI':16} {pct(o)}")

if __name__ == "__main__":
    {"make": lambda: make(sys.argv[2], sys.argv[3]), "score": lambda: score(sys.argv[2])}[sys.argv[1]]()
