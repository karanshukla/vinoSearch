use eframe::egui;

use crate::window::*;

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
        widgets::group_help(ui, "Indexed and watched for changes.");
        let mut roots = reachable.map(|s| s.roots.clone()).unwrap_or_default();
        self.forgetting.retain(|r| roots.contains(r));
        roots.retain(|r| !self.forgetting.contains(r));
        self.joining.retain(|r| !roots.contains(r));
        roots.extend(self.joining.iter().cloned());
        roots.sort();
        let indexing = reachable.is_some_and(|s| s.indexing);
        widgets::group(ui, |ui| {
            if reachable.is_some() && roots.is_empty() {
                widgets::group_section(ui, |ui| {
                    widgets::muted(ui, "Nothing is indexed yet.");
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
                            if of(ui.add(Button::destructive("Remove and drop its index").small()))
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
                            if of(ui.add_enabled(!indexing, Button::ghost("Re-index").small()))
                                .on_hover_text("Look for anything the watcher missed")
                                .clicked()
                            {
                                self.link.send(Request::Reindex(root.clone()));
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
        widgets::group_help(ui, "Never indexed, even inside an included folder.");
        if let Some(why) = &self.config_broken {
            widgets::banner(
                ui,
                Tone::Error,
                &format!(
                    "{} doesn't parse, so it isn't changed from here: {why}",
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
                    widgets::muted(ui, "Nothing is excluded.");
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
            // Wrapped: with large text on a narrow window the field and Add go under the button.
            ui.horizontal_wrapped(|ui| {
                let (button, hint) = match target {
                    Target::Root => ("Add folder…", "or type a path, e.g. ~/Documents"),
                    Target::Exclude => ("Exclude folder…", "or type a path, e.g. ~/Downloads/tax"),
                };
                if ui
                    .add_enabled(!self.picking, Button::secondary(button).icon(Icon::Plus))
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
                // The hint is only a placeholder; say what the field is for.
                let name = match target {
                    Target::Root => "Folder to add",
                    Target::Exclude => "Folder to exclude",
                };
                ui.ctx()
                    .accesskit_node_builder(edit.id, |node| node.set_label(name));
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
