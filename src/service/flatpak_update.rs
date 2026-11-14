//! `vinosearch flatpak-update [TAG]`: the About page's Update button in the
//! Flatpak, as `vinosearch-upgrade` (install.sh) is for a release install.
//! It downloads the release's bundle and its .sha256 into the app's cache
//! under ~/.var/app, which the host sees at the same path, refuses the
//! bundle unless the .sha256 names it and matches, has the host's flatpak
//! install it into the installation this build runs from, and restarts the
//! service if it was running, so the new build serves. A failure ends with
//! a `✗` line on stderr, the line the button shows.

use crate::service::start;
use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use vinosearch_shared::dbus::names;
use vinosearch_shared::sandbox;

const REPO: &str = "karanshukla/vinoSearch";
/// The release asset, beside `BUNDLE.sha256`; release.yml uploads both.
pub const BUNDLE: &str = "vinosearch-x86_64.flatpak";
/// Well past the bundle's 170 MB, and the checksum file's one line.
const MAX_BUNDLE: u64 = 2 << 30;
const MAX_SUMS: u64 = 4096;
/// The system installation, which flatpak's `--system` means.
const SYSTEM_DIR: &str = "/var/lib/flatpak";

/// Install release `tag` (else `$VINO_UPDATE_TAG`). False after printing
/// why it didn't.
pub fn run(tag: Option<String>) -> Result<bool> {
    match update(tag) {
        Ok(()) => Ok(true),
        Err(e) => {
            eprintln!("✗ {e:#}");
            Ok(false)
        }
    }
}

fn update(tag: Option<String>) -> Result<()> {
    let app = sandbox::flatpak_id()
        .context("Not running in the Flatpak: a release install updates with vinosearch-upgrade")?;
    let tag = tag
        .or_else(|| std::env::var("VINO_UPDATE_TAG").ok())
        .unwrap_or_default();
    if !release_tag(&tag) {
        bail!(
            "Not a release tag: {:?}",
            tag.chars().take(40).collect::<String>()
        );
    }
    let branch = sandbox::info_value("Instance", "branch").unwrap_or_default();
    if branch != sandbox::RELEASE_BRANCH {
        bail!(
            "This is a {branch} build, not a release: install the release's {BUNDLE} with flatpak install"
        );
    }
    let app_path = sandbox::info_value("Instance", "app-path")
        .context("/.flatpak-info doesn't say where the app is installed")?;
    let home = PathBuf::from(std::env::var_os("HOME").context("no HOME")?);
    let scope = installation(&app_path, app, &home)?;

    // A mirror, or a test's fake release. The checksum is checked the same.
    let base = match std::env::var("VINOSEARCH_RELEASE_BASE") {
        Ok(base) if !base.is_empty() => base.trim_end_matches('/').to_owned(),
        _ => format!("https://github.com/{REPO}/releases/download/{tag}"),
    };
    // The app's cache under ~/.var/app, not /tmp: the host's flatpak reads it.
    let scratch = Scratch::new(
        home.join(".var/app")
            .join(app)
            .join("cache/vinosearch-update"),
    )?;
    let bundle = scratch.0.join(BUNDLE);
    let agent = agent();
    eprintln!("downloading {tag} from {base}");
    let mut sums = Vec::new();
    fetch(
        &agent,
        &base,
        &format!("{BUNDLE}.sha256"),
        &mut sums,
        MAX_SUMS,
    )?;
    let checksum_failed =
        || anyhow!("Checksum verification failed for {BUNDLE}. Nothing was installed.");
    let want = expected_sum(&String::from_utf8_lossy(&sums), BUNDLE).ok_or_else(checksum_failed)?;
    let mut file = fs::File::create(&bundle)
        .with_context(|| format!("Couldn't write {}", bundle.display()))?;
    let got = fetch(&agent, &base, BUNDLE, &mut file, MAX_BUNDLE)?;
    file.sync_all()?;
    drop(file);
    if got != want {
        return Err(checksum_failed());
    }
    eprintln!("checksum verified");

    let service = &names().service;
    let was_running = start::owned(service) == Some(true);
    install(scope, &bundle, &tag)?;
    // Flatpak installs an older build over a newer one without a word, so
    // the build now installed is asked what it is.
    match installed_version(scope, app) {
        Some(version) if same_version(&tag, &version) => {}
        Some(version) => bail!(
            "{tag}'s bundle is vinoSearch {version}, and that is installed now; \
             the service wasn't restarted"
        ),
        None => bail!("{tag} is installed but doesn't run; the service wasn't restarted"),
    }
    // As install.sh does: the running service is restarted on the new
    // build. A stopped one stays stopped and starts on it next time.
    if was_running {
        match start::stop_runner().and_then(|_| start::start(service)) {
            Ok(()) => println!("restarted {service}"),
            Err(e) => eprintln!("{service} runs {tag} from its next start: {e:#}"),
        }
    }
    Ok(())
}

/// `v` and dot-separated numbers, as install.sh takes: the tag ends up in a
/// URL.
fn release_tag(tag: &str) -> bool {
    tag.len() <= 32
        && tag.strip_prefix('v').is_some_and(|rest| {
            rest.split('.')
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        })
}

/// Whether `version` (`11.0.0`) is release `tag` (`v11`): missing parts
/// count as 0.
fn same_version(tag: &str, version: &str) -> bool {
    let parts = |s: &str| -> Option<Vec<u64>> {
        let mut v = s
            .split('.')
            .map(|p| p.parse().ok())
            .collect::<Option<Vec<u64>>>()?;
        while v.len() > 1 && v.last() == Some(&0) {
            v.pop();
        }
        Some(v)
    };
    match (tag.strip_prefix('v').and_then(parts), parts(version)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    User,
    System,
}

impl Scope {
    fn flag(self) -> &'static str {
        match self {
            Scope::User => "--user",
            Scope::System => "--system",
        }
    }
}

/// `flatpak SUBCOMMAND --user|--system` on the host, for its arguments to
/// follow.
fn host_flatpak(subcommand: &str, scope: Scope) -> Command {
    let mut command = Command::new("flatpak-spawn");
    // The host has no sandbox paths to start in.
    command
        .current_dir("/")
        .args(["--host", "flatpak", subcommand, scope.flag()])
        .stdin(Stdio::null());
    command
}

/// The version of the build `scope` now has, from its own `--version`.
fn installed_version(scope: Scope, app: &str) -> Option<String> {
    let out = host_flatpak("run", scope)
        .args(["--command=vinosearch", app, "--version"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let version = text.trim().strip_prefix("vinosearch ")?;
    out.status.success().then(|| version.to_owned())
}

/// Which installation `app_path` (`/.flatpak-info`'s, `<dir>/app/<id>/...`)
/// is in, so the update goes where this build is rather than beside it.
fn installation(app_path: &str, app: &str, home: &Path) -> Result<Scope> {
    let dir = app_path
        .find(&format!("/app/{app}/"))
        .map(|at| Path::new(&app_path[..at]))
        .with_context(|| format!("Can't tell which Flatpak installation {app_path} is in"))?;
    if dir == Path::new(SYSTEM_DIR) {
        return Ok(Scope::System);
    }
    if dir.starts_with(home) || home.canonicalize().is_ok_and(|h| dir.starts_with(h)) {
        return Ok(Scope::User);
    }
    bail!(
        "vinoSearch is in the Flatpak installation {}, which the Update button can't update: \
         install the release's {BUNDLE} with flatpak install",
        dir.display()
    )
}

/// The hash `sums` (sha256sum's output) gives for `name`. Exactly one line
/// must name it, as install.sh checks: `sha256sum -c` alone would pass
/// whatever file it happened to list.
fn expected_sum(sums: &str, name: &str) -> Option<String> {
    let mut hashes = sums.lines().filter_map(|line| {
        let (hash, rest) = line.split_at_checked(64)?;
        let file = rest.strip_prefix(' ')?.strip_prefix([' ', '*'])?;
        let hex = hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        (hex && file == name).then(|| hash.to_owned())
    });
    let hash = hashes.next()?;
    hashes.next().is_none().then_some(hash)
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_per_read(Some(Duration::from_secs(60)))
        .build()
        .into()
}

/// `base/name` into `out`, at most `max` bytes; its sha256.
fn fetch(
    agent: &ureq::Agent,
    base: &str,
    name: &str,
    out: &mut dyn Write,
    max: u64,
) -> Result<String> {
    let url = format!("{base}/{name}");
    let failed = |why: String| anyhow!("Download failed: {name}: {why}");
    let mut resp = agent.get(&url).call().map_err(|e| match e {
        ureq::Error::StatusCode(code) => failed(format!("the server answered {code}")),
        e => failed(e.to_string()),
    })?;
    let mut body = resp.body_mut().as_reader();
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut got = 0;
    loop {
        let n = body.read(&mut buf).map_err(|e| failed(e.to_string()))?;
        if n == 0 {
            break;
        }
        got += n as u64;
        if got > max {
            return Err(failed(format!("more than {max} bytes")));
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])
            .with_context(|| format!("Couldn't save {name}"))?;
    }
    Ok(hex::encode(hasher.finalize()))
}

/// The host's flatpak installs `bundle` into `scope`. A system installation
/// asks for an administrator's password through the desktop (polkit).
fn install(scope: Scope, bundle: &Path, tag: &str) -> Result<()> {
    // A newer build of the installed branch replaces it; an instance still
    // running keeps the old one until it exits.
    let out = host_flatpak("install", scope)
        .args(["-y", "--bundle"])
        .arg(bundle)
        .output()
        .context("Couldn't run flatpak-spawn")?;
    let err = String::from_utf8_lossy(&out.stderr);
    eprint!("{err}");
    if out.status.success() {
        return Ok(());
    }
    let why = err
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.trim_start_matches("error:").trim().to_owned())
        .unwrap_or_else(|| out.status.to_string());
    bail!("flatpak couldn't install {tag}: {why}")
}

/// A directory emptied now and removed when dropped, however the update ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(dir: PathBuf) -> Result<Self> {
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).with_context(|| format!("Couldn't create {}", dir.display()))?;
        Ok(Scratch(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::net::TcpListener;

    #[test]
    fn only_a_release_tag_is_taken() {
        for tag in ["v1", "v10", "v10.1.2"] {
            assert!(release_tag(tag), "{tag}");
        }
        for tag in [
            "", "v", "10", "v1.", "v1..2", "v1.2-rc1", "v1/../x", "v1;rm", "V1",
        ] {
            assert!(!release_tag(tag), "{tag}");
        }
        assert!(!release_tag(&format!("v{}", "1".repeat(32))));
    }

    #[test]
    fn a_build_is_the_release_its_version_says() {
        assert!(same_version("v11", "11.0.0"));
        assert!(same_version("v11.1", "11.1.0"));
        assert!(same_version("v10.0.0", "10"));
        assert!(!same_version("v11", "10.0.0"));
        assert!(!same_version("v11", "11.0.1"));
        assert!(!same_version("v11", "dev"));
        assert!(!same_version("11", "11.0.0"));
    }

    #[test]
    fn the_checksum_must_name_exactly_the_bundle() {
        let h = "a".repeat(64);
        let one = format!("{h}  {BUNDLE}\n");
        assert_eq!(expected_sum(&one, BUNDLE), Some(h.clone()));
        assert_eq!(
            expected_sum(&format!("{h} *{BUNDLE}\n"), BUNDLE),
            Some(h.clone())
        );
        // Another file's line is no use, nor two for this one.
        assert_eq!(expected_sum(&format!("{h}  other.flatpak\n"), BUNDLE), None);
        assert_eq!(expected_sum(&format!("{h}  ./{BUNDLE}\n"), BUNDLE), None);
        assert_eq!(expected_sum(&format!("{one}{one}"), BUNDLE), None);
        assert_eq!(
            expected_sum(&format!("{}  {BUNDLE}\n", "A".repeat(64)), BUNDLE),
            None
        );
        assert_eq!(
            expected_sum(&format!("{}  {BUNDLE}\n", &h[1..]), BUNDLE),
            None
        );
        assert_eq!(expected_sum("", BUNDLE), None);
    }

    #[test]
    fn the_update_goes_to_the_installation_the_app_is_in() {
        let app = "dev.example.App";
        let home = Path::new("/home/me");
        let user = format!("/home/me/.local/share/flatpak/app/{app}/x86_64/stable/abc/files");
        let system = format!("/var/lib/flatpak/app/{app}/x86_64/stable/abc/files");
        assert_eq!(installation(&user, app, home).unwrap(), Scope::User);
        assert_eq!(installation(&system, app, home).unwrap(), Scope::System);
        let other = format!("/mnt/flatpak/app/{app}/x86_64/stable/abc/files");
        assert!(installation(&other, app, home).is_err());
        assert!(installation("/somewhere/else", app, home).is_err());
    }

    /// Answer one request on a local port with `status` and `body`.
    fn serve(status: &'static str, body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap() > 2 {
                line.clear();
            }
            let mut stream = stream;
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
        });
        base
    }

    #[test]
    fn a_download_is_hashed_and_capped() {
        let base = serve("200 OK", b"hello".to_vec());
        let mut out = Vec::new();
        let hash = fetch(&agent(), &base, "x", &mut out, 100).unwrap();
        assert_eq!(out, b"hello");
        assert_eq!(hash, hex::encode(Sha256::digest(b"hello")));

        let base = serve("200 OK", vec![0; 200]);
        let e = fetch(&agent(), &base, "x", &mut Vec::new(), 100).unwrap_err();
        assert_eq!(e.to_string(), "Download failed: x: more than 100 bytes");

        let base = serve("404 Not Found", Vec::new());
        let e = fetch(&agent(), &base, "x.sha256", &mut Vec::new(), 100).unwrap_err();
        assert_eq!(
            e.to_string(),
            "Download failed: x.sha256: the server answered 404"
        );
    }
}
