//! Opening a result where it matched (#105): a text file at its line in the
//! editor, a PDF at its page in the viewer. KRunner, GNOME Shell and
//! `vinosearch menu` all open through here.
//!
//! The app is the `editor` setting for text (a command, or an app's desktop
//! entry id as the settings app's picker saves it), else the default app for
//! the file's type (`xdg-mime query default`), whose desktop file says what
//! to run. Only apps known to take a line or page get one; anything else, a
//! terminal app or a Flatpak included, gets `xdg-open` as before. Every
//! command is an argv, never a shell line.
//!
//! A vinoWhisper session (#143) is a zip no editor opens: it goes to
//! vinoWhisper's settings window, or without vinoWhisper, the part that
//! matched is copied out to a private file and that is opened.
//!
//! An audio or video file (#188) plays from the second that matched, in the
//! `player` setting or the default app when it is a known player.
//!
//! In the Flatpak all of it runs on the host, through `flatpak-spawn
//! --host`: the apps, their desktop entries and xdg-mime's defaults are the
//! host's, and the OpenURI portal could open a file but not at a line, page
//! or second.

use crate::extract::session::{self, Part};
use crate::index::Kind;
use anyhow::{Context, Result};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Where in its file a result matched.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum At {
    /// A text file's line, from 1.
    Line(u32),
    /// A PDF's page, counted from 1 in document order (an index, not a
    /// printed page label).
    Page(u32),
    /// A vinoWhisper session's part, at a line of it (0 for the top).
    Session(Part, u32),
    /// An audio or video file's second, from 0.
    Second(u32),
}

impl At {
    /// Where a hit on `path` at chunk number `line` is, if an app can be told.
    /// An HTML page's lines are of its extracted text, and a document's
    /// numbers other than a PDF's page are slides, rows or chapters, which
    /// no common app takes from the command line.
    pub fn of(kind: Kind, path: &Path, line: i64) -> Option<At> {
        if kind == Kind::Session {
            let (part, n) = Part::of(line);
            return Some(At::Session(part, u32::try_from(n).unwrap_or(0)));
        }
        if kind == Kind::Media {
            let (_, second) = crate::extract::media::Track::of(line);
            return Some(At::Second(u32::try_from(second).unwrap_or(0)));
        }
        let n = u32::try_from(line).ok().filter(|&n| n > 0)?;
        match kind {
            Kind::Text if !crate::extract::is_html(path) => Some(At::Line(n)),
            Kind::Code => Some(At::Line(n)),
            Kind::Document if crate::extract::unit(path) == "page" => Some(At::Page(n)),
            _ => None,
        }
    }
}

/// How an app takes a line or page.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Style {
    /// `--page N FILE`
    Okular,
    /// `--page-index=N FILE` (Evince, Papers). `-p` is a page label.
    PageIndex,
    /// `-P N FILE`
    Zathura,
    /// `--goto FILE:L`
    VsCode,
    /// `--line L FILE` (Kate, KWrite, JetBrains IDEs)
    LineFlag,
    /// `FILE +L:1`. A bare `+L` leaves the column unset, and 50.1 then
    /// opens a blank view or aborts.
    GnomeTextEditor,
    /// `+L FILE`
    PlusLine,
    /// `FILE:L`
    FileColonLine,
}

const JETBRAINS: &[&str] = &[
    "idea",
    "idea64",
    "pycharm",
    "clion",
    "goland",
    "webstorm",
    "phpstorm",
    "rubymine",
    "rider",
    "rustrover",
    "datagrip",
    "dataspell",
    "aqua",
];

/// The style of the program `program` (a name or path), by its file name.
fn style(program: &str) -> Option<Style> {
    let name = Path::new(program).file_name()?.to_str()?;
    let name = name.strip_suffix(".sh").unwrap_or(name);
    Some(match name {
        "okular" => Style::Okular,
        "evince" | "papers" => Style::PageIndex,
        "zathura" => Style::Zathura,
        "code" | "code-oss" | "code-insiders" | "codium" | "vscodium" => Style::VsCode,
        "kate" | "kwrite" => Style::LineFlag,
        "gnome-text-editor" => Style::GnomeTextEditor,
        "gedit" => Style::PlusLine,
        "zed" | "zeditor" | "zedit" | "zed-editor" => Style::FileColonLine,
        n if JETBRAINS.contains(&n) => Style::LineFlag,
        _ => return None,
    })
}

/// The arguments after the program that open `file` at `at`, if `style`
/// takes that kind of place.
fn args(style: Style, file: &Path, at: At) -> Option<Vec<String>> {
    let f = file.display().to_string();
    Some(match (style, at) {
        (Style::Okular, At::Page(n)) => vec!["--page".into(), n.to_string(), f],
        (Style::PageIndex, At::Page(n)) => vec![format!("--page-index={n}"), f],
        (Style::Zathura, At::Page(n)) => vec!["-P".into(), n.to_string(), f],
        (Style::VsCode, At::Line(l)) => vec!["--goto".into(), format!("{f}:{l}")],
        (Style::LineFlag, At::Line(l)) => vec!["--line".into(), l.to_string(), f],
        (Style::GnomeTextEditor, At::Line(l)) => vec![f, format!("+{l}:1")],
        (Style::PlusLine, At::Line(l)) => vec![format!("+{l}"), f],
        (Style::FileColonLine, At::Line(l)) => vec![format!("{f}:{l}")],
        _ => return None,
    })
}

/// The command for a setting like `editor` or `player`: a template when it
/// has `{file}` (each word on its own, every `fields` placeholder replaced
/// too), else the command with what `place` gives for its program, else the
/// command with the file alone. Words are split on whitespace, never through
/// a shell, so a path with spaces or quotes stays one argument.
fn setting_argv(
    setting: &str,
    file: &Path,
    fields: &[(&str, String)],
    place: impl FnOnce(&str) -> Option<Vec<String>>,
) -> Option<Vec<String>> {
    let words: Vec<&str> = setting.split_whitespace().collect();
    let program = *words.first()?;
    let f = file.display().to_string();
    if words.iter().any(|w| w.contains("{file}")) {
        let fill = |w: &str| {
            let w = fields
                .iter()
                .fold(w.to_owned(), |w, (key, value)| w.replace(key, value));
            w.replace("{file}", &f)
        };
        return Some(words.iter().map(|w| fill(w)).collect());
    }
    let mut argv: Vec<String> = words.iter().map(|&w| w.to_owned()).collect();
    argv.extend(place(program).unwrap_or_else(|| vec![f]));
    Some(argv)
}

/// The command for the `editor` setting: a template with `{file}` and
/// `{line}`, else a known editor with its line flag, else the command with
/// the file and no line.
fn editor_argv(editor: &str, file: &Path, line: u32) -> Option<Vec<String>> {
    setting_argv(editor, file, &[("{line}", line.to_string())], |program| {
        args(style(program)?, file, At::Line(line))
    })
}

/// A desktop file's `Exec` and `Terminal`.
#[derive(Debug, PartialEq)]
struct Entry {
    exec: Vec<String>,
    terminal: bool,
}

/// `Exec` and `Terminal` from the `[Desktop Entry]` group of `text`.
fn parse_entry(text: &str) -> Option<Entry> {
    let mut in_entry = false;
    let (mut exec, mut terminal) = (None, false);
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "Exec" => exec = Some(value.trim()),
            "Terminal" => terminal = value.trim() == "true",
            _ => {}
        }
    }
    Some(Entry {
        exec: exec_words(&unescape(exec?))?,
        terminal,
    })
}

/// A desktop file string's escapes (`\s`, `\n`, `\t`, `\r`, `\\`).
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(o) => out.push(o),
            None => out.push('\\'),
        }
    }
    out
}

/// `Exec`'s words, quotes removed and the file field codes dropped. None for
/// a field code inside a word (`--file=%f`), which this doesn't fill in.
fn exec_words(exec: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let (mut quoted, mut started) = (false, false);
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '\\' if quoted => word.push(chars.next()?),
            c if c.is_whitespace() && !quoted => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if quoted {
        return None;
    }
    if started {
        words.push(word);
    }
    let mut out = Vec::new();
    for w in words {
        match w.as_str() {
            "%f" | "%F" | "%u" | "%U" | "%i" | "%c" | "%k" => continue,
            _ if w.replace("%%", "").contains('%') => return None,
            _ => out.push(w.replace("%%", "%")),
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The command a default app's desktop entry gives for `file` at `at`: its
/// `Exec` without field codes, then the app's own arguments. None for a
/// terminal app or one not known to take this kind of place (a Flatpak's
/// `Exec` is `flatpak run ...`, so it is one of those).
fn app_argv(entry: &Entry, file: &Path, at: At) -> Option<Vec<String>> {
    if entry.terminal {
        return None;
    }
    let a = args(style(&entry.exec[0])?, file, at)?;
    Some(entry.exec.iter().cloned().chain(a).collect())
}

/// The desktop entry with id `id` (`org.kde.okular.desktop`), first found.
fn find_entry(id: &str) -> Option<Entry> {
    if id.contains('/') || !id.ends_with(".desktop") {
        return None;
    }
    vinosearch_shared::apps::data_dirs().iter().find_map(|d| {
        let text = std::fs::read_to_string(d.join("applications").join(id)).ok()?;
        parse_entry(&text)
    })
}

/// One line of `xdg-mime`'s output, if it ran and printed one.
fn xdg_mime(args: &[&std::ffi::OsStr]) -> Option<String> {
    let out = host_command("xdg-mime")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let line = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && !line.is_empty() && !line.contains('\n')).then_some(line)
}

/// The default app's desktop entry for `file` at `at`.
fn default_entry(file: &Path, at: At) -> Option<Entry> {
    let mime = match at {
        At::Page(_) => "application/pdf".to_owned(),
        At::Line(_) => xdg_mime(&["query".as_ref(), "filetype".as_ref(), file.as_os_str()])?,
        At::Session(..) | At::Second(_) => return None,
    };
    default_app(&mime)
}

/// The default app's desktop entry for `mime`. A text subtype with no
/// default of its own (`text/rust`) takes text/plain's, as xdg-open does.
fn default_app(mime: &str) -> Option<Entry> {
    let default = |m: &str| xdg_mime(&["query".as_ref(), "default".as_ref(), m.as_ref()]);
    let id = match default(mime) {
        Some(id) => id,
        None if mime.starts_with("text/") => default("text/plain")?,
        None => return None,
    };
    find_entry(&id)
}

/// The command a picked editor's entry gives: at the line when it is an app
/// known to take one, else the file alone, as the app's own `Exec`. None for
/// a terminal app.
fn picked_argv(entry: &Entry, file: &Path, at: At) -> Option<Vec<String>> {
    if entry.terminal {
        return None;
    }
    app_argv(entry, file, at).or_else(|| Some(exec_with_file(entry, file)))
}

/// An entry's `Exec` with `file` where its `%U` was: inside a Flatpak's
/// `@@u ... @@`, which hands it to the sandbox, else at the end.
fn exec_with_file(entry: &Entry, file: &Path) -> Vec<String> {
    let mut argv = entry.exec.clone();
    let f = file.display().to_string();
    let opened = argv.iter().position(|w| w == "@@u" || w == "@@f");
    match opened.and_then(|o| argv[o..].iter().position(|w| w == "@@").map(|c| o + c)) {
        Some(close) => argv.insert(close, f),
        None => argv.push(f),
    }
    argv
}

/// The command that opens `file` at `at`, if anything known can.
fn argv_at(file: &Path, at: At, editor: Option<&str>) -> Option<Vec<String>> {
    if let (At::Line(l), Some(e)) = (at, editor.filter(|e| !e.trim().is_empty())) {
        if vinosearch_shared::apps::is_entry_id(e) {
            return picked_argv(&find_entry(e.trim())?, file, at);
        }
        return editor_argv(e, file, l);
    }
    app_argv(&default_entry(file, at)?, file, at)
}

/// A media player known to start a file at a time (#188), by how it takes it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Player {
    /// `--start=S`
    Mpv,
    /// `--start-time=S`, parsed with `us_atof`, so a decimal point in any locale.
    Vlc,
    /// `--mpv-start=S`, which Celluloid hands to mpv as `--start`.
    Celluloid,
}

/// The player `program` (a name or path) is, by its file name. Haruna, Totem
/// and Showtime (#188) aren't here because none takes a start time, read
/// from their sources on 2026-11-11: Haruna's only option is
/// `--ytdl-format-selection`, Showtime's is `--new-window`, and Totem
/// declares a hidden `--seek` that nothing reads. They get the file alone.
fn player(program: &str) -> Option<Player> {
    Some(match Path::new(program).file_name()?.to_str()? {
        "mpv" => Player::Mpv,
        "vlc" => Player::Vlc,
        "celluloid" => Player::Celluloid,
        _ => return None,
    })
}

/// `seconds`, 0 when negative or not a number.
fn clamped(seconds: f64) -> f64 {
    if seconds.is_finite() {
        seconds.max(0.0)
    } else {
        0.0
    }
}

/// `seconds` to the millisecond, without trailing zeros (`754`, `754.5`). All
/// three known players take a decimal, and a window can start mid-second.
fn decimal_seconds(seconds: f64) -> String {
    let s = format!("{:.3}", clamped(seconds));
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// `seconds` rounded down to a whole second, for a template's `{seconds}`:
/// its program is unknown and may not take a decimal, and rounding down never
/// skips past the matched words.
fn whole_seconds(seconds: f64) -> String {
    // Truncating is the floor here, as the value is never negative.
    (clamped(seconds) as u64).to_string()
}

/// The option that starts `player` at `seconds`.
fn start_flag(player: Player, seconds: f64) -> String {
    let s = decimal_seconds(seconds);
    match player {
        Player::Mpv => format!("--start={s}"),
        Player::Vlc => format!("--start-time={s}"),
        Player::Celluloid => format!("--mpv-start={s}"),
    }
}

/// The command a player's desktop entry gives for `file` at `seconds`, if it
/// is a known player that isn't a terminal app (a Flatpak's `Exec` is
/// `flatpak run ...`, so it isn't one). The start flag goes before a `--`,
/// as mpv's desktop file has, after which it would be taken for a file.
fn entry_play_argv(entry: &Entry, file: &Path, seconds: f64) -> Option<Vec<String>> {
    if entry.terminal {
        return None;
    }
    let flag = start_flag(player(&entry.exec[0])?, seconds);
    let mut argv = entry.exec.clone();
    let at = argv.iter().position(|w| w == "--").unwrap_or(argv.len());
    argv.insert(at, flag);
    argv.push(file.display().to_string());
    Some(argv)
}

/// The command for the `player` setting: an app's desktop entry id, a
/// template with `{file}` and `{seconds}` (whole seconds), a known player
/// with its start flag, or any other command with the file and no time.
/// None when blank, or for a desktop entry id not found or run in a terminal.
fn player_argv(setting: &str, file: &Path, seconds: f64) -> Option<Vec<String>> {
    if vinosearch_shared::apps::is_entry_id(setting) {
        let entry = find_entry(setting.trim())?;
        if entry.terminal {
            return None;
        }
        return entry_play_argv(&entry, file, seconds)
            .or_else(|| Some(exec_with_file(&entry, file)));
    }
    let fields = [("{seconds}", whole_seconds(seconds))];
    setting_argv(setting, file, &fields, |program| {
        let flag = start_flag(player(program)?, seconds);
        Some(vec![flag, file.display().to_string()])
    })
}

/// The command that plays the media `file` from `seconds` in (#188): the
/// `player` setting, else the default app for the file's type when it is a
/// known player, else `xdg-open`, which can't seek. Looking up the default
/// app runs `xdg-mime`, so call this off the bus.
pub fn play_argv(file: &Path, seconds: f64, player: Option<&str>) -> Vec<String> {
    player
        .filter(|p| !p.trim().is_empty())
        .and_then(|p| player_argv(p, file, seconds))
        .or_else(|| {
            let mime = xdg_mime(&["query".as_ref(), "filetype".as_ref(), file.as_os_str()])?;
            entry_play_argv(&default_app(&mime)?, file, seconds)
        })
        .unwrap_or_else(|| vec!["xdg-open".into(), file.display().to_string()])
}

/// `argv` in a scope of its own when this runs as a systemd unit, so the app
/// isn't in the service's cgroup and killed when the service restarts.
fn detached(argv: Vec<String>, in_unit: bool) -> Vec<String> {
    if !in_unit {
        return argv;
    }
    [
        "systemd-run",
        "--user",
        "--scope",
        "--collect",
        "--quiet",
        "--",
    ]
    .into_iter()
    .map(String::from)
    .chain(argv)
    .collect()
}

/// `program`, on the host when this runs in the Flatpak.
fn host_command(program: &str) -> Command {
    let argv = host_argv(vec![program.to_owned()]);
    let mut c = Command::new(&argv[0]);
    c.args(&argv[1..]);
    c
}

/// `argv` run on the host when this is the Flatpak. flatpak-spawn passes
/// none of the sandbox's environment, so the app gets the session's own.
fn host_argv(argv: Vec<String>) -> Vec<String> {
    in_sandbox_argv(argv, vinosearch_shared::sandbox::flatpak_id().is_some())
}

fn in_sandbox_argv(argv: Vec<String>, sandboxed: bool) -> Vec<String> {
    if !sandboxed {
        return argv;
    }
    ["flatpak-spawn", "--host"]
        .into_iter()
        .map(String::from)
        .chain(argv)
        .collect()
}

fn in_unit() -> bool {
    std::env::var_os("INVOCATION_ID").is_some() && crate::search::menu::on_path("systemd-run")
}

/// `KEY=value` entries as pairs, skipping any without a `=`.
fn env_pairs(entries: Vec<String>) -> Vec<(String, String)> {
    entries
        .iter()
        .filter_map(|entry| entry.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

/// The user manager's environment now. A unit keeps the one it started with,
/// which lacks `WAYLAND_DISPLAY` and the like when it started before the
/// graphical session. Blocks on the bus, so call this off the runner's.
fn manager_env() -> zbus::Result<Vec<(String, String)>> {
    zbus::block_on(async {
        let bus = zbus::Connection::session().await?;
        let manager = zbus_systemd::systemd1::ManagerProxy::new(&bus).await?;
        Ok(env_pairs(manager.environment().await?))
    })
}

/// Start `argv` for the desktop, and reap it when it exits. Blocks on the bus
/// in a unit, so call this off the runner's. In a process group of its own,
/// so a signal to the runner's (Ctrl+C on a `vinosearch krunner` in a
/// terminal) leaves it, and its output out of the runner's log; outside a
/// unit nothing else ties it to the runner, which can exit or crash under it.
pub fn spawn(argv: Vec<String>) -> std::io::Result<()> {
    let in_unit = in_unit();
    let env = if in_unit {
        manager_env().unwrap_or_else(|e| {
            eprintln!("reading the user manager's environment: {e}");
            Vec::new()
        })
    } else {
        Vec::new()
    };
    let argv = host_argv(detached(argv, in_unit));
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// Open `file` at `at` in the editor, player or default app, else with
/// `xdg-open`. Looking up the default app runs `xdg-mime`, so call this off
/// the bus.
pub fn open(file: &Path, at: Option<At>, editor: Option<&str>, player: Option<&str>) {
    if crate::extract::is_session(file) {
        let (part, line) = match at {
            Some(At::Session(part, line)) => (part, line),
            // From `vinosearch menu` or the search window, which pass a line.
            Some(At::Line(line)) => (Part::Transcript, line),
            _ => (Part::Transcript, 0),
        };
        if let Err(e) = open_session(file, part, line, editor) {
            eprintln!("opening {}: {e:#}", file.display());
        }
        return;
    }
    let argv = match at {
        Some(At::Second(s)) => Some(play_argv(file, f64::from(s), player)),
        Some(at) => argv_at(file, at, editor),
        None => None,
    };
    if let Some(argv) = argv {
        let program = argv[0].clone();
        match spawn(argv) {
            Ok(()) => return,
            Err(e) => eprintln!("{program}: {e}; opening with xdg-open"),
        }
    }
    xdg_open(file);
}

pub fn xdg_open(target: &Path) {
    if let Err(e) = spawn(vec!["xdg-open".into(), target.display().to_string()]) {
        eprintln!("xdg-open {}: {e}", target.display());
    }
}

/// vinoWhisper's settings window, which opens a session's part (and the
/// transcript at a `[MM:SS]`); KRunner's PATH may lack ~/.local/bin, where
/// its setup puts it.
fn vinowhisper_gui() -> Option<PathBuf> {
    const GUI: &str = "vinowhisper-gui";
    if vinosearch_shared::sandbox::flatpak_id().is_some() {
        // The host's /usr, where a package would put it, is under /run/host.
        let usr = Path::new("/usr/bin").join(GUI);
        if Path::new("/run/host")
            .join(usr.strip_prefix("/").unwrap())
            .is_file()
        {
            return Some(usr);
        }
    } else if crate::search::menu::on_path(GUI) {
        return Some(PathBuf::from(GUI));
    }
    let local = PathBuf::from(std::env::var_os("HOME")?)
        .join(".local/bin")
        .join(GUI);
    local.is_file().then_some(local)
}

/// What follows the bundle's path for vinoWhisper (vinoWhisper#123):
/// `transcript`, `transcript@MM:SS` or `summary`.
fn fragment(part: Part, stamp: Option<&str>) -> String {
    match (part, stamp) {
        (Part::Summary, _) => "summary".into(),
        (Part::Transcript, Some(s)) => format!("transcript@{s}"),
        (Part::Transcript, None) => "transcript".into(),
    }
}

fn session_argv(gui: &Path, file: &Path, part: Part, stamp: Option<&str>) -> Vec<String> {
    vec![
        gui.display().to_string(),
        "--page".into(),
        "sessions".into(),
        "--open".into(),
        format!("{}#{}", file.display(), fragment(part, stamp)),
    ]
}

fn open_session(file: &Path, part: Part, line: u32, editor: Option<&str>) -> Result<()> {
    let bytes = session::load(file)?;
    let text = session::text(&bytes, part)?;
    if let Some(gui) = vinowhisper_gui() {
        let stamp = match part {
            Part::Transcript if line > 0 => session::stamp_at(&text, line as usize),
            _ => None,
        };
        spawn(session_argv(&gui, file, part, stamp.as_deref()))?;
        return Ok(());
    }
    let copy = copy_out(file, part, &text)?;
    open(&copy, (line > 0).then_some(At::Line(line)), editor, None);
    Ok(())
}

/// `text`, `file`'s `part`, in a file only this user can read, under
/// $XDG_RUNTIME_DIR (memory, gone at logout), never /tmp. Copies from
/// before are deleted first: each open leaves at most one behind. In the
/// Flatpak, under its `app/<id>` directory there, which the host sees at the
/// same path.
fn copy_out(file: &Path, part: Part, text: &str) -> Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let mut runtime =
        PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").context("no XDG_RUNTIME_DIR")?);
    if let Some(app) = vinosearch_shared::sandbox::flatpak_id() {
        runtime = runtime.join("app").join(app);
    }
    let dir = runtime.join("vinosearch/sessions");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    for old in std::fs::read_dir(&dir)?.flatten() {
        let _ = std::fs::remove_file(old.path());
    }
    let stem = file
        .file_stem()
        .map_or("session".into(), |s| s.to_string_lossy());
    let copy = dir.join(match part {
        Part::Transcript => format!("{stem} transcript.txt"),
        Part::Summary => format!("{stem} summary.md"),
    });
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&copy)?
        .write_all(text.as_bytes())?;
    Ok(copy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn argv(entry: &str, file: &str, at: At) -> Option<Vec<String>> {
        app_argv(&parse_entry(entry).unwrap(), Path::new(file), at)
    }

    fn entry(exec: &str) -> String {
        format!("[Desktop Entry]\nName=X\nExec={exec}\n")
    }

    #[test]
    fn where_a_hit_opens() {
        let p = |s: &str| PathBuf::from(s);
        assert_eq!(At::of(Kind::Text, &p("/a.rs"), 12), Some(At::Line(12)));
        assert_eq!(At::of(Kind::Text, &p("/a.html"), 12), None);
        assert_eq!(At::of(Kind::Document, &p("/a.PDF"), 3), Some(At::Page(3)));
        assert_eq!(At::of(Kind::Document, &p("/a.pptx"), 3), None);
        assert_eq!(At::of(Kind::Document, &p("/a.srt"), 3), None);
        assert_eq!(At::of(Kind::Image, &p("/a.jpg"), 0), None);
        assert_eq!(
            At::of(Kind::Media, &p("/a.mkv"), 754),
            Some(At::Second(754))
        );
        assert_eq!(At::of(Kind::Media, &p("/a.mkv"), 0), Some(At::Second(0)));
        let frame = crate::extract::media::FRAME_LINES as i64 + 12;
        assert_eq!(
            At::of(Kind::Media, &p("/a.mkv"), frame),
            Some(At::Second(12))
        );
        assert_eq!(At::of(Kind::Text, &p("/a.rs"), 0), None);
        assert_eq!(
            At::of(Kind::Session, &p("/s.vwsession"), 7),
            Some(At::Session(Part::Transcript, 7))
        );
        assert_eq!(
            At::of(
                Kind::Session,
                &p("/s.vwsession"),
                session::SUMMARY_LINES as i64 + 2
            ),
            Some(At::Session(Part::Summary, 2))
        );
    }

    #[test]
    fn a_session_opens_in_vinowhisper_at_its_stamp() {
        let gui = Path::new("/home/u/.local/bin/vinowhisper-gui");
        let file = Path::new("/d/#1 2026-10-27-140512.vwsession");
        assert_eq!(
            session_argv(gui, file, Part::Transcript, Some("61:05")),
            [
                "/home/u/.local/bin/vinowhisper-gui",
                "--page",
                "sessions",
                "--open",
                "/d/#1 2026-10-27-140512.vwsession#transcript@61:05"
            ]
        );
        assert_eq!(
            session_argv(gui, file, Part::Summary, Some("01:00"))[4],
            "/d/#1 2026-10-27-140512.vwsession#summary"
        );
        assert_eq!(
            session_argv(gui, file, Part::Transcript, None)[4],
            "/d/#1 2026-10-27-140512.vwsession#transcript"
        );
    }

    #[test]
    fn a_part_copied_out_is_private_and_replaces_the_last() {
        use std::os::unix::fs::PermissionsExt;
        let runtime = std::env::temp_dir().join(format!("vs-copy-{}", std::process::id()));
        // SAFETY: no other test reads XDG_RUNTIME_DIR.
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", &runtime) };
        let file = Path::new("/d/2026-10-27-140512.vwsession");
        let first = copy_out(file, Part::Transcript, "one").unwrap();
        let second = copy_out(file, Part::Summary, "two").unwrap();
        assert!(!first.exists());
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "two");
        assert!(second.ends_with("2026-10-27-140512 summary.md"));
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&second), 0o600);
        assert_eq!(mode(second.parent().unwrap()), 0o700);
        std::fs::remove_dir_all(&runtime).unwrap();
    }

    #[test]
    fn pdf_viewers_open_at_the_page() {
        let at = At::Page(47);
        assert_eq!(
            argv(&entry("okular %U"), "/d/a b.pdf", at).unwrap(),
            ["okular", "--page", "47", "/d/a b.pdf"]
        );
        assert_eq!(
            argv(&entry("evince %U"), "/a.pdf", at).unwrap(),
            ["evince", "--page-index=47", "/a.pdf"]
        );
        assert_eq!(
            argv(&entry("papers %U"), "/a.pdf", at).unwrap(),
            ["papers", "--page-index=47", "/a.pdf"]
        );
        assert_eq!(
            argv(&entry("/usr/bin/zathura %U"), "/a.pdf", at).unwrap(),
            ["/usr/bin/zathura", "-P", "47", "/a.pdf"]
        );
    }

    #[test]
    fn editors_open_at_the_line() {
        let at = At::Line(812);
        let cases: &[(&str, &[&str])] = &[
            (
                "/usr/share/code/code %F",
                &["/usr/share/code/code", "--goto", "/a.rs:812"],
            ),
            (
                "/snap/bin/code --force-user-env %F",
                &["/snap/bin/code", "--force-user-env", "--goto", "/a.rs:812"],
            ),
            ("kate -b %U", &["kate", "-b", "--line", "812", "/a.rs"]),
            (
                "gnome-text-editor %U",
                &["gnome-text-editor", "/a.rs", "+812:1"],
            ),
            ("gedit %U", &["gedit", "+812", "/a.rs"]),
            ("zed %U", &["zed", "/a.rs:812"]),
            (
                "\"/home/u/.local/share/JetBrains/Toolbox/apps/pycharm/bin/pycharm.sh\" %u",
                &[
                    "/home/u/.local/share/JetBrains/Toolbox/apps/pycharm/bin/pycharm.sh",
                    "--line",
                    "812",
                    "/a.rs",
                ],
            ),
        ];
        for (exec, want) in cases {
            assert_eq!(argv(&entry(exec), "/a.rs", at).unwrap(), *want, "{exec}");
        }
    }

    #[test]
    fn anything_else_falls_back_to_xdg_open() {
        // A viewer asked for a line, an editor for a page.
        assert_eq!(argv(&entry("okular %U"), "/a.rs", At::Line(3)), None);
        assert_eq!(argv(&entry("kate %U"), "/a.pdf", At::Page(3)), None);
        // Unknown apps, LibreOffice, Flatpaks and terminal apps.
        assert_eq!(argv(&entry("mupdf %f"), "/a.pdf", At::Page(3)), None);
        assert_eq!(
            argv(&entry("libreoffice --draw %U"), "/a.pdf", At::Page(3)),
            None
        );
        let flatpak = "/usr/bin/flatpak run --branch=stable --arch=x86_64 \
                       --command=okular --file-forwarding org.kde.okular @@u %U @@";
        assert_eq!(argv(&entry(flatpak), "/a.pdf", At::Page(3)), None);
        let vim = "[Desktop Entry]\nExec=vim %F\nTerminal=true\n";
        assert_eq!(argv(vim, "/a.rs", At::Line(3)), None);
        // A field code inside a word isn't filled in.
        assert!(parse_entry(&entry("okular --file=%f")).is_none());
    }

    #[test]
    fn exec_is_read_from_the_desktop_entry_group_only() {
        let text = "[Desktop Entry]\nExec=okular %U\nTerminal=false\n\
                    [Desktop Action new]\nExec=okular --new %U\nTerminal=true\n";
        assert_eq!(
            parse_entry(text).unwrap(),
            Entry {
                exec: vec!["okular".into()],
                terminal: false
            }
        );
        assert!(parse_entry("[Desktop Entry]\nName=x\n").is_none());
        assert_eq!(
            exec_words(r#""/opt/my app/bin/zed" --x "a \"b\"" 100%% %U"#).unwrap(),
            ["/opt/my app/bin/zed", "--x", "a \"b\"", "100%"]
        );
        assert_eq!(unescape(r"a\sb\\c"), r"a b\c");
        assert!(exec_words("\"unterminated %U").is_none());
    }

    #[test]
    fn env_pairs_split_on_the_first_equals_and_skip_the_rest() {
        let entries = vec![
            "WAYLAND_DISPLAY=wayland-1".to_owned(),
            "OPTS=a=b".into(),
            "EMPTY=".into(),
            "no equals".into(),
        ];
        assert_eq!(
            env_pairs(entries),
            [
                ("WAYLAND_DISPLAY".to_owned(), "wayland-1".to_owned()),
                ("OPTS".into(), "a=b".into()),
                ("EMPTY".into(), String::new()),
            ]
        );
    }

    #[test]
    fn the_flatpak_runs_apps_on_the_host() {
        let argv = vec!["okular".to_owned(), "/home/u/a b.pdf".to_owned()];
        assert_eq!(in_sandbox_argv(argv.clone(), false), argv);
        assert_eq!(
            in_sandbox_argv(argv, true),
            ["flatpak-spawn", "--host", "okular", "/home/u/a b.pdf"]
        );
    }

    #[test]
    fn a_service_opens_apps_in_a_scope_of_their_own() {
        let argv = vec![
            "kwrite".to_owned(),
            "--line".into(),
            "42".into(),
            "/a b.md".into(),
        ];
        assert_eq!(detached(argv.clone(), false), argv);
        assert_eq!(
            detached(argv, true),
            [
                "systemd-run",
                "--user",
                "--scope",
                "--collect",
                "--quiet",
                "--",
                "kwrite",
                "--line",
                "42",
                "/a b.md"
            ]
        );
    }

    #[test]
    fn a_picked_editor_opens_at_the_line_when_it_can() {
        let picked = |exec: &str| {
            picked_argv(
                &parse_entry(&entry(exec)).unwrap(),
                Path::new("/a.rs"),
                At::Line(9),
            )
        };
        assert_eq!(
            picked("kate -b %U").unwrap(),
            ["kate", "-b", "--line", "9", "/a.rs"]
        );
        // A Flatpak can't be told the line, but still opens the file.
        assert_eq!(
            picked("/usr/bin/flatpak run --command=kate org.kde.kate @@u %U @@").unwrap(),
            [
                "/usr/bin/flatpak",
                "run",
                "--command=kate",
                "org.kde.kate",
                "@@u",
                "/a.rs",
                "@@"
            ]
        );
        let terminal = "[Desktop Entry]\nExec=nvim %F\nTerminal=true\n";
        assert_eq!(
            picked_argv(
                &parse_entry(terminal).unwrap(),
                Path::new("/a.rs"),
                At::Line(9)
            ),
            None
        );
    }

    #[test]
    fn the_editor_setting() {
        let f = Path::new("/a b/c.rs");
        assert_eq!(
            editor_argv("code", f, 9).unwrap(),
            ["code", "--goto", "/a b/c.rs:9"]
        );
        assert_eq!(
            editor_argv("/opt/idea/bin/idea.sh", f, 9).unwrap(),
            ["/opt/idea/bin/idea.sh", "--line", "9", "/a b/c.rs"]
        );
        // A template: each word filled in on its own, so the path stays one argument.
        assert_eq!(
            editor_argv("foot -e nvim +{line} {file}", f, 9).unwrap(),
            ["foot", "-e", "nvim", "+9", "/a b/c.rs"]
        );
        // Unknown editor, no template: the file, without a line.
        assert_eq!(
            editor_argv("mousepad", f, 9).unwrap(),
            ["mousepad", "/a b/c.rs"]
        );
        assert_eq!(editor_argv("  ", f, 9), None);
        // The setting wins over the default app, without asking xdg-mime.
        assert!(matches!(
            argv_at(f, At::Line(9), Some("kate")).as_deref(),
            Some([k, ..]) if k == "kate"
        ));
    }

    fn play(exec: &str, file: &str, seconds: f64) -> Option<Vec<String>> {
        entry_play_argv(
            &parse_entry(&entry(exec)).unwrap(),
            Path::new(file),
            seconds,
        )
    }

    #[test]
    fn known_players_start_at_the_time() {
        // mpv's own desktop file ends its options with `--`.
        assert_eq!(
            play(
                "mpv --player-operation-mode=pseudo-gui -- %U",
                "/v/a.mkv",
                754.0
            )
            .unwrap(),
            [
                "mpv",
                "--player-operation-mode=pseudo-gui",
                "--start=754",
                "--",
                "/v/a.mkv"
            ]
        );
        assert_eq!(
            play("/usr/bin/vlc --started-from-file %U", "/v/a.mkv", 754.0).unwrap(),
            [
                "/usr/bin/vlc",
                "--started-from-file",
                "--start-time=754",
                "/v/a.mkv"
            ]
        );
        assert_eq!(
            play("celluloid --new-window %U", "/v/a.mkv", 754.0).unwrap(),
            ["celluloid", "--new-window", "--mpv-start=754", "/v/a.mkv"]
        );
    }

    #[test]
    fn other_players_open_without_a_time() {
        for exec in [
            "haruna %u",
            "totem %U",
            "showtime %U",
            "/usr/bin/flatpak run --command=mpv io.mpv.Mpv @@u %U @@",
        ] {
            assert_eq!(play(exec, "/v/a.mkv", 754.0), None, "{exec}");
        }
        let terminal = "[Desktop Entry]\nExec=mpv %U\nTerminal=true\n";
        assert_eq!(
            entry_play_argv(&parse_entry(terminal).unwrap(), Path::new("/v/a.mkv"), 7.0),
            None
        );
    }

    #[test]
    fn known_players_take_the_time_to_the_millisecond() {
        let flag = |s| play("mpv %U", "/a.mkv", s).unwrap()[1].clone();
        assert_eq!(flag(754.5), "--start=754.5");
        assert_eq!(flag(1.0 / 3.0), "--start=0.333");
        assert_eq!(flag(59.9996), "--start=60");
        assert_eq!(flag(0.0), "--start=0");
        assert_eq!(flag(-3.0), "--start=0");
        assert_eq!(flag(f64::NAN), "--start=0");
    }

    #[test]
    fn the_player_setting() {
        let f = Path::new("/v/it's a \"film\".mkv");
        let name = f.display().to_string();
        // A template: whole seconds, rounded down, and the path one argument.
        assert_eq!(
            player_argv("mpv --start={seconds} {file}", f, 754.9).unwrap(),
            ["mpv", "--start=754", name.as_str()]
        );
        assert_eq!(
            player_argv("ffplay -ss {seconds} -i {file}", f, 61.0).unwrap(),
            ["ffplay", "-ss", "61", "-i", name.as_str()]
        );
        // A known player without a template gets its own flag.
        assert_eq!(
            player_argv("vlc", f, 754.5).unwrap(),
            ["vlc", "--start-time=754.5", name.as_str()]
        );
        assert_eq!(
            player_argv("/usr/bin/celluloid --new-window", f, 3.0).unwrap(),
            [
                "/usr/bin/celluloid",
                "--new-window",
                "--mpv-start=3",
                name.as_str()
            ]
        );
        // Unknown player, no template: the file, without a time.
        assert_eq!(
            player_argv("haruna", f, 754.0).unwrap(),
            ["haruna", name.as_str()]
        );
        assert_eq!(player_argv("  ", f, 1.0), None);
        // No start-time option to give: the file alone.
        for program in ["haruna", "totem", "showtime"] {
            assert_eq!(
                player_argv(program, f, 9.0),
                Some(vec![program.to_owned(), name.clone()])
            );
        }
        // The setting wins over the default app, without asking xdg-mime.
        assert_eq!(
            play_argv(f, 2.0, Some("mpv")),
            ["mpv", "--start=2", name.as_str()]
        );
    }
}
