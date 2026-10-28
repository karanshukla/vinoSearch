//! What adding a folder will cost, shown before it is added (#112): how many
//! files, how long and how much index, and the subfolders taking longest, so
//! one can be excluded first. `vinosearch estimate --json` does the walk,
//! with the indexer's own rules, on a thread of its own: a home directory
//! takes seconds.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui::{self, Align, Layout, RichText};
use vino_ui::a11y::for_what;
use vino_ui::icons::{self, Icon};
use vino_ui::theme;
use vino_ui::widgets::{self, Button};

use super::{SettingsWindow, Target, tilde};
use crate::integration;

/// One of the subfolders taking longest.
#[derive(Clone, Debug, PartialEq)]
pub struct Folder {
    pub path: String,
    /// "~12 min, 1,300 files".
    pub summary: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Answer {
    /// "~4,200 files, ~35 min, ~180 MB".
    pub summary: String,
    /// Why the times may be off: they are NPU rates, and indexing runs elsewhere.
    pub note: Option<String>,
    pub folders: Vec<Folder>,
}

/// Estimates one folder; blocking, so it runs off the UI thread.
pub type Estimator = fn(&Path) -> Result<Answer, String>;

/// `vinosearch estimate --json`.
pub fn run(path: &Path) -> Result<Answer, String> {
    let out = Command::new(integration::vinosearch())
        .args(["estimate", "--json"])
        .arg(path)
        .output()
        .map_err(|e| format!("Couldn't run vinosearch: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let last = err.lines().rev().find(|l| !l.trim().is_empty());
        return Err(last
            .map(|l| l.trim_start_matches("error: ").to_owned())
            .unwrap_or_else(|| format!("vinosearch estimate failed ({})", out.status)));
    }
    parse(&out.stdout)
}

fn parse(json: &[u8]) -> Result<Answer, String> {
    let v: serde_json::Value =
        serde_json::from_slice(json).map_err(|e| format!("Couldn't read the estimate: {e}"))?;
    let text = |v: &serde_json::Value, key: &str| v[key].as_str().unwrap_or_default().to_owned();
    Ok(Answer {
        summary: text(&v, "summary"),
        note: v["note"].as_str().map(str::to_owned),
        folders: v["folders"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|f| Folder {
                        path: text(f, "path"),
                        summary: text(f, "summary"),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// A folder chosen to add, waiting on its estimate and then a confirmation.
pub struct Pending {
    pub path: PathBuf,
    answer: Arc<Mutex<Option<Result<Answer, String>>>>,
}

impl Pending {
    pub fn start(path: &Path, estimator: Estimator) -> Self {
        let answer = Arc::new(Mutex::new(None));
        let out = Arc::clone(&answer);
        let to_estimate = path.to_owned();
        std::thread::spawn(move || {
            let got = estimator(&to_estimate);
            *out.lock().unwrap() = Some(got);
        });
        Pending {
            path: path.to_owned(),
            answer,
        }
    }

    fn answer(&self) -> Option<Result<Answer, String>> {
        self.answer.lock().unwrap().clone()
    }
}

impl SettingsWindow {
    /// Starts estimating `path`, replacing any folder already waiting.
    pub(super) fn propose_root(&mut self, path: &Path) {
        self.adding = Some(Pending::start(path, self.estimator));
    }

    /// The folder waiting to be added, under the included ones: its
    /// estimate, its longest subfolders with an Exclude each, and Add.
    pub(super) fn proposed(&mut self, ui: &mut egui::Ui) {
        let Some(pending) = &self.adding else {
            return;
        };
        let p = theme::palette(ui.ctx());
        let path = pending.path.clone();
        let name = tilde(&path);
        let answer = pending.answer();
        let mut add = false;
        let mut cancel = false;
        let mut exclude = None;
        widgets::group_section(ui, |ui| {
            ui.label(RichText::new(format!("Add {name}?")).family(theme::semibold()));
            match &answer {
                None => {
                    // The answer lands on another thread; look for it.
                    ui.ctx().request_repaint_after(Duration::from_millis(100));
                    widgets::chip_busy(ui, widgets::ChipKind::Neutral, "Estimating…");
                }
                Some(Err(why)) => {
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!("Couldn't estimate it: {why}")).color(p.error),
                        )
                        .wrap(),
                    );
                }
                Some(Ok(a)) => {
                    ui.label(&a.summary);
                    if let Some(note) = &a.note {
                        widgets::small(ui, note);
                    }
                    if !a.folders.is_empty() {
                        widgets::small(ui, "Longest to index; excluding one leaves it out:");
                        for f in &a.folders {
                            let entry = tilde(Path::new(&f.path));
                            let excluded = self.config.exclude.contains(&entry);
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 10.0;
                                icons::icon(ui, Icon::Folder, 16.0, p.muted);
                                widgets::mono(ui, &entry);
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    let can = !excluded && self.config_broken.is_none();
                                    let label = if excluded { "Excluded" } else { "Exclude" };
                                    if for_what(
                                        ui.add_enabled(can, Button::secondary(label).small()),
                                        &entry,
                                    )
                                    .clicked()
                                    {
                                        exclude = Some(PathBuf::from(&f.path));
                                    }
                                    widgets::small(ui, &f.summary);
                                });
                            });
                        }
                    }
                }
            }
            ui.horizontal(|ui| {
                if for_what(ui.add(Button::primary("Add and index")), &name).clicked() {
                    add = true;
                }
                if for_what(ui.add(Button::ghost("Cancel")), &format!("adding {name}")).clicked() {
                    cancel = true;
                }
            });
        });
        if let Some(dir) = exclude {
            self.add(Target::Exclude, &dir);
            // Again, without it: the config is saved, and the walk reads it.
            self.propose_root(&path);
        }
        if add {
            self.link
                .send(crate::control::Request::Reindex(path.display().to_string()));
            self.adding = None;
        } else if cancel {
            self.adding = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_vinosearch_estimate_prints() {
        let json = br#"{"root":"/home/u/Dev","files":4213,"seconds":2100,"index_bytes":180000000,
            "up_to_date":0,"summary":"~4,200 files, ~35 min, ~180 MB","note":null,"kinds":{},
            "folders":[{"path":"/home/u/Dev/linux","seconds":900,"files":1300,
            "summary":"~15 min, 1,300 files"}]}"#;
        let a = parse(json).unwrap();
        assert_eq!(a.summary, "~4,200 files, ~35 min, ~180 MB");
        assert_eq!(a.note, None);
        assert_eq!(
            a.folders,
            [Folder {
                path: "/home/u/Dev/linux".into(),
                summary: "~15 min, 1,300 files".into()
            }]
        );
        assert!(parse(b"not json").is_err());
    }
}
