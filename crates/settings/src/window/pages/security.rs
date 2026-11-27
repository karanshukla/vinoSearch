use eframe::egui;

use crate::window::{
    Button, ChipKind, Confirm, Icon, Request, Row, SettingsWindow, Status, Tone, a11y, seal, theme,
    widgets,
};

impl SettingsWindow {
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
            "Encrypts the index on disk so only your account on this machine can read it.",
        );
        if self.config_broken_banner(ui) {
            return;
        }
        let keyring = keyring_only(s);
        let no_tpm = s.is_some_and(|s| !s.tpm) && !keyring;
        let needs_host_key = no_tpm && !self.config.encryption.host_key_allowed();
        let switch_disabled = needs_host_key && !self.config.encryption.on();
        let mut changed = false;
        let rekeying = self.confirm == Some(Confirm::Rekey);
        widgets::group(ui, |ui| {
            let encrypt_help = if keyring {
                "Stores the key in your desktop keyring. No re-indexing needed."
            } else if needs_host_key {
                "No usable TPM found. Enable host-key fallback below first."
            } else if no_tpm {
                "Seals the key with systemd's host key. No re-indexing needed."
            } else {
                "Seals the key to your TPM. No re-indexing needed."
            };
            Row::new("Encrypt the index")
                .help(encrypt_help)
                .show(ui, |ui, _| {
                    ui.add_enabled_ui(!switch_disabled, |ui| {
                        let mut on = self.config.encryption.on();
                        if widgets::switch(ui, &mut on, "Encrypt the index").changed() {
                            self.config.encryption.enabled = on.then_some(true);
                            changed = true;
                        }
                    });
                });
            if no_tpm || self.config.encryption.host_key_allowed() {
                Row::new("Host-key fallback")
                    .help("Uses systemd's host key (weaker than TPM; readable by root).")
                    .show(ui, |ui, _| {
                        let mut allow = self.config.encryption.host_key_allowed();
                        if widgets::switch(ui, &mut allow, "Host-key fallback").changed() {
                            self.config.encryption.allow_host_key = allow.then_some(true);
                            changed = true;
                        }
                    });
            }
            let Some(s) = s else {
                return;
            };
            let temporary_key = s.encrypted
                && s.seal_waiting.is_empty()
                && !matches!(s.seal.as_str(), seal::TPM | seal::HOST | seal::KEYRING);
            let (now_help, now_tone) = if !s.encryption_error.is_empty() {
                (s.encryption_error.as_str(), Tone::Error)
            } else if temporary_key {
                (
                    "Key couldn't be sealed; index will rebuild on next start.",
                    Tone::Warn,
                )
            } else {
                ("", Tone::Neutral)
            };
            Row::new("Now")
                .help(now_help)
                .tone(now_tone)
                .show(ui, |ui, _| {
                    let (kind, text) = match (s.encrypted, s.seal.as_str()) {
                        (true, _) if !s.seal_waiting.is_empty() => {
                            (ChipKind::Warn, "Encrypted (waiting for key)")
                        }
                        (true, seal::TPM) => (ChipKind::Ok, "Encrypted (TPM)"),
                        (true, seal::HOST) => (ChipKind::Ok, "Encrypted (host key)"),
                        (true, seal::KEYRING) => (ChipKind::Ok, "Encrypted (keyring)"),
                        (true, _) => (ChipKind::Warn, "Encrypted (temporary key)"),
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
                        "Key unavailable ({}). Unlock to search, or turn encryption off to \
                         rebuild.",
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
                        "Previous key couldn't be unsealed ({}). Rebuilding the index.",
                        s.seal_broken
                    ))
                    .tone(Tone::Warn)
                    .show(ui, |_, _| {});
            }
            if s.encrypted {
                let row = Row::new("Key");
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
