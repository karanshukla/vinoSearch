//! Text in images (#106): screenshots, photographed pages and scanned PDFs,
//! read with PaddleOCR's PP-OCRv6 small detector and recogniser on the NPU.
//!
//! Both graphs take dynamic shapes, which the NPU can't compile, so each is
//! compiled at a few static ones. An image is scaled, aspect kept, into the
//! detector's landscape or portrait shape and padded with black; each line
//! the detector finds is cropped, scaled to the recogniser's height and
//! padded to the narrowest width that holds it. On 1920x1080 browser
//! screenshots that read 96-98% of the words shown in 0.3-0.4 s, where
//! RapidOCR's dynamic pipeline on the CPU read 86-94% in 1.7-2.9 s
//! (bench/ocr_static.py).
//!
//! Lines are boxed as the connected regions of the detector's map, each
//! turned to its own axis (the principal axis of its pixels) and grown by
//! DB's unclip distance. Upright boxes read a page scanned 0.8 degrees off at
//! 0.79 of its characters, turned ones at 1.00, as RapidOCR's polygons do;
//! the moments get there without contour or polygon code.

use anyhow::{Context, Result, ensure};
use image::{RgbImage, imageops::FilterType};
use openvino::{CompiledModel, Core, DeviceType, InferRequest};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// What this build keeps of what the model reads, in the image model tag:
/// raised when it changes, so images are read again. 2 drops texture read
/// as a letter or two; 3, images with under `MIN_IMAGE_TEXT` in all.
pub const READING: u32 = 3;
/// An image's lines are indexed only when they hold this many letters or
/// digits between them. A dog photo read as five short "words" (14) matched
/// "girl" at 0.657, over the text floor: a few characters embed close to a
/// one-word query whatever they say. Screenshots and pages hold hundreds;
/// a picture under this is still found by what it looks like.
pub const MIN_IMAGE_TEXT: usize = 20;

/// The detector's shapes, as (height, width): landscape, then portrait.
/// 1088x1920 holds a 1080p screenshot unscaled; 64 ms on the NPU.
pub const DET_SHAPES: [(usize, usize); 2] = [(1088, 1920), (1920, 1088)];
const REC_HEIGHT: usize = 48;
/// The recogniser's widths, 3 to 11 ms a line. A line wider than the last
/// is squeezed into it.
pub const REC_WIDTHS: [usize; 3] = [320, 640, 1280];
/// A small image is scaled up to the detector's size, but no further than
/// this: an icon blown up 17 times reads as noise.
const MAX_UPSCALE: f32 = 4.0;
/// DB postprocessing, as RapidOCR runs PP-OCRv6: a pixel is text above
/// `THRESH`, a region is kept when its mean is `BOX_THRESH` or more, and grows
/// by `area * UNCLIP / perimeter` on every side.
const THRESH: f32 = 0.3;
const BOX_THRESH: f32 = 0.5;
const UNCLIP: f32 = 1.6;
/// Regions narrower than this, in the detector's pixels, are dropped.
const MIN_SIDE: usize = 3;
/// Lines whose characters average a lower probability are dropped.
const MIN_SCORE: f32 = 0.5;
/// A line needs this many letters or digits, and under `SHORT` of them this
/// probability, to be text rather than texture. Photos with no text read as
/// one- and two-character lines at 0.3-0.8, and a chunk of one character
/// matches any query a little; real lines in the same images had 5 or more
/// at 0.87 or above.
const MIN_ALNUM: usize = 3;
const SHORT: usize = 6;
const SHORT_SCORE: f32 = 0.8;
/// A region turns to its principal axis only when that axis is this many
/// times its other's spread: a word is, a lone glyph's angle means nothing.
const MIN_ELONGATION: f32 = 2.0;

/// A line's box in the image's own pixels: its centre, its length along
/// `(cos, sin)` and its height across it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub cx: f32,
    pub cy: f32,
    pub w: f32,
    pub h: f32,
    pub cos: f32,
    pub sin: f32,
}

impl Rect {
    /// The upright bounds' top and left.
    fn top(&self) -> f32 {
        self.cy - (self.w * self.sin.abs() + self.h * self.cos.abs()) / 2.0
    }

    fn left(&self) -> f32 {
        self.cx - (self.w * self.cos.abs() + self.h * self.sin.abs()) / 2.0
    }
}

/// Running sums over a region's pixels: count, probability, and the first
/// and second moments of their centres.
#[derive(Default)]
struct Moments {
    n: f64,
    prob: f64,
    x: f64,
    y: f64,
    xx: f64,
    yy: f64,
    xy: f64,
}

impl Moments {
    fn add(&mut self, x: usize, y: usize, p: f32) {
        let (x, y) = (x as f64 + 0.5, y as f64 + 0.5);
        self.n += 1.0;
        self.prob += f64::from(p);
        self.x += x;
        self.y += y;
        self.xx += x * x;
        self.yy += y * y;
        self.xy += x * y;
    }

    /// The direction of the principal axis, pointing right (or down, when
    /// it stands upright); along x unless the region is elongated.
    fn axis(&self) -> (f64, f64) {
        let (mx, my) = (self.x / self.n, self.y / self.n);
        let sxx = self.xx / self.n - mx * mx;
        let syy = self.yy / self.n - my * my;
        let sxy = self.xy / self.n - mx * my;
        // Eigenvalues of the covariance: the spread along each axis.
        let mid = (sxx + syy) / 2.0;
        let gap = (((sxx - syy) / 2.0).powi(2) + sxy * sxy).sqrt();
        let (major, minor) = (mid + gap, (mid - gap).max(0.0));
        if major < f64::from(MIN_ELONGATION).powi(2) * minor.max(1e-9) {
            return (1.0, 0.0);
        }
        let angle = 0.5 * (2.0 * sxy).atan2(sxx - syy);
        let (sin, cos) = angle.sin_cos();
        if cos < -1e-6 || (cos.abs() <= 1e-6 && sin < 0.0) {
            (-cos, -sin)
        } else {
            (cos, sin)
        }
    }
}

/// `img` scaled into a detector shape `(h, w)` and padded with black, as the
/// detector's input: BGR, `(x / 255 - 0.5) / 0.5`, channels first. Returns
/// the input, the scale and the scaled image's (height, width).
fn letterbox(img: &RgbImage, (h, w): (usize, usize)) -> (Vec<f32>, f32, (usize, usize)) {
    let (ih, iw) = (img.height() as f32, img.width() as f32);
    let scale = (h as f32 / ih).min(w as f32 / iw).min(MAX_UPSCALE);
    let nh = ((ih * scale).round() as usize).clamp(1, h);
    let nw = ((iw * scale).round() as usize).clamp(1, w);
    let scaled;
    let src = if (nw, nh) == (img.width() as usize, img.height() as usize) {
        img
    } else {
        scaled = image::imageops::resize(img, nw as u32, nh as u32, FilterType::Triangle);
        &scaled
    };
    // Black is -1 once normalised.
    let mut x = vec![-1f32; 3 * h * w];
    let plane = h * w;
    for (px, py, p) in src.enumerate_pixels() {
        let at = py as usize * w + px as usize;
        for (c, &v) in [p[2], p[1], p[0]].iter().enumerate() {
            x[c * plane + at] = f32::from(v) / 127.5 - 1.0;
        }
    }
    (x, scale, (nh, nw))
}

/// The detector's map `prob` (`w` wide), over the scaled image's `(nh, nw)`,
/// to line boxes in the image's pixels. Each 8-connected region above
/// `THRESH`, after a 2x2 dilation, is a line.
fn boxes(prob: &[f32], w: usize, (nh, nw): (usize, usize), scale: f32) -> Vec<Rect> {
    // Dilated by a 2x2 kernel anchored at its centre, as cv2.dilate does:
    // a pixel is set when it or its left, upper or upper-left neighbour is.
    let on = |x: usize, y: usize| prob[y * w + x] > THRESH;
    let mut mask = vec![false; nh * nw];
    for y in 0..nh {
        for x in 0..nw {
            mask[y * nw + x] = on(x, y)
                || (x > 0 && on(x - 1, y))
                || (y > 0 && on(x, y - 1))
                || (x > 0 && y > 0 && on(x - 1, y - 1));
        }
    }
    let mut seen = vec![false; nh * nw];
    let mut stack = Vec::new();
    let mut region = Vec::new();
    let mut out = Vec::new();
    for start in 0..nh * nw {
        if !mask[start] || seen[start] {
            continue;
        }
        seen[start] = true;
        stack.push(start);
        region.clear();
        let mut m = Moments::default();
        while let Some(i) = stack.pop() {
            let (x, y) = (i % nw, i / nw);
            region.push(i);
            m.add(x, y, prob[y * w + x]);
            for dy in -1i64..=1 {
                for dx in -1i64..=1 {
                    let (nx, ny) = (x as i64 + dx, y as i64 + dy);
                    if nx < 0 || ny < 0 || nx >= nw as i64 || ny >= nh as i64 {
                        continue;
                    }
                    let j = ny as usize * nw + nx as usize;
                    if mask[j] && !seen[j] {
                        seen[j] = true;
                        stack.push(j);
                    }
                }
            }
        }
        if m.prob / m.n < f64::from(BOX_THRESH) {
            continue;
        }
        // The region's extent along its axis and across it, each pixel taken
        // as its whole square.
        let (cos, sin) = m.axis();
        let (mx, my) = (m.x / m.n, m.y / m.n);
        let (mut u0, mut u1, mut v0, mut v1) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
        for &i in &region {
            let (dx, dy) = ((i % nw) as f64 + 0.5 - mx, (i / nw) as f64 + 0.5 - my);
            let (u, v) = (dx * cos + dy * sin, -dx * sin + dy * cos);
            (u0, u1, v0, v1) = (u0.min(u), u1.max(u), v0.min(v), v1.max(v));
        }
        let pad = 0.5 * (cos.abs() + sin.abs());
        let (bw, bh) = (u1 - u0 + 2.0 * pad, v1 - v0 + 2.0 * pad);
        if bw.min(bh) < MIN_SIDE as f64 {
            continue;
        }
        let (um, vm) = ((u0 + u1) / 2.0, (v0 + v1) / 2.0);
        let grow = bw * bh * f64::from(UNCLIP) / (2.0 * (bw + bh));
        let s = f64::from(scale);
        out.push(Rect {
            cx: ((mx + um * cos - vm * sin) / s) as f32,
            cy: ((my + um * sin + vm * cos) / s) as f32,
            w: ((bw + 2.0 * grow) / s) as f32,
            h: ((bh + 2.0 * grow) / s) as f32,
            cos: cos as f32,
            sin: sin as f32,
        });
    }
    reading_order(out)
}

/// Top to bottom, then left to right among boxes on the same row: within
/// half the shorter one's height of each other.
fn reading_order(mut rects: Vec<Rect>) -> Vec<Rect> {
    rects.sort_by(|a, b| {
        a.top()
            .total_cmp(&b.top())
            .then(a.left().total_cmp(&b.left()))
    });
    let mut out: Vec<Rect> = Vec::with_capacity(rects.len());
    let mut row: Vec<Rect> = Vec::new();
    for r in rects {
        if let Some(first) = row.first()
            && r.top() - first.top() > first.h.min(r.h) / 2.0
        {
            row.sort_by(|a, b| a.left().total_cmp(&b.left()));
            out.append(&mut row);
        }
        row.push(r);
    }
    row.sort_by(|a, b| a.left().total_cmp(&b.left()));
    out.append(&mut row);
    out
}

/// The line `r` sampled out of `img` along its own axes, bilinear, the
/// image's edge repeated past it (as RapidOCR's warp does); turned a quarter
/// left when it stands 1.5 times taller than wide, for vertical text.
fn crop(img: &RgbImage, r: Rect) -> Option<RgbImage> {
    let (w, h) = (r.w.round() as u32, r.h.round() as u32);
    if w < 2 || h < 2 || u64::from(w) * u64::from(h) > 1 << 26 {
        return None;
    }
    let (iw, ih) = (img.width() as f32, img.height() as f32);
    let raw = img.as_raw();
    let stride = img.width() as usize * 3;
    let at = |x: usize, y: usize, c: usize| f32::from(raw[y * stride + x * 3 + c]);
    let line = RgbImage::from_fn(w, h, |i, j| {
        let (u, v) = (i as f32 + 0.5 - r.w / 2.0, j as f32 + 0.5 - r.h / 2.0);
        // The source point, as a pixel's centre is at +0.5.
        let x = (r.cx + u * r.cos - v * r.sin - 0.5).clamp(0.0, iw - 1.0);
        let y = (r.cy + u * r.sin + v * r.cos - 0.5).clamp(0.0, ih - 1.0);
        let (x0, y0) = (x.floor() as usize, y.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(iw as usize - 1), (y0 + 1).min(ih as usize - 1));
        let (fx, fy) = (x - x0 as f32, y - y0 as f32);
        image::Rgb(std::array::from_fn(|c| {
            let top = at(x0, y0, c) * (1.0 - fx) + at(x1, y0, c) * fx;
            let bottom = at(x0, y1, c) * (1.0 - fx) + at(x1, y1, c) * fx;
            (top * (1.0 - fy) + bottom * fy).round() as u8
        }))
    });
    Some(if line.height() as f32 >= 1.5 * line.width() as f32 {
        image::imageops::rotate270(&line)
    } else {
        line
    })
}

/// A line as the recogniser's input: scaled to its height, aspect kept up to
/// the widest shape, normalised as the detector's, padded with 0 on the
/// right. Returns the width picked and the input.
fn rec_input(line: &RgbImage) -> (usize, Vec<f32>) {
    let aspect = line.width() as f32 / line.height() as f32;
    let fit = ((REC_HEIGHT as f32 * aspect).round() as usize).max(1);
    let width = REC_WIDTHS
        .iter()
        .copied()
        .find(|&w| w >= fit)
        .unwrap_or(REC_WIDTHS[REC_WIDTHS.len() - 1]);
    (width, rec_fill(line, fit.min(width), width))
}

/// `line` scaled to `nw` by the recogniser's height, then padded to `width`.
fn rec_fill(line: &RgbImage, nw: usize, width: usize) -> Vec<f32> {
    let scaled = image::imageops::resize(line, nw as u32, REC_HEIGHT as u32, FilterType::Triangle);
    let mut x = vec![0f32; 3 * REC_HEIGHT * width];
    let plane = REC_HEIGHT * width;
    for (px, py, p) in scaled.enumerate_pixels() {
        let at = py as usize * width + px as usize;
        for (c, &v) in [p[2], p[1], p[0]].iter().enumerate() {
            x[c * plane + at] = f32::from(v) / 127.5 - 1.0;
        }
    }
    x
}

/// Greedy CTC over the recogniser's `[steps, classes]` probabilities: the
/// likeliest class per step, repeats collapsed and blanks (0) dropped.
/// Class `i` is `chars[i - 1]`; the one past the dict is a space. Returns
/// the text and its characters' mean probability.
fn ctc(probs: &[f32], classes: usize, chars: &[String]) -> (String, f32) {
    let mut text = String::new();
    let (mut sum, mut n) = (0f32, 0usize);
    let mut last = 0;
    for step in probs.chunks_exact(classes) {
        let (best, p) = step
            .iter()
            .copied()
            .enumerate()
            .fold((0, f32::MIN), |m, (i, p)| if p > m.1 { (i, p) } else { m });
        if best != 0 && best != last {
            text.push_str(chars.get(best - 1).map_or(" ", String::as_str));
            sum += p;
            n += 1;
        }
        last = best;
    }
    let score = if n == 0 { 0.0 } else { sum / n as f32 };
    (text, score)
}

/// Pages past this in a scanned PDF are left unread: at ~0.5 s a page, a
/// 100-page scan takes under a minute.
pub const MAX_SCAN_PAGES: usize = 100;
/// pdfinfo and pdftoppm get what the document parser does (#84): memory and
/// CPU seconds a run, and one deadline for the whole scan, so a crafted PDF
/// that stalls every page holds indexing minutes, not a minute a page.
const POPPLER_ADDRESS_SPACE: u64 = 1 << 30;
const POPPLER_CPU_SECONDS: u64 = 30;
pub const SCAN_WALL: std::time::Duration = std::time::Duration::from_secs(300);

/// Poppler's pdfinfo and pdftoppm, if both are on PATH (poppler-utils, or
/// poppler on Arch). Looked up once.
pub fn poppler() -> bool {
    static FOUND: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FOUND.get_or_init(|| crate::menu::on_path("pdfinfo") && crate::menu::on_path("pdftoppm"))
}

/// `tool` reading the PDF on stdin, under POPPLER's limits.
fn poppler_command(tool: &str) -> std::process::Command {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(tool);
    // SAFETY: setrlimit is async-signal-safe, and the closure allocates nothing.
    unsafe {
        cmd.pre_exec(|| {
            for (resource, value) in [
                (libc::RLIMIT_AS, POPPLER_ADDRESS_SPACE),
                (libc::RLIMIT_CPU, POPPLER_CPU_SECONDS),
            ] {
                let lim = libc::rlimit {
                    rlim_cur: value as libc::rlim_t,
                    rlim_max: value as libc::rlim_t,
                };
                if libc::setrlimit(resource, &lim) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    cmd
}

/// The time left before `deadline`, or an error once it has passed.
fn left(deadline: Instant) -> Result<std::time::Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    ensure!(
        !left.is_zero(),
        "over {} s reading the scan",
        SCAN_WALL.as_secs()
    );
    Ok(left)
}

/// The page count of the PDF `bytes`, by `deadline`.
pub fn pdf_pages(bytes: &[u8], deadline: Instant) -> Result<usize> {
    let mut cmd = poppler_command("pdfinfo");
    cmd.arg("-");
    let out = crate::proc::run(cmd, bytes, left(deadline)?).context("pdfinfo")?;
    String::from_utf8_lossy(&out)
        .lines()
        .find_map(|l| l.strip_prefix("Pages:")?.trim().parse().ok())
        .context("pdfinfo gave no page count")
}

/// Page `page` (from 1) of the PDF `bytes`, rendered with its longer side at
/// the detector's (no more pixels than it reads), by `deadline`.
pub fn render_page(bytes: &[u8], page: usize, deadline: Instant) -> Result<RgbImage> {
    let longest = DET_SHAPES[0].1.to_string();
    let page = page.to_string();
    let mut cmd = poppler_command("pdftoppm");
    cmd.args(["-scale-to", &longest, "-f", &page, "-l", &page, "-"]);
    let ppm = crate::proc::run(cmd, bytes, left(deadline)?).context("pdftoppm")?;
    Ok(
        image::load_from_memory_with_format(&ppm, image::ImageFormat::Pnm)
            .context("reading pdftoppm's page")?
            .to_rgb8(),
    )
}

/// Letters and digits in `lines`, CJK counting double, as `is_text` counts.
pub fn text_size(lines: &[String]) -> usize {
    lines.iter().map(|l| alnum(l)).sum()
}

fn alnum(line: &str) -> usize {
    let cjk = |c: char| matches!(c as u32, 0x3040..=0x30ff | 0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xac00..=0xd7af | 0xf900..=0xfaff);
    line.chars()
        .filter(|c| c.is_alphanumeric())
        .map(|c| if cjk(c) { 2 } else { 1 })
        .sum()
}

/// Whether a recognised line, with its mean probability, is worth indexing.
/// Two CJK characters make a word, so they count double.
fn is_text(line: &str, score: f32) -> bool {
    let n = alnum(line);
    score >= MIN_SCORE && n >= MIN_ALNUM && (n >= SHORT || score >= SHORT_SCORE)
}

/// The recogniser's characters, one a line.
pub fn read_dict(path: &Path) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(path).with_context(|| path.display().to_string())?;
    let chars: Vec<String> = text.lines().map(str::to_owned).collect();
    ensure!(!chars.is_empty(), "{} is empty", path.display());
    Ok(chars)
}

struct Graph {
    _compiled: CompiledModel,
    request: InferRequest,
    fresh: bool,
}

impl Graph {
    fn new(
        core: &mut Core,
        path: &Path,
        shape: [i64; 4],
        device: &str,
        cache: &Path,
    ) -> Result<Self> {
        let (mut compiled, fresh) = crate::embed::compile_cached(core, device, cache, |core| {
            let mut model = core
                .read_model_from_file(path.to_str().context("model path is not UTF-8")?, "")
                .with_context(|| format!("reading {}", path.display()))?;
            model
                .reshape_single_input(&openvino::PartialShape::new_static(4, &shape)?)
                .context("reshaping to a static shape")?;
            core.compile_model(&model, DeviceType::from(device))
                .with_context(|| format!("compiling {}", path.display()))
        })?;
        Ok(Self {
            request: compiled.create_infer_request()?,
            _compiled: compiled,
            fresh,
        })
    }

    /// Run on `input`, the graph's whole input, and hand its output to `read`.
    fn run<T>(&mut self, input: &[f32], read: impl FnOnce(&[f32]) -> T) -> Result<T> {
        // Into the request's own input, rather than a new 25 MB tensor for
        // every image.
        let mut t = self.request.get_input_tensor()?;
        let data = t.get_data_mut::<f32>()?;
        ensure!(
            data.len() == input.len(),
            "{} input values for a graph that takes {}",
            input.len(),
            data.len()
        );
        data.copy_from_slice(input);
        self.request.infer()?;
        Ok(read(self.request.get_output_tensor()?.get_data::<f32>()?))
    }
}

/// The detector at each of `DET_SHAPES` and the recogniser at each of
/// `REC_WIDTHS`, compiled on one device.
pub struct Ocr {
    det: Vec<Graph>,
    rec: Vec<Graph>,
    chars: Vec<String>,
    classes: usize,
    /// Where the parity check is recorded.
    cache: PathBuf,
    pub device: String,
}

fn det_shape((h, w): (usize, usize)) -> [i64; 4] {
    [1, 3, h as i64, w as i64]
}

fn rec_shape(w: usize) -> [i64; 4] {
    [1, 3, REC_HEIGHT as i64, w as i64]
}

/// Cosine of two outputs, summed in f64: over millions of values, f32 sums
/// drift past 1.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (mut ab, mut aa, mut bb) = (0f64, 0f64, 0f64);
    for (&x, &y) in a.iter().zip(b) {
        let (x, y) = (f64::from(x), f64::from(y));
        ab += x * y;
        aa += x * x;
        bb += y * y;
    }
    (ab / (aa.sqrt() * bb.sqrt())) as f32
}

/// Each graph's cache directory under `root`: its own, since a cache miss
/// drops the older blobs in its directory.
pub fn stages() -> Vec<String> {
    let det = DET_SHAPES.iter().map(|(h, w)| format!("ocrdet{h}x{w}"));
    let rec = REC_WIDTHS.iter().map(|w| format!("ocrrec{w}"));
    det.chain(rec).collect()
}

impl Ocr {
    fn compile(core: &mut Core, dir: &Path, device: &str, cache_root: &Path) -> Result<Self> {
        let key = |stage: &str| {
            cache_root.join(crate::embed::stage_key(
                crate::model::OCR_REVISION,
                device,
                stage,
            ))
        };
        let stages = stages();
        let (det_stages, rec_stages) = stages.split_at(DET_SHAPES.len());
        let det = DET_SHAPES
            .iter()
            .zip(det_stages)
            .map(|(&s, stage)| {
                Graph::new(
                    core,
                    &crate::model::ocr_det_path(dir),
                    det_shape(s),
                    device,
                    &key(stage),
                )
            })
            .collect::<Result<Vec<_>>>()?;
        let rec = REC_WIDTHS
            .iter()
            .zip(rec_stages)
            .map(|(&w, stage)| {
                Graph::new(
                    core,
                    &crate::model::ocr_rec_path(dir),
                    rec_shape(w),
                    device,
                    &key(stage),
                )
            })
            .collect::<Result<Vec<_>>>()?;
        let chars = read_dict(&crate::model::ocr_dict_path(dir))?;
        Ok(Self {
            det,
            rec,
            classes: chars.len() + 2,
            chars,
            cache: key(&rec_stages[0]),
            device: device.to_owned(),
        })
    }

    /// Compile on `device`; on another than the CPU, check it against the
    /// CPU on a fixed input, as the embedding stages are (#76), whenever it
    /// compiled fresh or wasn't checked under this OpenVINO. Off the CPU's
    /// answer, it runs on the CPU instead.
    pub fn new(core: &mut Core, dir: &Path, device: &str, cache_root: &Path) -> Result<Self> {
        ensure!(
            crate::model::ocr_present(dir),
            "the OCR model is not in {}; run: vinosearch fetch-model",
            dir.display()
        );
        let mut ocr = Self::compile(core, dir, device, cache_root)?;
        if device == "CPU" {
            return Ok(ocr);
        }
        let build = openvino::version().build_number;
        let fresh = ocr.det.iter().chain(&ocr.rec).any(|g| g.fresh);
        let cosine = match crate::parity::recorded(&ocr.cache) {
            Some((checked, cosine)) if checked == build && !fresh => cosine,
            _ => {
                let mut cpu = Self::compile(core, dir, "CPU", cache_root)?;
                let cosine = cosine(&ocr.probe()?, &cpu.probe()?);
                crate::parity::record(&ocr.cache, &build, cosine)?;
                eprintln!("ocr stage on {device}: cosine {cosine:.5} to the CPU");
                cosine
            }
        };
        if cosine < crate::parity::MIN_COSINE {
            eprintln!(
                "ocr stage on {device} is off the CPU (cosine {cosine:.4}); running it on CPU"
            );
            ocr = Self::compile(core, dir, "CPU", cache_root)?;
        }
        Ok(ocr)
    }

    /// Every graph's output for a fixed input: dark bars on white, like lines
    /// of text, so the detector's map isn't all zero.
    fn probe(&mut self) -> Result<Vec<f32>> {
        let page = RgbImage::from_fn(960, 540, |x, y| {
            let bar = (y / 18) % 3 == 1 && (x / 9) % 7 != 0 && x > 40 && x < 900;
            image::Rgb(if bar { [30, 30, 40] } else { [245, 245, 240] })
        });
        let mut out = Vec::new();
        for (g, &s) in self.det.iter_mut().zip(&DET_SHAPES) {
            g.run(&letterbox(&page, s).0, |o| out.extend_from_slice(o))?;
        }
        let line = image::imageops::crop_imm(&page, 30, 14, 880, 28).to_image();
        for (g, &w) in self.rec.iter_mut().zip(&REC_WIDTHS) {
            g.run(&rec_fill(&line, w * 3 / 4, w), |o| out.extend_from_slice(o))?;
        }
        Ok(out)
    }

    /// The lines of text in `img`, in reading order. `share` of the device's
    /// time, as document embedding takes it.
    pub fn read(&mut self, img: &RgbImage, share: f64) -> Result<Vec<String>> {
        if img.width() < 2 || img.height() < 2 {
            return Ok(Vec::new());
        }
        let t0 = Instant::now();
        let d = usize::from(img.height() > img.width());
        let shape = DET_SHAPES[d];
        let (input, scale, scaled) = letterbox(img, shape);
        let found = self.det[d].run(&input, |prob| boxes(prob, shape.1, scaled, scale))?;
        let mut lines = Vec::new();
        for r in found {
            let Some(line) = crop(img, r) else { continue };
            let (width, input) = rec_input(&line);
            let g = REC_WIDTHS
                .iter()
                .position(|&w| w == width)
                .expect("a listed width");
            let (classes, chars) = (self.classes, &self.chars);
            let (text, score) = self.rec[g].run(&input, |p| ctc(p, classes, chars))?;
            let text = text.trim();
            if is_text(text, score) {
                lines.push(text.to_owned());
            }
        }
        crate::embed::yield_device(t0, share);
        Ok(lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letterbox_keeps_the_aspect_and_pads_black() {
        let img = RgbImage::from_pixel(960, 540, image::Rgb([255, 0, 0]));
        let (x, scale, (nh, nw)) = letterbox(&img, (1088, 1920));
        assert_eq!(scale, 2.0);
        assert_eq!((nh, nw), (1080, 1920));
        let plane = 1088 * 1920;
        // Red is the third channel (BGR), at +1; padding rows are -1 in all.
        assert_eq!(x[2 * plane], 1.0);
        assert_eq!(x[0], -1.0);
        assert_eq!(x[1085 * 1920], -1.0);
    }

    #[test]
    fn a_tiny_image_is_scaled_up_no_more_than_four_times() {
        let img = RgbImage::new(40, 20);
        let (_, scale, (nh, nw)) = letterbox(&img, (1088, 1920));
        assert_eq!(scale, MAX_UPSCALE);
        assert_eq!((nh, nw), (80, 160));
    }

    #[test]
    fn regions_become_boxes_grown_by_the_unclip_distance() {
        let (w, h) = (64, 32);
        let mut prob = vec![0f32; w * h];
        // A 20x4 line at (10, 5), and a speck too small to keep.
        for y in 5..9 {
            for x in 10..30 {
                prob[y * w + x] = 0.9;
            }
        }
        prob[20 * w + 50] = 0.9;
        let b = boxes(&prob, w, (h, w), 2.0);
        assert_eq!(b.len(), 1);
        // Dilation adds a pixel right and down: 21x5 at (10, 5), grown by
        // 21*5*1.6/52 a side, all halved by the scale.
        let grow = 21.0 * 5.0 * 1.6 / 52.0;
        let r = b[0];
        let near = |a: f32, b: f32| (a - b).abs() < 1e-3;
        assert!(near(r.cos, 1.0) && near(r.sin, 0.0), "{r:?}");
        assert!(near(r.cx, 20.5 / 2.0) && near(r.cy, 7.5 / 2.0), "{r:?}");
        assert!(near(r.w, (21.0 + 2.0 * grow) / 2.0), "{r:?}");
        assert!(near(r.h, (5.0 + 2.0 * grow) / 2.0), "{r:?}");
    }

    #[test]
    fn a_skewed_line_is_boxed_along_its_slope() {
        // A bar 200 long and 6 high, rising 1 pixel in 20 (about 2.9 degrees).
        let (w, h) = (240, 60);
        let mut prob = vec![0f32; w * h];
        for x in 20..220 {
            let top = 40 - (x - 20) / 20;
            for y in top..top + 6 {
                prob[y * w + x] = 0.9;
            }
        }
        let b = boxes(&prob, w, (h, w), 1.0);
        assert_eq!(b.len(), 1);
        let angle = b[0].sin.atan2(b[0].cos).to_degrees();
        assert!((angle + 2.86).abs() < 0.3, "{angle} degrees");
    }

    #[test]
    fn a_faint_region_is_dropped() {
        let (w, h) = (32, 16);
        let mut prob = vec![0f32; w * h];
        for y in 4..10 {
            for x in 4..20 {
                prob[y * w + x] = 0.4;
            }
        }
        assert!(boxes(&prob, w, (h, w), 1.0).is_empty());
    }

    #[test]
    fn lines_read_by_row_then_left_to_right() {
        let r = |x0: f32, y0: f32| Rect {
            cx: x0 + 25.0,
            cy: y0 + 10.0,
            w: 50.0,
            h: 20.0,
            cos: 1.0,
            sin: 0.0,
        };
        let got = reading_order(vec![
            r(300.0, 104.0),
            r(10.0, 40.0),
            r(10.0, 100.0),
            r(200.0, 38.0),
        ]);
        let xs: Vec<(f32, f32)> = got.iter().map(|r| (r.left(), r.top())).collect();
        assert_eq!(
            xs,
            [(10.0, 40.0), (200.0, 38.0), (10.0, 100.0), (300.0, 104.0)]
        );
    }

    #[test]
    fn lines_take_the_narrowest_width_that_fits() {
        assert_eq!(rec_input(&RgbImage::new(200, 48)).0, 320);
        assert_eq!(rec_input(&RgbImage::new(330, 48)).0, 640);
        let (w, x) = rec_input(&RgbImage::new(5000, 48));
        assert_eq!(w, 1280);
        assert_eq!(x.len(), 3 * 48 * 1280);
        // Padding is 0, the black of the line itself -1.
        let (w, x) = rec_input(&RgbImage::new(100, 48));
        assert_eq!((w, x[0], x[319]), (320, -1.0, 0.0));
    }

    #[test]
    fn a_tall_crop_is_turned_to_read_across() {
        let img = RgbImage::new(100, 300);
        let upright = |w: f32, h: f32| Rect {
            cx: 50.0,
            cy: 100.0,
            w,
            h,
            cos: 1.0,
            sin: 0.0,
        };
        assert_eq!(
            crop(&img, upright(30.0, 190.0)).unwrap().dimensions(),
            (190, 30)
        );
        assert!(crop(&img, upright(0.5, 40.0)).is_none());
    }

    #[test]
    fn a_turned_crop_samples_along_the_line() {
        // A red dot at (60, 40); the box centred there and turned 90 degrees
        // has it at its centre, and black (out of the dot) at its ends.
        let img = RgbImage::from_fn(120, 80, |x, y| {
            image::Rgb(if (58..63).contains(&x) && (38..43).contains(&y) {
                [255, 0, 0]
            } else {
                [0, 0, 0]
            })
        });
        let r = Rect {
            cx: 60.5,
            cy: 40.5,
            w: 30.0,
            h: 10.0,
            cos: 0.0,
            sin: 1.0,
        };
        let line = crop(&img, r).unwrap();
        assert_eq!(line.dimensions(), (30, 10));
        assert_eq!(line.get_pixel(15, 5)[0], 255);
        assert_eq!(line.get_pixel(0, 5)[0], 0);
    }

    #[test]
    fn no_page_is_rendered_past_the_scans_deadline() {
        let past = Instant::now() - std::time::Duration::from_secs(1);
        let e = render_page(b"%PDF-1.4", 1, past).unwrap_err().to_string();
        assert!(e.contains("reading the scan"), "{e}");
        assert!(left(Instant::now() + SCAN_WALL).unwrap() > std::time::Duration::from_secs(299));
    }

    #[test]
    fn texture_read_as_a_letter_or_two_is_dropped() {
        // What photos with no text gave, and lines real text gave.
        assert!(!is_text("x", 0.54));
        assert!(!is_text("Il", 0.80));
        assert!(!is_text("a-1", 0.70));
        assert!(!is_text("", 1.0));
        assert!(is_text("Retry", 0.87));
        assert!(!is_text("Retry", 0.6));
        assert!(is_text("Request ID 7f3a9c21", 0.55));
        // Two CJK characters are a word.
        assert!(is_text("出口", 0.9));
    }

    #[test]
    fn an_image_needs_more_than_a_few_words_of_text() {
        let lines = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(text_size(&lines(&["Ooo abc", "Ill nnm", "ww"])) < MIN_IMAGE_TEXT);
        assert!(text_size(&lines(&["Could not connect to db-prod-07"])) >= MIN_IMAGE_TEXT);
        assert_eq!(text_size(&lines(&["出口", "a1"])), 6);
    }

    #[test]
    fn ctc_collapses_repeats_and_drops_blanks() {
        let chars: Vec<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        let classes = chars.len() + 2;
        // a a blank a b space b: "aab b".
        let steps = [1, 1, 0, 1, 2, 3, 2];
        let mut probs = vec![0f32; steps.len() * classes];
        for (i, &c) in steps.iter().enumerate() {
            probs[i * classes + c] = 0.8;
        }
        let (text, score) = ctc(&probs, classes, &chars);
        assert_eq!(text, "aab b");
        assert!((score - 0.8).abs() < 1e-6);
        assert_eq!(ctc(&vec![0.0; classes], classes, &chars).0, "");
    }
}
