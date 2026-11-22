---
title: Privacy
nav_order: 5
---

# Privacy

The only network requests vinoSearch makes are for downloading models and the OpenVINO runtime, and every model file is verified against a pinned SHA-256 checksum. Indexed content and search queries are processed locally.

The index is stored in SQLite at `~/.local/share/vinosearch/index.sqlite` with permissions restricted to your user (`0600` file inside a `0700` directory). Because vector embeddings can be partially inverted back to source text, treat the index with the same sensitivity as the files it covers.

## Encryption at rest

Index encryption is optional and disabled by default ([#4](https://github.com/karanshukla/vinoSearch/issues/4)). Enable it from the settings window's Security page, or by setting `enabled = true` under `[encryption]` in `config.toml`.

- **Full-database encryption:** Uses SQLCipher (AES-256) across the entire database, protecting extracted text, embeddings, filenames, and the FTS5 word index.
- **In-place conversion:** Turning encryption on or off converts the existing database in place without re-indexing, and search behavior remains unchanged.
- **TPM sealing (`systemd-creds`):** On systems with systemd 256+, a random key is sealed via `systemd-creds` to your machine's TPM and user ID. Without a TPM, encryption is refused unless you explicitly allow systemd's host key (`allow_host_key = true`), which `root` can read. If a sealed key can no longer be unsealed (for example, after clearing the TPM or replacing the motherboard), the index is rebuilt from your files rather than left readable.
- **Flatpak support:** The Flatpak seals and unseals keys using the host's `systemd-creds` over `flatpak-spawn --host`, piping the key directly to `systemd-creds` so it binds to the same TPM and user as a native install. If the host cannot be reached, the encrypted index is preserved while waiting and never silently migrated to the keyring.
- **No silent downgrade between backends:** Once an index is sealed with `systemd-creds`, it is never migrated to the keyring; an index is always unsealed with the same mechanism that sealed it.

### Desktop keyring fallback (without `systemd-creds`)

On systems without systemd or with systemd older than 256, the key is stored in your desktop keyring via the freedesktop Secret Service API (GNOME Keyring, KWallet, or compatible backends):

- **Threat model:** Keyring storage is weaker than TPM sealing. Once unlocked at login, any process running under your user session can read the key. The key also travels across your session D-Bus unencrypted (`plain` Secret Service session); since only your own processes can connect to the session bus and could already query the unlocked keyring directly, this does not expand exposure beyond the keyring itself. Keyring encryption primarily protects backups, synced home directories, and removed disks, provided your keyring has a non-empty password (an empty keyring password stores the keyring unencrypted). If you log in without a password (such as autologin or fingerprint), the keyring stays locked until unlocked manually.
- **Locked keyring or missing entry:** While the keyring is locked, the encrypted index is preserved and searches return empty results. The runner prompts once to unlock at startup (timing out after 1 minute), retries in the background every 30 seconds, and re-prompts when you click **Unlock** on the Security page; it never falls back to plaintext. The same applies if no matching item is found in the current keyring (which can happen when switching desktop keyrings). To start over, turn encryption off: if the key still cannot be retrieved after prompting, the index is rebuilt in plaintext from your folders, and re-enabling encryption generates a fresh key.

See [Encryption at rest](how-it-works.md#encryption-at-rest) for low-level implementation details.

## Documents and code

Document indexing is disabled by default because document folders often contain financial statements, tax forms, and bank exports whose extracted text remains readable in the index unless encryption is enabled.

Source code indexing is also disabled by default. When enabled, files named for secrets and files containing private keys or recognizable API tokens are automatically excluded. See [What gets indexed](how-it-works.md#what-gets-indexed) for the full filtering rules.
