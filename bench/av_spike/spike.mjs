// The #187 spike: one file searched by its vinoWhisper transcript, 10 s audio
// windows, frame windows, and audio and frames fused, with EmbeddingGemma 2
// (onnx-community q8 graphs, CPU, Transformers.js 4.3.2).
//
//     bench/av_spike/make_clip.sh DIR
//     vinowhisper-transcribe DIR/mixed.mp4 -o DIR/vw
//     cd DIR && npm i @huggingface/transformers@4.3.2 && node <repo>/bench/av_spike/spike.mjs > result.json
//
// Reads audio.f32, frames/ and vw/*.vwsession from the working directory;
// downloads ~850 MB of graphs to ./hf-cache. 2 vCPU: about 20 min, 2.8 GB RSS.
// The 2026-11-10 linuxlab run is bench/results/av_spike.json.
import { AutoModel, AutoProcessor, RawImage, RawVideo, env } from "@huggingface/transformers";
import fs from "node:fs";
import { execSync } from "node:child_process";

env.cacheDir = "hf-cache";
const id = "onnx-community/embeddinggemma-2-ONNX";
const t0 = Date.now();
const processor = await AutoProcessor.from_pretrained(id);
const model = await AutoModel.from_pretrained(id, {
  device: "cpu",
  dtype: { model: "q8", vision_encoder: "q8", audio_encoder: "q8" },
});
console.error(`loaded in ${((Date.now() - t0) / 1000).toFixed(1)} s`);
const norm = (v) => { const n = Math.hypot(...v); return v.map((x) => x / n); };
const rows = (t) => { const [b, d] = t.dims; return Array.from({ length: b }, (_, i) => norm(Array.from(t.data.slice(i * d, (i + 1) * d)))); };
const embed = async (...a) => rows((await model(await processor(...a))).sentence_embedding);
const dot = (a, b) => a.reduce((s, x, i) => s + x * b[i], 0);
const timed = async (label, f) => { const t = Date.now(); const r = await f(); timing[label] = (timing[label] ?? 0) + (Date.now() - t) / 1000; return r; };
const timing = {};

const SR = 16000, WIN = 10, HOP = 5, FPS = 0.5;
const buf = fs.readFileSync("audio.f32");
const audio = new Float32Array(buf.buffer, buf.byteOffset, buf.length / 4);
const dur = audio.length / SR;
const frameFiles = fs.readdirSync("frames").sort();
const frames = [];
for (const f of frameFiles) frames.push(await RawImage.read("frames/" + f));

// Windows: audio alone, frames alone, both in one input.
const windows = [];
for (let s = 0; s + 1 < dur; s += HOP) {
  const e = Math.min(s + WIN, dur);
  const clip = audio.slice(Math.floor(s * SR), Math.floor(e * SR));
  const fr = frames.filter((_, i) => i / FPS >= s && i / FPS < e);
  const video = new RawVideo(fr, e - s);
  const [a] = await timed("audio", () => embed(null, null, [clip]));
  const [v] = await timed("frames", () => embed(null, null, null, [video]));
  const [av] = await timed("fused", () => embed(null, null, [clip], [video]));
  windows.push({ s, e, a, v, av, nframes: fr.length });
  console.error(`window ${s}-${e.toFixed(1)}: ${fr.length} frames`);
}

// Transcript paragraphs from the vinoWhisper session.
const bundle = fs.readdirSync("vw").find((f) => f.endsWith(".vwsession"));
const text = execSync(`unzip -p vw/${bundle} transcript.txt`).toString();
const paras = [...text.matchAll(/^\[(\d+):(\d\d)\] (.*)$/gm)].map((m) => ({ s: +m[1] * 60 + +m[2], text: m[3] }));
paras.forEach((p, i) => (p.e = paras[i + 1]?.s ?? dur));
const pv = await timed("transcript", () => embed(paras.map((p) => `title: none | text: ${p.text}`)));
paras.forEach((p, i) => (p.t = pv[i]));

const Q = [
  ["speech", "how do I fix my bicycle drivetrain", 26.1, 44.5],
  ["speech", "baking bread with wild yeast", 0, 16.1],
  ["speech", "when is my tax paperwork due", 64.5, 81],
  ["sound", "a dog barking", 16.1, 26.1],
  ["sound", "church bells ringing", 44.5, 54.5],
  ["sound", "rain falling", 54.5, 64.5],
  ["sound", "an ambulance siren", 81, 91],
  ["sound", "someone talking", 0, 16.1, true],
  ["picture", "a sea turtle swimming", 16.1, 26.1],
  ["picture", "a football match in a stadium", 44.5, 54.5],
  ["picture", "cats sleeping on a couch", 54.5, 64.5],
  ["picture", "a tiger", 81, 91],
  ["picture", "an airport terminal", 64.5, 81],
  ["picture", "zebras and giraffes on the savanna", 0, 16.1],
  ["picture", "a busy city street", 26.1, 44.5],
  ["both", "a tiger with a siren wailing", 81, 91],
];
const qv = await embed(Q.map((q) => `task: search result | query: ${q[1]}`));
// Right if the hit's middle is inside the expected span (speech: any talk counts for "someone talking").
const talk = [[0, 16.1], [26.1, 44.5], [64.5, 81]];
const right = (q, s, e) => { const m = (s + Math.min(e, s + WIN)) / 2; return q[4] ? talk.some(([a, b]) => m >= a && m < b) : m >= q[2] && m < q[3]; };
const fmt = (x) => x.toFixed(3);
const out = [];
for (const [i, q] of Q.entries()) {
  const row = { kind: q[0], query: q[1] };
  for (const [name, items, key] of [["transcript", paras, "t"], ["audio", windows, "a"], ["frames", windows, "v"], ["fused", windows, "av"]]) {
    const scored = items.map((it) => ({ it, sc: dot(qv[i], it[key]) })).sort((x, y) => y.sc - x.sc);
    const best = scored[0];
    const bestRight = scored.find((x) => right(q, x.it.s, x.it.e));
    const bestWrong = scored.find((x) => !right(q, x.it.s, x.it.e));
    row[name] = `${right(q, best.it.s, best.it.e) ? "ok " : "NO "}${fmt(best.sc)} @${best.it.s.toFixed(0)}s (right ${bestRight ? fmt(bestRight.sc) : "-"}, wrong ${bestWrong ? fmt(bestWrong.sc) : "-"})`;
  }
  out.push(row);
}
console.log(JSON.stringify({ timing, windows: windows.length, frames: frames.length, paras: paras.map((p) => [p.s, p.text.slice(0, 40)]), out }, null, 1));
