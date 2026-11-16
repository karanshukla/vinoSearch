use eframe::egui;

use crate::window::*;

impl SettingsWindow {
    /// The models on disk while the search service isn't installed: nothing has them loaded
    /// or is downloading, so they're deleted from here.
    pub(in crate::window) fn model_files(&mut self, ui: &mut egui::Ui) {
        let Some(dir) = self.models_dir.clone() else {
            return;
        };
        let p = theme::palette(ui.ctx());
        let bytes = models::bytes(&dir);
        let deleting = self.confirm == Some(Confirm::DeleteModel);
        widgets::group_label(ui, "EmbeddingGemma 2");
        widgets::group(ui, |ui| {
            let warning = format!(
                "Deletes {} of models. Search and indexing need them downloaded again.",
                size(bytes)
            );
            let row = if deleting {
                Row::new("Download")
                    .help(&warning)
                    .tint(p.error_tint)
                    .tone(Tone::Error)
            } else {
                Row::new("Download").help("Downloading needs the search service running.")
            };
            row.show(ui, |ui, _| {
                if deleting {
                    if ui
                        .add(Button::destructive("Delete the model").small())
                        .clicked()
                    {
                        if let Err(e) = models::delete(&dir) {
                            self.notice = Some(format!("Couldn't delete the model: {e:#}"));
                        }
                        self.confirm = None;
                    }
                    if self.cancel_button(ui).clicked() {
                        self.confirm = None;
                    }
                    return;
                }
                if ui
                    .add_enabled(bytes > 0, Button::secondary("Delete…"))
                    .clicked()
                {
                    self.confirm = Some(Confirm::DeleteModel);
                    self.focus_cancel = true;
                }
                if bytes > 0 {
                    widgets::chip(ui, ChipKind::Neutral, &format!("On disk, {}", size(bytes)));
                } else {
                    widgets::chip(ui, ChipKind::Warn, "Not downloaded");
                }
            });
        });
    }

    pub(in crate::window) fn model(
        &mut self,
        ui: &mut egui::Ui,
        status: Option<&Result<Status, String>>,
    ) {
        let s = match status {
            Some(Ok(s)) => Some(s),
            Some(Err(why)) => {
                ui.add_space(10.0);
                widgets::banner(ui, Tone::Error, why, |_| {});
                None
            }
            None => None,
        };
        // Only with no service at all: one that answers with an error may
        // have the models loaded, or be downloading them.
        let files =
            matches!(&status, Some(Err(why)) if why == NOT_INSTALLED) && self.models_dir.is_some();
        if s.is_none() && !files {
            self.running(ui);
            return;
        }
        widgets::columns(ui, |ui, column| match column {
            Column::Left => match s {
                Some(s) => self.model_on_disk(ui, s),
                None => self.model_files(ui),
            },
            Column::Right => self.running(ui),
        });
    }

    pub(in crate::window) fn model_on_disk(&mut self, ui: &mut egui::Ui, s: &Status) {
        let p = theme::palette(ui.ctx());
        widgets::group_label(ui, "EmbeddingGemma 2");
        let deleting = self.confirm == Some(Confirm::DeleteModel);
        widgets::group(ui, |ui| {
            let warning = format!(
                "Deletes {} of models. Search and indexing stop until you download them \
                 again.",
                size(s.model_bytes)
            );
            let row = if deleting {
                Row::new("Download")
                    .help(&warning)
                    .tint(p.error_tint)
                    .tone(Tone::Error)
            } else {
                Row::new("Download").help("Needed for search and indexing.")
            };
            row.show(ui, |ui, _| {
                if deleting {
                    if ui
                        .add(Button::destructive("Delete the model").small())
                        .clicked()
                    {
                        self.link.send(Request::DeleteModel);
                        self.confirm = None;
                    }
                    if self.cancel_button(ui).clicked() {
                        self.confirm = None;
                    }
                    return;
                }
                // Whatever is on disk, a partial set included.
                let deletable = s.model_bytes > 0 && !s.fetching;
                if ui
                    .add_enabled(deletable, Button::secondary("Delete…"))
                    .clicked()
                {
                    self.confirm = Some(Confirm::DeleteModel);
                    self.focus_cancel = true;
                }
                let label = if s.model_present {
                    "Verify"
                } else {
                    "Download"
                };
                let button = if s.model_present {
                    Button::secondary(label)
                } else {
                    Button::primary(label).icon(Icon::Download)
                };
                if ui.add_enabled(!s.fetching, button).clicked() {
                    self.link.send(Request::FetchModel);
                }
                if s.fetching {
                    fetch_progress(ui, s);
                } else if s.model_present {
                    widgets::chip(
                        ui,
                        ChipKind::Ok,
                        &format!("Downloaded, {}", size(s.model_bytes)),
                    );
                } else if s.model_bytes > 0 {
                    widgets::chip(
                        ui,
                        ChipKind::Warn,
                        &format!("Incomplete, {}", size(s.model_bytes)),
                    );
                } else {
                    widgets::chip(ui, ChipKind::Warn, "Not downloaded");
                }
            });
            if !s.fetch_error.is_empty() {
                Row::new("Last download")
                    .help(&format!("Failed: {}", s.fetch_error))
                    .tone(Tone::Error)
                    .show(ui, |_, _| {});
            }
            Row::new("Memory").show(ui, |ui, _| {
                if ui
                    .add_enabled(s.model_loaded, Button::secondary("Unload now"))
                    .clicked()
                {
                    self.link.send(Request::Unload);
                }
                if s.model_loaded {
                    ui.label(format!("Loaded on {}", s.device));
                } else {
                    widgets::muted(ui, "Not loaded");
                }
            });
            Row::new("Compiled cache")
                .help("Clearing it makes the next load slower.")
                .show(ui, |ui, _| {
                    if ui
                        .add_enabled(s.cache_bytes > 0, Button::secondary("Clear"))
                        .clicked()
                    {
                        self.link.send(Request::ClearCache);
                    }
                    ui.label(size(s.cache_bytes));
                });
        });
    }

    pub(in crate::window) fn running(&mut self, ui: &mut egui::Ui) {
        widgets::group_label(ui, "Running");
        if self.config_broken.is_some() {
            widgets::group_help(ui, "The settings file doesn't parse; see the Folders page.");
            return;
        }
        let mut changed = false;
        widgets::group(ui, |ui| {
            Row::new("Device")
                .help("The NPU is fastest and uses the least power. Without one, NPU means the GPU, else the CPU.")
                .show(ui, |ui, _| {
                    let current = self.config.device.as_deref().unwrap_or(DEFAULT_DEVICE);
                    let mut picked = DEVICES
                        .into_iter()
                        .find(|d| d.eq_ignore_ascii_case(current))
                        .unwrap_or(DEFAULT_DEVICE);
                    let options: Vec<(&str, &str)> = DEVICES.iter().map(|d| (*d, *d)).collect();
                    if widgets::segmented(ui, "Device", &mut picked, &options) {
                        self.config.device = (picked != DEFAULT_DEVICE).then(|| picked.to_owned());
                        changed = true;
                    }
                });
            Row::new("Unload after")
                .help("0 keeps it loaded.")
                .show(ui, |ui, named| {
                    let mut minutes = self
                        .config
                        .idle_unload_minutes
                        .unwrap_or(DEFAULT_IDLE_UNLOAD_MINUTES);
                    widgets::small(ui, "min");
                    let response = ui
                        .add(
                            egui::DragValue::new(&mut minutes)
                                .range(0.0..=1440.0)
                                .speed(0.25)
                                .custom_formatter(|m, _| {
                                    if m.fract() == 0.0 {
                                        format!("{m:.0}")
                                    } else {
                                        format!("{m:.1}")
                                    }
                                }),
                        )
                        // DragValue's own hover cursor is a horizontal resize arrow, which reads as a splitter here.
                        .on_hover_cursor(egui::CursorIcon::Text);
                    let response = named(response);
                    if response.changed() {
                        self.config.idle_unload_minutes = Some(minutes);
                        self.idle_dirty = true;
                    }
                });
            Row::new("Indexing share")
                .help("Halved on battery.")
                .show(ui, |ui, named| {
                    let mut percent = self
                        .config
                        .index_busy_percent
                        .unwrap_or(DEFAULT_INDEX_BUSY_PERCENT);
                    let response = slider(ui, named, &mut percent, 10..=100, 1.0, 0, " %");
                    if response.changed() {
                        self.config.index_busy_percent = Some(percent);
                        self.idle_dirty = true;
                    }
                });
        });
        if changed {
            self.save_config();
        }
    }
}
