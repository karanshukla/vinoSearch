use eframe::egui;

use crate::window::{
    Arc, Button, ChipKind, Column, RichText, Row, SettingsWindow, a11y, integration, widgets,
};

impl SettingsWindow {
    pub(in crate::window) fn integration(&mut self, ui: &mut egui::Ui) {
        let state = self.integration.lock().unwrap().clone();
        let (kde, gnome) = integration::desktops();
        let fixed = integration::places_fixed();
        let mut flip: Option<(integration::Switch, bool)> = None;
        let mut switch_row = |ui: &mut egui::Ui,
                              label: &str,
                              help: &str,
                              switch: integration::Switch,
                              absent: Option<&str>| {
            let help = match absent {
                Some(note) => format!("{help} {note}"),
                None => help.to_owned(),
            };
            Row::new(label).help(&help).show(ui, |ui, _| {
                let on = state.is_on(switch);
                let button = if on {
                    Button::secondary("Turn off")
                } else {
                    Button::tonal("Turn on")
                };
                let button = ui.add_enabled(!state.running, button);
                if a11y::for_what(button, label).clicked() {
                    flip = Some((switch, !on));
                }
                if state.running {
                    widgets::chip(ui, ChipKind::Neutral, "Working…");
                } else if on {
                    widgets::chip(ui, ChipKind::Ok, "On");
                } else {
                    widgets::chip(ui, ChipKind::Neutral, "Off");
                }
            });
        };
        widgets::columns(ui, |ui, column| match column {
            Column::Left => {
                widgets::group_label(ui, "Search places");
                widgets::group(ui, |ui| {
                    switch_row(
                        ui,
                        "KRunner",
                        "KRunner (Alt+Space) and Plasma launcher.",
                        integration::Switch::Kde,
                        (!kde).then_some("Plasma isn't running."),
                    );
                    switch_row(
                        ui,
                        "GNOME Shell",
                        if fixed {
                            "GNOME Shell overview."
                        } else {
                            "GNOME Shell overview (applies next login)."
                        },
                        integration::Switch::Gnome,
                        (!gnome).then_some("GNOME isn't running."),
                    );
                    switch_row(
                        ui,
                        "App menu",
                        "Standalone search entry in your app menu.",
                        integration::Switch::Menu,
                        None,
                    );
                });
            }
            Column::Right => {
                widgets::group_label(ui, "Service");
                widgets::group(ui, |ui| {
                    switch_row(
                        ui,
                        "Search service",
                        "Starts at login. Required for desktop search integrations.",
                        integration::Switch::Service,
                        None,
                    );
                    if let Some(output) = &state.output {
                        widgets::group_section(ui, |ui| {
                            ui.add(egui::Label::new(RichText::new(output).monospace()).wrap());
                        });
                    }
                });
                widgets::group_label(ui, "Command line");
                widgets::group(ui, |ui| {
                    for (what, command) in [
                        ("Search", "vinosearch search \"what you're looking for\""),
                        (
                            "Similar photos",
                            "vinosearch similar ~/Pictures/IMG_2041.jpg",
                        ),
                        ("Check the NPU", "vinosearch doctor"),
                    ] {
                        Row::new(what).show(ui, |ui, _| {
                            widgets::mono(ui, command);
                        });
                    }
                });
            }
        });
        if let Some((switch, on)) = flip {
            integration::run(Arc::clone(&self.integration), ui.ctx(), switch, on);
        }
    }
}
