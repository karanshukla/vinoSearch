use eframe::egui;

use crate::window::{
    Button, ChipKind, Confirm, Icon, Request, Row, SettingsWindow, Status, Tone, a11y, seal, theme,
    widgets,
};

impl SettingsWindow {
    /// Never weaker than asked (pinned by
    /// `window::ui_tests::no_tpm_refuses_encryption_until_the_host_key_is_allowed`).
    fn refused_as_the_runner_would(&self, on: bool, no_tpm: bool) -> bool {
        on && no_tpm && !self.config.encryption.host_key_allowed()
    }

    /// Encrypting the index at rest: the switch, what the key is sealed with, and a new
    /// key on demand.
    pub(in crate::window) fn security(
        &mut self,
        ui: &mut egui::Ui,
        status: Option<&Result<Status, String>>,
    ) {
        let p = theme::palette(ui.ctx());
        let s = match status {
            Some(Ok(s)) => Some(s),
            Some(Err(why)) => {
                ui.add_space(10.0);
                widgets::banner(ui, Tone::Error, why, |_| {});
                None
            }
            None => None,
        };
        widgets::group_label(ui, "Encryption at rest");
        widgets::group_help(
            ui,
            "The index holds the text of what it covers. Encrypted, it reads only on this \
             machine, as you: not from a backup, a copy of your home folder or a disk taken out.",
        );
        if let Some(why) = &self.config_broken {
            widgets::group_help(
                ui,
                &format!("The settings file doesn't parse, so it isn't changed from here: {why}"),
            );
            return;
        }
        let keyring = keyring_only(s);
        let no_tpm = s.is_some_and(|s| !s.tpm) && !keyring;
        let mut changed = false;
        let rekeying = self.confirm == Some(Confirm::Rekey);
        widgets::group(ui, |ui| {
            Row::new("Encrypt the index")
                .help(if keyring {
                    "Key kept in your desktop keyring, which your session can read once you \
                     log in; this system has no systemd-creds to seal it to a TPM. Nothing is \
                     re-indexed."
                } else {
                    "Key sealed to the TPM. Nothing is re-indexed."
                })
                .show(ui, |ui, _| {
                    let mut on = self.config.encryption.on();
                    if widgets::switch(ui, &mut on, "Encrypt the index").changed() {
                        if self.refused_as_the_runner_would(on, no_tpm) {
                            self.notice = Some(
                                "This machine has no TPM that systemd can use, so the key can't \
                                 be sealed to it. Allow the host key below, or leave encryption \
                                 off."
                                    .into(),
                            );
                        } else {
                            self.config.encryption.enabled = on.then_some(true);
                            changed = true;
                        }
                    }
                });
            if no_tpm || self.config.encryption.host_key_allowed() {
                Row::new("Without a TPM")
                    .help(
                        "Seals the key with systemd's host key, which root can read: safe in a \
                         backup, not on a disk taken out with the system.",
                    )
                    .show(ui, |ui, _| {
                        let mut allow = self.config.encryption.host_key_allowed();
                        if widgets::switch(ui, &mut allow, "Use the host key instead (weaker)")
                            .changed()
                        {
                            self.config.encryption.allow_host_key = allow.then_some(true);
                            changed = true;
                        }
                    });
            }
            let Some(s) = s else {
                return;
            };
            let error = (!s.encryption_error.is_empty()).then_some(s.encryption_error.as_str());
            Row::new("Now")
                .help(error.unwrap_or(""))
                .tone(Tone::Error)
                .show(ui, |ui, _| {
                    let (kind, text) = match (s.encrypted, s.seal.as_str()) {
                        (true, _) if !s.seal_waiting.is_empty() => {
                            (ChipKind::Warn, "Encrypted, waiting for its key")
                        }
                        (true, seal::TPM) => (
                            ChipKind::Ok,
                            "Encrypted, the key sealed to this machine's TPM",
                        ),
                        (true, seal::HOST) => (
                            ChipKind::Ok,
                            "Encrypted, the key sealed with systemd's host key",
                        ),
                        (true, seal::KEYRING) => {
                            (ChipKind::Ok, "Encrypted, the key in your desktop keyring")
                        }
                        (true, _) => (
                            ChipKind::Warn,
                            "Encrypted with a key held in memory only: nothing could seal it, so \
                             the index is rebuilt at the next start",
                        ),
                        (false, _)
                            if self.config.encryption.on() && s.encryption_error.is_empty() =>
                        {
                            (ChipKind::Neutral, "Encrypting…")
                        }
                        (false, _) => (ChipKind::Neutral, "Not encrypted"),
                    };
                    a11y::live(&widgets::chip(ui, kind, text));
                });
            if !s.seal_waiting.is_empty() {
                Row::new("Waiting")
                    .help(&format!(
                        "The index's key can't be read yet ({}). Until then searches find \
                         nothing, and the index is kept as it is. To start over instead, turn \
                         encryption off: the index is rebuilt from your files.",
                        s.seal_waiting
                    ))
                    .tone(Tone::Warn)
                    .show(ui, |ui, _| {
                        let label = if s.seal == seal::KEYRING {
                            "Unlock…"
                        } else {
                            "Try again"
                        };
                        if ui.add(Button::secondary(label).small()).clicked() {
                            self.link.send(Request::ApplyEncryption);
                        }
                    });
            }
            if !s.seal_broken.is_empty() {
                Row::new("Rebuilt")
                    .help(&format!(
                        "The index's key stopped working ({}), so the index is being built again \
                         from your files.",
                        s.seal_broken
                    ))
                    .tone(Tone::Warn)
                    .show(ui, |_, _| {});
            }
            if s.encrypted {
                let row = Row::new("Key").help("Re-encrypt under a new key.");
                let row = if rekeying {
                    row.tint(p.error_tint)
                } else {
                    row
                };
                row.show(ui, |ui, _| {
                    if rekeying {
                        if ui
                            .add(Button::destructive("Replace the key").small())
                            .clicked()
                        {
                            self.link.send(Request::Rekey);
                            self.confirm = None;
                        }
                        if self.cancel_button(ui).clicked() {
                            self.confirm = None;
                        }
                    } else if ui
                        .add(Button::secondary("New key…").icon(Icon::Key))
                        .clicked()
                    {
                        self.confirm = Some(Confirm::Rekey);
                        self.focus_cancel = true;
                    }
                });
            }
        });
        ui.add_space(8.0);
        widgets::small(
            ui,
            "Old plaintext may linger in free disk space until overwritten; full-disk \
             encryption covers it, and swap.",
        );
        if changed {
            self.save_config();
            if self.config_broken.is_none() {
                self.link.send(Request::ApplyEncryption);
            }
        }
    }
}

/// Without systemd-creds the keyring is all there is: no TPM to ask about
/// (pinned by `window::ui_tests::without_systemd_creds_the_keyring_is_used_and_waited_for`).
fn keyring_only(s: Option<&Status>) -> bool {
    s.is_some_and(|s| s.new_seal == seal::KEYRING)
}
