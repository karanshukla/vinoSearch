//! Opening a result where it matched (#105): a text file at its line in the
//! editor, a PDF at its page in the viewer. KRunner, GNOME Shell and
//! `vinosearch menu` all open through here.
//!
//! The app is the `editor` setting for text, else the default app for the
//! file's type (`xdg-mime query default`), whose desktop file says what to
//! run. Only apps known to take a line or page get one; anything else, a
//! terminal app or a Flatpak included, gets `xdg-open` as before. Every
//! command is an argv, never a shell line.

use crate::index::Kind;
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
}

impl At {
    /// Where a hit on `path` at chunk number `line` is, if an app can be told.
    /// An HTML page's lines are of its extracted text, and a document's
    /// numbers other than a PDF's page are slides, rows or chapters, which
    /// no common app takes from the command line.
    pub fn of(kind: Kind, path: &Path, line: i64) -> Option<At> {
        let n = u32::try_from(line).ok().filter(|&n| n > 0)?;
        match kind {
            Kind::Text if !crate::extract::is_html(path) => Some(At::Line(n)),
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

/// The command for the `editor` setting: a template when it has `{file}`
/// (each word on its own, `{line}` replaced too), else a known editor with
/// its line flag, else the command with the file and no line. Words are
/// split on whitespace, never through a shell.
fn editor_argv(editor: &str, file: &Path, line: u32) -> Option<Vec<String>> {
    let words: Vec<&str> = editor.split_whitespace().collect();
    let program = *words.first()?;
    let f = file.display().to_string();
    if words.iter().any(|w| w.contains("{file}")) {
        return Some(
            words
                .iter()
                .map(|w| w.replace("{line}", &line.to_string()).replace("{file}", &f))
                .collect(),
        );
    }
    let mut argv: Vec<String> = words.iter().map(|&w| w.to_owned()).collect();
    match style(program).and_then(|s| args(s, file, At::Line(line))) {
        Some(a) => argv.extend(a),
        None => argv.push(f),
    }
    Some(argv)
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

fn data_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    match std::env::var_os("XDG_DATA_HOME") {
        Some(d) if !d.is_empty() => dirs.push(PathBuf::from(d)),
        _ => {
            if let Some(h) = std::env::var_os("HOME") {
                dirs.push(PathBuf::from(h).join(".local/share"));
            }
        }
    }
    let system = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    dirs.extend(
        system
            .split(':')
            .filter(|d| !d.is_empty())
            .map(PathBuf::from),
    );
    dirs
}

/// The desktop entry with id `id` (`org.kde.okular.desktop`), first found.
fn find_entry(id: &str) -> Option<Entry> {
    if id.contains('/') || !id.ends_with(".desktop") {
        return None;
    }
    data_dirs().iter().find_map(|d| {
        let text = std::fs::read_to_string(d.join("applications").join(id)).ok()?;
        parse_entry(&text)
    })
}

/// One line of `xdg-mime`'s output, if it ran and printed one.
fn xdg_mime(args: &[&std::ffi::OsStr]) -> Option<String> {
    let out = Command::new("xdg-mime")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let line = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && !line.is_empty() && !line.contains('\n')).then_some(line)
}

/// The default app's desktop entry for `file`. A text subtype with no
/// default of its own (`text/rust`) takes text/plain's, as xdg-open does.
fn default_entry(file: &Path, at: At) -> Option<Entry> {
    let mime = match at {
        At::Page(_) => "application/pdf".to_owned(),
        At::Line(_) => xdg_mime(&["query".as_ref(), "filetype".as_ref(), file.as_os_str()])?,
    };
    let default = |m: &str| xdg_mime(&["query".as_ref(), "default".as_ref(), m.as_ref()]);
    let id = match default(&mime) {
        Some(id) => id,
        None if mime.starts_with("text/") => default("text/plain")?,
        None => return None,
    };
    find_entry(&id)
}

/// The command that opens `file` at `at`, if anything known can.
fn argv_at(file: &Path, at: At, editor: Option<&str>) -> Option<Vec<String>> {
    if let (At::Line(l), Some(e)) = (at, editor.filter(|e| !e.trim().is_empty())) {
        return editor_argv(e, file, l);
    }
    app_argv(&default_entry(file, at)?, file, at)
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

fn in_unit() -> bool {
    std::env::var_os("INVOCATION_ID").is_some() && crate::menu::on_path("systemd-run")
}

fn spawn(argv: Vec<String>) -> std::io::Result<()> {
    let argv = detached(argv, in_unit());
    Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .spawn()
        .map(drop)
}

/// Open `file` at `at` in the editor or default app, else with `xdg-open`.
/// Looking up the default app runs `xdg-mime`, so call this off the bus.
pub fn open(file: &Path, at: Option<At>, editor: Option<&str>) {
    if let Some(argv) = at.and_then(|at| argv_at(file, at, editor)) {
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(At::of(Kind::Text, &p("/a.rs"), 0), None);
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
}
