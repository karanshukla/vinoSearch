use eframe::egui;

use crate::window::{
    AppPicker, Button, ChipKind, Column, DEFAULT_CODE_MIN_SCORE, DEFAULT_IMAGE_EXTENSIONS,
    DEFAULT_IMAGE_MIN_SCORE, DEFAULT_MEDIA_MAX_MINUTES, DEFAULT_MEDIA_MIN_SCORE,
    DEFAULT_TEXT_MIN_SCORE, Icon, Request, Row, SettingsWindow, Status, Tone, a11y, app_picker,
    apps, count, fetch_progress, icons, score_row, theme, widgets,
};

impl SettingsWindow {
    /// What gets indexed, and how close a result must be to show.
    pub(in crate::window) fn search(
        &mut self,
        ui: &mut egui::Ui,
        status: Option<&Result<Status, String>>,
    ) {
        if let Some(why) = &self.config_broken {
            ui.add_space(10.0);
            widgets::banner(
                ui,
                Tone::Error,
                &format!(
                    "Could not parse {} ({why}). Fix the file before changing settings here.",
                    self.config_path.display()
                ),
                |_| {},
            );
            return;
        }
        let [indexing, results] = widgets::columns(ui, |ui, column| match column {
            Column::Left => self.what_to_index(ui, status),
            Column::Right => self.opening_and_ranking(ui),
        });
        if indexing | results {
            self.save_config();
        }
    }

    pub(in crate::window) fn what_to_index(
        &mut self,
        ui: &mut egui::Ui,
        status: Option<&Result<Status, String>>,
    ) -> bool {
        let p = theme::palette(ui.ctx());
        let mut changed = false;
        let s = match status {
            Some(Ok(s)) => Some(s),
            _ => None,
        };
        let share = |files, chunks| share_of_index(s, files, chunks);
        widgets::group_label(ui, "What to index");
        widgets::group(ui, |ui| {
            Row::new("Text")
                .help("Plain text, Markdown, and HTML files.")
                .show(ui, |ui, _| {
                    widgets::chip(ui, ChipKind::Neutral, "Always on");
                    if let Some(s) = s
                        && let Some(text) = share(s.text_files, Some(s.text_chunks))
                    {
                        widgets::small(ui, text);
                    }
                });
            let mut images = self.config.formats.images_enabled();
            Row::new("Images")
                .help("Search photos by visual content and text (671 MB download).")
                .show(ui, |ui, _| {
                    if widgets::switch(ui, &mut images, "Index images").changed() {
                        self.config.formats.images.enabled = images.then_some(true);
                        changed = true;
                    }
                    if let Some(s) = s
                        && let Some(text) = share(s.image_files, None)
                    {
                        widgets::small(ui, text);
                    }
                });
            if images
                && let Some(s) = s
                && !s.images_model_present
            {
                Row::new("").tint(p.warn_tint).custom(
                    ui,
                    |ui| {
                        ui.horizontal(|ui| {
                            icons::icon(ui, Icon::Warning, 16.0, p.warn);
                            let shown = ui.add(
                                egui::Label::new("Vision model is not downloaded yet.").wrap(),
                            );
                            a11y::live(&shown);
                        });
                        None
                    },
                    |ui, _| {
                        if s.fetching {
                            fetch_progress(ui, s);
                        } else if ui.add(Button::primary("Download").small()).clicked() {
                            self.link.send(Request::FetchModel);
                        }
                    },
                );
            }
            if images {
                Row::new("Image types")
                    .help("Leave blank to restore default extensions.")
                    .show(ui, |ui, named| {
                        let current = self
                            .config
                            .formats
                            .images
                            .extensions
                            .clone()
                            .unwrap_or_else(|| {
                                DEFAULT_IMAGE_EXTENSIONS
                                    .iter()
                                    .map(|&e| e.to_owned())
                                    .collect()
                            })
                            .join(", ");
                        let text = self.image_extensions.get_or_insert(current);
                        let width = ui.available_width().min(220.0);
                        let response = named(ui.add(widgets::field(text, "", width)));
                        if response.lost_focus() {
                            let typed: Vec<String> = text
                                .split([',', ' '])
                                .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
                                .filter(|e| !e.is_empty())
                                .collect();
                            let mut next = self.config.formats.clone();
                            next.images.extensions = (!typed.is_empty()
                                && typed != DEFAULT_IMAGE_EXTENSIONS)
                                .then_some(typed);
                            match next.image_extensions() {
                                Ok(_) => {
                                    if next != self.config.formats {
                                        self.config.formats = next;
                                        changed = true;
                                    }
                                    self.image_extensions = None;
                                }
                                Err(e) => self.notice = Some(format!("{e:#}")),
                            }
                        }
                    });
            }
            let documents = if self.config.encryption.on() {
                "PDF, Office, EPUB, subtitles, and CSV files."
            } else {
                "PDF, Office, EPUB, subtitles, and CSV files. Stored unencrypted unless index \
                 encryption is enabled (Security)."
            };
            Row::new("Documents").help(documents).show(ui, |ui, _| {
                let mut on = self.config.formats.documents_enabled();
                if widgets::switch(ui, &mut on, "Index documents").changed() {
                    self.config.formats.documents.enabled = on.then_some(true);
                    changed = true;
                }
                if let Some(s) = s
                    && let Some(text) = share(s.document_files, Some(s.document_chunks))
                {
                    widgets::small(ui, text);
                }
            });
            Row::new("Code")
                .help("Matched only by “code:” queries. Secrets and ignored files are skipped.")
                .show(ui, |ui, _| {
                    let mut on = self.config.formats.code_enabled();
                    if widgets::switch(ui, &mut on, "Index code").changed() {
                        self.config.formats.code.enabled = on.then_some(true);
                        changed = true;
                    }
                    if let Some(s) = s
                        && let Some(text) = share(s.code_files, Some(s.code_chunks))
                    {
                        widgets::small(ui, text);
                    }
                });
            let sessions_on = self.config.formats.sessions_enabled();
            let (sessions_help, tone) = if sessions_on && !self.config.encryption.on() {
                (
                    "Enable encryption on the Security page to keep transcripts encrypted.",
                    Tone::Warn,
                )
            } else {
                ("Transcripts and summaries.", Tone::Neutral)
            };
            Row::new("vinoWhisper sessions")
                .help(sessions_help)
                .tone(tone)
                .show(ui, |ui, _| {
                    let mut on = sessions_on;
                    if widgets::switch(ui, &mut on, "Index vinoWhisper sessions").changed() {
                        self.config.formats.sessions.enabled = on.then_some(true);
                        changed = true;
                    }
                    if let Some(s) = s
                        && let Some(text) = share(s.session_files, Some(s.session_chunks))
                    {
                        widgets::small(ui, text);
                    }
                });
            let media = self.config.formats.media_enabled();
            Row::new("Media")
                .help(
                    "Search audio and video by sound and video frames. Indexes on AC power only (295 MB download, plus 671 MB if images are off).",
                )
                .show(ui, |ui, _| {
                    let mut on = media;
                    if widgets::switch(ui, &mut on, "Index media").changed() {
                        self.config.formats.media.enabled = on.then_some(true);
                        changed = true;
                    }
                    if let Some(s) = s
                        && let Some(text) = share(s.media_files, Some(s.media_chunks))
                    {
                        widgets::small(ui, text);
                    }
                });
            if media
                && let Some(s) = s
                && !s.ffmpeg
            {
                Row::new("")
                    .help("Requires ffmpeg. Install it using your distribution's package manager.")
                    .tone(Tone::Error)
                    .show(ui, |_, _| {});
            } else if media
                && let Some(s) = s
                && !s.media_model_present
            {
                Row::new("").tint(p.warn_tint).custom(
                    ui,
                    |ui| {
                        ui.horizontal(|ui| {
                            icons::icon(ui, Icon::Warning, 16.0, p.warn);
                            let shown = ui.add(
                                egui::Label::new("Audio and vision models are not downloaded yet.")
                                    .wrap(),
                            );
                            a11y::live(&shown);
                        });
                        None
                    },
                    |ui, _| {
                        if s.fetching {
                            fetch_progress(ui, s);
                        } else if ui.add(Button::primary("Download").small()).clicked() {
                            self.link.send(Request::FetchModel);
                        }
                    },
                );
            } else if media
                && let Some(s) = s
                && s.media_waiting_for_ac > 0
            {
                let n = s.media_waiting_for_ac;
                Row::new("")
                    .help(&format!(
                        "{} {} waiting for AC power.",
                        count(n as i64),
                        if n == 1 { "file is" } else { "files are" }
                    ))
                    .show(ui, |_, _| {});
            }
            if media {
                let mut limit_on = self.config.formats.media_max_minutes().is_some();
                Row::new("Skip long media")
                    .help("Skip audio and video files longer than a set duration.")
                    .show(ui, |ui, _| {
                        if widgets::switch(ui, &mut limit_on, "Skip long media").changed() {
                            self.config.formats.media.max_minutes =
                                limit_on.then_some(DEFAULT_MEDIA_MAX_MINUTES);
                            self.config.formats.media.max_hours = None;
                            changed = true;
                        }
                    });
                if let Some(mut minutes) = self.config.formats.media_max_minutes() {
                    Row::new("Maximum duration")
                        .help("Audio and video files longer than this are not indexed.")
                        .show(ui, |ui, named| {
                            widgets::small(ui, "min");
                            let response = ui
                                .add(
                                    egui::DragValue::new(&mut minutes)
                                        .range(1.0..=1440.0)
                                        .speed(1.0)
                                        .custom_formatter(|m, _| {
                                            if m.fract() == 0.0 {
                                                format!("{m:.0}")
                                            } else {
                                                format!("{m:.1}")
                                            }
                                        }),
                                )
                                // DragValue's resize-arrow cursor reads as a splitter here.
                                .on_hover_cursor(egui::CursorIcon::Text);
                            let response = named(response);
                            if response.changed() {
                                self.config.formats.media.max_minutes = Some(minutes);
                                self.config.formats.media.max_hours = None;
                                self.idle_dirty = true;
                            }
                        });
                }
            }
            if let Some(s) = s
                && s.sessions_unread > 0
            {
                let n = s.sessions_unread;
                Row::new("Unreadable")
                    .help(&format!(
                        "{n} {} could not be read (newer vinoWhisper format or damaged file).",
                        if n == 1 { "session" } else { "sessions" }
                    ))
                    .tone(Tone::Error)
                    .show(ui, |_, _| {});
            }
        });
        changed
    }

    pub(in crate::window) fn opening_and_ranking(&mut self, ui: &mut egui::Ui) -> bool {
        let mut changed = false;
        widgets::group_label(ui, "Opening results");
        let editors = self.editors.get_or_insert_with(apps::editors).clone();
        let players = self.players.get_or_insert_with(apps::players).clone();
        widgets::group(ui, |ui| {
            changed |= app_picker(
                ui,
                &AppPicker {
                    label: "Editor",
                    help: "Opens text results at the matching line.",
                    command_help: "Include {file} and {line} placeholders (run directly without a shell).",
                    hint: "foot -e nvim +{line} {file}",
                    apps: &editors,
                },
                &mut self.config.editor,
                &mut self.editor,
                &mut self.custom_editor,
            );
            changed |= app_picker(
                ui,
                &AppPicker {
                    label: "Player",
                    help: "Plays audio and video from the matching timestamp (mpv, VLC, or Celluloid).",
                    command_help: "Include {file} and {seconds} placeholders (run directly without a shell).",
                    hint: "mpv --start={seconds} {file}",
                    apps: &players,
                },
                &mut self.config.player,
                &mut self.player,
                &mut self.custom_player,
            );
        });

        widgets::group_label(ui, "Match threshold");
        widgets::group_help(ui, "Higher values return fewer, stricter matches.");
        widgets::group(ui, |ui| {
            let text = self.config.formats.text.min_score;
            if let Some(v) = score_row(ui, "Text", text, DEFAULT_TEXT_MIN_SCORE, 0.45..=0.85) {
                self.config.formats.text.min_score = v;
                self.idle_dirty = true;
            }
            let image = self.config.formats.images.min_score;
            if let Some(v) = score_row(ui, "Images", image, DEFAULT_IMAGE_MIN_SCORE, 0.45..=0.75) {
                self.config.formats.images.min_score = v;
                self.idle_dirty = true;
            }
            let code = self.config.formats.code.min_score;
            if let Some(v) = score_row(ui, "Code", code, DEFAULT_CODE_MIN_SCORE, 0.45..=0.85) {
                self.config.formats.code.min_score = v;
                self.idle_dirty = true;
            }
            let media = self.config.formats.media.min_score;
            if let Some(v) = score_row(ui, "Media", media, DEFAULT_MEDIA_MIN_SCORE, 0.45..=0.80) {
                self.config.formats.media.min_score = v;
                self.idle_dirty = true;
            }
        });
        changed
    }
}

/// What a format holds now and its share of the index, so its switch says what
/// it costs. Images count files, as each is one chunk.
fn share_of_index(s: Option<&Status>, files: i64, chunks: Option<i64>) -> Option<String> {
    let s = s.filter(|_| files > 0)?;
    let what = match chunks {
        Some(_) => format!("{} files", count(files)),
        None => format!("{} photos", count(files)),
    };
    let pct = chunks.unwrap_or(files) as f64 * 100.0 / s.chunks.max(1) as f64;
    Some(format!("{what} · {pct:.0}%"))
}
