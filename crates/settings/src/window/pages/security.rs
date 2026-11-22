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
            "The index stores extracted text from your files. When encrypted, it can only be \
             read by your user account on this machine, protecting backups, copies of your home \
             folder, and removed disks.",
        );
        if let Some(why) = &self.config_broken {
            widgets::group_help(
                ui,
                &format!(
                    "Could not parse the settings file ({why}). Fix it before changing settings \
                     here."
                ),
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
                    "Stores the key in your desktop keyring (readable by your session after \
                     login) because systemd-creds is unavailable for TPM sealing. Converts in \
                     place without re-indexing."
                } else {
                    "Seals the key to your TPM. Converts in place without re-indexing."
                })
                .show(ui, |ui, _| {
                    let mut on = self.config.encryption.on();
                    if widgets::switch(ui, &mut on, "Encrypt the index").changed() {
                        if self.refused_as_the_runner_would(on, no_tpm) {
                            self.notice = Some(
                                "This machine has no TPM usable by systemd, so the key cannot be \
                                 TPM-sealed. Allow host-key fallback below, or leave encryption \
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
                        "Seals the key with systemd's host key (readable by root). Protects \
                         backups, but not a removed system disk.",
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
                            (ChipKind::Warn, "Encrypted (waiting for key)")
                        }
                        (true, seal::TPM) => (ChipKind::Ok, "Encrypted (TPM-sealed key)"),
                        (true, seal::HOST) => (ChipKind::Ok, "Encrypted (host-key sealed)"),
                        (true, seal::KEYRING) => (ChipKind::Ok, "Encrypted (desktop keyring)"),
                        (true, _) => (
                            ChipKind::Warn,
                            "Encrypted with an in-memory key (key could not be sealed; index \
                             will rebuild on next start)",
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
                        "The index key is currently unavailable ({}). Searches return no results \
                         while the encrypted index is preserved. To start over, turn encryption \
                         off to rebuild the index from your files.",
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
                        "The previous index key could not be unsealed ({}), so the index is being \
                         rebuilt from your files.",
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
            "Previously unencrypted data may remain in free disk space until overwritten. \
             Full-disk encryption also protects free space and swap.",
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
