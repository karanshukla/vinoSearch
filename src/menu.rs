//! `vinosearch menu`: search from a dmenu-style launcher, for desktops with
//! neither KRunner nor GNOME Shell (sway, Hyprland, niri, COSMIC...). The
//! launcher asks for the query, the running runner answers it over D-Bus
//! (the same `Match` KRunner calls, so warm queries take milliseconds rather
//! than a model load), the launcher shows the files, and the pick opens at
//! its line or page from here, in the session's environment.

use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use zbus::OwnedVariant;

use crate::open::At;
use vinosearch_shared::dbus::{RUNNER_PATH, SERVICE};

/// Launchers tried in order when none is given: each speaks dmenu (choices
/// on stdin, the pick or the typed text on stdout) and runs on Wayland.
const LAUNCHERS: [&[&str]; 5] = [
    &["fuzzel", "--dmenu", "--prompt", "vinoSearch: "],
    &["wofi", "--dmenu", "--prompt", "vinoSearch"],
    &["rofi", "-dmenu", "-p", "vinoSearch"],
    // Without --require-match=false tofi won't return a typed query.
    &[
        "tofi",
        "--prompt-text",
        "vinoSearch: ",
        "--require-match=false",
    ],
    &["bemenu", "-p", "vinoSearch"],
];

/// The launcher's command line: `given` split on whitespace, else the first
/// known launcher `on_path` finds.
fn launcher(given: Option<&str>, on_path: impl Fn(&str) -> bool) -> Result<Vec<String>> {
    if let Some(cmd) = given {
        let words: Vec<String> = cmd.split_whitespace().map(str::to_owned).collect();
        if words.is_empty() {
            bail!("--launcher is empty");
        }
        return Ok(words);
    }
    LAUNCHERS
        .iter()
        .find(|l| on_path(l[0]))
        .map(|l| l.iter().map(|w| (*w).to_owned()).collect())
        .context(
            "no launcher found: install fuzzel, wofi, rofi, tofi or bemenu, \
             or pass --launcher \"CMD\" with any dmenu-style command",
        )
}

pub(crate) fn on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| {
        std::env::split_paths(&p).any(|d| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(d.join(name))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    })
}

/// Run the launcher with `choices` on stdin. None when it was dismissed.
fn ask(cmd: &[String], choices: &[String]) -> Result<Option<String>> {
    let mut child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting {}", cmd[0]))?;
    {
        let mut stdin = child.stdin.take().expect("piped");
        for c in choices {
            // A launcher that quits early closes the pipe; that's a dismissal.
            if writeln!(stdin, "{c}").is_err() {
                break;
            }
        }
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Ok(None);
    }
    let picked = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned();
    Ok((!picked.is_empty()).then_some(picked))
}

/// One result: the file and how the launcher shows it.
#[derive(Debug, PartialEq)]
struct Row {
    path: String,
    line: String,
    /// Where it matched, from the match's `line` or `page`.
    at: Option<At>,
}

type Match = (
    String,
    String,
    String,
    i32,
    f64,
    HashMap<String, OwnedVariant>,
);

/// `text` (a path, or KRunner's `dir:line  snippet` subtext) with a leading
/// home directory written as `~`.
fn tilde(path: &str, home: Option<&str>) -> String {
    match home {
        Some(h) if !h.is_empty() && h != "/" => match path.strip_prefix(h) {
            Some(rest) if rest.is_empty() || rest.starts_with(['/', ':', ',', ' ']) => {
                format!("~{rest}")
            }
            _ => path.to_owned(),
        },
        _ => path.to_owned(),
    }
}

/// The runner's matches as launcher rows, best first. A row is the file name
/// then KRunner's subtext (folder, line, snippet), on one line.
fn rows(mut matches: Vec<Match>, home: Option<&str>) -> Vec<Row> {
    matches.sort_by(|a, b| b.4.total_cmp(&a.4));
    matches
        .into_iter()
        .map(|(path, name, _, _, _, props)| {
            let sub = props
                .get("subtext")
                .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
                .unwrap_or_default();
            let sub: String = tilde(&sub, home)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let line = if sub.is_empty() {
                name
            } else {
                format!("{name}   {sub}")
            };
            let number = |key: &str| {
                props
                    .get(key)
                    .and_then(|v| u32::try_from(v).ok())
                    .filter(|&n| n > 0)
            };
            let at = number("line")
                .map(At::Line)
                .or_else(|| number("page").map(At::Page));
            Row { path, line, at }
        })
        .collect()
}

/// Ask the runner, starting it through D-Bus activation if it isn't running.
fn search(query: &str) -> Result<Vec<Match>> {
    let reply = zbus::block_on(async {
        let conn = zbus::Connection::session()
            .await
            .context("no session bus")?;
        Ok::<_, anyhow::Error>(
            conn.call_method(
                Some(SERVICE),
                RUNNER_PATH,
                Some("org.kde.krunner1"),
                "Match",
                &(query,),
            )
            .await,
        )
    })?;
    match reply {
        Ok(m) => Ok(m.body().deserialize()?),
        Err(zbus::Error::MethodError(name, _, _))
            if name.as_str() == "org.freedesktop.DBus.Error.ServiceUnknown" =>
        {
            bail!("the vinoSearch service isn't installed: run vinosearch krunner --install")
        }
        Err(e) => Err(e).context("asking the vinoSearch runner"),
    }
}

pub fn run(given: Option<&str>, editor: Option<&str>) -> Result<()> {
    let cmd = launcher(given, on_path)?;
    let Some(query) = ask(&cmd, &[])? else {
        return Ok(());
    };
    let home = std::env::var("HOME").ok();
    let rows = rows(search(&query)?, home.as_deref());
    if rows.is_empty() {
        ask(&cmd, &[format!("No results for \"{query}\"")])?;
        return Ok(());
    }
    let lines: Vec<String> = rows.iter().map(|r| r.line.clone()).collect();
    let Some(picked) = ask(&cmd, &lines)? else {
        return Ok(());
    };
    // Only a listed row opens: typed text that matches nothing is ignored.
    let Some(row) = rows.iter().find(|r| r.line == picked) else {
        return Ok(());
    };
    crate::open::open(Path::new(&row.path), row.at, editor);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::Variant;

    #[test]
    fn the_first_launcher_on_path_wins() {
        let cmd = launcher(None, |n| n == "wofi" || n == "bemenu").unwrap();
        assert_eq!(cmd[0], "wofi");
        assert!(launcher(None, |_| false).is_err());
        assert_eq!(
            launcher(Some("rofi -dmenu -i"), |_| false).unwrap(),
            ["rofi", "-dmenu", "-i"]
        );
        assert!(launcher(Some("  "), |_| true).is_err());
    }

    fn m(path: &str, name: &str, relevance: f64, subtext: &str) -> Match {
        let mut props = HashMap::new();
        props.insert(
            "subtext".to_owned(),
            OwnedVariant::try_from(Variant::from(subtext)).unwrap(),
        );
        (
            path.into(),
            name.into(),
            "text-x-generic".into(),
            50,
            relevance,
            props,
        )
    }

    #[test]
    fn rows_are_best_first_with_home_as_tilde() {
        let r = rows(
            vec![
                m("/home/u/a.md", "a.md", 0.2, "/home/u:3  some\n text"),
                m("/home/u/b/c.jpg", "c.jpg", 0.9, "/home/u/b"),
                m("/srv/d.txt", "d.txt", 0.5, ""),
            ],
            Some("/home/u"),
        );
        let lines: Vec<&str> = r.iter().map(|r| r.line.as_str()).collect();
        assert_eq!(lines, ["c.jpg   ~/b", "d.txt", "a.md   ~:3 some text"]);
        assert_eq!(r[0].path, "/home/u/b/c.jpg");
    }

    #[test]
    fn rows_carry_the_line_or_page() {
        let mut text = m("/a.rs", "a.rs", 0.5, "/:12  x");
        text.5.insert(
            "line".to_owned(),
            OwnedVariant::try_from(Variant::from(12u32)).unwrap(),
        );
        let mut pdf = m("/b.pdf", "b.pdf", 0.4, "/, page 3  x");
        pdf.5.insert(
            "page".to_owned(),
            OwnedVariant::try_from(Variant::from(3u32)).unwrap(),
        );
        let r = rows(vec![text, pdf, m("/c.png", "c.png", 0.3, "/")], None);
        let at: Vec<Option<At>> = r.iter().map(|r| r.at).collect();
        assert_eq!(at, [Some(At::Line(12)), Some(At::Page(3)), None]);
    }

    #[test]
    fn tilde_only_replaces_a_whole_home_prefix() {
        assert_eq!(tilde("/home/u/x", Some("/home/u")), "~/x");
        assert_eq!(tilde("/home/user2/x", Some("/home/u")), "/home/user2/x");
        assert_eq!(tilde("/home/u:3  x", Some("/home/u")), "~:3  x");
        assert_eq!(tilde("/x", Some("/")), "/x");
        assert_eq!(tilde("/home/u/x", None), "/home/u/x");
    }

    #[test]
    fn a_dismissed_launcher_is_none() {
        let sh = |script: &str| vec!["sh".to_owned(), "-c".to_owned(), script.to_owned()];
        assert_eq!(ask(&sh("exit 1"), &[]).unwrap(), None);
        assert_eq!(ask(&sh("true"), &[]).unwrap(), None);
        // dmenu protocol: the choices arrive on stdin, the pick comes back.
        let picked = ask(&sh("sed -n 2p"), &["one".into(), "two".into()]).unwrap();
        assert_eq!(picked.as_deref(), Some("two"));
    }
}
