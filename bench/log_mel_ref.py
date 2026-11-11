"""Reference log-mel features from transformers' Gemma4AudioFeatureExtractor, for src/ml/audio.rs (#187).

    uv run --python 3.13 --no-project --with numpy --with transformers python -I \
        bench/log_mel_ref.py <audio dir> fixture src/ml/audio/fixture.f32
    uv run ... bench/log_mel_ref.py <audio dir> <clips.f32> <out.f32>

<audio dir> holds preprocessor_config.json (see #190). `fixture` writes the
features of the synthetic signal audio.rs's tests build (FIXTURE below, which
they must match), valid frames only. Otherwise <clips.f32> (16 kHz mono
float32) is cut into 10 s windows with no overlap, the last one short, and
each window's 999 x 128 features are written in turn, padded rows zero, as
the static encoder takes them. Little-endian float32 throughout.
"""

import sys
from pathlib import Path

import numpy as np
from transformers import AutoFeatureExtractor

SR, FRAMES = 16000, 999
WINDOW = 10 * SR


def fixture():
    """Half a second: two tones, a chirp, then a silent gap. Built in f64, rounded to f32."""
    n = 8011
    t = np.arange(n, dtype=np.float64) / SR
    x = 0.5 * np.sin(2 * np.pi * 440 * t) + 0.25 * np.sin(2 * np.pi * 3000 * t) \
        + 0.1 * np.sin(2 * np.pi * (200 + 3000 * t) * t)
    x[3000:4600] = 0
    return x.astype(np.float32)


def features(fe, wave):
    f = fe([wave], sampling_rate=SR, return_tensors="np")
    x, m = f["input_features"][0], f["input_features_mask"][0].astype(bool)
    return x[:m.sum()].astype(np.float32)


def main():
    fe = AutoFeatureExtractor.from_pretrained(Path(sys.argv[1]))
    src, out = sys.argv[2], Path(sys.argv[3])
    if src == "fixture":
        x = features(fe, fixture())
        x.astype("<f4").tofile(out)
        print(f"{out}: {x.shape[0]} frames")
        return
    wave = np.fromfile(src, np.float32)
    rows = []
    for s in range(0, len(wave), WINDOW):
        x = features(fe, wave[s:s + WINDOW])
        padded = np.zeros((FRAMES, x.shape[1]), np.float32)
        padded[:len(x)] = x
        rows.append(padded)
    np.stack(rows).astype("<f4").tofile(out)
    print(f"{out}: {len(rows)} windows")


if __name__ == "__main__":
    main()
