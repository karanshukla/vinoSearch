//! The index key (#4), sealed with `systemd-creds`, so there is no TPM code of
//! our own; where that isn't usable (no systemd, or one older than 256), kept
//! in the desktop keyring instead (keyring.rs). Which one sealed a key is in
//! `index.key` itself, so it is always unsealed the same way.
//!
//! As a user, `systemd-creds --user` hands the work to systemd's
//! `io.systemd.Credentials` service, which seals a credential scoped to the
//! caller's uid without asking polkit, and unseals it only for that uid. That
//! service ignores the client's `--with-key` and `--tpm2-pcrs`: it uses the
//! TPM when the firmware reports one (`systemd-analyze has-tpm2`) and the host
//! key alone otherwise, with no PCR policy (or the signed PCR 11 policy of a
//! system booted from a UKI with a PCR public key). Which one it used is in
//! the credential's first 16 bytes, so `seal` reads it back rather than
//! trusting what it asked for. Talking to the TPM directly (tss-esapi) would
//! allow PCRs of our choosing, but `/dev/tpmrm0` is for the `tss` group only.
//!
//! The Flatpak runs the host's `systemd-creds` through `flatpak-spawn
//! --host` (its `--talk-name=org.freedesktop.Flatpak`), the key on a pipe the
//! host's process reads itself, so what it seals is the same credential a
//! native install seals, and either unseals the other's. Not reaching the
//! host (no session helper, the permission taken away, no answer) is
//! `Unavailable`: waited out, never taken for a lost key.

use anyhow::{Context, Result, bail};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;
use zeroize::{Zeroize, Zeroizing};

pub(crate) mod keyring;
pub use keyring::Keyring;

/// How long a keyring unlock prompt waits for the user before it is
/// dismissed.
pub const PROMPT_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// Root-owned places systemd's tools live, so PATH never picks the binary:
/// merged /usr, split /usr, NixOS.
const SYSTEM_BIN_DIRS: [&str; 3] = ["/usr/bin", "/bin", "/run/current-system/sw/bin"];

/// The runtime's, which runs a command on the host.
const FLATPAK_SPAWN: &str = "/usr/bin/flatpak-spawn";
/// Where the Flatpak sees the host's /usr, /bin and /etc, read-only
/// (`--filesystem=host-os:ro`).
const HOST_ROOT: &str = "/run/host";
/// A tool on the host that hasn't answered in this long is given up on for
/// now, as if the host couldn't be reached.
const HOST_TIMEOUT: Duration = Duration::from_secs(30);

/// One of systemd's tools: this system's, or from the Flatpak the host's.
#[derive(Clone, Debug)]
struct Tool {
    /// As the system it runs on sees it.
    path: PathBuf,
    /// Run on the host through flatpak-spawn.
    host: Option<Spawn>,
}

#[derive(Clone, Debug)]
struct Spawn {
    program: PathBuf,
    timeout: Duration,
}

/// `name` from the root-owned places; None when it isn't there.
fn system_tool(name: &str) -> Option<Tool> {
    if vinosearch_shared::sandbox::flatpak_id().is_none() {
        let path = SYSTEM_BIN_DIRS
            .iter()
            .map(|d| Path::new(d).join(name))
            .find(|p| p.is_file())?;
        return Some(Tool { path, host: None });
    }
    Some(Tool {
        path: host_tool(Path::new(HOST_ROOT), name)?,
        host: Some(Spawn {
            program: FLATPAK_SPAWN.into(),
            timeout: HOST_TIMEOUT,
        }),
    })
}

/// The host's `name`, looked for under `root`, where the sandbox sees the
/// host's /usr and /bin. NixOS's /run isn't shared, so there it is assumed.
fn host_tool(root: &Path, name: &str) -> Option<PathBuf> {
    SYSTEM_BIN_DIRS
        .iter()
        .map(|d| Path::new(d).join(name))
        .find(|p| match p.strip_prefix("/") {
            Ok(rel) if rel.starts_with("run") => root.join("etc/NIXOS").is_file(),
            Ok(rel) => root.join(rel).is_file(),
            Err(_) => false,
        })
}
/// Sealed into the credential: one sealed under another name doesn't unseal
/// as this one.
const NAME: &str = "vinosearch-index";
pub const KEY_BYTES: usize = 32;

/// An SQLCipher key: 32 random bytes, zeroed when dropped.
pub struct Key(Zeroizing<[u8; KEY_BYTES]>);

impl Key {
    pub fn random() -> Result<Key> {
        let mut bytes = Zeroizing::new([0u8; KEY_BYTES]);
        random(&mut bytes[..])?;
        Ok(Key(bytes))
    }

    fn from_slice(b: &[u8]) -> Result<Key> {
        let bytes: [u8; KEY_BYTES] = b.try_into().map_err(|_| {
            anyhow::anyhow!("the unsealed key is {} bytes, not {KEY_BYTES}", b.len())
        })?;
        Ok(Key(Zeroizing::new(bytes)))
    }

    /// SQLCipher's raw-key form, `x'<64 hex digits>'`: used as the key as is,
    /// with no passphrase derivation (PBKDF2 would add ~0.1 s to every open
    /// and nothing to a random key).
    pub fn raw(&self) -> Zeroizing<String> {
        let mut s = Zeroizing::new(String::with_capacity(3 + 2 * KEY_BYTES));
        s.push_str("x'");
        for b in self.0.iter() {
            s.push(char::from(b"0123456789abcdef"[usize::from(b >> 4)]));
            s.push(char::from(b"0123456789abcdef"[usize::from(b & 15)]));
        }
        s.push('\'');
        s
    }
}

/// Fill `buf` from the kernel's CSPRNG.
fn random(buf: &mut [u8]) -> Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        // SAFETY: writes at most the rest of `buf`.
        let n =
            unsafe { libc::getrandom(buf[filled..].as_mut_ptr().cast(), buf.len() - filled, 0) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e).context("getrandom");
        }
        filled += n as usize;
    }
    Ok(())
}

/// The key can't be had right now (the keyring is locked, its prompt was
/// dismissed or not answered, there is no Secret Service): wait and try
/// again, never take it for a lost key.
#[derive(Debug)]
pub struct Unavailable(pub String);

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unavailable {}

pub fn is_unavailable(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<Unavailable>().is_some())
}

/// Never the bytes.
impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("Key(..)")
    }
}

/// What a sealed key needs to unseal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Seal {
    /// This machine's TPM (and systemd's host key with it).
    Tpm,
    /// systemd's host key alone, `/var/lib/systemd/credential.secret`,
    /// readable by root.
    Host,
    /// The desktop keyring, unlocked at login and readable by this user's
    /// session; no TPM.
    Keyring,
}

impl Seal {
    /// `Status`'s `seal` values.
    pub fn name(self) -> &'static str {
        match self {
            Seal::Tpm => vinosearch_shared::dbus::seal::TPM,
            Seal::Host => vinosearch_shared::dbus::seal::HOST,
            Seal::Keyring => vinosearch_shared::dbus::seal::KEYRING,
        }
    }
}

/// Key ids from systemd's creds-util.h: what a credential is sealed with,
/// its first 16 bytes.
const BY_HOST_SCOPED: [u8; 16] = id("55b9ed1d38594d43a8319d2ebb332ac6");
const BY_HOST_AND_TPM2_SCOPED: [u8; 16] = id("ef4ac13679a9480ea7db68897f9f165d");
const BY_HOST_AND_TPM2_PK_SCOPED: [u8; 16] = id("adbc4ca3efb64201ba881b6f2e4095ea");
// systemd 262 pins TPM credentials to the TPM's SRK, under new ids.
const BY_HOST_AND_TPM2_SCOPED_SRK: [u8; 16] = id("2a1f877a4275431ab3f9ed1f5d8f6601");
const BY_HOST_AND_TPM2_PK_SCOPED_SRK: [u8; 16] = id("16e492949f94400286758f94b7c52bc7");

const fn id(hex: &str) -> [u8; 16] {
    let h = hex.as_bytes();
    let mut out = [0u8; 16];
    let mut i = 0;
    while i < 16 {
        out[i] = nibble(h[2 * i]) << 4 | nibble(h[2 * i + 1]);
        i += 1;
    }
    out
}

const fn nibble(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        _ => panic!("not hex"),
    }
}

/// What an `index.key` is sealed with: the keyring's item reference, or a
/// credential from `systemd-creds encrypt` (base64, as it writes them); None
/// for anything else, a system-scoped credential included.
pub fn sealed_with(credential: &[u8]) -> Option<Seal> {
    if credential.starts_with(keyring::HEAD.as_bytes()) {
        return keyring::id_of(credential).map(|_| Seal::Keyring);
    }
    let head = base64_prefix(credential, 16)?;
    match <[u8; 16]>::try_from(&head[..16]).ok()? {
        BY_HOST_AND_TPM2_SCOPED
        | BY_HOST_AND_TPM2_PK_SCOPED
        | BY_HOST_AND_TPM2_SCOPED_SRK
        | BY_HOST_AND_TPM2_PK_SCOPED_SRK => Some(Seal::Tpm),
        BY_HOST_SCOPED => Some(Seal::Host),
        _ => None,
    }
}

/// The first `n` bytes of standard base64, ignoring whitespace.
fn base64_prefix(text: &[u8], n: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(n + 2);
    let (mut acc, mut bits) = (0u32, 0);
    for &c in text {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b' ' | b'\n' | b'\r' | b'\t' => continue,
            _ => return None,
        };
        acc = acc << 6 | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            if out.len() == n {
                return Some(out);
            }
        }
    }
    None
}

/// Whether systemd will seal with the TPM: one is there and the firmware
/// reports it. Without that, the host key alone.
pub fn tpm_available() -> bool {
    system_tool("systemd-analyze").is_some_and(|t| run(&t, &["has-tpm2", "--quiet"], &[]).is_ok())
}

/// Why turning encryption on was refused without a TPM.
pub const NO_TPM: &str = "This machine has no TPM that systemd can use, so the index key can't be \
     sealed to it. Allow the host key (weaker) or leave encryption off.";

/// `systemd-creds` (or a stand-in for tests), and the keyring for where it
/// can't be used.
#[derive(Clone)]
pub struct Sealer {
    /// None for none at all.
    creds: Option<Creds>,
    keyring: Keyring,
    /// The user asked for this now (ApplyEncryption, the config file), so
    /// turning encryption off while the key can't be had starts over.
    start_over: bool,
}

#[derive(Clone)]
enum Creds {
    /// The system's (from the Flatpak, the host's), where it can be used.
    System,
    #[cfg(test)]
    At(Tool),
}

impl Default for Sealer {
    fn default() -> Self {
        Sealer {
            creds: Some(Creds::System),
            keyring: Keyring {
                address: None,
                prompt: Some(PROMPT_WAIT),
            },
            start_over: false,
        }
    }
}

/// systemd runs this machine and is new enough for `systemd-creds --user`
/// (256): not so on a distribution without systemd, or in a container. From
/// the Flatpak, the host's; `Unavailable` while it can't be reached.
fn creds_usable(creds: &Tool) -> Result<bool> {
    let booted = match &creds.host {
        None => Path::new("/run/systemd/system").is_dir(),
        // The same test, made on the host.
        Some(_) => match system_tool("systemd-notify") {
            Some(t) => succeeded(run(&t, &["--booted"], &[]))?.is_some(),
            None => false,
        },
    };
    if !booted {
        return Ok(false);
    }
    let Some(out) = succeeded(run(creds, &["--version"], &[]))? else {
        return Ok(false);
    };
    // "systemd 259 (259.1-1.fc45)"
    Ok(String::from_utf8_lossy(&out)
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.parse::<u32>().ok())
        .is_some_and(|v| v >= 256))
}

/// A tool's output, None when it failed; Err only when it couldn't be run
/// for now (`Unavailable`).
fn succeeded(r: Result<Vec<u8>>) -> Result<Option<Vec<u8>>> {
    match r {
        Ok(out) => Ok(Some(out)),
        Err(e) if is_unavailable(&e) => Err(e),
        Err(_) => Ok(None),
    }
}

impl Sealer {
    #[cfg(test)]
    pub fn at(program: &Path) -> Self {
        Sealer {
            creds: Some(Creds::At(Tool {
                path: program.to_owned(),
                host: None,
            })),
            keyring: Keyring::default(),
            start_over: false,
        }
    }

    /// `program` on the "host", through `spawn`, a stand-in for flatpak-spawn.
    #[cfg(test)]
    pub fn on_host(spawn: &Path, program: &Path, timeout: Duration) -> Self {
        Sealer {
            creds: Some(Creds::At(Tool {
                path: program.to_owned(),
                host: Some(Spawn {
                    program: spawn.to_owned(),
                    timeout,
                }),
            })),
            keyring: Keyring::default(),
            start_over: false,
        }
    }

    /// No `systemd-creds`: the Secret Service `keyring` points at.
    #[cfg(test)]
    pub fn keyring_only(keyring: Keyring) -> Self {
        Sealer {
            creds: None,
            keyring,
            start_over: false,
        }
    }

    /// `systemd-creds`, if it can seal a new key here; None for the keyring.
    /// Looked at once per process, and only for a new key: not on the way to
    /// unsealing one. From the Flatpak, a host that can't be reached is
    /// `Unavailable`, never the keyring, and looked at again next time.
    fn sealing(&self) -> Result<Option<Tool>> {
        static USABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        match self.creds.as_ref() {
            None => Ok(None),
            Some(Creds::System) => {
                let Some(tool) = system_tool("systemd-creds") else {
                    return Ok(None);
                };
                let usable = match USABLE.get() {
                    Some(u) => *u,
                    None => {
                        let u = creds_usable(&tool)?;
                        *USABLE.get_or_init(|| u)
                    }
                };
                Ok(usable.then_some(tool))
            }
            #[cfg(test)]
            Some(Creds::At(t)) => Ok(Some(t.clone())),
        }
    }

    /// `systemd-creds`, to unseal what it sealed.
    fn unsealing(&self) -> Option<Tool> {
        match self.creds.as_ref()? {
            Creds::System => system_tool("systemd-creds"),
            #[cfg(test)]
            Creds::At(t) => Some(t.clone()),
        }
    }

    /// Whether turning encryption off without the key may rebuild the index.
    pub fn starts_over(&self) -> bool {
        self.start_over
    }

    /// The same, for a change the user asked for now: see `start_over`.
    pub fn asked(&self) -> Self {
        Sealer {
            start_over: true,
            ..self.clone()
        }
    }

    /// The same, but a locked keyring is `Unavailable` at once rather than
    /// prompting: for tries nobody asked for.
    pub fn quiet(&self) -> Self {
        Sealer {
            keyring: Keyring {
                prompt: None,
                ..self.keyring.clone()
            },
            ..self.clone()
        }
    }

    /// What a new key is sealed with here: the TPM or the host key (`tpm`
    /// says which) while `systemd-creds` works, else the keyring.
    pub fn new_seal(&self, tpm: bool) -> Seal {
        match (self.sealing(), tpm) {
            (Ok(None), _) => Seal::Keyring,
            // A host out of reach for now: still `systemd-creds` that seals.
            (_, true) => Seal::Tpm,
            (_, false) => Seal::Host,
        }
    }

    /// Seal `key` for this user, with `systemd-creds` wherever it works: a
    /// key sealed with the host key alone is refused unless `allow_host` says
    /// otherwise, never weaker than asked, and never put in the keyring
    /// instead. Only where `systemd-creds` can't be used at all, the keyring.
    pub fn seal(&self, key: &Key, allow_host: bool) -> Result<(Vec<u8>, Seal)> {
        let Some(tool) = self.sealing().context("systemd-creds")? else {
            let blob = self
                .keyring
                .store(&key.0[..])
                .context("storing the key in the keyring")?;
            return Ok((blob, Seal::Keyring));
        };
        let out = run(
            &tool,
            &["--user", &format!("--name={NAME}"), "encrypt", "-", "-"],
            &key.0[..],
        )
        .context("systemd-creds encrypt")?;
        match sealed_with(&out) {
            Some(Seal::Tpm) => Ok((out, Seal::Tpm)),
            Some(Seal::Host) if allow_host => Ok((out, Seal::Host)),
            Some(Seal::Host) => bail!("{NO_TPM}"),
            _ => bail!(
                "systemd-creds wrote a credential of a kind vinoSearch doesn't know ({})",
                base64_prefix(&out, 16)
                    .map(|h| h.iter().map(|b| format!("{b:02x}")).collect::<String>())
                    .unwrap_or_else(|| "not base64".into())
            ),
        }
    }

    /// The key in `credential`, from whatever sealed it. Any failure is an
    /// error and nothing else: the caller must not fall back to anything.
    /// An `Unavailable` one is to be waited out.
    pub fn unseal(&self, credential: &[u8]) -> Result<Key> {
        if sealed_with(credential) == Some(Seal::Keyring) {
            let out = self
                .keyring
                .fetch(credential)
                .context("reading the key from the keyring")?;
            // An item that isn't a key is the keyring's oddity, waited out
            // like the rest: never a lost key, which rebuilds the index.
            return Key::from_slice(&out).map_err(|e| Unavailable(format!("{e:#}")).into());
        }
        // Not the key's fault: waited for, never taken for a lost key.
        let tool = self.unsealing().ok_or_else(|| {
            Unavailable("systemd-creds sealed this key, and it isn't here".into())
        })?;
        let out = Zeroizing::new(
            run(
                &tool,
                &["--user", &format!("--name={NAME}"), "decrypt", "-", "-"],
                credential,
            )
            .context("systemd-creds decrypt")?,
        );
        Key::from_slice(&out)
    }

    /// Once `credential` is replaced, drop what it keeps outside its file:
    /// the keyring's item. Nothing for `systemd-creds`, whose credential is
    /// the file.
    pub fn forget(&self, credential: &[u8]) {
        if sealed_with(credential) == Some(Seal::Keyring)
            && let Err(e) = self.keyring.delete(credential)
        {
            eprintln!("removing the old key from the keyring: {e:#}");
        }
    }
}

fn run(tool: &Tool, args: &[&str], input: &[u8]) -> Result<Vec<u8>> {
    // Nothing of ours reaches it; it talks to systemd over its socket.
    let mut cmd = match &tool.host {
        None => Command::new(&tool.path),
        Some(spawn) => {
            let mut c = Command::new(&spawn.program);
            // -v, for its word on how the tool exited (`host_exit`). The
            // tool gets no environment and its stdin and stdout are these
            // pipes, passed to it: the key never passes through flatpak-spawn
            // or the session helper.
            c.args(["-v", "--clear-env", "--directory=/", "--host"])
                .arg(&tool.path);
            c
        }
    };
    cmd.args(args).env_clear();
    if tool.host.is_some()
        && let Some(bus) = std::env::var_os("DBUS_SESSION_BUS_ADDRESS")
    {
        // flatpak-spawn's way to the host.
        cmd.env("DBUS_SESSION_BUS_ADDRESS", bus);
    }
    let spawned = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) if tool.host.is_some() => bail!(Unavailable(format!(
            "{} can't run {} on the host: {e}",
            FLATPAK_SPAWN,
            tool.path.display()
        ))),
        Err(e) => return Err(e).with_context(|| tool.path.display().to_string()),
    };
    // Written from a thread so a full stdout pipe can't deadlock us.
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let mut input = Zeroizing::new(input.to_vec());
    let writer = std::thread::spawn(move || {
        let r = stdin.write_all(&input);
        input.zeroize();
        r
    });
    let out = match &tool.host {
        None => child.wait_with_output()?,
        Some(spawn) => output_within(child, spawn.timeout).map_err(|why| {
            Unavailable(format!(
                "{} on the host didn't answer: {why}",
                tool.path.display()
            ))
        })?,
    };
    let written = writer.join().expect("stdin writer panicked");
    let mut stdout = out.stdout;
    let stderr = String::from_utf8_lossy(&out.stderr);
    let (ok, why) = match &tool.host {
        None => (out.status.success(), stderr.trim().to_owned()),
        Some(_) => match host_exit(&stderr) {
            (Some(code), why) => (code == 0 && out.status.success(), why),
            // It never ran, or never finished: the host's fault, not the
            // key's.
            (None, why) => {
                stdout.zeroize();
                bail!(Unavailable(format!(
                    "{} couldn't be run on the host: {why}",
                    tool.path.display()
                )));
            }
        },
    };
    // Status first: a child that exits early breaks the pipe, and its stderr
    // says why.
    if !ok {
        stdout.zeroize();
        bail!("{why}");
    }
    if let Err(e) = written {
        stdout.zeroize();
        return Err(e.into());
    }
    Ok(stdout)
}

/// flatpak-spawn -v's stderr: the exit code of what it ran on the host, if
/// it says it ran and exited (`F: child exit code PID: CODE`), and the rest,
/// which is that tool's stderr or why flatpak-spawn failed. Its own exit
/// code can't tell those apart: 1 for both.
fn host_exit(stderr: &str) -> (Option<i32>, String) {
    let mut pid = None;
    let mut code = None;
    let mut rest = Vec::new();
    for line in stderr.lines() {
        if let Some(p) = line.strip_prefix("F: child_pid: ") {
            pid = Some(p.trim());
        } else if let Some((p, c)) = line
            .strip_prefix("F: child exit code ")
            .and_then(|l| l.split_once(": "))
        {
            if Some(p) == pid {
                code = c.trim().parse().ok();
            }
        } else if !line.starts_with("F: ") {
            rest.push(line);
        }
    }
    (code, rest.join("\n").trim().to_owned())
}

/// `child`'s output, or why there is none after `timeout`: then it is sent
/// SIGTERM, which flatpak-spawn passes on to what it runs on the host, and
/// SIGKILL if that isn't enough.
fn output_within(child: Child, timeout: Duration) -> std::result::Result<Output, String> {
    // SAFETY: a pid of ours, not yet waited for.
    let pidfd = unsafe {
        libc::syscall(
            libc::SYS_pidfd_open,
            child.id() as libc::pid_t,
            0 as libc::c_uint,
        )
    };
    let signal = |sig: libc::c_int| {
        if pidfd >= 0 {
            // SAFETY: the pidfd names the child even once it is reaped.
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    pidfd,
                    sig,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            };
        }
    };
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        if let Err(mpsc::SendError(Ok(mut late))) = tx.send(child.wait_with_output()) {
            late.stdout.zeroize();
        }
    });
    let r = match rx.recv_timeout(timeout) {
        Ok(out) => out.map_err(|e| e.to_string()),
        Err(_) => {
            signal(libc::SIGTERM);
            if let Ok(Ok(mut late)) = rx.recv_timeout(Duration::from_secs(2)) {
                late.stdout.zeroize();
            } else {
                signal(libc::SIGKILL);
            }
            Err(format!("no answer in {} s", timeout.as_secs()))
        }
    };
    if pidfd >= 0 {
        // SAFETY: ours, closed once.
        unsafe { libc::close(pidfd as libc::c_int) };
    }
    r
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    /// A stand-in for `systemd-creds`: records its argv (one per line) and
    /// its environment beside itself, then runs `body`. It "seals" by
    /// prefixing a key id, in base64 as systemd writes them, and "unseals"
    /// by stripping it.
    pub struct Fake {
        pub dir: PathBuf,
    }

    /// A shell body that seals with `head` (base64 of a 16-byte id plus two
    /// zero bytes, so it ends on a 3-byte boundary) and unseals anything.
    pub fn creds_body(head: &str) -> String {
        format!(
            r#"for a in "$@"; do case "$a" in encrypt) mode=e;; decrypt) mode=d;; esac; done
if [ "$mode" = e ]; then printf '%s' '{head}'; base64 -w0; echo; else tail -c +25 | base64 -d; fi"#
        )
    }

    pub const TPM_HEAD: &str = "70rBNnmpSA6n22iJf58WXQAA";
    pub const HOST_HEAD: &str = "VbntHThZTUOoMZ0uuzMqxgAA";

    impl Fake {
        pub fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("vinosearch-seal-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        pub fn tool(&self, body: &str) -> PathBuf {
            let path = self.dir.join("systemd-creds");
            let tmp = self.dir.join("systemd-creds.tmp");
            fs::write(
                &tmp,
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n/usr/bin/env > '{}'\n{body}\n",
                    self.dir.join("argv").display(),
                    self.dir.join("env").display()
                ),
            )
            .unwrap();
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
            // Renamed into place, so no fork elsewhere holds it open for writing.
            fs::rename(&tmp, &path).unwrap();
            path
        }

        pub fn argv(&self) -> Vec<String> {
            fs::read_to_string(self.dir.join("argv"))
                .unwrap()
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    /// Exec of a script another test thread just wrote can fail with
    /// ETXTBSY when a concurrent fork inherited the write fd; retry that.
    pub fn retry<T>(mut f: impl FnMut() -> Result<T>) -> Result<T> {
        for _ in 0..50 {
            match f() {
                Err(e) if format!("{e:#}").contains("Text file busy") => {
                    std::thread::sleep(std::time::Duration::from_millis(20))
                }
                other => return other,
            }
        }
        f()
    }

    #[test]
    fn key_ids_match_what_systemd_writes() {
        // Real credentials from systemd 259: `systemd-creds --user encrypt`
        // without a TPM, and the head of one sealed with a TPM.
        let host = b"VbntHThZTUOoMZ0uuzMqxiAAAAABAAAADAAAABAAAAAHCZcL";
        assert_eq!(sealed_with(host), Some(Seal::Host));
        assert_eq!(sealed_with(TPM_HEAD.as_bytes()), Some(Seal::Tpm));
        // systemd 262, TPM-sealed and pinned to its SRK.
        assert_eq!(sealed_with(b"Kh+HekJ1Qxqz+e0fXY9mASAA"), Some(Seal::Tpm));
        // A system-scoped host credential, and junk.
        assert_eq!(sealed_with(b"Whxqht+dQJax1aZeCGLxmgAA"), None);
        assert_eq!(sealed_with(b"not base64!"), None);
        assert_eq!(sealed_with(b"AAAA"), None);
    }

    #[test]
    fn raw_key_is_sqlciphers_hex_form() {
        let k = Key::from_slice(&[0xab; KEY_BYTES]).unwrap();
        let raw = k.raw();
        assert_eq!(raw.len(), 67);
        assert!(raw.starts_with("x'abab") && raw.ends_with("ab'"));
        assert!(Key::from_slice(&[0; 31]).is_err());
        let (a, b) = (Key::random().unwrap(), Key::random().unwrap());
        assert_ne!(*a.raw(), *b.raw());
    }

    #[test]
    fn seal_asks_for_a_user_scoped_credential_and_round_trips() {
        let fake = Fake::new("roundtrip");
        let s = Sealer::at(&fake.tool(&creds_body(TPM_HEAD)));
        let key = Key::random().unwrap();
        let (blob, seal) = retry(|| s.seal(&key, false)).unwrap();
        assert_eq!(seal, Seal::Tpm);
        assert_eq!(
            fake.argv(),
            ["--user", "--name=vinosearch-index", "encrypt", "-", "-"]
        );
        let back = retry(|| s.unseal(&blob)).unwrap();
        assert_eq!(*back.raw(), *key.raw());
        assert_eq!(
            fake.argv(),
            ["--user", "--name=vinosearch-index", "decrypt", "-", "-"]
        );
        // Nothing of ours in its environment: sh sets these itself.
        let env = fs::read_to_string(fake.dir.join("env")).unwrap();
        for line in env.lines() {
            let k = line.split('=').next().unwrap();
            assert!(
                matches!(k, "PWD" | "SHLVL" | "_" | "OLDPWD"),
                "leaked {line}"
            );
        }
    }

    #[test]
    fn a_host_key_seal_is_refused_unless_allowed() {
        let fake = Fake::new("hostonly");
        let s = Sealer::at(&fake.tool(&creds_body(HOST_HEAD)));
        let key = Key::random().unwrap();
        let e = retry(|| s.seal(&key, false)).unwrap_err();
        assert!(format!("{e:#}").contains("no TPM"), "{e:#}");
        let (_, seal) = retry(|| s.seal(&key, true)).unwrap();
        assert_eq!(seal, Seal::Host);
    }

    #[test]
    fn failures_are_errors_with_stderr_and_no_output() {
        let fake = Fake::new("fail");
        // stdout before failing: none of it may be taken as a key.
        let s = Sealer::at(
            &fake.tool("printf '%032d' 0; echo 'Failed to unseal secret using TPM2' >&2; exit 1"),
        );
        let e = retry(|| s.unseal(b"blob")).unwrap_err();
        assert!(format!("{e:#}").contains("unseal secret"), "{e:#}");
        // An unknown kind of credential, and a missing tool.
        let s = Sealer::at(&fake.tool("cat >/dev/null; echo AAAAAAAAAAAAAAAAAAAAAAAA"));
        assert!(retry(|| s.seal(&Key::random().unwrap(), true)).is_err());
        let s = Sealer::at(&fake.dir.join("absent"));
        assert!(s.unseal(b"blob").is_err());
    }

    /// A stand-in for flatpak-spawn: records its argv and environment, then
    /// does what `body` says. `HOST` runs the "host" tool (the rest of argv
    /// after `--host`) and reports as `flatpak-spawn -v` does.
    pub fn spawner(fake: &Fake, body: &str) -> PathBuf {
        let path = fake.dir.join("flatpak-spawn");
        let tmp = fake.dir.join("flatpak-spawn.tmp");
        fs::write(
            &tmp,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{dir}/spawn-argv'\n/usr/bin/env > '{dir}/spawn-env'\n{body}\n",
                dir = fake.dir.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(&tmp, &path).unwrap();
        path
    }

    pub const HOST: &str = r#"while [ "$1" != --host ]; do shift; done; shift
echo "F: child_pid: 4242" >&2
"$@"; c=$?
echo "F: child exited 4242: $((c * 256))" >&2
echo "F: child exit code 4242: $c" >&2
exit $c"#;

    #[test]
    fn on_the_host_the_tool_runs_through_flatpak_spawn_and_round_trips() {
        let fake = Fake::new("host");
        let creds = fake.tool(&creds_body(TPM_HEAD));
        let s = Sealer::on_host(&spawner(&fake, HOST), &creds, Duration::from_secs(10));
        let key = Key::random().unwrap();
        let (blob, seal) = retry(|| s.seal(&key, false)).unwrap();
        assert_eq!(seal, Seal::Tpm);
        assert_eq!(*retry(|| s.unseal(&blob)).unwrap().raw(), *key.raw());
        let argv = fs::read_to_string(fake.dir.join("spawn-argv")).unwrap();
        let creds = creds.display().to_string();
        assert_eq!(
            argv.lines().collect::<Vec<_>>(),
            [
                "-v",
                "--clear-env",
                "--directory=/",
                "--host",
                &creds,
                "--user",
                "--name=vinosearch-index",
                "decrypt",
                "-",
                "-"
            ]
        );
        // flatpak-spawn gets the bus, and nothing else of ours.
        let env = fs::read_to_string(fake.dir.join("spawn-env")).unwrap();
        for line in env.lines() {
            let k = line.split('=').next().unwrap();
            assert!(
                matches!(
                    k,
                    "PWD" | "SHLVL" | "_" | "OLDPWD" | "DBUS_SESSION_BUS_ADDRESS"
                ),
                "leaked {line}"
            );
        }
    }

    #[test]
    fn on_the_host_only_the_tools_own_failure_is_a_failure() {
        let fake = Fake::new("hostfail");
        let wait = Duration::from_secs(10);
        // systemd-creds ran on the host and refused: the key's fault.
        let creds = fake.tool("echo 'Failed to unseal secret using TPM2' >&2; exit 1");
        let s = Sealer::on_host(&spawner(&fake, HOST), &creds, wait);
        let e = retry(|| s.unseal(b"blob")).unwrap_err();
        assert!(!is_unavailable(&e), "{e:#}");
        assert_eq!(
            format!("{e:#}"),
            "systemd-creds decrypt: Failed to unseal secret using TPM2"
        );
        // The host never ran it: no session helper, no permission. The same
        // exit code, and a key that is only out of reach.
        for body in [
            "echo 'Portal call failed: org.freedesktop.DBus.Error.ServiceUnknown' >&2; exit 1",
            "echo \"Can't find bus: no address\" >&2; exit 1",
            // Ran, but never said how it ended (the bus went away).
            "echo 'F: child_pid: 4242' >&2; exit 1",
            // Claims a success it doesn't report.
            "printf '%032d' 0; exit 0",
        ] {
            let s = Sealer::on_host(&spawner(&fake, body), &creds, wait);
            let e = retry(|| s.unseal(b"blob")).unwrap_err();
            assert!(is_unavailable(&e), "{body}: {e:#}");
            assert!(retry(|| s.seal(&Key::random().unwrap(), true)).is_err());
        }
        // No flatpak-spawn at all.
        let s = Sealer::on_host(&fake.dir.join("absent"), &creds, wait);
        assert!(is_unavailable(&s.unseal(b"blob").unwrap_err()));
    }

    #[test]
    fn a_host_tool_that_hangs_is_given_up_on_as_unavailable() {
        let fake = Fake::new("hosthang");
        let creds = fake.tool("exec sleep 30");
        let s = Sealer::on_host(&spawner(&fake, HOST), &creds, Duration::from_millis(300));
        let t = std::time::Instant::now();
        let e = retry(|| s.unseal(b"blob")).unwrap_err();
        assert!(is_unavailable(&e), "{e:#}");
        assert!(format!("{e:#}").contains("didn't answer"), "{e:#}");
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    }

    #[test]
    fn host_tools_are_found_under_the_hosts_root_owned_dirs() {
        let fake = Fake::new("hostroot");
        let root = &fake.dir;
        assert_eq!(host_tool(root, "systemd-creds"), None);
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::write(root.join("bin/systemd-creds"), "").unwrap();
        assert_eq!(
            host_tool(root, "systemd-creds").as_deref(),
            Some(Path::new("/bin/systemd-creds"))
        );
        fs::create_dir_all(root.join("usr/bin")).unwrap();
        fs::write(root.join("usr/bin/systemd-creds"), "").unwrap();
        assert_eq!(
            host_tool(root, "systemd-creds").as_deref(),
            Some(Path::new("/usr/bin/systemd-creds"))
        );
        // NixOS: its /run isn't shared, so taken on trust.
        assert_eq!(host_tool(root, "systemd-notify"), None);
        fs::create_dir_all(root.join("etc")).unwrap();
        fs::write(root.join("etc/NIXOS"), "").unwrap();
        assert_eq!(
            host_tool(root, "systemd-notify").as_deref(),
            Some(Path::new("/run/current-system/sw/bin/systemd-notify"))
        );
    }

    /// In the Flatpak, against the host's real flatpak-spawn and systemd:
    /// `cargo test` builds the binary, run inside the sandbox with
    /// `flatpak run --command=<test binary> <app> --ignored in_the_flatpak`.
    #[test]
    #[ignore = "needs the Flatpak sandbox"]
    fn in_the_flatpak_the_hosts_systemd_creds_seals_and_unseals() {
        assert!(vinosearch_shared::sandbox::flatpak_id().is_some());
        let s = Sealer::default();
        let tpm = tpm_available();
        assert_eq!(s.new_seal(tpm), if tpm { Seal::Tpm } else { Seal::Host });
        let key = Key::random().unwrap();
        let (blob, seal) = s.seal(&key, true).unwrap();
        assert_eq!(seal, s.new_seal(tpm));
        assert_eq!(*s.unseal(&blob).unwrap().raw(), *key.raw());
        // Sealed under another name: the host's refusal, a failure.
        let other = run(
            &system_tool("systemd-creds").unwrap(),
            &["--user", "--name=other", "encrypt", "-", "-"],
            &key.0[..],
        )
        .unwrap();
        let e = s.unseal(&other).unwrap_err();
        assert!(!is_unavailable(&e), "{e:#}");
        // A host tool that hangs: given up on, and stopped on the host.
        let sleep = Tool {
            path: "/usr/bin/sleep".into(),
            host: Some(Spawn {
                program: FLATPAK_SPAWN.into(),
                timeout: Duration::from_millis(500),
            }),
        };
        let e = run(&sleep, &["7.25"], &[]).unwrap_err();
        assert!(is_unavailable(&e), "{e:#}");
        std::thread::sleep(Duration::from_millis(500));
        let left = run(
            &system_tool("pgrep").unwrap(),
            &["-f", "-x", "/usr/bin/sleep 7.25"],
            &[],
        );
        assert!(
            left.as_ref().is_err_and(|e| !is_unavailable(e)),
            "still running on the host: {left:?}"
        );
    }

    /// The same, with the Flatpak's permission to reach the host taken away
    /// (`flatpak run --no-talk-name=org.freedesktop.Flatpak`): waited for,
    /// and never the keyring instead.
    #[test]
    #[ignore = "needs the Flatpak sandbox, without org.freedesktop.Flatpak"]
    fn in_the_flatpak_without_the_host_the_key_is_waited_for() {
        assert!(vinosearch_shared::sandbox::flatpak_id().is_some());
        let s = Sealer::default();
        let e = s.seal(&Key::random().unwrap(), true).unwrap_err();
        assert!(is_unavailable(&e), "{e:#}");
        let e = s.unseal(TPM_HEAD.as_bytes()).unwrap_err();
        assert!(is_unavailable(&e), "{e:#}");
        assert!(!tpm_available());
        assert_eq!(s.new_seal(false), Seal::Host);
    }
}
