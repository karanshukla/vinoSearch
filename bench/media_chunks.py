"""How many audio windows and frames each way of chunking a media file sends to the encoders (#187, #192).

    uv run --python 3.13 --no-project --with numpy bench/media_chunks.py FILE... [--audio-s S]

Needs ffmpeg. Nothing is embedded: this counts what would be, per hour of
media, and turns frames into NPU time at 1.3 s a frame (140 soft tokens,
measured for images, src/ml/vision.rs) and audio windows at --audio-s
seconds each once that's measured on the NPU (#190).

Audio is cut into windows; a window is quiet when 95% of its 100 ms frames
are under -50 dBFS, and quiet windows are skipped. Frames are taken per shot:
ffmpeg's scene score (decoded at 160 px wide) over THRESHOLD is a cut, a frame
is kept at each cut at most one every MIN_GAP s, and inside a shot longer than
MAX_GAP s one every MAX_GAP s. Hashing frames (dHash) was tried first and kept
1,200 to 1,600 frames an hour of film: camera moves and animation change too
many bits. Shots don't have that problem.

The app can take the same frames in one decode pass, at the encoder's size:

    ffmpeg -an -i FILE -vf "select='isnan(prev_selected_t)+gte(t-prev_selected_t\\,30)
        +gt(scene\\,0.3)*gte(t-prev_selected_t\\,5)',scale=768:-2" -fps_mode vfr out%04d.jpg

which gave the same counts as below (Sintel: 60 frames for 14.8 min).
"""

import subprocess
import sys
import time

import numpy as np

SR = 16000
QUIET_DB, QUIET_SHARE = -50.0, 0.95
NPU_FRAME_S = 1.3
THRESHOLD = 0.3
MAX_GAP = 30.0


def decode(args):
    return subprocess.run(["ffmpeg", "-loglevel", "error", *args, "-"], capture_output=True, check=True).stdout


def audio(path):
    raw = decode(["-i", path, "-vn", "-f", "f32le", "-ar", str(SR), "-ac", "1"])
    return np.frombuffer(raw, np.float32)


def cuts(path):
    """Times of the scene cuts, in seconds."""
    vf = f"scale=160:-2,select='gt(scene,{THRESHOLD})',metadata=print:file=-"
    out = decode(["-i", path, "-an", "-vf", vf, "-f", "null"]).decode()
    return [float(l.split("pts_time:")[1].split()[0]) for l in out.splitlines() if "pts_time:" in l]


def windows(wave, win, hop, skip_quiet):
    frames = len(wave) // (SR // 10)
    db = 20 * np.log10(np.sqrt((wave[: frames * (SR // 10)].reshape(frames, -1) ** 2).mean(1)) + 1e-10)
    kept = quiet = 0
    for start in np.arange(0, len(wave) / SR - 1, hop):
        f = db[int(start * 10): int((start + win) * 10)]
        if skip_quiet and (f < QUIET_DB).mean() >= QUIET_SHARE:
            quiet += 1
            continue
        kept += 1
    return kept, quiet


def per_shot(cut_times, duration, min_gap):
    """Frames kept: one at the start and at each cut, at most one per min_gap s, and one per MAX_GAP s in a long shot."""
    kept, last = 1, 0.0
    for t in [*cut_times, duration]:
        while t - last > MAX_GAP:
            last += MAX_GAP
            kept += 1
        if t < duration and t - last >= min_gap:
            kept, last = kept + 1, t
    return kept


def main():
    args = sys.argv[1:]
    audio_s = None
    if "--audio-s" in args:
        i = args.index("--audio-s")
        audio_s = float(args[i + 1])
        del args[i: i + 2]
    for path in args:
        t = time.time()
        wave = audio(path)
        t_audio = time.time() - t
        t = time.time()
        c = cuts(path)
        t_frames = time.time() - t
        duration = len(wave) / SR
        hours = duration / 3600
        print(f"\n{path}: {len(wave) / SR / 60:.1f} min; decode {t_audio:.1f} s audio, {t_frames:.1f} s frames (CPU)")
        print("  per hour of media                    chunks    NPU min")
        for name, win, hop, skip in [
            ("audio 10 s every 5 s (spike)", 10, 5, False),
            ("audio 10 s, no overlap", 10, 10, False),
            ("audio 10 s, no overlap, skip quiet", 10, 10, True),
            ("audio 30 s, no overlap, skip quiet", 30, 30, True),
        ]:
            kept, quiet = windows(wave, win, hop, skip)
            n = kept / hours
            npu = f"{n * audio_s * win / 10 / 60:9.1f}" if audio_s else "        ?"
            extra = f"  ({quiet / hours:.0f} quiet skipped)" if skip else ""
            print(f"  {name:36} {n:6.0f} {npu}{extra}")
        for name, n in [
            ("frames 1 every 2 s (spike)", duration / 2),
            ("frames 1 every 10 s", duration / 10),
            *[(f"frames per shot, {g:.0f} s apart or more", per_shot(c, duration, g)) for g in (2, 5, 10)],
        ]:
            print(f"  {name:36} {n / hours:6.0f} {n / hours * NPU_FRAME_S / 60:9.1f}")


if __name__ == "__main__":
    main()
