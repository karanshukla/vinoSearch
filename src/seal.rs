//! The index key (#4), sealed with `systemd-creds`, so there is no TPM code of
//! our own.
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

use anyhow::{Context, Result, bail};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use zeroize::{Zeroize, Zeroizing};

/// Root-owned places systemd's tools live, so PATH never picks the binary:
/// merged /usr, split /usr, NixOS.
const SYSTEM_BIN_DIRS: [&str; 3] = ["/usr/bin", "/bin", "/run/current-system/sw/bin"];

fn system_tool(name: &str) -> PathBuf {
    SYSTEM_BIN_DIRS
        .iter()
        .map(|d| Path::new(d).join(name))
        .find(|p| p.is_file())
        .unwrap_or_else(|| Path::new(SYSTEM_BIN_DIRS[0]).join(name))
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
        let mut filled = 0;
        while filled < KEY_BYTES {
            // SAFETY: writes at most the rest of `bytes`.
            let n = unsafe {
                libc::getrandom(bytes[filled..].as_mut_ptr().cast(), KEY_BYTES - filled, 0)
            };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e).context("getrandom");
            }
            filled += n as usize;
        }
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
}

impl Seal {
    /// `Status`'s `seal` values.
    pub fn name(self) -> &'static str {
        match self {
            Seal::Tpm => vinosearch_shared::dbus::seal::TPM,
            Seal::Host => vinosearch_shared::dbus::seal::HOST,
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

/// What a credential from `systemd-creds encrypt` (base64, as it writes
/// them) is sealed with; None for anything but a user-scoped one.
pub fn sealed_with(credential: &[u8]) -> Option<Seal> {
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
    Command::new(system_tool("systemd-analyze"))
        .args(["has-tpm2", "--quiet"])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Why turning encryption on was refused without a TPM.
pub const NO_TPM: &str = "This machine has no TPM that systemd can use, so the index key can't be \
     sealed to it. Allow the host key (weaker) or leave encryption off.";

/// `systemd-creds`, or a stand-in for tests.
#[derive(Clone)]
pub struct Sealer {
    program: PathBuf,
}

impl Default for Sealer {
    fn default() -> Self {
        Sealer {
            program: system_tool("systemd-creds"),
        }
    }
}

impl Sealer {
    #[cfg(test)]
    pub fn at(program: &Path) -> Self {
        Sealer {
            program: program.to_owned(),
        }
    }

    /// Seal `key` for this user. A key sealed with the host key alone is
    /// refused unless `allow_host` says otherwise: never weaker than asked.
    pub fn seal(&self, key: &Key, allow_host: bool) -> Result<(Vec<u8>, Seal)> {
        let out = run(
            &self.program,
            &["--user", &format!("--name={NAME}"), "encrypt", "-", "-"],
            &key.0[..],
        )
        .context("systemd-creds encrypt")?;
        match sealed_with(&out) {
            Some(Seal::Tpm) => Ok((out, Seal::Tpm)),
            Some(Seal::Host) if allow_host => Ok((out, Seal::Host)),
            Some(Seal::Host) => bail!("{NO_TPM}"),
            None => bail!(
                "systemd-creds wrote a credential of a kind vinoSearch doesn't know ({})",
                base64_prefix(&out, 16)
                    .map(|h| h.iter().map(|b| format!("{b:02x}")).collect::<String>())
                    .unwrap_or_else(|| "not base64".into())
            ),
        }
    }

    /// The key in `credential`. Any failure is an error and nothing else:
    /// the caller must not fall back to anything.
    pub fn unseal(&self, credential: &[u8]) -> Result<Key> {
        let out = Zeroizing::new(
            run(
                &self.program,
                &["--user", &format!("--name={NAME}"), "decrypt", "-", "-"],
                credential,
            )
            .context("systemd-creds decrypt")?,
        );
        Key::from_slice(&out)
    }
}

fn run(program: &Path, args: &[&str], input: &[u8]) -> Result<Vec<u8>> {
    // Nothing of ours reaches it; it talks to systemd over its socket.
    let mut child = Command::new(program)
        .args(args)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| program.display().to_string())?;
    // Written from a thread so a full stdout pipe can't deadlock us.
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let mut input = Zeroizing::new(input.to_vec());
    let writer = std::thread::spawn(move || {
        let r = stdin.write_all(&input);
        input.zeroize();
        r
    });
    let out = child.wait_with_output()?;
    let written = writer.join().expect("stdin writer panicked");
    let mut stdout = out.stdout;
    // Status first: a child that exits early breaks the pipe, and its stderr
    // says why.
    if !out.status.success() {
        stdout.zeroize();
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    if let Err(e) = written {
        stdout.zeroize();
        return Err(e.into());
    }
    Ok(stdout)
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
}
