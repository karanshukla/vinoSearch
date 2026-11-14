---
title: Privacy
nav_order: 5
---

# Privacy

The only network calls download the model and the runtime, and each model file is checked against a
pinned SHA-256. Nothing you index or search leaves the machine.

The index is SQLite in `~/.local/share/vinosearch/index.sqlite`, readable only by you (0600,
directory 0700). Embeddings can be partly inverted back to their text, so treat the index like the
files it covers.

## Encryption at rest

The index can be encrypted at rest ([#4](https://github.com/karanshukla/vinoSearch/issues/4)): the
settings window's Security page, or `enabled = true` under `[encryption]` in the config file. Off by
default.

- The whole database is SQLCipher (AES-256), so text, vectors, file names and the word index are all covered.
- Its random key is sealed by `systemd-creds` to this machine's TPM and your user.
- Turning it on or off converts the index in place; nothing is indexed again, and search is unchanged.
- Without a TPM it's refused unless you allow systemd's host key, which root can read.
- If the key stops unsealing (TPM cleared, board replaced), the index is rebuilt from your files, never left readable.
- Where `systemd-creds` can't be used (no systemd, or one older than 256), the key goes in your desktop keyring instead (gnome-keyring, KWallet or anything else speaking the Secret Service API). That is weaker than the TPM: the keyring is unlocked when you log in, and from then on anything running as you in your session can read the key. The key also crosses your session bus unencrypted on its way to and from the keyring (a `plain` Secret Service session). Only your own processes can connect to that bus, and any of them could read the key from the unlocked keyring anyway, so this exposes it to no one the keyring doesn't already. What it protects is a backup, a synced copy of your home folder, or a disk taken out: the keyring file on disk is encrypted with your keyring's password, usually your login password. A keyring set up with an empty password is stored unencrypted, and then the key with it. If you log in without a password (autologin, a fingerprint), the keyring stays locked until something asks you to unlock it.
- A machine that seals with `systemd-creds` today keeps doing so; an index is never moved from the TPM to the keyring, and an index sealed one way is always unsealed the same way.
- The Flatpak seals and unseals with the host's `systemd-creds` (through `flatpak-spawn --host`), so its key is sealed to the same TPM and user as a native install's, and the key is handed to the host's `systemd-creds` on a pipe, never through another process. If it can't reach the host, the index is kept and waited for, never moved to the keyring.
- While the keyring is locked, the index is kept as it is and searches find nothing; the runner asks you to unlock it once at start (the prompt gives up after a minute), looks again quietly every 30 seconds, and asks again from the Security page's Unlock button. It never falls back to a plaintext index. The same goes when the keyring has no item for the key: unlike a cleared TPM that may not be for good (another desktop's keyring serving this login, or one not opened yet), so the index isn't dropped for it. To start over instead, turn encryption off: once the key still can't be had after the unlock prompt, the index is rebuilt from your files, and turning encryption on again makes a new key.

Details in [How it works](how-it-works.md#encryption-at-rest).

## Documents and code

Documents are off by default because they're where statements, tax forms and bank exports live, and
their text sits in the index readable unless it's encrypted. Code is off by default too; when it's
on, files named for secrets and any file holding a private key or a recognisable API token are left
out. [What gets indexed](how-it-works.md#what-gets-indexed) has the rules.
