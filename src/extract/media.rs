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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Generous for ffmpeg's decoders and its threads' stacks; `-threads 2` keeps
/// the count down.
const ADDRESS_SPACE: u64 = 2 << 30;
/// CPU seconds per second of media, on top of a minute. 4K HEVC decoded in
/// software runs near real time on a small laptop.
const CPU_PER_SECOND: f64 = 2.0;
/// No output for this long and the decoder is killed: a hang, or a FIFO
/// that never fills.
const STALL: Duration = Duration::from_secs(120);
const PROBE_WALL: Duration = Duration::from_secs(30);

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
/// A last window shorter than this is dropped.
const MIN_WINDOW_SAMPLES: usize = SAMPLE_RATE;

/// ffmpeg and ffprobe, if both are on PATH. Looked up once.
pub fn ffmpeg() -> bool {
    static FOUND: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FOUND.get_or_init(|| {
        crate::search::menu::on_path("ffmpeg") && crate::search::menu::on_path("ffprobe")
    })
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

/// The first audio stream from second `from`, in 10 s windows with no
/// overlap.
pub struct Audio {
    decoder: Decoder,
    next: u32,
    done: bool,
}

pub fn audio(path: &Path, from: u32, probe: &Probe) -> Result<Audio> {
    let mut cmd = limited("ffmpeg", cpu_seconds(probe));
    cmd.args(["-nostdin", "-hide_banner", "-v", "error", "-threads", "2"])
        .args(["-ss", &from.to_string()])
        .arg("-i")
        .arg(input(path))
        .args(["-map", "0:a:0", "-vn", "-f", "f32le", "-ac", "1"])
        .args(["-ar", &SAMPLE_RATE.to_string(), "pipe:1"]);
    Ok(Audio {
        decoder: Decoder::spawn(cmd, STALL, |_| None)?,
        next: from,
        done: false,
    })
}

impl Iterator for Audio {
    type Item = Result<Window>;

    fn next(&mut self) -> Option<Result<Window>> {
        if self.done {
            return None;
        }
        let mut bytes = vec![0u8; WINDOW_SAMPLES * 4];
        let got = match self.decoder.read_up_to(&mut bytes) {
            Ok(n) => n,
            Err(e) => {
                self.done = true;
                return Some(Err(e));
            }
        };
        if got < bytes.len() {
            self.done = true;
            if let Err(e) = self.decoder.finish() {
                return Some(Err(e));
            }
        }
        let samples: Vec<f32> = bytes[..got - got % 4]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        if samples.len() < MIN_WINDOW_SAMPLES {
            return None;
        }
        let start = self.next;
        self.next += (WINDOW_SAMPLES / SAMPLE_RATE) as u32;
        Some(Ok(Window { start, samples }))
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
         scale='min({FRAME_SIDE}\\,iw)':'min({FRAME_SIDE}\\,ih)':force_original_aspect_ratio=decrease,showinfo"
    );
    let mut cmd = limited("ffmpeg", cpu_seconds(probe));
    // showinfo reports each kept frame's time at the info level.
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

fn cpu_seconds(probe: &Probe) -> u64 {
    60 + (probe.seconds * CPU_PER_SECOND) as u64
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
/// thread, and a watchdog that kills it when nothing comes out for `stall`.
/// Dropping it kills the child.
struct Decoder {
    child: Child,
    stdout: ChildStdout,
    stderr: Option<JoinHandle<String>>,
    progress: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    stalled: Arc<AtomicBool>,
    stall: Duration,
    watchdog: Option<JoinHandle<()>>,
    started: Instant,
}

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
        let progress = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let stalled = Arc::new(AtomicBool::new(false));
        let stderr = std::thread::spawn(move || {
            let mut kept = Vec::new();
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if on_line(&line).is_none() && !line.trim().is_empty() {
                    kept.push(line);
                    if kept.len() > 5 {
                        kept.remove(0);
                    }
                }
            }
            kept.join("\n")
        });
        let pid = child.id() as libc::pid_t;
        let watchdog = {
            let (progress, stop, stalled) = (progress.clone(), stop.clone(), stalled.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(200));
                    let last = Duration::from_millis(progress.load(Ordering::Relaxed));
                    if started.elapsed().saturating_sub(last) > stall {
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
            progress,
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
        while got < buf.len() {
            match self.stdout.read(&mut buf[got..]) {
                Ok(0) => break,
                Ok(n) => {
                    got += n;
                    self.progress
                        .store(self.started.elapsed().as_millis() as u64, Ordering::Relaxed);
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
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
        assert_eq!(starts, [0, 10, 20]);
        assert_eq!(windows[0].samples.len(), WINDOW_SAMPLES);
        let quiet: Vec<bool> = windows.iter().map(Window::quiet).collect();
        assert_eq!(quiet, [false, true, false]);

        let resumed: Vec<u32> = audio(&path, 10, &p)
            .unwrap()
            .map(|w| w.unwrap().start)
            .collect();
        assert_eq!(resumed, [10, 20]);
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
    fn a_name_is_never_read_as_a_url() {
        assert_eq!(
            input(Path::new("http://example.com/a.mp4")),
            std::ffi::OsString::from("file:http://example.com/a.mp4")
        );
    }

    #[test]
    fn a_decoder_that_hangs_is_killed() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 30"]);
        let started = Instant::now();
        let mut d = Decoder::spawn(cmd, Duration::from_secs(1), |_| None).unwrap();
        assert_eq!(d.read_up_to(&mut [0u8; 16]).unwrap(), 0);
        let err = d.finish().unwrap_err().to_string();
        assert!(err.contains("gave nothing"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10));
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
        assert_eq!(windows, (0..10).map(|i| i * 10).collect::<Vec<_>>());
    }

    #[test]
    fn showinfo_lines_give_times() {
        let line = "[Parsed_showinfo_1 @ 0x55] n:   1 pts:  12000 pts_time:12      duration:100";
        assert_eq!(showinfo_time(line), Some(12.0));
        assert_eq!(showinfo_time("[mp4 @ 0x1] pts_time:3"), None);
    }
}
