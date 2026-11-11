//! Audio windows to the audio encoder's input (#187): log-mel features as
//! transformers' `Gemma4AudioFeatureExtractor` makes them, for the static 10 s
//! graph (#190). 128 HTK mels from 0 to 8 kHz over the magnitude of a 512-point
//! FFT, 320-sample periodic Hann frames every 160 samples, the first centred
//! on sample 0, then `ln(mel + 0.001)`. No preemphasis, dither or per-bin
//! normalisation. `bench/log_mel_ref.py` writes the reference.

use anyhow::{Result, ensure};
use std::f64::consts::PI;
use std::sync::OnceLock;

pub const SAMPLE_RATE: usize = 16_000;
/// The longest window the static encoder takes.
pub const WINDOW_SAMPLES: usize = 10 * SAMPLE_RATE;
/// Frames in the static encoder's input, `valid_frames(WINDOW_SAMPLES)`.
pub const FRAMES: usize = 999;
pub const MELS: usize = 128;

const FRAME: usize = 320;
const HOP: usize = 160;
const FFT: usize = 512;
const BINS: usize = FFT / 2 + 1;
const MEL_FLOOR: f64 = 0.001;
const MAX_HZ: f64 = 8000.0;

/// One window's encoder input: `FRAMES` rows of `MELS`, the rows past
/// `frames` zero, as the extractor leaves padded frames.
pub struct Features {
    pub mel: Vec<f32>,
    pub frames: usize,
}

impl Features {
    /// `input_features_mask`.
    pub fn mask(&self) -> Vec<bool> {
        (0..FRAMES).map(|i| i < self.frames).collect()
    }

    /// The rows of `audio_features` that are real: two stride-2 convolutions.
    pub fn tokens(&self) -> usize {
        self.frames.div_ceil(2).div_ceil(2)
    }
}

/// Frames the extractor counts as real audio. The first starts half a frame
/// before sample 0, and a frame is real only when the sample just past it is.
fn valid_frames(samples: usize) -> usize {
    samples.saturating_sub(1) / HOP
}

/// Up to `WINDOW_SAMPLES` of 16 kHz mono audio.
pub fn log_mel(samples: &[f32]) -> Result<Features> {
    ensure!(
        samples.len() <= WINDOW_SAMPLES,
        "{} samples, more than a {WINDOW_SAMPLES}-sample window",
        samples.len()
    );
    // A float file can hold NaN or infinity, which would go through every
    // bin to the encoder and come back as no embedding at all.
    ensure!(
        samples.iter().all(|x| x.is_finite()),
        "samples that are not numbers"
    );
    let t = tables();
    let frames = valid_frames(samples.len());
    let mut mel = vec![0f32; FRAMES * MELS];
    let mut re = [0f64; FFT];
    let mut im = [0f64; FFT];
    let mut mag = [0f64; BINS];
    for (i, row) in mel
        .as_chunks_mut::<MELS>()
        .0
        .iter_mut()
        .take(frames)
        .enumerate()
    {
        let start = (i * HOP) as isize - (FRAME / 2) as isize;
        re.fill(0.0);
        im.fill(0.0);
        for (n, w) in t.window.iter().enumerate() {
            let at = start + n as isize;
            if at >= 0 {
                // In f32, as the extractor windows.
                re[n] = f64::from(samples[at as usize] * w);
            }
        }
        fft(&mut re, &mut im, &t.twiddles);
        for (k, m) in mag.iter_mut().enumerate() {
            *m = re[k].hypot(im[k]);
        }
        for (j, out) in row.iter_mut().enumerate() {
            let energy: f64 = t.filters[j]
                .iter()
                .map(|&(k, weight)| mag[k] * weight)
                .sum();
            *out = (energy + MEL_FLOOR).ln() as f32;
        }
    }
    Ok(Features { mel, frames })
}

struct Tables {
    window: [f32; FRAME],
    twiddles: Vec<(f64, f64)>,
    /// Per mel, its nonzero (bin, weight) pairs.
    filters: Vec<Vec<(usize, f64)>>,
}

fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| Tables {
        window: std::array::from_fn(|n| {
            (0.5 - 0.5 * (2.0 * PI * n as f64 / FRAME as f64).cos()) as f32
        }),
        twiddles: (0..FFT / 2)
            .map(|k| {
                let a = -2.0 * PI * k as f64 / FFT as f64;
                (a.cos(), a.sin())
            })
            .collect(),
        filters: filters(),
    })
}

/// `mel_filter_bank(257, 128, 0, 8000, 16000, norm=None, mel_scale="htk")`:
/// triangles on HTK mel points, over bins spaced linearly from 0 to 8 kHz.
fn filters() -> Vec<Vec<(usize, f64)>> {
    let to_mel = |hz: f64| 2595.0 * (1.0 + hz / 700.0).log10();
    let to_hz = |mel: f64| 700.0 * (10f64.powf(mel / 2595.0) - 1.0);
    let points: Vec<f64> = linspace(to_mel(0.0), to_mel(MAX_HZ), MELS + 2)
        .into_iter()
        .map(to_hz)
        .collect();
    let bins = linspace(0.0, (SAMPLE_RATE / 2) as f64, BINS);
    (0..MELS)
        .map(|m| {
            let (lo, mid, hi) = (points[m], points[m + 1], points[m + 2]);
            bins.iter()
                .enumerate()
                .filter_map(|(k, &hz)| {
                    let w = ((hz - lo) / (mid - lo)).min((hi - hz) / (hi - mid));
                    (w > 0.0).then_some((k, w))
                })
                .collect()
        })
        .collect()
}

/// numpy's: `start + i * step`, with the last point exactly `stop`.
fn linspace(start: f64, stop: f64, n: usize) -> Vec<f64> {
    let step = (stop - start) / (n - 1) as f64;
    let mut v: Vec<f64> = (0..n).map(|i| start + i as f64 * step).collect();
    v[n - 1] = stop;
    v
}

/// In-place iterative radix-2 FFT of `FFT` points.
fn fft(re: &mut [f64; FFT], im: &mut [f64; FFT], twiddles: &[(f64, f64)]) {
    let bits = FFT.trailing_zeros();
    for i in 0..FFT {
        let j = i.reverse_bits() >> (usize::BITS - bits);
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= FFT {
        let stride = FFT / len;
        for start in (0..FFT).step_by(len) {
            for k in 0..len / 2 {
                let (wr, wi) = twiddles[k * stride];
                let (a, b) = (start + k, start + k + len / 2);
                let (xr, xi) = (re[b] * wr - im[b] * wi, re[b] * wi + im[b] * wr);
                re[b] = re[a] - xr;
                im[b] = im[a] - xi;
                re[a] += xr;
                im[a] += xi;
            }
        }
        len *= 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_f32(bytes: &[u8]) -> Vec<f32> {
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect()
    }

    fn worst(got: &[f32], want: &[f32]) -> f32 {
        assert_eq!(got.len(), want.len());
        got.iter()
            .zip(want)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f32::max)
    }

    /// `fixture()` in bench/log_mel_ref.py.
    fn fixture() -> Vec<f32> {
        (0..8011)
            .map(|i| {
                if (3000..4600).contains(&i) {
                    return 0.0;
                }
                let t = i as f64 / SAMPLE_RATE as f64;
                (0.5 * (2.0 * PI * 440.0 * t).sin()
                    + 0.25 * (2.0 * PI * 3000.0 * t).sin()
                    + 0.1 * (2.0 * PI * (200.0 + 3000.0 * t) * t).sin()) as f32
            })
            .collect()
    }

    #[test]
    fn a_full_window_is_the_static_encoders_input() {
        assert_eq!(valid_frames(WINDOW_SAMPLES), FRAMES);
        let f = log_mel(&vec![0.0; WINDOW_SAMPLES]).unwrap();
        assert_eq!((f.frames, f.tokens()), (FRAMES, 250));
    }

    #[test]
    fn short_windows_count_frames_and_tokens_as_the_extractor_does() {
        // 6 s, from audio_check.py's "siren 6 s": 599 frames, 150 tokens.
        let f = log_mel(&vec![0.0; 6 * SAMPLE_RATE]).unwrap();
        assert_eq!((f.frames, f.tokens()), (599, 150));
        assert_eq!(f.mask().iter().filter(|&&m| m).count(), 599);
        assert!(f.mel[599 * MELS..].iter().all(|&x| x == 0.0));
        assert_eq!(log_mel(&[0.0; 160]).unwrap().frames, 0);
        assert_eq!(log_mel(&[0.0; 161]).unwrap().frames, 1);
    }

    #[test]
    fn more_than_a_window_is_refused() {
        assert!(log_mel(&vec![0.0; WINDOW_SAMPLES + 1]).is_err());
    }

    #[test]
    fn samples_that_are_not_numbers_are_refused() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut samples = vec![0.1; SAMPLE_RATE];
            samples[777] = bad;
            assert!(log_mel(&samples).is_err(), "{bad}");
        }
    }

    #[test]
    fn matches_transformers_on_the_fixture() {
        let want = read_f32(include_bytes!("audio/fixture.f32"));
        let f = log_mel(&fixture()).unwrap();
        assert_eq!(f.frames * MELS, want.len());
        let w = worst(&f.mel[..want.len()], &want);
        assert!(w < 1e-4, "worst bin off by {w}");
    }

    /// The #187 acceptance check, on the spike's clip, which isn't checked in
    /// (ESC-50 is CC BY-NC). bench/av_spike/make_clip.sh, then
    /// bench/log_mel_ref.py on its audio.f32, then:
    /// `VINOSEARCH_AV_SPIKE=~/.cache/vinosearch/bench/av_spike cargo test -- --ignored spike`
    #[test]
    #[ignore = "needs the spike's clip"]
    fn matches_transformers_on_the_spike_clip() {
        let dir = std::path::PathBuf::from(std::env::var("VINOSEARCH_AV_SPIKE").unwrap());
        let wave = read_f32(&std::fs::read(dir.join("audio.f32")).unwrap());
        let want = read_f32(&std::fs::read(dir.join("log_mel.f32")).unwrap());
        let windows: Vec<_> = wave.chunks(WINDOW_SAMPLES).collect();
        assert_eq!(windows.len() * FRAMES * MELS, want.len());
        for (i, (w, r)) in windows.iter().zip(want.chunks(FRAMES * MELS)).enumerate() {
            let got = log_mel(w).unwrap();
            let off = worst(&got.mel, r);
            eprintln!(
                "window {i}: {} frames, worst bin off by {off:.2e}",
                got.frames
            );
            assert!(off < 1e-4, "window {i}: worst bin off by {off}");
        }
    }
}
