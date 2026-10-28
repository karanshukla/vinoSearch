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
- Needs systemd 256 or newer.

Details in [How it works](how-it-works.md#encryption-at-rest).

## Documents and code

Documents are off by default because they're where statements, tax forms and bank exports live, and
their text sits in the index readable unless it's encrypted. Code is off by default too; when it's
on, files named for secrets and any file holding a private key or a recognisable API token are left
out. [What gets indexed](how-it-works.md#what-gets-indexed) has the rules.
