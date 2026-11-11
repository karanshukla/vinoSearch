//! Sound and picture out of audio and video files (#187, #192), streamed from
//! ffmpeg so a two-hour film is never held in memory. Audio comes as 10 s
//! windows of 16 kHz mono, frames as one per shot. Both start at a given
//! second, so an interrupted file carries on where it stopped.
//!
//! The files come from anywhere, so ffmpeg and ffprobe run under limits, as
//! poppler does for OCR (#106): address space and CPU per process, and a
//! watchdog that kills one that stops producing output.

use crate::ml::audio::{SAMPLE_RATE, WINDOW_SAMPLES};
use anyhow::{Context, Result, bail, ensure};
use image::RgbImage;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Generous for ffmpeg's decoders and its threads' stacks; `-threads 2` keeps
/// the count down.
const ADDRESS_SPACE: u64 = 2 << 30;
/// CPU seconds per second of media, on top of a minute. 4K HEVC decoded in
/// software runs near real time on a small laptop.
const CPU_PER_SECOND: f64 = 2.0;
/// The longest duration the CPU limit allows for, whatever a file claims.
const MAX_SECONDS: f64 = 12.0 * 3600.0;
/// No output for this long and the decoder is killed: a hang, or a FIFO
/// that never fills.
const STALL: Duration = Duration::from_secs(120);
const PROBE_WALL: Duration = Duration::from_secs(30);
/// While ffmpeg is missing it is looked for again this often at most, so the
/// status the settings window polls doesn't walk PATH every time.
const LOOK_AGAIN: Duration = Duration::from_secs(30);

/// A frame at each scene cut over this (#192).
const SCENE: f64 = 0.3;
/// At most one frame per this many seconds, however many cuts.
const MIN_GAP: u32 = 5;
/// At least one frame per this many seconds, in a long shot.
const MAX_GAP: u32 = 30;
/// Frames bigger than this square are shrunk to fit; `vision::prepare`
/// resizes them again for the encoder.
const FRAME_SIDE: u32 = 768;

/// 100 ms frames, and a window is quiet when this share of them are under
/// QUIET_DBFS (#192).
const QUIET_DBFS: f32 = -50.0;
const QUIET_SHARE: f32 = 0.95;
/// Windows start every 5 s, so each moment is whole in one of them. Without
/// the overlap the #187 spike's dog, bells and speech fell across windows
/// and lost to others; with it audio is still ~1.2 NPU min an hour (#190).
const HOP_SAMPLES: usize = 5 * SAMPLE_RATE;
/// A file shorter than this has no window, and at the end a last hop
/// shorter than this adds none. A last hop longer is a window ending at the
/// end: a second of sound padded with nine of silence embeds as little but
/// silence, and outscored every real window in the #187 spike.
const MIN_WINDOW_SAMPLES: usize = SAMPLE_RATE;

/// A media file's frames are numbered from here up by their second; its
/// sound's windows below, by theirs.
pub const FRAME_LINES: usize = 1 << 32;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Track {
    Sound,
    Picture,
}

impl Track {
    /// The track a chunk numbered `n` came from, and its second in it.
    pub fn of(n: i64) -> (Track, i64) {
        if n >= FRAME_LINES as i64 {
            (Track::Picture, n - FRAME_LINES as i64)
        } else {
            (Track::Sound, n)
        }
    }
}

/// `seconds` as a player shows it: 04:05, or 1:02:03 past an hour.
pub fn stamp(seconds: i64) -> String {
    let s = seconds.max(0);
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// ffmpeg and ffprobe, if both are on PATH. Found, they are taken to stay.
/// Missing, they are looked for again every LOOK_AGAIN, so installing them
/// shows without a restart.
pub fn ffmpeg() -> bool {
    static FOUND: Lookup = Lookup::new();
    FOUND.get(Instant::now(), || {
        crate::search::menu::on_path("ffmpeg") && crate::search::menu::on_path("ffprobe")
    })
}

/// Whether something was found: a yes kept for good, a no for LOOK_AGAIN.
struct Lookup {
    found: AtomicBool,
    looked: Mutex<Option<Instant>>,
}

impl Lookup {
    const fn new() -> Self {
        Self {
            found: AtomicBool::new(false),
            looked: Mutex::new(None),
        }
    }

    fn get(&self, now: Instant, look: impl FnOnce() -> bool) -> bool {
        if self.found.load(Ordering::Relaxed) {
            return true;
        }
        let mut looked = self.looked.lock().unwrap();
        if looked.is_some_and(|at| now.saturating_duration_since(at) < LOOK_AGAIN) {
            return false;
        }
        *looked = Some(now);
        let found = look();
        self.found.store(found, Ordering::Relaxed);
        found
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Probe {
    pub seconds: f64,
    pub audio: bool,
    pub video: bool,
}

/// What `path` holds. A cover image in an audio file is an attached picture,
/// not video.
pub fn probe(path: &Path) -> Result<Probe> {
    let mut cmd = limited("ffprobe", PROBE_WALL.as_secs());
    cmd.args(["-v", "error", "-of", "json", "-show_entries"])
        .arg("format=duration:stream=codec_type:stream_disposition=attached_pic")
        .arg(input(path));
    let out = crate::proc::run(cmd, &[], PROBE_WALL).context("ffprobe")?;
    parse_probe(&out)
}

fn parse_probe(json: &[u8]) -> Result<Probe> {
    #[derive(serde::Deserialize)]
    struct Out {
        format: Format,
        #[serde(default)]
        streams: Vec<Stream>,
    }
    #[derive(serde::Deserialize)]
    struct Format {
        duration: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct Stream {
        codec_type: Option<String>,
        #[serde(default)]
        disposition: Disposition,
    }
    #[derive(serde::Deserialize, Default)]
    struct Disposition {
        #[serde(default)]
        attached_pic: u8,
    }
    let out: Out = serde_json::from_slice(json).context("ffprobe's output")?;
    let seconds: f64 = out
        .format
        .duration
        .context("no duration")?
        .parse()
        .context("duration")?;
    ensure!(seconds.is_finite() && seconds > 0.0, "duration {seconds}");
    let has = |kind: &str| {
        out.streams
            .iter()
            .any(|s| s.codec_type.as_deref() == Some(kind) && s.disposition.attached_pic == 0)
    };
    Ok(Probe {
        seconds,
        audio: has("audio"),
        video: has("video"),
    })
}

/// A run of audio: `samples` from second `start`.
pub struct Window {
    pub start: u32,
    pub samples: Vec<f32>,
}

impl Window {
    /// Mostly silence, so not worth an encoder run.
    pub fn quiet(&self) -> bool {
        let tenth = SAMPLE_RATE / 10;
        let frames = self.samples.len() / tenth;
        if frames == 0 {
            return true;
        }
        let floor = 10f32.powf(QUIET_DBFS / 20.0);
        let quiet = self
            .samples
            .chunks_exact(tenth)
            .filter(|f| (f.iter().map(|x| x * x).sum::<f32>() / f.len() as f32).sqrt() < floor)
            .count();
        quiet as f32 >= QUIET_SHARE * frames as f32
    }
}

/// The first audio stream from second `from`, in 10 s windows every 5 s.
pub struct Audio {
    decoder: Decoder,
    from: u32,
    /// The last window given, and where it started, in samples from `from`.
    window: Vec<f32>,
    at: usize,
    done: bool,
}

pub fn audio(path: &Path, from: u32, probe: &Probe) -> Result<Audio> {
    let mut cmd = limited("ffmpeg", cpu_seconds(probe));
    cmd.args(["-nostdin", "-hide_banner", "-v", "error", "-threads", "2"])
        .args(["-ss", &from.to_string()])
        .arg("-i")
        .arg(input(path))
        .args(["-map", "0:a:0", "-vn"])
        // Silence where the stream starts late or has a gap, which the raw
        // output would otherwise close up: windows are timed by counting
        // samples from `from`.
        .args(["-af", "aresample=async=1:first_pts=0"])
        .args(["-f", "f32le", "-ac", "1"])
        .args(["-ar", &SAMPLE_RATE.to_string(), "pipe:1"]);
    Ok(Audio {
        decoder: Decoder::spawn(cmd, STALL, |_| None)?,
        from,
        window: Vec::new(),
        at: 0,
        done: false,
    })
}

impl Audio {
    /// Up to `n` samples; fewer only at the end, when ffmpeg is checked.
    fn read(&mut self, n: usize) -> Result<Vec<f32>> {
        let mut bytes = vec![0u8; n * 4];
        let got = self.decoder.read_up_to(&mut bytes)?;
        if got < bytes.len() {
            self.done = true;
            self.decoder.finish()?;
        }
        Ok(bytes[..got - got % 4]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect())
    }
}

impl Iterator for Audio {
    type Item = Result<Window>;

    fn next(&mut self) -> Option<Result<Window>> {
        if self.done {
            return None;
        }
        if self.window.is_empty() {
            // The first: a whole window, or what there is of a short file.
            let samples = match self.read(WINDOW_SAMPLES) {
                Ok(s) => s,
                Err(e) => return Some(Err(e)),
            };
            if samples.len() < MIN_WINDOW_SAMPLES {
                return None;
            }
            self.window = samples;
        } else {
            // Then HOP on, the last of them filled out from the one before.
            let new = match self.read(HOP_SAMPLES) {
                Ok(s) => s,
                Err(e) => return Some(Err(e)),
            };
            if new.len() < MIN_WINDOW_SAMPLES || self.window.len() < WINDOW_SAMPLES {
                return None;
            }
            self.window.drain(..new.len());
            self.window.extend(new.iter());
            self.at += new.len();
        }
        Some(Ok(Window {
            start: self.from + (self.at / SAMPLE_RATE) as u32,
            samples: self.window.clone(),
        }))
    }
}

/// A frame of video at `second`.
pub struct Frame {
    pub second: f64,
    pub image: RgbImage,
}

/// One frame per shot from second `from`: the first, then one at each cut
/// at least MIN_GAP s after the last kept, and one every MAX_GAP s in a
/// long shot. One decode pass, as in bench/media_chunks.py.
pub struct Frames {
    decoder: Decoder,
    times: mpsc::Receiver<f64>,
    from: u32,
    done: bool,
}

pub fn frames(path: &Path, from: u32, probe: &Probe) -> Result<Frames> {
    let select = format!(
        "select='isnan(prev_selected_t)+gte(t-prev_selected_t\\,{MAX_GAP})\
         +gt(scene\\,{SCENE})*gte(t-prev_selected_t\\,{MIN_GAP})',\
         scale='min({FRAME_SIDE}\\,iw)':'min({FRAME_SIDE}\\,ih)':force_original_aspect_ratio=decrease,\
         format=rgb24,showinfo"
    );
    let mut cmd = limited("ffmpeg", cpu_seconds(probe));
    // rgb24, or a 10-bit source (HEVC Main10, a phone's HDR video) comes as
    // 16-bit PPM. showinfo reports each kept frame's time at the info level.
    cmd.args(["-nostdin", "-hide_banner", "-nostats", "-v", "info"])
        .args(["-threads", "2", "-ss", &from.to_string()])
        .arg("-i")
        .arg(input(path))
        .args(["-map", "0:v:0", "-an", "-sn", "-vf", &select])
        .args(["-fps_mode", "vfr", "-f", "image2pipe", "-c:v", "ppm"])
        .arg("pipe:1");
    let (tx, times) = mpsc::channel();
    let decoder = Decoder::spawn(cmd, STALL, move |line| match showinfo_time(line) {
        Some(t) => {
            let _ = tx.send(t);
            Some(())
        }
        None => None,
    })?;
    Ok(Frames {
        decoder,
        times,
        from,
        done: false,
    })
}

impl Iterator for Frames {
    type Item = Result<Frame>;

    fn next(&mut self) -> Option<Result<Frame>> {
        if self.done {
            return None;
        }
        match self.read() {
            Ok(Some(frame)) => Some(Ok(frame)),
            Ok(None) => {
                self.done = true;
                self.decoder.finish().err().map(Err)
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

impl Frames {
    fn read(&mut self) -> Result<Option<Frame>> {
        let Some((width, height)) = ppm_header(&mut self.decoder)? else {
            return Ok(None);
        };
        ensure!(
            width <= FRAME_SIDE && height <= FRAME_SIDE,
            "a {width}x{height} frame"
        );
        let mut pixels = vec![0u8; width as usize * height as usize * 3];
        ensure!(
            self.decoder.read_up_to(&mut pixels)? == pixels.len(),
            "a frame cut short"
        );
        let t = self
            .times
            .recv_timeout(STALL)
            .context("a frame without a time")?;
        Ok(Some(Frame {
            second: f64::from(self.from) + t,
            image: RgbImage::from_raw(width, height, pixels).context("frame size")?,
        }))
    }
}

/// `P6 <w> <h> 255` and one whitespace byte, or None at the end of the stream.
fn ppm_header(decoder: &mut Decoder) -> Result<Option<(u32, u32)>> {
    let mut fields = Vec::new();
    let mut field = Vec::new();
    while fields.len() < 4 {
        let mut b = [0u8];
        if decoder.read_up_to(&mut b)? == 0 {
            ensure!(fields.is_empty() && field.is_empty(), "a frame cut short");
            return Ok(None);
        }
        if b[0].is_ascii_whitespace() {
            if !field.is_empty() {
                fields.push(std::mem::take(&mut field));
            }
        } else {
            field.push(b[0]);
            ensure!(field.len() <= 8, "not a PPM header");
        }
    }
    let number = |f: &[u8]| -> Result<u32> { Ok(std::str::from_utf8(f)?.parse()?) };
    ensure!(fields[0] == b"P6", "not a PPM frame");
    ensure!(number(&fields[3])? == 255, "not 8-bit");
    Ok(Some((number(&fields[1])?, number(&fields[2])?)))
}

/// `pts_time:12.5` in a showinfo line.
fn showinfo_time(line: &str) -> Option<f64> {
    if !line.contains("Parsed_showinfo") {
        return None;
    }
    let rest = line.split("pts_time:").nth(1)?;
    rest.split_whitespace().next()?.parse().ok()
}

/// A file, never a URL or another protocol, whatever its name.
fn input(path: &Path) -> std::ffi::OsString {
    let mut s = std::ffi::OsString::from("file:");
    s.push(path);
    s
}

/// From the duration the file claims, so held to MAX_SECONDS: an enormous
/// claim would otherwise cast to `u64::MAX`, which is RLIM_INFINITY.
fn cpu_seconds(probe: &Probe) -> u64 {
    60 + (probe.seconds.clamp(0.0, MAX_SECONDS) * CPU_PER_SECOND) as u64
}

/// `tool` with the address space and CPU limits, reading only files.
fn limited(tool: &str, cpu_seconds: u64) -> Command {
    let mut cmd = Command::new(tool);
    cmd.args(["-protocol_whitelist", "file"]);
    // SAFETY: setrlimit is async-signal-safe, and the closure allocates nothing.
    unsafe {
        cmd.pre_exec(move || {
            for (resource, value) in [
                (libc::RLIMIT_AS, ADDRESS_SPACE),
                (libc::RLIMIT_CPU, cpu_seconds),
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

/// A running ffmpeg: its stdout read as needed, its stderr drained on a
/// thread, and a watchdog that kills it when a read has waited `stall` for
/// it. Time nobody is reading doesn't count: the embedder may take minutes
/// over a window, and ffmpeg only waits on the full pipe meanwhile. Dropping
/// it kills the child.
struct Decoder {
    child: Child,
    stdout: ChildStdout,
    stderr: Option<JoinHandle<String>>,
    /// Since when a read has been waiting, in ms from `started`; IDLE while
    /// none is.
    waiting: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    stalled: Arc<AtomicBool>,
    stall: Duration,
    watchdog: Option<JoinHandle<()>>,
    started: Instant,
}

/// `Decoder::waiting` while no read is.
const IDLE: u64 = u64::MAX;

impl Decoder {
    /// `on_line` sees each stderr line; lines it returns None for are kept
    /// for the error message.
    fn spawn(
        mut cmd: Command,
        stall: Duration,
        mut on_line: impl FnMut(&str) -> Option<()> + Send + 'static,
    ) -> Result<Self> {
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("starting ffmpeg")?;
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let started = Instant::now();
        let waiting = Arc::new(AtomicU64::new(IDLE));
        let stop = Arc::new(AtomicBool::new(false));
        let stalled = Arc::new(AtomicBool::new(false));
        let stderr = std::thread::spawn(move || {
            let mut kept: Vec<String> = Vec::new();
            let mut stderr = BufReader::new(stderr);
            let mut line = Vec::new();
            // By bytes: ffmpeg prints a file's tags as they are, and one
            // that isn't UTF-8 must not end the reading.
            while stderr.read_until(b'\n', &mut line).is_ok_and(|n| n > 0) {
                let text = String::from_utf8_lossy(&line);
                let text = text.trim_end_matches(['\n', '\r']);
                if on_line(text).is_none() && !text.trim().is_empty() {
                    kept.push(text.to_owned());
                    if kept.len() > 5 {
                        kept.remove(0);
                    }
                }
                line.clear();
            }
            kept.join("\n")
        });
        let pid = child.id() as libc::pid_t;
        let watchdog = {
            let (waiting, stop, stalled) = (waiting.clone(), stop.clone(), stalled.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(200));
                    let since = waiting.load(Ordering::Relaxed);
                    if since != IDLE
                        && started
                            .elapsed()
                            .saturating_sub(Duration::from_millis(since))
                            > stall
                    {
                        stalled.store(true, Ordering::Relaxed);
                        // SAFETY: the child isn't reaped until this thread is
                        // joined, so the pid is still the child's.
                        unsafe { libc::kill(pid, libc::SIGKILL) };
                        return;
                    }
                }
            })
        };
        Ok(Self {
            child,
            stdout,
            stderr: Some(stderr),
            waiting,
            stop,
            stalled,
            stall,
            watchdog: Some(watchdog),
            started,
        })
    }

    /// Fill `buf` unless the stream ends first; how much was read.
    fn read_up_to(&mut self, buf: &mut [u8]) -> Result<usize> {
        let mut got = 0;
        let mut result = Ok(());
        while got < buf.len() {
            // The stall clock starts with each read, and stops below.
            self.waiting
                .store(self.started.elapsed().as_millis() as u64, Ordering::Relaxed);
            match self.stdout.read(&mut buf[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        self.waiting.store(IDLE, Ordering::Relaxed);
        result?;
        Ok(got)
    }

    /// At the end of stdout: Err unless ffmpeg exited cleanly.
    fn finish(&mut self) -> Result<()> {
        self.stop_watchdog();
        let status = self.child.wait()?;
        let err = self
            .stderr
            .take()
            .map(|t| t.join().unwrap_or_default())
            .unwrap_or_default();
        if self.stalled.load(Ordering::Relaxed) {
            bail!(
                "ffmpeg gave nothing for {} s and was stopped",
                self.stall.as_secs()
            );
        }
        if !status.success() {
            bail!("ffmpeg failed ({status}): {}", err.trim());
        }
        Ok(())
    }

    fn stop_watchdog(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(w) = self.watchdog.take() {
            let _ = w.join();
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        self.stop_watchdog();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 25 s from ffmpeg's own sources: red for 12 s then blue, a 440 Hz tone
    /// with silence from 10 to 20 s. None if ffmpeg isn't installed.
    fn clip(name: &str, video: bool, audio: bool) -> Option<std::path::PathBuf> {
        if !ffmpeg() {
            eprintln!("no ffmpeg; skipped");
            return None;
        }
        let dir = std::env::temp_dir().join(format!("vinosearch-media-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-v", "error", "-y"]);
        if video {
            cmd.args(["-f", "lavfi", "-i", "color=red:s=320x240:r=10:d=12"]);
            cmd.args(["-f", "lavfi", "-i", "color=blue:s=320x240:r=10:d=13"]);
        }
        if audio {
            cmd.args(["-f", "lavfi", "-i", "sine=f=440:r=16000:d=25"]);
        }
        let mut graph = Vec::new();
        if video {
            graph.push("[0:v][1:v]concat=n=2:v=1:a=0[v]".to_string());
        }
        if audio {
            let a = if video { 2 } else { 0 };
            graph.push(format!(
                "[{a}:a]volume='if(between(t,10,20),0,0.5)':eval=frame[a]"
            ));
        }
        cmd.args(["-filter_complex", &graph.join(";")]);
        if video {
            cmd.args(["-map", "[v]", "-c:v", "libx264", "-pix_fmt", "yuv420p"]);
        }
        if audio {
            cmd.args(["-map", "[a]", "-c:a", "aac"]);
        }
        let ok = cmd.arg(&path).status().unwrap().success();
        assert!(ok, "making {name}");
        Some(path)
    }

    #[test]
    fn a_probe_says_what_streams_there_are() {
        let Some(both) = clip("probe-both.mp4", true, true) else {
            return;
        };
        let p = probe(&both).unwrap();
        assert!((p.seconds - 25.0).abs() < 0.2, "{p:?}");
        assert!(p.audio && p.video);
        let silent = clip("probe-silent.mp4", true, false).unwrap();
        assert!(!probe(&silent).unwrap().audio);
        let sound = clip("probe-sound.m4a", false, true).unwrap();
        assert!(!probe(&sound).unwrap().video);
    }

    #[test]
    fn a_cover_picture_isnt_video() {
        let json = br#"{"format":{"duration":"181.4"},"streams":[{"codec_type":"audio","disposition":{"attached_pic":0}},{"codec_type":"video","disposition":{"attached_pic":1}}]}"#;
        let p = parse_probe(json).unwrap();
        assert_eq!((p.audio, p.video), (true, false));
        assert!(parse_probe(br#"{"format":{}}"#).is_err());
        assert!(parse_probe(br#"{"format":{"duration":"N/A"}}"#).is_err());
    }

    #[test]
    fn audio_comes_in_ten_second_windows_and_silence_is_quiet() {
        let Some(path) = clip("audio.mp4", true, true) else {
            return;
        };
        let p = probe(&path).unwrap();
        let windows: Vec<Window> = audio(&path, 0, &p).unwrap().map(|w| w.unwrap()).collect();
        let starts: Vec<u32> = windows.iter().map(|w| w.start).collect();
        assert_eq!(starts, [0, 5, 10, 15]);
        assert!(windows.iter().all(|w| w.samples.len() == WINDOW_SAMPLES));
        // Only 10 to 20 s is all silence.
        let quiet: Vec<bool> = windows.iter().map(Window::quiet).collect();
        assert_eq!(quiet, [false, false, true, false]);

        let resumed: Vec<u32> = audio(&path, 10, &p)
            .unwrap()
            .map(|w| w.unwrap().start)
            .collect();
        assert_eq!(resumed, [10, 15]);
    }

    #[test]
    fn a_file_without_sound_gives_an_error_not_windows() {
        let Some(path) = clip("silent.mp4", true, false) else {
            return;
        };
        let p = probe(&path).unwrap();
        let got: Vec<_> = audio(&path, 0, &p).unwrap().collect();
        assert_eq!(got.len(), 1);
        assert!(got[0].is_err());
    }

    #[test]
    fn frames_come_one_per_shot() {
        let Some(path) = clip("frames.mp4", true, true) else {
            return;
        };
        let p = probe(&path).unwrap();
        let frames: Vec<Frame> = frames(&path, 0, &p).unwrap().map(|f| f.unwrap()).collect();
        let seconds: Vec<f64> = frames.iter().map(|f| f.second).collect();
        assert_eq!(seconds.len(), 2, "{seconds:?}");
        assert!(
            seconds[0] < 0.2 && (seconds[1] - 12.0).abs() < 0.2,
            "{seconds:?}"
        );
        assert_eq!(frames[0].image.dimensions(), (320, 240));
        let red = frames[0].image.get_pixel(160, 120);
        let blue = frames[1].image.get_pixel(160, 120);
        assert!(red[0] > 200 && red[2] < 60, "{red:?}");
        assert!(blue[2] > 200 && blue[0] < 60, "{blue:?}");

        let resumed: Vec<f64> = super::frames(&path, 15, &p)
            .unwrap()
            .map(|f| f.unwrap().second)
            .collect();
        assert_eq!(resumed.len(), 1, "{resumed:?}");
        assert!((resumed[0] - 15.0).abs() < 0.2, "{resumed:?}");
    }

    #[test]
    fn a_long_shot_gets_a_frame_every_thirty_seconds() {
        if !ffmpeg() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("vinosearch-media-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("long.mp4");
        let ok = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("testsrc2=s=160x120:r=5:d=65")
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .arg(&path)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let p = probe(&path).unwrap();
        let seconds: Vec<f64> = frames(&path, 0, &p)
            .unwrap()
            .map(|f| f.unwrap().second)
            .collect();
        assert_eq!(seconds.len(), 3, "{seconds:?}");
        assert!((seconds[1] - 30.0).abs() < 0.5 && (seconds[2] - 60.0).abs() < 0.5);
    }

    #[test]
    fn a_claimed_duration_cant_lift_the_cpu_limit() {
        let claim = |seconds| Probe {
            seconds,
            audio: true,
            video: true,
        };
        let most = 60 + (MAX_SECONDS * CPU_PER_SECOND) as u64;
        assert_eq!(cpu_seconds(&claim(1e300)), most);
        assert_eq!(cpu_seconds(&claim(f64::INFINITY)), most);
        assert_eq!(cpu_seconds(&claim(10.0)), 80);
    }

    #[test]
    fn a_name_is_never_read_as_a_url() {
        assert_eq!(
            input(Path::new("http://example.com/a.mp4")),
            std::ffi::OsString::from("file:http://example.com/a.mp4")
        );
    }

    #[test]
    fn a_decoder_that_hangs_is_killed() {
        // Not through sh: dash forks the sleep, which then holds the pipe
        // open after sh is killed. ffmpeg doesn't fork.
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let started = Instant::now();
        let mut d = Decoder::spawn(cmd, Duration::from_secs(1), |_| None).unwrap();
        assert_eq!(d.read_up_to(&mut [0u8; 16]).unwrap(), 0);
        let err = d.finish().unwrap_err().to_string();
        assert!(err.contains("gave nothing"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// A scratch directory for clips made by one test.
    fn scratch() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("vinosearch-media-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn ten_bit_video_gives_eight_bit_frames() {
        if !ffmpeg() {
            return;
        }
        let path = scratch().join("ten-bit.mkv");
        let made = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("color=red:s=160x120:r=5:d=3")
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p10le"])
            .arg(&path)
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        if !made {
            eprintln!("this ffmpeg has no 10-bit encoder; skipped");
            return;
        }
        let p = probe(&path).unwrap();
        let frames: Vec<Frame> = frames(&path, 0, &p).unwrap().map(|f| f.unwrap()).collect();
        assert_eq!(frames.len(), 1);
        let red = frames[0].image.get_pixel(80, 60);
        assert!(red[0] > 200 && red[2] < 60, "{red:?}");
    }

    #[test]
    fn a_tag_that_isnt_utf8_doesnt_lose_the_frames() {
        use std::os::unix::ffi::OsStrExt;
        if !ffmpeg() {
            return;
        }
        // Latin-1 "Café", as an old tagger wrote it; ffmpeg prints it as is.
        let path = scratch().join("latin1.mkv");
        let ok = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("color=red:s=160x120:r=5:d=3")
            .arg("-metadata")
            .arg(std::ffi::OsStr::from_bytes(b"title=Caf\xe9 \xff\xfe"))
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .arg(&path)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let p = probe(&path).unwrap();
        let seconds: Vec<f64> = frames(&path, 0, &p)
            .unwrap()
            .map(|f| f.unwrap().second)
            .collect();
        assert_eq!(seconds.len(), 1, "{seconds:?}");
    }

    #[test]
    fn sound_that_starts_late_is_timed_from_the_start_of_the_file() {
        if !ffmpeg() {
            return;
        }
        // 20 s of picture, a tone from 12 s on.
        let path = scratch().join("late-sound.mkv");
        let ok = Command::new("ffmpeg")
            .args(["-v", "error", "-y"])
            .args(["-f", "lavfi", "-i", "color=red:s=64x48:r=5:d=20"])
            .args(["-itsoffset", "12", "-f", "lavfi", "-i"])
            .arg("sine=f=440:r=16000:d=8")
            .args(["-map", "0:v", "-map", "1:a", "-c:v", "libx264"])
            .args(["-pix_fmt", "yuv420p", "-c:a", "aac"])
            .arg(&path)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let p = probe(&path).unwrap();
        let windows: Vec<Window> = audio(&path, 0, &p).unwrap().map(|w| w.unwrap()).collect();
        let starts: Vec<u32> = windows.iter().map(|w| w.start).collect();
        assert_eq!(starts, [0, 5, 10]);
        // Nothing to hear until 12 s: the first window is the silence
        // before it, not the tone moved up to 0.
        let quiet: Vec<bool> = windows.iter().map(Window::quiet).collect();
        assert_eq!(quiet, [true, false, false]);
        let tenth = SAMPLE_RATE / 10;
        let loud = |w: &Window, second: usize| {
            w.samples[second * SAMPLE_RATE..][..tenth]
                .iter()
                .any(|x| x.abs() > 0.05)
        };
        // The window from 5 s hears it from its seventh second on.
        assert!(!loud(&windows[1], 6) && loud(&windows[1], 8));

        // From a second past its start, nothing is padded.
        let resumed: Vec<bool> = audio(&path, 14, &p)
            .unwrap()
            .map(|w| w.unwrap().quiet())
            .collect();
        assert_eq!(resumed, [false]);
    }

    #[test]
    fn a_decoder_nobody_is_reading_isnt_taken_for_hung() {
        let mut d =
            Decoder::spawn(Command::new("yes"), Duration::from_millis(600), |_| None).unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(d.read_up_to(&mut buf).unwrap(), 16);
        // The embedder busy with a window for longer than the stall.
        std::thread::sleep(Duration::from_millis(1500));
        assert!(!d.stalled.load(Ordering::Relaxed));
        assert_eq!(d.read_up_to(&mut buf).unwrap(), 16);
        assert_eq!(&buf[..4], b"y\ny\n");
    }

    #[test]
    fn ffmpeg_is_looked_for_again_only_while_missing_and_not_too_often() {
        let found = Lookup::new();
        let t0 = Instant::now();
        let mut looks = 0;
        let mut get = |after: u64, there: bool| {
            found.get(t0 + Duration::from_secs(after), || {
                looks += 1;
                there
            })
        };
        assert!(!get(0, false));
        // Installed a moment later: not seen until LOOK_AGAIN has passed.
        assert!(!get(10, true));
        assert!(get(31, true));
        // Found: never looked for again.
        assert!(get(32, false));
        assert!(get(3600, false));
        assert_eq!(looks, 2);
    }

    /// The #187 spike's clip, seven segments with a cut between each
    /// (bench/av_spike/make_clip.sh), which isn't checked in:
    /// `VINOSEARCH_AV_SPIKE=~/.cache/vinosearch/bench/av_spike cargo test -- --ignored spike`
    #[test]
    #[ignore = "needs the spike's clip"]
    fn the_spike_clip_gives_a_frame_per_segment() {
        let dir = std::path::PathBuf::from(std::env::var("VINOSEARCH_AV_SPIKE").unwrap());
        let path = dir.join("mixed.mp4");
        let p = probe(&path).unwrap();
        let seconds: Vec<f64> = frames(&path, 0, &p)
            .unwrap()
            .map(|f| f.unwrap().second)
            .collect();
        eprintln!("frames at {seconds:?}");
        let want = [0.0, 16.1, 26.1, 44.5, 54.5, 64.5, 81.0];
        assert_eq!(seconds.len(), want.len(), "{seconds:?}");
        for (got, want) in seconds.iter().zip(want) {
            assert!((got - want).abs() < 0.3, "{seconds:?}");
        }
        let windows: Vec<u32> = audio(&path, 0, &p)
            .unwrap()
            .map(|w| w.unwrap().start)
            .collect();
        // 91 s: every 5 s to 80, then the last 10 s.
        let mut want: Vec<u32> = (0..17).map(|i| i * 5).collect();
        want.push(81);
        assert_eq!(windows, want);
    }

    #[test]
    fn chunks_say_their_track_and_second() {
        assert_eq!(Track::of(30), (Track::Sound, 30));
        assert_eq!(Track::of(FRAME_LINES as i64 + 12), (Track::Picture, 12));
        assert_eq!(stamp(0), "00:00");
        assert_eq!(stamp(245), "04:05");
        assert_eq!(stamp(3723), "1:02:03");
    }

    #[test]
    fn showinfo_lines_give_times() {
        let line = "[Parsed_showinfo_1 @ 0x55] n:   1 pts:  12000 pts_time:12      duration:100";
        assert_eq!(showinfo_time(line), Some(12.0));
        assert_eq!(showinfo_time("[mp4 @ 0x1] pts_time:3"), None);
    }
}
