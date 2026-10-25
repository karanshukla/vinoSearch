"""PP-OCRv6 small at static shapes on the NPU against RapidOCR's dynamic-shape
pipeline on the CPU (#106), on synthetic screenshots and a scanned page whose
text is known, or on real screenshots with their text beside them.

    hf download arankays/pp-ocrv6-small-onnx --local-dir <model dir>
    ENV="uv run --python 3.13 --no-project --with rapidocr==3.10.0 --with openvino==2026.4.1 \
         --with opencv-python-headless --with pillow --with pyclipper --with shapely python -I"
    [VINOSEARCH=target/release/vinosearch] $ENV bench/ocr_static.py <model dir>/onnx <out dir> [NPU|CPU] [<screenshots dir>]

Pipelines: RapidOCR as shipped (the reference; dynamic shapes, so CPU only),
then the detector at 1088x1920 or 1920x1088 and the recogniser at widths 320,
640 and 1280 on the device, boxing lines as RapidOCR's DB polygons do
("rotated") and as upright connected regions ("upright"). With VINOSEARCH set,
`vinosearch read-text` too, which is what indexing runs. Prints, per image,
the share of its words found, of its ID-like words (4+ characters with a
digit: invoice and request numbers), and of its characters (each true line
against its best OCR line), and the time. Word scores hide garbled lines when
the same words appear elsewhere on the page; the character score doesn't.

A <screenshots dir> holds PNGs, each with its visible text as a JSON string in
a .txt beside it (document.body's text nodes inside the viewport, from a
browser). Fonts are Fedora's: Noto Sans, Cascadia Mono, Liberation Serif.
"""
import difflib, json, os, re, subprocess, sys, time, random
from pathlib import Path
import cv2
import numpy as np
import openvino as ov
from PIL import Image, ImageDraw, ImageFont, ImageFilter

M, OUT = Path(sys.argv[1]), Path(sys.argv[2])
DEV = sys.argv[3] if len(sys.argv) > 3 else "NPU"
OUT.mkdir(parents=True, exist_ok=True)
CACHE = Path.home() / ".cache/vinosearch/bench/ocr/ov-cache"
F = "/usr/share/fonts/"
SANS = F + "google-noto/NotoSans-Regular.ttf"
BOLD = F + "google-noto/NotoSans-Bold.ttf"
MONO = F + "cascadia-mono-nf-fonts/CascadiaMonoNF-Regular.otf"
SERIF = F + "liberation-serif-fonts/LiberationSerif-Regular.ttf"
random.seed(1)

# ---------------------------------------------------------------- scenes

def font(path, size):
    return ImageFont.truetype(path, size)


def lines_at(d, x, y, lines, f, fill, gap=1.45):
    h = f.size
    for ln in lines:
        d.text((x, y), ln, font=f, fill=fill)
        y += int(h * gap)
    return y


WORDS = ("rotate signing key before the release deadline invoice payment overdue "
         "connection refused database migration failed retry backup storage "
         "quarterly report meeting moved thursday parcel delivered customer").split()


def sentence(n):
    return " ".join(random.choice(WORDS) for _ in range(n)).capitalize()


def desktop(scale):
    W, H = int(1920 * scale), int(1080 * scale)
    img = Image.new("RGB", (W, H), (236, 239, 244))
    d = ImageDraw.Draw(img)
    s = lambda v: int(v * scale)
    gt = []
    # Panel with a clock and app names.
    d.rectangle([0, H - s(44), W, H], fill=(32, 36, 44))
    t = ["Files", "Terminal", "Firefox", "10:42 AM"]
    f = font(SANS, s(13))
    for i, w in enumerate(t):
        d.text((s(60 + i * 140), H - s(32)), w, font=f, fill=(230, 230, 230))
    gt += t
    # Error dialog.
    d.rectangle([s(560), s(300), s(1360), s(560)], fill=(255, 255, 255), outline=(180, 180, 180))
    head = "Could not connect to db-prod-07.internal:5432"
    body = ["FATAL: password authentication failed for user \"billing_svc\"",
            "Error code ECONNREFUSED (errno 111) at 2026-10-25 10:41:58",
            "Request ID 7f3a9c21-bd04-4e6e-9a1f-03c58e2b6d19"]
    d.text((s(590), s(325)), head, font=font(BOLD, s(17)), fill=(20, 20, 20))
    lines_at(d, s(590), s(370), body, font(SANS, s(14)), (60, 60, 60))
    d.rectangle([s(1210), s(505), s(1340), s(540)], fill=(53, 132, 228))
    d.text((s(1250), s(512)), "Retry", font=font(SANS, s(14)), fill=(255, 255, 255))
    gt += [head, *body, "Retry"]
    # Dark terminal.
    d.rectangle([s(40), s(620), s(1000), s(1010)], fill=(30, 30, 30))
    term = ["$ cargo build --release --workspace",
            "   Compiling vinosearch v3.0.0 (/home/user/vinoSearch)",
            "error[E0308]: mismatched types",
            "  --> src/index.rs:812:17",
            "    expected `u32`, found `i64`",
            "$ git log --oneline -1",
            "53074ea Merge pull request #119 from krunner/open-at-match"]
    lines_at(d, s(56), s(640), term, font(MONO, s(14)), (220, 220, 210))
    gt += term
    # Notes window: paragraph text, small.
    d.rectangle([s(1080), s(620), s(1880), s(1010)], fill=(250, 250, 250))
    notes = [sentence(10) for _ in range(9)]
    lines_at(d, s(1100), s(640), notes, font(SANS, s(13)), (40, 40, 40))
    gt += notes
    return img, gt


def receipt():
    W, H = 900, 1400
    img = Image.new("RGB", (W, H), (255, 255, 255))
    d = ImageDraw.Draw(img)
    rows = ["ACME HARDWARE SUPPLY", "1250 Bloor St W, Toronto ON", "Invoice INV-2026-004817",
            "Date 2026-10-19  Cashier 042", "Cordless drill 18V      $129.99",
            "Drill bits set (21)      $24.50", "Wood glue 500ml           $8.75",
            "Subtotal                $163.24", "HST 13%                  $21.22",
            "TOTAL                   $184.46", "VISA **** 4471  AUTH 08812Q",
            "Thank you for shopping with us"]
    lines_at(d, 60, 60, rows, font(MONO, 26), (10, 10, 10), gap=1.9)
    return img, rows


def phone():
    W, H = 1080, 2400
    img = Image.new("RGB", (W, H), (245, 245, 245))
    d = ImageDraw.Draw(img)
    f = font(SANS, 42)
    gt = []
    msgs = [("Are we still on for dinner at Kinka Izakaya?", 0),
            ("Yes! Booking is under Shukla for 7:30", 1),
            ("Can you send me the parking code", 0),
            ("It is 4471 then press the green button", 1),
            ("Running ten minutes late sorry", 0),
            ("No worries see you soon", 1)]
    y = 220
    d.text((60, 90), "Priya", font=font(BOLD, 52), fill=(0, 0, 0))
    gt.append("Priya")
    for text, me in msgs:
        words, rows, cur = text.split(), [], ""
        for w in words:
            if d.textlength(cur + " " + w, font=f) > 700:
                rows.append(cur.strip()); cur = w
            else:
                cur += " " + w
        rows.append(cur.strip())
        h = len(rows) * 60 + 40
        x0 = W - 820 if me else 40
        d.rounded_rectangle([x0, y, x0 + 780, y + h], 30, fill=(0, 132, 255) if me else (228, 230, 235))
        lines_at(d, x0 + 30, y + 18, rows, f, (255, 255, 255) if me else (20, 20, 20), gap=60 / 42)
        gt += rows
        y += h + 40
    return img, gt


def scan():
    W, H = 2480, 3508  # A4 at 300 dpi
    img = Image.new("L", (W, H), 250)
    d = ImageDraw.Draw(img)
    gt = ["Statement of Account", "Policy number HX-77120-B   Period 01/09/2026 to 30/09/2026"]
    d.text((200, 220), gt[0], font=font(BOLD, 80), fill=20)
    d.text((200, 360), gt[1], font=font(SERIF, 46), fill=25)
    body = [sentence(12) for _ in range(40)]
    lines_at(d, 200, 480, body, font(SERIF, 46), 25, gap=1.5)
    gt += body
    img = img.rotate(0.8, resample=Image.BICUBIC, fillcolor=250)
    a = np.asarray(img).astype(np.float32)
    a += np.random.default_rng(2).normal(0, 9, a.shape)
    img = Image.fromarray(a.clip(0, 255).astype(np.uint8)).filter(ImageFilter.GaussianBlur(0.6))
    return img.convert("RGB"), gt


SCENES = {"desktop_1080p": lambda: desktop(1.0), "desktop_hidpi": lambda: desktop(1.5),
          "receipt": receipt, "phone": phone, "scan_a4": scan}
if len(sys.argv) > 4:  # a directory of real screenshots, each with its text as JSON
    def real(png):
        return lambda: (Image.open(png).convert("RGB"), json.loads(png.with_suffix(".txt").read_text()).splitlines())
    SCENES = {p.stem: real(p) for p in sorted(Path(sys.argv[4]).glob("*.png"))}

# ---------------------------------------------------------------- metric

def words(text):
    return re.findall(r"[a-z0-9]+", text.lower())


def recall(gt_lines, found):
    have = set(words(" ".join(found)))
    want = [w for ln in gt_lines for w in words(ln)]
    hit = sum(w in have for w in want)
    ids = [w for w in want if re.search(r"\d", w) and len(w) >= 4]
    return hit / len(want), sum(w in have for w in ids) / max(1, len(ids)), chars(gt_lines, found)


def chars(gt_lines, found):
    """Each true line's difflib ratio to its closest OCR line, averaged."""
    got = [f.strip().lower() for f in found if f.strip()]
    total = 0.0
    for g in (l.strip().lower() for l in gt_lines if l.strip()):
        m, best = difflib.SequenceMatcher(None, g, "", autojunk=False), 0.0
        for o in got:
            m.set_seq2(o)
            if m.real_quick_ratio() > best and m.quick_ratio() > best:
                best = max(best, m.ratio())
        total += best
    return total / max(1, sum(1 for l in gt_lines if l.strip()))

# ---------------------------------------------------------------- static pipeline

core = ov.Core()
CHARS = core.read_model(str(M / "PP-OCRv6_rec_small.onnx")).get_rt_info()["framework"]["character"].value.splitlines()
CHARS = ["blank"] + CHARS + [" "]
DET_SHAPES = {"land": (1088, 1920), "port": (1920, 1088)}
REC_WIDTHS = (320, 640, 1280)
compiled = {}


def model(name, shape):
    key = (name, shape)
    if key not in compiled:
        m = core.read_model(str(M / name))
        m.reshape({m.inputs[0].any_name: shape})
        compiled[key] = core.compile_model(m, DEV, {"CACHE_DIR": str(CACHE / DEV)}).create_infer_request()
    return compiled[key]


def norm(bgr):
    return ((bgr.astype(np.float32) / 255 - 0.5) / 0.5).transpose(2, 0, 1)[None]


def detect(bgr, upright):
    h, w = bgr.shape[:2]
    H, W = DET_SHAPES["land" if w >= h else "port"]
    s = min(H / h, W / w)
    nh, nw = int(round(h * s)), int(round(w * s))
    canvas = np.zeros((H, W, 3), np.uint8)
    canvas[:nh, :nw] = cv2.resize(bgr, (nw, nh), interpolation=cv2.INTER_LINEAR if s < 1 else cv2.INTER_CUBIC)
    prob = model("PP-OCRv6_det_small.onnx", (1, 3, H, W)).infer({0: norm(canvas)})[0][0, 0, :nh, :nw]
    if upright:
        return boxes_cc(prob, s)
    from rapidocr.ch_ppocr_det.utils import DBPostProcess
    pp = DBPostProcess(thresh=0.3, box_thresh=0.5, unclip_ratio=1.6, use_dilation=True)
    boxes, _ = pp(prob[None, None], (h, w))
    return [b.astype(np.float32) for b in boxes]


def boxes_cc(prob, s):
    """Axis-aligned boxes from connected components of the thresholded map,
    grown by DB's unclip distance: what a port without contours would do."""
    mask = cv2.dilate((prob > 0.3).astype(np.uint8), np.ones((2, 2), np.uint8))
    n, lab, stats, _ = cv2.connectedComponentsWithStats(mask, connectivity=8)
    out = []
    for i in range(1, n):
        x, y, bw, bh, area = stats[i]
        if min(bw, bh) < 3:
            continue
        score = prob[y:y + bh, x:x + bw][lab[y:y + bh, x:x + bw] == i].mean()
        if score < 0.5:
            continue
        dist = bw * bh * 1.6 / (2 * (bw + bh))
        x0, y0, x1, y1 = x - dist, y - dist, x + bw + dist, y + bh + dist
        x0, y0, x1, y1 = [v / s for v in (x0, y0, x1, y1)]
        out.append(np.array([[x0, y0], [x1, y0], [x1, y1], [x0, y1]], np.float32))
    return out


def crop(bgr, box):
    from rapidocr.utils.process_img import get_rotate_crop_image
    h, w = bgr.shape[:2]
    box = box.copy()
    box[:, 0] = box[:, 0].clip(0, w - 1); box[:, 1] = box[:, 1].clip(0, h - 1)
    return get_rotate_crop_image(bgr, box)


def recognise(c):
    h, w = c.shape[:2]
    if h < 2 or w < 2:
        return "", 0
    nw = max(1, int(round(48 * w / h)))
    W = next((b for b in REC_WIDTHS if b >= nw), REC_WIDTHS[-1])
    nw = min(nw, W)
    x = np.zeros((48, W, 3), np.float32)
    x[:, :nw] = (cv2.resize(c, (nw, 48)).astype(np.float32) / 255 - 0.5) / 0.5
    out = model("PP-OCRv6_rec_small.onnx", (1, 3, 48, W)).infer({0: x.transpose(2, 0, 1)[None]})[0][0]
    idx, prob = out.argmax(-1), out.max(-1)
    keep = (idx != 0) & np.r_[True, idx[1:] != idx[:-1]]
    text = "".join(CHARS[i] for i in idx[keep])
    return text, float(prob[keep].mean()) if keep.any() else 0.0


def static_ocr(bgr, upright=False):
    found = []
    for b in detect(bgr, upright):
        text, score = recognise(crop(bgr, b))
        if score >= 0.5 and text.strip():
            found.append(text)
    return found

# ---------------------------------------------------------------- run

from rapidocr import EngineType, RapidOCR
ref = RapidOCR(params={**{f"{st}.engine_type": EngineType.OPENVINO for st in ("Det", "Cls", "Rec")}, "Global.log_level": "warning"})
print(f"{'scene':15} {'size':>10}  {'pipeline':22} {'words':>6} {'ids':>5} {'chars':>6} {'time':>7} lines")
for name, make in SCENES.items():
    img, gt = make()
    img.save(OUT / f"{name}.png")
    (OUT / f"{name}.gt.txt").write_text("\n".join(gt))
    bgr = cv2.cvtColor(np.asarray(img), cv2.COLOR_RGB2BGR)
    size = f"{img.width}x{img.height}"
    runs = [("rapidocr CPU dynamic", lambda: list(ref(bgr).txts or ()))]
    runs += [(f"static {DEV} rotated", lambda: static_ocr(bgr)),
             (f"static {DEV} upright", lambda: static_ocr(bgr, True))]
    if os.environ.get("VINOSEARCH"):
        def rust(png=OUT / f"{name}.png"):
            out = subprocess.run([os.environ["VINOSEARCH"], "--device", DEV, "read-text", str(png)],
                                 capture_output=True, text=True, check=True).stdout
            return out.splitlines()
        runs.append((f"vinosearch {DEV}", rust))
    for label, run in runs:
        run()  # warm: compile / cache
        t = time.perf_counter(); found = run(); dt = time.perf_counter() - t
        wr, ir, cr = recall(gt, found)
        print(f"{name:15} {size:>10}  {label:22} {wr:6.3f} {ir:5.2f} {cr:6.3f} {dt*1000:5.0f}ms {len(found)}", flush=True)
        (OUT / f"{name}.{label.replace(' ', '_')}.txt").write_text("\n".join(found))
