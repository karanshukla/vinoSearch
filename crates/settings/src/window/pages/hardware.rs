use eframe::egui;

use crate::window::{Arc, Button, ChipKind, Column, Row, SettingsWindow, Tone, doctor, widgets};

impl SettingsWindow {
    /// What `vinosearch doctor` reports, run the first time the page shows.
    /// The device it probes is the one picked on the Model page.
    pub(in crate::window) fn hardware(&mut self, ui: &mut egui::Ui) {
        widgets::columns(ui, |ui, column| match column {
            Column::Left => self.this_machine(ui),
            Column::Right => self.power(ui),
        });
    }

    pub(in crate::window) fn this_machine(&mut self, ui: &mut egui::Ui) {
        let state = self.doctor.lock().unwrap().clone();
        if state.report.is_none() && !state.running {
            doctor::run(Arc::clone(&self.doctor), ui.ctx());
        }
        widgets::group_label(ui, "This machine");
        widgets::group(ui, |ui| {
            if let Some(report) = &state.report {
                for (label, text) in &report.lines {
                    let failed = label == "Probe:" && !report.ok;
                    let tone = if failed {
                        Tone::Error
                    } else if label == "Warning:" {
                        Tone::Warn
                    } else {
                        Tone::Neutral
                    };
                    Row::new(label.trim_end_matches(':'))
                        .help(text)
                        .tone(tone)
                        .show(ui, |_, _| {});
                }
                if let Some(error) = &report.error {
                    Row::new("Problem")
                        .help(error)
                        .tone(Tone::Error)
                        .show(ui, |_, _| {});
                }
            }
            Row::new("Check")
                .help("Runs the same diagnostics as vinosearch doctor.")
                .show(ui, |ui, _| {
                    if state.running {
                        widgets::chip(ui, ChipKind::Neutral, "Checking…");
                    } else if ui.add(Button::tonal("Check again")).clicked() {
                        doctor::run(Arc::clone(&self.doctor), ui.ctx());
                    }
                });
        });
    }

    pub(in crate::window) fn power(&mut self, ui: &mut egui::Ui) {
        widgets::group_label(ui, "Power");
        if self.config_broken.is_some() {
            widgets::group_help(
                ui,
                "Could not parse the settings file. See the Folders page for details.",
            );
            return;
        }
        widgets::group(ui, |ui| {
            Row::new("Index on battery")
                .help("When off, indexing pauses on battery until plugged in. Power saver mode always pauses indexing.")
                .show(ui, |ui, _| {
                    let mut on = self.config.index_on_battery();
                    if widgets::switch(ui, &mut on, "Index on battery").changed() {
                        self.config.index_on_battery = (!on).then_some(false);
                        self.save_config();
                    }
                });
        });
    }
}
