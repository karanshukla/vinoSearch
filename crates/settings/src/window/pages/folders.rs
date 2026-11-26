use eframe::egui;

use crate::control::Walk;
use crate::window::{
    Button, Column, Confirm, Icon, Path, Request, Row, SettingsWindow, Status, Target, Tone, a11y,
    path_label, theme, tilde, widgets,
};

impl SettingsWindow {
    /// What is indexed and what never is.
    pub(in crate::window) fn folders(
        &mut self,
        ui: &mut egui::Ui,
        status: Option<&Result<Status, String>>,
    ) {
        let reachable = match status {
            Some(Ok(s)) => Some(s),
            Some(Err(why)) => {
                ui.add_space(10.0);
                widgets::banner(ui, Tone::Error, why, |_| {});
                None
            }
            None => None,
        };
        widgets::columns(ui, |ui, column| match column {
            Column::Left => self.included(ui, reachable),
            Column::Right => self.excluded(ui),
        });
    }

    pub(in crate::window) fn included(&mut self, ui: &mut egui::Ui, reachable: Option<&Status>) {
        let p = theme::palette(ui.ctx());
        widgets::group_label(ui, "Included");
        widgets::group_help(ui, "These folders are indexed and watched for changes.");
        let mut roots = reachable.map(|s| s.roots.clone()).unwrap_or_default();
        self.forgetting.retain(|r| roots.contains(r));
        roots.retain(|r| !self.forgetting.contains(r));
        self.joining.retain(|r| !roots.contains(r));
        roots.extend(self.joining.iter().cloned());
        roots.sort();
        self.reindexing.retain(|r, _| roots.contains(r));
        self.reindexed.retain(|r| roots.contains(r));
        let indexing = reachable.is_some_and(|s| s.indexing);
        widgets::group(ui, |ui| {
            if reachable.is_some() && roots.is_empty() {
                widgets::group_section(ui, |ui| {
                    widgets::muted(ui, "No folders are indexed yet.");
                });
            }
            for root in &roots {
                if self.joining.contains(root) {
                    let path = tilde(Path::new(root));
                    Row::new("").custom(
                        ui,
                        |ui| {
                            path_label(ui, Icon::Folder, &path, false);
                            None
                        },
                        |ui, _| {
                            widgets::small(ui, "Waiting to index");
                        },
                    );
                    continue;
                }
                let forget = Confirm::Forget(root.clone());
                let confirming = self.confirm.as_ref() == Some(&forget);
                let path = tilde(Path::new(root));
                let row = if confirming {
                    Row::new("").tint(p.error_tint)
                } else {
                    Row::new("")
                };
                row.custom(
                    ui,
                    |ui| {
                        path_label(ui, Icon::Folder, &path, confirming);
                        None
                    },
                    |ui, _| {
                        let of = |r| a11y::for_what(r, &path);
                        if confirming {
                            if of(ui.add(Button::destructive("Remove and clear index").small()))
                                .clicked()
                            {
                                self.link.send(Request::Forget(root.clone()));
                                self.forgetting.insert(root.clone());
                                self.confirm = None;
                            }
                            if self.cancel_button(ui).clicked() {
                                self.confirm = None;
                            }
                        } else {
                            if of(ui.add(Button::secondary("Remove").small())).clicked() {
                                self.confirm = Some(forget.clone());
                                self.focus_cancel = true;
                            }
                            let walk = reachable.and_then(|s| s.walk(root));
                            let told = reachable.is_some_and(|s| s.walks.is_some());
                            if let Some(&asked) = self.reindexing.get(root)
                                && (!told || walk.is_some_and(|w| w.seq != asked))
                            {
                                self.reindexing.remove(root);
                                self.reindexed.insert(root.clone());
                            }
                            if self.reindexing.contains_key(root) {
                                widgets::small(ui, "Checking…");
                            } else {
                                if of(ui.add_enabled(!indexing, Button::ghost("Re-index").small()))
                                    .on_hover_text("Scan for changes missed by the file watcher")
                                    .clicked()
                                {
                                    self.link.send(Request::Reindex(root.clone()));
                                    self.reindexed.remove(root);
                                    self.reindexing
                                        .insert(root.clone(), walk.map_or(0, |w| w.seq));
                                }
                                if self.reindexed.contains(root)
                                    && let Some(w) = walk
                                {
                                    walk_result(ui, w);
                                }
                            }
                        }
                    },
                );
            }
            self.proposed(ui);
            ui.add_enabled_ui(reachable.is_some(), |ui| {
                self.adder(ui, Target::Root);
            });
        });
    }

    pub(in crate::window) fn excluded(&mut self, ui: &mut egui::Ui) {
        widgets::group_label(ui, "Excluded");
        widgets::group_help(ui, "Skipped even when inside an included folder.");
        if let Some(why) = &self.config_broken {
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
        let mut remove = None;
        widgets::group(ui, |ui| {
            if self.config.exclude.is_empty() {
                widgets::group_section(ui, |ui| {
                    widgets::muted(ui, "No folders are excluded.");
                });
            }
            for (i, entry) in self.config.exclude.iter().enumerate() {
                Row::new("").custom(
                    ui,
                    |ui| {
                        path_label(ui, Icon::Excluded, entry, false);
                        None
                    },
                    |ui, _| {
                        if a11y::for_what(ui.add(Button::secondary("Remove").small()), entry)
                            .clicked()
                        {
                            remove = Some(i);
                        }
                    },
                );
            }
            self.adder(ui, Target::Exclude);
        });
        if let Some(i) = remove {
            self.config.exclude.remove(i);
            self.save_config();
        }
    }

    pub(in crate::window) fn cancel_button(&mut self, ui: &mut egui::Ui) -> egui::Response {
        let cancel = ui.add(Button::ghost("Cancel").small());
        if std::mem::take(&mut self.focus_cancel) {
            cancel.request_focus();
        }
        cancel
    }

    /// "Add folder…" through the desktop's chooser, or a typed path, at the
    /// bottom of a group.
    pub(in crate::window) fn adder(&mut self, ui: &mut egui::Ui, target: Target) {
        widgets::group_section(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                let (button, hint) = match target {
                    Target::Root => ("Add folder…", "or enter a path, e.g. ~/Documents"),
                    Target::Exclude => ("Exclude folder…", "or enter a path, e.g. ~/Downloads/tax"),
                };
                if ui
                    .add_enabled(!self.picking, Button::tonal(button).icon(Icon::Plus))
                    .clicked()
                {
                    self.pick(ui.ctx(), target);
                }
                let text = match target {
                    Target::Root => &mut self.new_root,
                    Target::Exclude => &mut self.new_exclude,
                };
                let room = match ui.available_width() {
                    beside if beside >= 180.0 => beside,
                    _ => ui.max_rect().width(),
                };
                let width = (room - 60.0).clamp(120.0, 320.0);
                let edit = ui.add(widgets::field(text, hint, width));
                ui.ctx()
                    .accesskit_node_builder(edit.id, |node| node.set_label(field_name(target)));
                let entered = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let typed = !text.trim().is_empty();
                let to = match target {
                    Target::Root => "to included",
                    Target::Exclude => "to excluded",
                };
                let add = a11y::for_what(ui.add_enabled(typed, Button::secondary("Add")), to);
                if (add.clicked() || entered) && typed {
                    self.add_typed(target);
                }
            });
        });
    }
}

/// What the path field is for, read out in place of its hint, which is only a
/// placeholder.
fn field_name(target: Target) -> &'static str {
    match target {
        Target::Root => "Folder to add",
        Target::Exclude => "Folder to exclude",
    }
}

/// What a Re-index found: nothing, changes now being indexed, or why it couldn't look.
fn walk_result(ui: &mut egui::Ui, w: &Walk) {
    if !w.error.is_empty() {
        widgets::small(ui, "Check failed").on_hover_text(&w.error);
    } else if w.found == 0 {
        widgets::small(ui, "Up to date");
    } else {
        let s = if w.found == 1 { "" } else { "s" };
        widgets::small(ui, format!("{} change{s} found", w.found));
    }
}
