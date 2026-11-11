#!/usr/bin/env bash
# Build the spike's test clip (#187): 91 s, sound and picture deliberately mismatched.
#
#     bench/av_spike/make_clip.sh [dir]     (default ~/.cache/vinosearch/bench/av_spike)
#
# Needs espeak-ng, ffmpeg and curl. Downloads ESC-50 clips (CC BY-NC 3.0) and
# Transformers.js demo images; nothing here is committed. Writes mixed.mp4,
# audio.f32 (16 kHz mono float32, what audio_check.py and spike.mjs read),
# frames/ (one every 2 s) and timeline.tsv.
set -euo pipefail
dir=${1:-$HOME/.cache/vinosearch/bench/av_spike}
mkdir -p "$dir/src" "$dir/parts" && cd "$dir"
D=https://huggingface.co/datasets/Xenova/transformers.js-docs/resolve/main
E=https://raw.githubusercontent.com/karolpiczak/ESC-50/master/audio
for f in savanna.jpg sea-turtle.mp4 city-streets.jpg football-match.jpg cats.jpg airport.jpg tiger.jpg; do
  [ -f src/$f ] || curl -sfLo src/$f $D/$f
done
for f in 1-100032-A-0 1-110389-A-0 1-13571-A-46 1-13572-A-46 1-17367-A-10 1-21189-A-10 1-31482-A-42 1-31482-B-42; do
  [ -f src/$f.wav ] || curl -sfLo src/$f.wav $E/$f.wav
done
talk() { espeak-ng -s 150 -w src/$1.raw.wav "$2" && ffmpeg -loglevel error -y -i src/$1.raw.wav -ar 16000 -ac 1 src/$1.wav; }
talk bread "To make sourdough bread, feed your starter the night before. In the morning, mix flour, water and salt with a cup of the bubbly starter, then fold the dough every half hour. Let it rise overnight in the fridge, and bake it in a very hot covered pot."
talk bike "When your bike chain starts skipping, it is probably worn out. Measure it with a chain checker. To replace it, use a chain tool to push out a pin, thread the new chain over the cassette and the front rings, and join it with a quick link. Then lube it and wipe off the extra oil."
talk tax "Remember that the income tax return is due at the end of April. Gather your slips from your employer and the bank, claim your rent and medical receipts, and file online. If you owe money, pay it by the deadline to avoid interest and a late filing penalty."

rm -f timeline.tsv
seg() { # n picture sound...
  local n=$1 vis=$2; shift 2
  local ins=() a; for a in "$@"; do ins+=(-i "src/$a.wav"); done
  ffmpeg -loglevel error -y "${ins[@]}" -filter_complex "concat=n=$#:v=0:a=1,aresample=16000,pan=mono|c0=c0" -ar 16000 -ac 1 parts/a$n.wav
  local d; d=$(ffprobe -v error -show_entries format=duration -of csv=p=0 parts/a$n.wav)
  local vin=(-loop 1 -i "src/$vis"); [[ $vis == *.mp4 ]] && vin=(-stream_loop -1 -i "src/$vis")
  ffmpeg -loglevel error -y "${vin[@]}" -i parts/a$n.wav -t "$d" \
    -vf "scale=640:360:force_original_aspect_ratio=decrease,pad=640:360:(ow-iw)/2:(oh-ih)/2,fps=10,format=yuv420p" \
    -c:v libx264 -preset veryfast -c:a aac -ar 16000 -ac 1 -map 0:v -map 1:a parts/p$n.mp4
  printf "%s\t%s\t%s\t%s\n" "$n" "$d" "$*" "$vis" >> timeline.tsv
}
seg 1 savanna.jpg bread
seg 2 sea-turtle.mp4 1-100032-A-0 1-110389-A-0          # dog
seg 3 city-streets.jpg bike
seg 4 football-match.jpg 1-13571-A-46 1-13572-A-46      # church bells
seg 5 cats.jpg 1-17367-A-10 1-21189-A-10                # rain
seg 6 airport.jpg tax
seg 7 tiger.jpg 1-31482-A-42 1-31482-B-42               # siren
for i in 1 2 3 4 5 6 7; do echo "file p$i.mp4"; done > parts/list.txt
ffmpeg -loglevel error -y -f concat -i parts/list.txt -c copy mixed.mp4
ffmpeg -loglevel error -y -i mixed.mp4 -f f32le -ar 16000 -ac 1 audio.f32
rm -rf frames && mkdir frames && ffmpeg -loglevel error -i mixed.mp4 -vf fps=0.5 frames/f%03d.png
awk -F'\t' 'BEGIN{t=0}{printf "%6.1f-%6.1f  %-40s %s\n", t, t+$2, $3, $4; t+=$2}' timeline.tsv
echo "$dir/mixed.mp4"
