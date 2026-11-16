use eframe::egui;

use crate::window::{
    Align, Button, ChipKind, Column, LARGEST, LEGEND_SWATCH, Layout, Path, PathBuf, Request,
    RichText, Row, SettingsWindow, Status, Target, Tone, a11y, ago, config, count, kind_colors,
    middle_cut, partial_line, power_hold, size, theme, tilde, widgets,
};

/// Documents indexed in part listed by name on the Index page; the rest are counted.
const PARTIAL_SHOWN: usize = 3;

/// About how wide a letter of the Index page's 12.5 pt rows is.
const LETTER_WIDTH: f32 = 7.0;

impl SettingsWindow {
    /// Beside the Index page's title: what the service is doing, in words.
    pub(in crate::window) fn index_status(
        ui: &mut egui::Ui,
        status: Option<&Result<Status, String>>,
    ) {
        match status {
            None => {
                widgets::chip(ui, ChipKind::Neutral, "Connecting…");
            }
            Some(Err(_)) => {
                widgets::chip(ui, ChipKind::Err, "Service stopped");
            }
            Some(Ok(s)) => {
                if s.paused {
                    widgets::chip(ui, ChipKind::Neutral, "Stopped");
                } else if power_hold(&s.power_hold).is_some() {
                    widgets::chip(ui, ChipKind::Warn, "On hold");
                } else if s.indexing {
                    widgets::chip(ui, ChipKind::Ok, "Indexing");
                } else {
                    widgets::chip(ui, ChipKind::Ok, "Up to date");
                }
            }
        }
    }

    pub(in crate::window) fn index(
        &mut self,
        ui: &mut egui::Ui,
        status: Option<&Result<Status, String>>,
        largest: Option<&Vec<(String, u64)>>,
    ) {
        let indexed = match status {
            Some(Ok(s)) if s.chunks > 0 => Some(s),
            _ => None,
        };
        widgets::columns(ui, |ui, column| match column {
            Column::Left => self.status_rows(ui, status),
            Column::Right => {
                if let Some(s) = indexed {
                    self.largest(ui, s, largest);
                }
            }
        });
    }

    pub(in crate::window) fn status_rows(
        &mut self,
        ui: &mut egui::Ui,
        status: Option<&Result<Status, String>>,
    ) {
        let p = theme::palette(ui.ctx());
        widgets::group_label(ui, "Status");
        let reachable = match status {
            Some(Ok(s)) => Some(s),
            _ => None,
        };
        widgets::group(ui, |ui| {
            match status {
                None => {
                    Row::new("Now")
                        .help("Asking the search service…")
                        .show(ui, |_, _| {});
                }
                Some(Err(why)) => {
                    Row::new("Now")
                        .help(why)
                        .tone(Tone::Error)
                        .show(ui, |_, _| {});
                }
                Some(Ok(_)) => {}
            }
            let Some(s) = reachable else {
                return;
            };
            if s.files > 0 {
                widgets::group_section(ui, |ui| {
                    ui.columns(3, |cols| {
                        widgets::stat(&mut cols[0], &count(s.files), "files");
                        widgets::stat(&mut cols[1], &count(s.chunks), "chunks");
                        widgets::stat(&mut cols[2], &size(s.index_bytes), "on disk");
                    });
                    let total = s.chunks.max(1) as f32;
                    let shares = [
                        (s.text_chunks, "Text"),
                        (s.document_chunks, "Documents"),
                        (s.image_files, "Images"),
                        (s.code_chunks, "Code"),
                        (s.session_chunks, "Sessions"),
                        (s.media_chunks, "Media"),
                    ];
                    let colors = kind_colors(p, ui.visuals().dark_mode);
                    let bar: Vec<(f32, egui::Color32)> = shares
                        .iter()
                        .zip(colors)
                        .map(|((n, _), color)| (*n as f32 / total, color))
                        .collect();
                    ui.add_space(4.0);
                    widgets::share_bar(ui, &bar, 8.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 16.0;
                        for ((n, name), color) in shares.iter().zip(colors) {
                            if *n > 0 {
                                let pct = *n as f64 * 100.0 / f64::from(total);
                                let text = if pct < 0.5 {
                                    format!("{name} <1%")
                                } else {
                                    format!("{name} {:.0}%", pct.round())
                                };
                                legend_entry(ui, p, color, text);
                            }
                        }
                    });
                });
            }
            let now = if s.paused {
                "Indexing stopped. What's indexed so far is kept.".to_owned()
            } else if let Some(why) = power_hold(&s.power_hold) {
                why.to_owned()
            } else if s.indexing && s.indexing_rest {
                "Indexing the rest of long documents".to_owned()
            } else if s.indexing {
                format!("Indexing, {} files so far", count(s.indexed_files as i64))
            } else {
                "Up to date".to_owned()
            };
            Row::new("Now").help(&now).show(ui, |ui, _| {
                if s.paused {
                    if ui.add(Button::secondary("Resume")).clicked() {
                        self.link.send(Request::ResumeIndexing);
                    }
                } else if s.indexing
                    && power_hold(&s.power_hold).is_none()
                    && ui.add(Button::secondary("Stop")).clicked()
                {
                    self.link.send(Request::PauseIndexing);
                }
            });
            if !s.model_changed.is_empty() {
                Row::new("Indexing again")
                    .help(&s.model_changed)
                    .show(ui, |_, _| {});
            }
            if s.no_decoder > 0 {
                Row::new("Skipped")
                    .help(&format!(
                        "{} photos: HEIC and AVIF need heif-convert ({}).",
                        count(s.no_decoder),
                        config::HEIF_CONVERT_PACKAGES
                    ))
                    .tone(Tone::Warn)
                    .show(ui, |_, _| {});
            }
            if !s.partial.is_empty() {
                Row::new("In part")
                    .help("Long documents finish later.")
                    .show(ui, |ui, _| partial_list(ui, &s.partial));
            }
            Row::new("Last change").show(ui, |ui, _| {
                if s.last_indexed == 0 {
                    widgets::muted(ui, "None since the service started");
                } else {
                    ui.label(format!("Indexed {}", ago(s.last_indexed)));
                }
            });
            if self.check_asked.is_some_and(|at| s.last_checked != at) {
                self.check_asked = None;
            }
            Row::new("Checked")
                .help("Also hourly and after sleep.")
                .show(ui, |ui, _| {
                    if self.check_asked.is_some() {
                        let text = if s.indexing {
                            "Waiting for indexing to finish"
                        } else {
                            "Checking…"
                        };
                        widgets::chip(ui, ChipKind::Neutral, text);
                    } else {
                        if ui.add(Button::secondary("Check now")).clicked() {
                            self.check_asked = Some(s.last_checked);
                            self.link.send(Request::CheckNow);
                        }
                        if s.last_checked == 0 {
                            widgets::muted(ui, "Not yet");
                        } else {
                            widgets::muted(ui, ago(s.last_checked));
                        }
                    }
                });
        });
    }

    /// Asked at once, then again whenever the index has changed and indexing
    /// has settled: a first index can run for hours.
    fn largest_is_stale(&self, s: &Status) -> bool {
        (self.largest_for.is_none() || !s.indexing) && self.largest_for != Some(s.chunks)
    }

    /// Where the index goes, with a one-click exclude: generated and copied
    /// trees are often most of it, which nobody would guess without seeing it.
    pub(in crate::window) fn largest(
        &mut self,
        ui: &mut egui::Ui,
        s: &Status,
        largest: Option<&Vec<(String, u64)>>,
    ) {
        let p = theme::palette(ui.ctx());
        if self.largest_is_stale(s) {
            self.largest_for = Some(s.chunks);
            self.link.send(Request::LargestFolders(LARGEST));
        }
        widgets::group_label(ui, "Largest folders");
        widgets::group_help(ui, "Excluding one drops it from the index.");
        let mut exclude = None;
        widgets::group(ui, |ui| {
            let Some(largest) = largest else {
                widgets::group_section(ui, |ui| {
                    widgets::muted(ui, "Counting…");
                });
                return;
            };
            if largest.is_empty() {
                widgets::group_section(ui, |ui| {
                    widgets::muted(ui, "No folders inside the indexed ones yet.");
                });
                return;
            }
            for (path, chunks) in largest {
                let entry = tilde(Path::new(path));
                let excluded = self.config.exclude.contains(&entry);
                let share = *chunks as f32 / s.chunks.max(1) as f32;
                Row::new("").custom(
                    ui,
                    |ui| {
                        ui.spacing_mut().item_spacing.y = 6.0;
                        largest_heading(ui, p, &entry, *chunks, share);
                        let (rect, _) = ui.allocate_exact_size(
                            egui::vec2(ui.available_width(), 4.0),
                            egui::Sense::hover(),
                        );
                        ui.painter().rect_filled(rect, 2.0, p.pressed);
                        let done = egui::Rect::from_min_size(
                            rect.min,
                            egui::vec2(rect.width() * share.clamp(0.0, 1.0), 4.0),
                        );
                        ui.painter().rect_filled(done, 2.0, p.kinds[0]);
                        None
                    },
                    |ui, _| {
                        let can = !excluded && self.config_broken.is_none();
                        let label = if excluded { "Excluded" } else { "Exclude" };
                        if a11y::for_what(
                            ui.add_enabled(can, Button::secondary(label).small()),
                            &entry,
                        )
                        .clicked()
                        {
                            exclude = Some(PathBuf::from(path));
                        }
                    },
                );
            }
        });
        if let Some(path) = exclude {
            self.add(Target::Exclude, &path);
        }
    }
}

/// egui wraps a legend's label rather than move it to the next row, so one
/// that won't fit starts a row of its own (pinned by
/// `window::ui_tests::each_legend_entry_stays_on_one_line`).
fn legend_entry(ui: &mut egui::Ui, p: &theme::Palette, color: egui::Color32, text: String) {
    let width = LEGEND_SWATCH
        + ui.painter()
            .layout_no_wrap(text.clone(), egui::FontId::proportional(12.5), p.muted)
            .size()
            .x;
    if width > ui.available_size_before_wrap().x {
        ui.end_row();
    }
    widgets::legend(ui, color, &text);
}

/// The documents indexed in part, in part of the row only: a long file name
/// is cut short (whole on hover), not laid over the row's label. The name and
/// how far it got are two labels, so the cut never takes the count (pinned by
/// `window::ui_tests::the_index_page_lists_documents_indexed_in_part`).
fn partial_list(ui: &mut egui::Ui, partial: &[crate::control::Partial]) {
    ui.with_layout(Layout::top_down(Align::Max), |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        let width = (ui.available_width() * 0.6).max(160.0);
        for d in partial.iter().take(PARTIAL_SHOWN) {
            let (name, progress) = partial_line(d);
            ui.allocate_ui_with_layout(
                egui::vec2(width, 18.0),
                Layout::right_to_left(Align::Center),
                |ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    ui.label(RichText::new(format!(", {progress}")).size(12.5));
                    let room = (ui.available_width() / LETTER_WIDTH) as usize;
                    ui.add(
                        egui::Label::new(RichText::new(middle_cut(&name, room)).size(12.5))
                            .truncate(),
                    )
                    .on_hover_text(&name);
                },
            );
        }
        let more = partial.len().saturating_sub(PARTIAL_SHOWN);
        if more > 0 {
            widgets::small(ui, format!("and {} more", count(more as i64)));
        }
    });
}

/// The count first, whole and on one line; the path gets what is left and is
/// cut short to fit, so a long path can't leave the count a sliver to wrap in.
fn largest_heading(ui: &mut egui::Ui, p: &theme::Palette, entry: &str, chunks: u64, share: f32) {
    ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width(), 18.0),
        Layout::right_to_left(Align::Center),
        |ui| {
            ui.add(
                egui::Label::new(
                    RichText::new(format!(
                        "{} chunks · {:.0}%",
                        count(chunks as i64),
                        share * 100.0
                    ))
                    .size(12.5)
                    .color(p.muted),
                )
                .extend(),
            );
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                widgets::mono(ui, entry);
            });
        },
    );
}
