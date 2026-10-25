use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use eframe::egui::{
    self, Align, Color32, FontId, Id, Layout, RichText, Sense, Stroke, WidgetInfo, accesskit,
};
use vinosearch_shared::config::{
    self, Config, DEFAULT_DEVICE, DEFAULT_IDLE_UNLOAD_MINUTES, DEFAULT_IMAGE_EXTENSIONS,
    DEFAULT_IMAGE_MIN_SCORE, DEFAULT_INDEX_BUSY_PERCENT, DEFAULT_TEXT_MIN_SCORE,
};

use vinosearch_shared::dbus::hold;

use crate::control::{Link, Request, Status};
use crate::{doctor, integration, portal, theme};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Index,
    Folders,
    Search,
    Model,
    Hardware,
    Integration,
}

impl Page {
    const ALL: [Page; 6] = [
        Page::Index,
        Page::Folders,
        Page::Search,
        Page::Model,
        Page::Hardware,
        Page::Integration,
    ];

    fn parse(name: &str) -> Option<Page> {
        Page::ALL
            .into_iter()
            .find(|page| page.title().eq_ignore_ascii_case(name))
    }

    fn title(self) -> &'static str {
        match self {
            Page::Index => "Index",
            Page::Folders => "Folders",
            Page::Search => "Search",
            Page::Model => "Model",
            Page::Hardware => "Hardware",
            Page::Integration => "Integration",
        }
    }
}

const TITLE: &str = "vinoSearch settings";

const DEVICES: [&str; 3] = ["NPU", "GPU", "CPU"];

/// What the Index page says while indexing is held for power (#109).
fn power_hold(key: &str) -> Option<&'static str> {
    match key {
        hold::CHARGER => Some("Waiting for a charger"),
        hold::POWER_SAVER => Some("Paused: power saver"),
        _ => None,
    }
}

fn device_label(device: &'static str) -> &'static str {
    match device {
        "NPU" => "NPU (recommended)",
        "GPU" => "GPU",
        _ => "CPU",
    }
}

// Below this the sidebar folds into tabs in the header, and form labels go above their control.
const SIDEBAR_FROM: f32 = 620.0;
const SIDEBAR_WIDTH: f32 = 180.0;
const LABEL_WIDTH: f32 = 150.0;
const FORM_MAX_WIDTH: f32 = 620.0;
// Between the form and the scroll bar, which sits on the window's right edge.
const SCROLL_GAP: f32 = 24.0;
const TWO_COLUMNS_FROM: f32 = 440.0;
/// Folders listed under Largest folders.
const LARGEST: u32 = 8;

/// What a folder from the chooser is for.
#[derive(Clone, Copy, PartialEq)]
enum Target {
    Root,
    Exclude,
}

/// What the chooser answered: None when it was cancelled.
type Picked = (Target, Result<Option<PathBuf>, String>);

/// A destructive action waiting for a second click.
#[derive(Clone, PartialEq)]
enum Confirm {
    Forget(String),
    DeleteModel,
}

struct SettingsWindow {
    page: Page,
    link: Link,
    config_path: PathBuf,
    config: Config,
    // Set when the file doesn't parse: saving would replace what the user wrote.
    config_broken: Option<String>,
    idle_dirty: bool,
    new_root: String,
    new_exclude: String,
    /// The image extensions as typed, until Enter or leaving the field.
    image_extensions: Option<String>,
    picked: Arc<Mutex<Option<Picked>>>,
    picking: bool,
    confirm: Option<Confirm>,
    /// Move focus to Cancel when a confirmation replaces the button that asked for it, so a
    /// keyboard user isn't left focused on nothing.
    focus_cancel: bool,
    /// `last_checked` when Check now was pressed, until a newer check lands.
    check_asked: Option<u64>,
    /// Roots asked to be forgotten that Status still lists, hidden meanwhile.
    forgetting: HashSet<String>,
    notice: Option<String>,
    integration: Arc<Mutex<integration::State>>,
    doctor: Arc<Mutex<doctor::State>>,
    /// The chunk count Largest folders was last asked for at, to ask again
    /// once the index changes.
    largest_for: Option<i64>,
    focused: bool,
    kde_header: Option<theme::KdeHeader>,
    /// The desktop's text scale as last applied. Only a change is applied again, so a Ctrl +
    /// zoom isn't undone every time the window gets focus.
    text_scale: f32,
}

impl SettingsWindow {
    fn new(ctx: &egui::Context, page: Option<Page>) -> Self {
        let config_path = config::default_path().unwrap_or_default();
        let mut window = SettingsWindow::with(page, Link::start(ctx), config_path);
        window.follow_text_scale(ctx);
        window
    }

    fn follow_text_scale(&mut self, ctx: &egui::Context) {
        let scale = theme::text_scale();
        if scale != self.text_scale {
            self.text_scale = scale;
            ctx.set_zoom_factor(scale);
        }
    }

    fn with(page: Option<Page>, link: Link, config_path: PathBuf) -> Self {
        let mut window = SettingsWindow {
            page: page.unwrap_or(Page::Index),
            link,
            config_path,
            config: Config::default(),
            config_broken: None,
            idle_dirty: false,
            new_root: String::new(),
            new_exclude: String::new(),
            image_extensions: None,
            picked: Arc::new(Mutex::new(None)),
            picking: false,
            confirm: None,
            focus_cancel: false,
            check_asked: None,
            forgetting: HashSet::new(),
            notice: None,
            integration: Arc::new(Mutex::new(integration::State::read())),
            doctor: Arc::default(),
            largest_for: None,
            focused: true,
            kde_header: theme::kde_header(),
            text_scale: 1.0,
        };
        window.reload_config();
        window
    }

    fn reload_config(&mut self) {
        match Config::load(&self.config_path) {
            Ok(c) => {
                self.config = c;
                self.config_broken = None;
            }
            Err(e) => self.config_broken = Some(format!("{e:#}")),
        }
    }

    fn save_config(&mut self) {
        if self.config_broken.is_some() {
            return;
        }
        if let Err(e) = self.config.save(&self.config_path) {
            self.notice = Some(format!("Couldn't save the settings: {e:#}"));
        }
    }

    fn pick(&mut self, ctx: &egui::Context, target: Target) {
        self.picking = true;
        let picked = Arc::clone(&self.picked);
        let ctx = ctx.clone();
        let title = match target {
            Target::Root => "Add a folder to the index",
            Target::Exclude => "Exclude a folder",
        };
        std::thread::spawn(move || {
            let answer = portal::pick_folder(title).map_err(|e| format!("{e:#}"));
            *picked.lock().unwrap() = Some((target, answer));
            ctx.request_repaint();
        });
    }

    fn add(&mut self, target: Target, path: &Path) {
        match target {
            Target::Root => self.link.send(Request::Reindex(path.display().to_string())),
            Target::Exclude => {
                let entry = tilde(path);
                if !self.config.exclude.contains(&entry) {
                    self.config.exclude.push(entry);
                    self.save_config();
                }
            }
        }
    }

    /// A typed path: `~/` expanded, then it must be absolute.
    fn add_typed(&mut self, target: Target) {
        let typed = match target {
            Target::Root => std::mem::take(&mut self.new_root),
            Target::Exclude => std::mem::take(&mut self.new_exclude),
        };
        let typed = typed.trim();
        let path = match (typed.strip_prefix("~/"), home()) {
            (Some(rest), Some(home)) => home.join(rest),
            _ if typed == "~" => home().unwrap_or_default(),
            _ => PathBuf::from(typed),
        };
        if !path.is_absolute() {
            self.notice = Some(format!("{typed}: use a full path, or one starting with ~/"));
            return;
        }
        self.add(target, &path);
    }

    fn index(
        &mut self,
        ui: &mut egui::Ui,
        status: Option<&Result<Status, String>>,
        largest: Option<&Vec<(String, u64)>>,
    ) {
        let p = theme::palette(ui.ctx());
        section(ui, "Status");
        let reachable = match status {
            None => {
                row(ui, "Now:", "", |ui| {
                    value(
                        ui,
                        RichText::new("Asking the search service…").color(p.muted),
                    );
                });
                None
            }
            Some(Err(why)) => {
                row(ui, "Now:", "", |ui| {
                    let color = ui.visuals().error_fg_color;
                    value(ui, RichText::new(why).color(color));
                });
                None
            }
            Some(Ok(s)) => Some(s),
        };
        if let Some(s) = reachable {
            row(ui, "Now:", "", |ui| {
                ui.horizontal(|ui| {
                    ui.set_min_height(ui.spacing().interact_size.y);
                    if s.paused {
                        ui.label("Indexing stopped. What's indexed so far is kept.");
                        if ui.button("Resume").clicked() {
                            self.link.send(Request::ResumeIndexing);
                        }
                    } else if let Some(why) = power_hold(&s.power_hold) {
                        ui.label(why);
                    } else if s.indexing {
                        spinner(ui);
                        ui.label(format!(
                            "Indexing, {} files so far",
                            count(s.indexed_files as i64)
                        ));
                        if ui.button("Stop").clicked() {
                            self.link.send(Request::PauseIndexing);
                        }
                    } else {
                        ui.label("Up to date");
                    }
                });
                // Why it is indexing after a start: a model changed (#80).
                if !s.model_changed.is_empty() {
                    ui.add(
                        egui::Label::new(RichText::new(&s.model_changed).size(12.5).color(p.muted))
                            .wrap(),
                    );
                }
            });
            row(ui, "Contents:", "", |ui| {
                value(
                    ui,
                    format!(
                        "{} files, {} chunks, {} on disk",
                        count(s.files),
                        count(s.chunks),
                        size(s.index_bytes)
                    ),
                );
            });
            if s.no_decoder > 0 {
                row(
                    ui,
                    "Skipped:",
                    &format!(
                        "HEIC and AVIF need heif-convert: {}.",
                        config::HEIF_CONVERT_PACKAGES
                    ),
                    |ui| {
                        let color = ui.visuals().warn_fg_color;
                        value(
                            ui,
                            RichText::new(format!(
                                "{} photos, no HEIF decoder installed",
                                count(s.no_decoder)
                            ))
                            .color(color),
                        );
                    },
                );
            }
            row(ui, "Last change:", "", |ui| {
                if s.last_indexed == 0 {
                    value(
                        ui,
                        RichText::new("None since the service started").color(p.muted),
                    );
                } else {
                    value(ui, format!("Indexed {}", ago(s.last_indexed)));
                }
            });
            if self.check_asked.is_some_and(|at| s.last_checked != at) {
                self.check_asked = None;
            }
            row(
                ui,
                "Checked:",
                "Also checked hourly and after sleep, for missed changes.",
                |ui| {
                    ui.horizontal(|ui| {
                        ui.set_min_height(ui.spacing().interact_size.y);
                        if self.check_asked.is_some() {
                            spinner(ui);
                            ui.label(if s.indexing {
                                "Waiting for indexing to finish"
                            } else {
                                "Checking…"
                            });
                        } else {
                            ui.label(if s.last_checked == 0 {
                                "Not yet".to_owned()
                            } else {
                                ago(s.last_checked)
                            });
                            if ui.button("Check now").clicked() {
                                self.check_asked = Some(s.last_checked);
                                self.link.send(Request::CheckNow);
                            }
                        }
                    });
                },
            );
        }
        if let Some(s) = reachable.filter(|s| s.chunks > 0) {
            self.largest(ui, s, largest);
        }
    }

    /// What is indexed and what never is.
    fn folders(&mut self, ui: &mut egui::Ui, status: Option<&Result<Status, String>>) {
        let p = theme::palette(ui.ctx());
        let reachable = match status {
            Some(Ok(s)) => Some(s),
            Some(Err(why)) => {
                ui.add_space(10.0);
                ui.label(RichText::new(why).color(ui.visuals().error_fg_color));
                None
            }
            None => None,
        };
        section(ui, "Included");
        help_line(ui, "Indexed and watched for changes.");
        let mut roots = reachable.map(|s| s.roots.clone()).unwrap_or_default();
        self.forgetting.retain(|r| roots.contains(r));
        roots.retain(|r| !self.forgetting.contains(r));
        let indexing = reachable.is_some_and(|s| s.indexing);
        if reachable.is_some() && roots.is_empty() {
            ui.label(RichText::new("Nothing is indexed yet.").color(p.muted));
        }
        for root in &roots {
            let forget = Confirm::Forget(root.clone());
            listed(ui, &tilde(Path::new(root)), |ui, of| {
                if self.confirm.as_ref() == Some(&forget) {
                    if self.cancel_button(ui).clicked() {
                        self.confirm = None;
                    }
                    if of(ui.button("Remove and drop its index")).clicked() {
                        self.link.send(Request::Forget(root.clone()));
                        self.forgetting.insert(root.clone());
                        self.confirm = None;
                    }
                } else {
                    if of(ui.button("Remove")).clicked() {
                        self.confirm = Some(forget);
                        self.focus_cancel = true;
                    }
                    if of(ui.add_enabled(!indexing, egui::Button::new("Re-index")))
                        .on_hover_text("Look for anything the watcher missed")
                        .clicked()
                    {
                        self.link.send(Request::Reindex(root.clone()));
                    }
                }
            });
        }
        ui.add_enabled_ui(reachable.is_some(), |ui| {
            self.adder(ui, Target::Root);
        });

        section(ui, "Excluded");
        help_line(ui, "Never indexed, even inside an included folder.");
        if let Some(why) = &self.config_broken {
            ui.label(
                RichText::new(format!(
                    "{} doesn't parse, so it isn't changed from here: {why}",
                    self.config_path.display()
                ))
                .color(ui.visuals().error_fg_color),
            );
            return;
        }
        if self.config.exclude.is_empty() {
            ui.label(RichText::new("Nothing is excluded.").color(p.muted));
        }
        let mut remove = None;
        for (i, entry) in self.config.exclude.iter().enumerate() {
            listed(ui, entry, |ui, of| {
                if of(ui.button("Remove")).clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            self.config.exclude.remove(i);
            self.save_config();
        }
        self.adder(ui, Target::Exclude);
    }

    /// Where the index goes, with a one-click exclude: generated and copied
    /// trees are often most of it, which nobody would guess without seeing it.
    fn largest(&mut self, ui: &mut egui::Ui, s: &Status, largest: Option<&Vec<(String, u64)>>) {
        let p = theme::palette(ui.ctx());
        // Asked at once, then again whenever the index has changed and indexing
        // has settled: a first index can run for hours.
        if (self.largest_for.is_none() || !s.indexing) && self.largest_for != Some(s.chunks) {
            self.largest_for = Some(s.chunks);
            self.link.send(Request::LargestFolders(LARGEST));
        }
        section(ui, "Largest folders");
        help_line(ui, "Excluding one drops it from the index.");
        let Some(largest) = largest else {
            ui.label(RichText::new("Counting…").color(p.muted));
            return;
        };
        if largest.is_empty() {
            ui.label(RichText::new("No folders inside the indexed ones yet.").color(p.muted));
            return;
        }
        let mut exclude = None;
        for (path, chunks) in largest {
            let entry = tilde(Path::new(path));
            let excluded = self.config.exclude.contains(&entry);
            listed(ui, &entry, |ui, of| {
                let can = !excluded && self.config_broken.is_none();
                let label = if excluded { "Excluded" } else { "Exclude" };
                if of(ui.add_enabled(can, egui::Button::new(label))).clicked() {
                    exclude = Some(PathBuf::from(path));
                }
                let share = *chunks as f64 * 100.0 / s.chunks.max(1) as f64;
                ui.label(
                    RichText::new(format!("{} chunks, {share:.0}%", count(*chunks as i64)))
                        .color(p.muted),
                );
            });
        }
        if let Some(path) = exclude {
            self.add(Target::Exclude, &path);
        }
    }

    fn cancel_button(&mut self, ui: &mut egui::Ui) -> egui::Response {
        let cancel = ui.button("Cancel");
        if std::mem::take(&mut self.focus_cancel) {
            cancel.request_focus();
        }
        cancel
    }

    /// "Add folder…" through the desktop's chooser, or a typed path.
    fn adder(&mut self, ui: &mut egui::Ui, target: Target) {
        ui.add_space(4.0);
        // Wrapped: with large text on a narrow window the field and Add go under the button.
        ui.horizontal_wrapped(|ui| {
            let (button, hint) = match target {
                Target::Root => ("Add folder…", "or type a path, e.g. ~/Documents"),
                Target::Exclude => ("Exclude folder…", "or type a path, e.g. ~/Downloads/tax"),
            };
            if ui
                .add_enabled(!self.picking, egui::Button::new(button))
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
            let width = (room - 60.0).clamp(120.0, 300.0);
            let edit = ui.add(
                egui::TextEdit::singleline(text)
                    .hint_text(hint)
                    .desired_width(width)
                    // As tall as the buttons beside it.
                    .min_size(egui::vec2(0.0, ui.spacing().interact_size.y))
                    .align(egui::Align2::LEFT_CENTER),
            );
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
            let add = for_what(ui.add_enabled(typed, egui::Button::new("Add")), to);
            if (add.clicked() || entered) && typed {
                self.add_typed(target);
            }
        });
    }

    fn model(&mut self, ui: &mut egui::Ui, status: Option<&Result<Status, String>>) {
        let p = theme::palette(ui.ctx());
        let s = match status {
            Some(Ok(s)) => Some(s),
            Some(Err(why)) => {
                ui.add_space(10.0);
                ui.label(RichText::new(why).color(ui.visuals().error_fg_color));
                None
            }
            None => None,
        };
        if let Some(s) = s {
            section(ui, "EmbeddingGemma 2");
            row(ui, "Download:", "Needed for search and indexing.", |ui| {
                // Status and buttons on one line, as on the rows below.
                ui.horizontal_wrapped(|ui| {
                    ui.set_min_height(ui.spacing().interact_size.y);
                    if s.fetching {
                        let fraction = s.fetch_done as f32 / s.fetch_total.max(1) as f32;
                        ui.add(
                            egui::ProgressBar::new(fraction)
                                .text(format!("{} of {}", size(s.fetch_done), size(s.fetch_total)))
                                .desired_width(220.0),
                        );
                    } else if s.model_present {
                        ui.label(format!("Downloaded, {}", size(s.model_bytes)));
                    } else {
                        let color = ui.visuals().warn_fg_color;
                        ui.label(RichText::new("Not downloaded").color(color));
                    }
                    let label = if s.model_present {
                        "Verify"
                    } else {
                        "Download"
                    };
                    if ui
                        .add_enabled(!s.fetching, egui::Button::new(label))
                        .clicked()
                    {
                        self.link.send(Request::FetchModel);
                    }
                    let deletable = s.model_present && !s.fetching;
                    if self.confirm == Some(Confirm::DeleteModel) {
                        if self.cancel_button(ui).clicked() {
                            self.confirm = None;
                        }
                        if ui.button("Delete the model").clicked() {
                            self.link.send(Request::DeleteModel);
                            self.confirm = None;
                        }
                    } else if ui
                        .add_enabled(deletable, egui::Button::new("Delete…"))
                        .clicked()
                    {
                        self.confirm = Some(Confirm::DeleteModel);
                        self.focus_cancel = true;
                    }
                });
                if !s.fetch_error.is_empty() {
                    ui.label(
                        RichText::new(format!("The last download failed: {}", s.fetch_error))
                            .color(ui.visuals().error_fg_color),
                    );
                }
            });
            row(ui, "Memory:", "", |ui| {
                ui.horizontal(|ui| {
                    if s.model_loaded {
                        ui.label(format!("Loaded on {}", s.device));
                    } else {
                        ui.label(RichText::new("Not loaded").color(p.muted));
                    }
                    if ui
                        .add_enabled(s.model_loaded, egui::Button::new("Unload now"))
                        .clicked()
                    {
                        self.link.send(Request::Unload);
                    }
                });
            });
            row(
                ui,
                "Compiled cache:",
                "Clearing it makes the next load slower.",
                |ui| {
                    ui.horizontal(|ui| {
                        ui.label(size(s.cache_bytes));
                        if ui
                            .add_enabled(s.cache_bytes > 0, egui::Button::new("Clear"))
                            .clicked()
                        {
                            self.link.send(Request::ClearCache);
                        }
                    });
                },
            );
        }

        section(ui, "Running");
        if self.config_broken.is_some() {
            help_line(ui, "The settings file doesn't parse; see the Folders page.");
            return;
        }
        let mut changed = false;
        labelled_row(
            ui,
            "Device:",
            "The NPU is fastest and uses the least power.",
            |ui, named| {
                let current = self.config.device.as_deref().unwrap_or(DEFAULT_DEVICE);
                let mut picked = DEVICES
                    .into_iter()
                    .find(|d| d.eq_ignore_ascii_case(current))
                    .unwrap_or(DEFAULT_DEVICE);
                let before = picked;
                let combo = egui::ComboBox::from_id_salt("device")
                    .width(ui.available_width().min(240.0))
                    .selected_text(device_label(picked))
                    .show_ui(ui, |ui| {
                        for d in DEVICES {
                            ui.selectable_value(&mut picked, d, device_label(d));
                        }
                    });
                named(combo.response);
                if picked != before {
                    self.config.device = (picked != DEFAULT_DEVICE).then(|| picked.to_owned());
                    changed = true;
                }
            },
        );
        labelled_row(ui, "Unload after:", "0 keeps it loaded.", |ui, named| {
            let mut minutes = self
                .config
                .idle_unload_minutes
                .unwrap_or(DEFAULT_IDLE_UNLOAD_MINUTES);
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
                        })
                        .suffix(" min"),
                )
                // DragValue's own hover cursor is a horizontal resize arrow, which reads as a splitter here.
                .on_hover_cursor(egui::CursorIcon::Text);
            let response = named(response);
            if response.changed() {
                self.config.idle_unload_minutes = Some(minutes);
                self.idle_dirty = true;
            }
        });
        labelled_row(ui, "Indexing share:", "Halved on battery.", |ui, named| {
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
        if changed {
            self.save_config();
        }
    }

    /// What gets indexed, and how close a result must be to show.
    fn search(&mut self, ui: &mut egui::Ui, status: Option<&Result<Status, String>>) {
        if let Some(why) = &self.config_broken {
            ui.add_space(10.0);
            ui.label(
                RichText::new(format!(
                    "{} doesn't parse, so it isn't changed from here: {why}",
                    self.config_path.display()
                ))
                .color(ui.visuals().error_fg_color),
            );
            return;
        }
        let mut changed = false;
        let s = match status {
            Some(Ok(s)) => Some(s),
            _ => None,
        };
        // What a format holds now and its share of the index, so the switch
        // says what it costs (#73).
        let share = |files: i64, chunks: Option<i64>| -> Option<String> {
            let s = s.filter(|_| files > 0)?;
            let what = match chunks {
                Some(c) => format!("{} files, {} chunks", count(files), count(c)),
                None => format!("{} photos", count(files)),
            };
            let pct = chunks.unwrap_or(files) as f64 * 100.0 / s.chunks.max(1) as f64;
            Some(format!("{what}, {pct:.0}% of the index"))
        };
        let p = theme::palette(ui.ctx());
        section(ui, "What to index");
        row(ui, "Text:", "Notes, Markdown and source code.", |ui| {
            ui.horizontal(|ui| {
                ui.set_min_height(ui.spacing().interact_size.y);
                ui.label("On");
                if let Some(s) = s
                    && let Some(text) = share(s.text_files, Some(s.text_chunks))
                {
                    ui.label(RichText::new(text).color(p.muted));
                }
            });
        });
        row(
            ui,
            "Images:",
            "Found by what's in them. A 671 MB download.",
            |ui| {
                let mut on = self.config.formats.images_enabled();
                ui.horizontal(|ui| {
                    if ui.checkbox(&mut on, "Index images").changed() {
                        self.config.formats.images.enabled = on.then_some(true);
                        changed = true;
                    }
                    if let Some(s) = s
                        && let Some(text) = share(s.image_files, None)
                    {
                        ui.label(RichText::new(text).color(p.muted));
                    }
                });
                if on
                    && let Some(Ok(s)) = status
                    && !s.model_present
                {
                    ui.horizontal_wrapped(|ui| {
                        let color = ui.visuals().warn_fg_color;
                        ui.label(
                            RichText::new("The vision model isn't downloaded yet.").color(color),
                        );
                        if ui
                            .add_enabled(!s.fetching, egui::Button::new("Download"))
                            .clicked()
                        {
                            self.link.send(Request::FetchModel);
                        }
                    });
                }
            },
        );
        if self.config.formats.images_enabled() {
            labelled_row(
                ui,
                "Image types:",
                "Comma-separated. Empty restores the defaults.",
                |ui, named| {
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
                    let response =
                        named(ui.add(egui::TextEdit::singleline(text).desired_width(320.0)));
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
                },
            );
        }

        row(
            ui,
            "Documents:",
            "PDF, Office, RTF, EPUB, subtitles and CSV. Their text is stored unencrypted.",
            |ui| {
                let mut on = self.config.formats.documents_enabled();
                ui.horizontal(|ui| {
                    if ui.checkbox(&mut on, "Index documents").changed() {
                        self.config.formats.documents.enabled = on.then_some(true);
                        changed = true;
                    }
                    if let Some(s) = s
                        && let Some(text) = share(s.document_files, Some(s.document_chunks))
                    {
                        ui.label(RichText::new(text).color(p.muted));
                    }
                });
            },
        );

        section(ui, "How close results must be");
        help_line(ui, "Right for more results, left for fewer, closer ones.");
        let text = self.config.formats.text.min_score;
        if let Some(v) = score_row(ui, "Text:", "", text, DEFAULT_TEXT_MIN_SCORE, 0.85..=0.45) {
            self.config.formats.text.min_score = v;
            self.idle_dirty = true;
        }
        let image = self.config.formats.images.min_score;
        if let Some(v) = score_row(
            ui,
            "Images:",
            "",
            image,
            DEFAULT_IMAGE_MIN_SCORE,
            0.75..=0.45,
        ) {
            self.config.formats.images.min_score = v;
            self.idle_dirty = true;
        }
        if changed {
            self.save_config();
        }
    }

    /// What `vinosearch doctor` reports, run the first time the page shows.
    /// The device it probes is the one picked on the Model page.
    fn hardware(&mut self, ui: &mut egui::Ui) {
        let state = self.doctor.lock().unwrap().clone();
        if state.report.is_none() && !state.running {
            doctor::run(Arc::clone(&self.doctor), ui.ctx());
        }
        section(ui, "This machine");
        if let Some(report) = &state.report {
            for (label, text) in &report.lines {
                let failed = label == "Probe:" && !report.ok;
                row(ui, label, "", |ui| {
                    let text = RichText::new(text);
                    value(
                        ui,
                        if failed {
                            text.color(ui.visuals().error_fg_color)
                        } else if label == "Warning:" {
                            text.color(ui.visuals().warn_fg_color)
                        } else {
                            text
                        },
                    );
                });
            }
            if let Some(error) = &report.error {
                row(ui, "Problem:", "", |ui| {
                    let color = ui.visuals().error_fg_color;
                    ui.add(egui::Label::new(RichText::new(error).color(color)).wrap());
                });
            }
        }
        row(ui, "", "Same as vinosearch doctor.", |ui| {
            ui.horizontal(|ui| {
                ui.set_min_height(ui.spacing().interact_size.y);
                if state.running {
                    spinner(ui);
                    ui.label("Checking…");
                } else if ui.button("Check again").clicked() {
                    doctor::run(Arc::clone(&self.doctor), ui.ctx());
                }
            });
        });

        section(ui, "Power");
        if self.config_broken.is_some() {
            help_line(ui, "The settings file doesn't parse; see the Folders page.");
            return;
        }
        row(
            ui,
            "Battery:",
            "Off holds indexing until a charger is plugged in. Indexing also pauses while power saver is on.",
            |ui| {
                let mut on = self.config.index_on_battery();
                if ui.checkbox(&mut on, "Index on battery").changed() {
                    self.config.index_on_battery = (!on).then_some(false);
                    self.save_config();
                }
            },
        );
    }

    fn integration(&mut self, ui: &mut egui::Ui) {
        let p = theme::palette(ui.ctx());
        let state = self.integration.lock().unwrap().clone();
        let (kde, gnome) = integration::desktops();
        section(ui, "Search places");
        help_line(
            ui,
            "Each place has its own switch; only what's on is installed.",
        );
        let mut flip: Option<(integration::Switch, bool)> = None;
        let mut switch_row = |ui: &mut egui::Ui,
                              label: &str,
                              help: &str,
                              switch: integration::Switch,
                              absent: Option<&str>| {
            row(ui, label, help, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.set_min_height(ui.spacing().interact_size.y);
                    let on = state.is_on(switch);
                    if state.running {
                        spinner(ui);
                        ui.label("Working…");
                    } else if on {
                        ui.label("On");
                    } else {
                        ui.label(RichText::new("Off").color(p.muted));
                    }
                    let button = if on { "Turn off" } else { "Turn on" };
                    let button = ui.add_enabled(!state.running, egui::Button::new(button));
                    if for_what(button, label.trim_end_matches(':')).clicked() {
                        flip = Some((switch, !on));
                    }
                    if let Some(note) = absent {
                        ui.label(RichText::new(note).color(p.muted));
                    }
                });
            });
        };
        switch_row(
            ui,
            "KRunner:",
            "Alt+Space, and the search in the Plasma app launcher.",
            integration::Switch::Kde,
            (!kde).then_some("Plasma isn't running in this session."),
        );
        switch_row(
            ui,
            "GNOME Shell:",
            "The overview's search, from the next login.",
            integration::Switch::Gnome,
            (!gnome).then_some("GNOME isn't running in this session."),
        );
        section(ui, "Service");
        switch_row(
            ui,
            "Search service:",
            "Runs at login. Turning it off removes every search place too.",
            integration::Switch::Service,
            None,
        );
        if let Some((switch, on)) = flip {
            integration::run(Arc::clone(&self.integration), ui.ctx(), switch, on);
        }
        if let Some(output) = &state.output {
            row(ui, "", "", |ui| {
                egui::Frame::NONE
                    .fill(p.code_bg)
                    .stroke(Stroke::new(1.0_f32, p.rule))
                    .corner_radius(4.0)
                    .inner_margin(egui::Margin::symmetric(8, 6))
                    .show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new(output).monospace()).wrap());
                    });
            });
        }
        section(ui, "Command line");
        row(ui, "Search:", "", |ui| {
            command(ui, "vinosearch search \"what you're looking for\"");
        });
        row(ui, "Similar photos:", "", |ui| {
            command(ui, "vinosearch similar ~/Pictures/IMG_2041.jpg");
        });
        row(ui, "Check the NPU:", "", |ui| {
            command(ui, "vinosearch doctor")
        });
    }
}

/// `/home/u/x` as `~/x`, the way the config file writes it.
fn tilde(path: &Path) -> String {
    match home().and_then(|h| path.strip_prefix(&h).ok().map(Path::to_owned)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Unix seconds as "just now", "12 min ago", "3 h ago" or "2 days ago".
fn ago(at: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    ago_from(at, now)
}

fn ago_from(at: u64, now: u64) -> String {
    let s = now.saturating_sub(at);
    match s {
        0..60 => "just now".to_owned(),
        60..3600 => format!("{} min ago", s / 60),
        3600..86400 => format!("{} h ago", s / 3600),
        _ if s < 2 * 86400 => "1 day ago".to_owned(),
        _ => format!("{} days ago", s / 86400),
    }
}

/// 41924 as "41,924".
fn count(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

/// Decimal units, as the README and `fetch-model` give sizes.
fn size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = "B";
    for u in UNITS {
        if value < 1000.0 {
            break;
        }
        value /= 1000.0;
        unit = u;
    }
    if value < 10.0 {
        format!("{value:.1} {unit}")
    } else {
        format!("{value:.0} {unit}")
    }
}

/// egui's Spinner asks for a repaint every frame, and with vsync off that is
/// as fast as the GPU goes: 71% of a core here (2026-10-18), taken from
/// indexing, which runs at idle priority. This one draws at 10 fps.
/// It stands still when the desktop asks for less motion, and a screen reader skips it: the
/// text beside it always says what is going on.
fn spinner(ui: &mut egui::Ui) {
    let size = 14.0;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), Sense::hover());
    decorative(ui, response.id);
    let still = theme::still(ui.ctx());
    let t = if still { 0.0 } else { ui.input(|i| i.time) };
    let start = (t * 6.0) % std::f64::consts::TAU;
    let points: Vec<egui::Pos2> = (0..=16)
        .map(|i| {
            let a = start + i as f64 / 16.0 * std::f64::consts::PI * 1.5;
            rect.center() + egui::vec2(a.cos() as f32, a.sin() as f32) * (size / 2.0 - 1.5)
        })
        .collect();
    let color = ui.visuals().strong_text_color();
    ui.painter()
        .add(egui::Shape::line(points, Stroke::new(2.0_f32, color)));
    if !still {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(100));
    }
}

/// egui's scroll bar, and the drag-to-scroll area it puts behind an overflowing page, both
/// take keyboard focus but have no role or name of their own.
fn name_scrolling<R>(ui: &egui::Ui, scrolled: &egui::scroll_area::ScrollAreaOutput<R>, page: &str) {
    let view = scrolled.inner_rect;
    let overflow = (scrolled.content_size.y - view.height()).max(0.0);
    ui.ctx()
        .accesskit_node_builder(scrolled.id.with(1_usize), |node| {
            node.set_role(accesskit::Role::ScrollBar);
            node.set_label("Scroll the page");
            node.set_orientation(accesskit::Orientation::Vertical);
            node.set_min_numeric_value(0.0);
            node.set_max_numeric_value(f64::from(overflow));
            node.set_numeric_value(f64::from(scrolled.state.clamped_offset().y));
        });
    if overflow > 0.0 {
        ui.ctx()
            .accesskit_node_builder(scrolled.id.with("area"), |node| {
                node.set_role(accesskit::Role::ScrollView);
                node.set_label(page);
            });
    }
}

/// A button's name with what it acts on after its text, where the same text shows more than
/// once: "Turn on KRunner", not three "Turn on"s.
fn for_what(button: egui::Response, what: &str) -> egui::Response {
    button.ctx.accesskit_node_builder(button.id, |node| {
        let text = node.label().unwrap_or_default();
        node.set_label(format!("{text} {what}"));
    });
    button
}

/// Painted only for the eye: kept out of the accessibility tree.
fn decorative(ui: &egui::Ui, id: Id) {
    ui.ctx()
        .accesskit_node_builder(id, |node| node.set_hidden());
}

/// A heading for screen readers to list and jump between; egui only has labels.
fn heading(ui: &egui::Ui, label: &egui::Response, text: &str, level: usize) {
    ui.ctx().accesskit_node_builder(label.id, |node| {
        node.set_role(accesskit::Role::Heading);
        node.set_level(level);
        node.clear_value();
        node.set_label(text);
    });
}

/// The page switcher, sidebar or tabs, as a tab list.
fn tab_list(ui: &egui::Ui) {
    ui.ctx().accesskit_node_builder(ui.unique_id(), |node| {
        node.set_role(accesskit::Role::TabList);
        node.set_label("Pages");
    });
}

/// One page in `tab_list`: its title is painted, so name it, and say which one is showing.
fn page_tab(ui: &egui::Ui, response: &egui::Response, page: Page, selected: bool) {
    response.widget_info(|| WidgetInfo::selected(egui::Role::Tab, true, selected, page.title()));
    ui.ctx().accesskit_node_builder(response.id, |node| {
        node.set_role(accesskit::Role::Tab);
        node.clear_toggled();
        node.set_selected(selected);
    });
}

fn command(ui: &mut egui::Ui, text: &str) {
    value(ui, RichText::new(text).monospace());
}

/// Text in a row's control column, centred on the label as a button would be.
fn value(ui: &mut egui::Ui, text: impl Into<egui::WidgetText>) {
    ui.horizontal(|ui| {
        ui.set_min_height(ui.spacing().interact_size.y);
        ui.label(text);
    });
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(14.0);
    let label = ui.label(RichText::new(title).family(theme::bold()).size(14.0));
    heading(ui, &label, title, 2);
    ui.add_space(6.0);
}

fn help_line(ui: &mut egui::Ui, text: &str) {
    let p = theme::palette(ui.ctx());
    ui.add(egui::Label::new(RichText::new(text).size(12.5).color(p.muted)).wrap());
    ui.add_space(4.0);
}

/// One folder in a list: its path, with its buttons on the right. Pass a button through the
/// function `buttons` is given and a screen reader hears which folder it is for: "Remove
/// ~/Downloads", not one "Remove" after another.
fn listed(
    ui: &mut egui::Ui,
    path: &str,
    buttons: impl FnOnce(&mut egui::Ui, &dyn Fn(egui::Response) -> egui::Response),
) {
    let p = theme::palette(ui.ctx());
    egui::Frame::NONE
        .stroke(Stroke::new(1.0_f32, p.rule))
        .corner_radius(4.0)
        .inner_margin(egui::Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    buttons(ui, &|button| for_what(button, path));
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.add(egui::Label::new(RichText::new(path).monospace()).truncate())
                            .on_hover_text(path);
                    });
                });
            });
        });
    ui.add_space(2.0);
}

/// A minimum score as a slider, strict on the left and loose on the right,
/// with a way back to the default. Some(new value) when it changed, None
/// meaning the default.
fn score_row(
    ui: &mut egui::Ui,
    label: &str,
    help: &str,
    current: Option<f32>,
    default: f32,
    range: std::ops::RangeInclusive<f32>,
) -> Option<Option<f32>> {
    let mut out = None;
    labelled_row(ui, label, help, |ui, named| {
        ui.horizontal_wrapped(|ui| {
            let p = theme::palette(ui.ctx());
            ui.label(RichText::new("Fewer").color(p.muted));
            let mut v = current.unwrap_or(default);
            let response = slider(ui, named, &mut v, range, 0.01, 2, "");
            ui.label(RichText::new("More").color(p.muted));
            if response.changed() {
                // Stored rounded, as the slider shows it; the default is left unwritten.
                let v = (v * 100.0).round() / 100.0;
                out = Some(((v - default).abs() > 0.001).then_some(v));
            }
            if current.is_some()
                && for_what(ui.button("Default"), label.trim_end_matches(':')).clicked()
            {
                out = Some(None);
            }
        });
    });
    out
}

/// A slider and its number box, both named by the row's label. egui's Slider draws the box
/// itself but leaves it unnamed, and while the box has focus the response it returns is the
/// box's, so naming that response would name the slider only some of the time.
fn slider<N: egui::emath::Numeric>(
    ui: &mut egui::Ui,
    named: &dyn Fn(egui::Response) -> egui::Response,
    value: &mut N,
    range: std::ops::RangeInclusive<N>,
    step: f64,
    decimals: usize,
    suffix: &str,
) -> egui::Response {
    ui.horizontal(|ui| {
        let bar = egui::Slider::new(value, range.clone())
            .show_value(false)
            .step_by(step);
        let bar = named(ui.add(bar));
        let number = egui::DragValue::new(value)
            .range(range)
            .speed(step)
            .fixed_decimals(decimals)
            .suffix(suffix);
        let number = named(ui.add(number));
        bar | number
    })
    .inner
}

// Like a Kirigami FormLayout: right-aligned labels beside their controls, or above them
// when the window is too narrow for two columns.
fn row(ui: &mut egui::Ui, label: &str, help: &str, control: impl FnOnce(&mut egui::Ui)) {
    labelled_row(ui, label, help, |ui, _| control(ui));
}

/// A `row` whose control is named by its label for a screen reader: pass the control's
/// response through the function it is given.
fn labelled_row(
    ui: &mut egui::Ui,
    label: &str,
    help: &str,
    control: impl FnOnce(&mut egui::Ui, &dyn Fn(egui::Response) -> egui::Response),
) {
    let p = theme::palette(ui.ctx());
    let help = |ui: &mut egui::Ui| {
        if !help.is_empty() {
            ui.add(egui::Label::new(RichText::new(help).size(12.5).color(p.muted)).wrap());
        }
    };
    let width = ui.available_width();
    if width >= TWO_COLUMNS_FROM {
        let column = width - LABEL_WIDTH - ui.spacing().item_spacing.x;
        ui.horizontal_top(|ui| {
            let label_id = ui
                .allocate_ui_with_layout(
                    egui::vec2(LABEL_WIDTH, ui.spacing().interact_size.y),
                    Layout::right_to_left(Align::Center),
                    |ui| {
                        ui.set_min_width(LABEL_WIDTH);
                        ui.label(label).id
                    },
                )
                .inner;
            ui.vertical(|ui| {
                // A horizontal parent leaves its children unbounded; wrap at the window's edge.
                ui.set_max_width(column);
                control(ui, &|response| response.labelled_by(label_id));
                help(ui);
            });
        });
    } else {
        let label_id = (!label.is_empty()).then(|| ui.label(label).id);
        control(ui, &|response| match label_id {
            Some(id) => response.labelled_by(id),
            None => response,
        });
        help(ui);
    }
    ui.add_space(8.0);
}

/// `data/dev.vinosearch.svg`, painted at `size` points square, so it stays sharp at any
/// scale without an SVG renderer.
fn logo(ui: &mut egui::Ui, size: f32) {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), Sense::hover());
    decorative(ui, response.id);
    // The SVG's viewBox is 60 units from (2, 2).
    let k = size / 60.0;
    let at = |x: f32, y: f32| rect.min + egui::vec2((x - 2.0) * k, (y - 2.0) * k);
    let box_ =
        |x: f32, y: f32, w: f32, h: f32| egui::Rect::from_min_max(at(x, y), at(x + w, y + h));
    let green = Color32::from_rgb(0x56, 0xcc, 0x78);
    let painter = ui.painter();
    painter.rect_filled(
        box_(4.0, 4.0, 56.0, 56.0),
        14.0 * k,
        Color32::from_rgb(0x0e, 0x0e, 0x10),
    );
    painter.rect_stroke(
        box_(5.0, 5.0, 54.0, 54.0),
        13.0 * k,
        Stroke::new(2.0 * k, Color32::from_white_alpha(46)),
        egui::StrokeKind::Middle,
    );
    painter.circle_stroke(
        at(28.0, 28.0),
        12.0 * k,
        Stroke::new(5.0 * k, Color32::from_rgb(0xfa, 0xfa, 0xfa)),
    );
    painter.rect_filled(
        box_(22.0, 22.0, 12.0, 4.0),
        2.0 * k,
        Color32::from_rgb(0xa0, 0xa4, 0xac).gamma_multiply(0.5),
    );
    painter.rect_filled(box_(22.0, 30.0, 8.0, 4.0), 2.0 * k, green);
    // The handle's round caps: egui strokes end square.
    painter.line_segment(
        [at(37.0, 37.0), at(47.0, 47.0)],
        Stroke::new(6.0 * k, green),
    );
    painter.circle_filled(at(37.0, 37.0), 3.0 * k, green);
    painter.circle_filled(at(47.0, 47.0), 3.0 * k, green);
}

fn sidebar_item(ui: &mut egui::Ui, page: Page, current: &mut Page) {
    let p = theme::palette(ui.ctx());
    let selected = *current == page;
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 32.0), Sense::click());
    let fill = if selected {
        p.link.gamma_multiply(0.22)
    } else if response.hovered() {
        p.code_bg
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, 4.0, fill);
    if response.has_focus() {
        ui.painter().rect_stroke(
            rect,
            4.0,
            Stroke::new(2.0_f32, p.link),
            egui::StrokeKind::Inside,
        );
    }
    // Bold as well as tinted, as the tabs are, so the current page isn't told by colour alone.
    ui.painter().text(
        rect.left_center() + egui::vec2(10.0, 0.0),
        egui::Align2::LEFT_CENTER,
        page.title(),
        if selected {
            FontId::new(14.0, theme::bold())
        } else {
            FontId::proportional(14.0)
        },
        p.text,
    );
    page_tab(ui, &response, page, selected);
    if response.clicked() {
        *current = page;
    }
}

fn tab_item(ui: &mut egui::Ui, page: Page, current: &mut Page, header: theme::Header) {
    let active = *current == page;
    let galley = ui.painter().layout_no_wrap(
        page.title().to_owned(),
        if active {
            FontId::new(14.0, theme::bold())
        } else {
            FontId::proportional(14.0)
        },
        // left for the paint call below, so hovering can change it
        Color32::PLACEHOLDER,
    );
    let (rect, response) =
        ui.allocate_exact_size(galley.size() + egui::vec2(0.0, 6.0), Sense::click());
    let lit = active || response.hovered() || response.has_focus();
    if response.has_focus() {
        let p = theme::palette(ui.ctx());
        ui.painter().rect_stroke(
            rect.expand(3.0),
            4.0,
            Stroke::new(2.0_f32, p.link),
            egui::StrokeKind::Inside,
        );
    }
    ui.painter().galley(
        rect.left_top(),
        galley,
        if lit { header.text } else { header.muted },
    );
    if active {
        let p = theme::palette(ui.ctx());
        ui.painter().hline(
            rect.left()..=rect.right(),
            rect.bottom(),
            Stroke::new(2.0_f32, p.link),
        );
    }
    page_tab(ui, &response, page, active);
    if response.clicked() {
        *current = page;
    }
}

impl eframe::App for SettingsWindow {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if self.idle_dirty {
            self.save_config();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }
}

impl SettingsWindow {
    fn show(&mut self, root: &mut egui::Ui) {
        let ctx = &root.ctx().clone();
        let focused = ctx.input(|input| input.focused);
        if focused && !self.focused {
            // The desktop's light/dark switch or text size may have changed while we were in the
            // background, and the config file or the KRunner registration may have changed under us.
            theme::follow_desktop(ctx);
            self.kde_header = theme::kde_header();
            self.follow_text_scale(ctx);
            if !self.idle_dirty {
                self.reload_config();
            }
            let mut integration = self.integration.lock().unwrap();
            if !integration.running {
                let output = integration.output.take();
                *integration = integration::State {
                    output,
                    ..integration::State::read()
                };
            }
        }
        self.focused = focused;

        let picked = self.picked.lock().unwrap().take();
        if let Some((target, answer)) = picked {
            self.picking = false;
            match answer {
                Ok(Some(path)) => self.add(target, &path),
                Ok(None) => {}
                Err(e) => self.notice = Some(format!("No folder chooser: {e}")),
            }
        }
        let shared = self.link.shared.lock().unwrap().clone();
        if let Some(refused) = shared.refused {
            self.link.shared.lock().unwrap().refused = None;
            self.notice = Some(refused);
        }

        // The title bar is the compositor's: give the tree's window the same name.
        ctx.accesskit_node_builder(egui::accesskit_root_id(), |node| node.set_label(TITLE));

        // Escape backs out of a pending Remove or Delete.
        if self.confirm.is_some() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.confirm = None;
        }

        let p = theme::palette(ctx);
        let header = theme::header(ctx, self.kde_header.as_ref(), focused);
        let wide = ctx.content_rect().width() >= SIDEBAR_FROM;

        egui::Panel::top("header")
            // 0.33's central panel painted over this line; 0.37 draws it on top.
            .show_separator_line(false)
            .frame(
                egui::Frame::NONE
                    .fill(header.bg)
                    .inner_margin(egui::Margin::symmetric(16, 10)),
            )
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    let start = ui.cursor().left();
                    logo(ui, 22.0);
                    if wide {
                        ui.label(
                            RichText::new("vinoSearch")
                                .family(theme::bold())
                                .size(15.0)
                                .color(header.text),
                        );
                        // The page title lines up with the content, past the sidebar.
                        let title_at = start + SIDEBAR_WIDTH - 16.0 + 24.0;
                        ui.add_space((title_at - ui.cursor().left()).max(12.0));
                        let title = ui.label(
                            RichText::new(self.page.title())
                                .family(theme::bold())
                                .size(16.0)
                                .color(header.text),
                        );
                        heading(ui, &title, self.page.title(), 1);
                    } else {
                        // Five tabs: wrap rather than clip at the narrowest widths.
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = egui::vec2(18.0, 6.0);
                            tab_list(ui);
                            for page in Page::ALL {
                                tab_item(ui, page, &mut self.page, header);
                            }
                        });
                    }
                });
            });

        if wide {
            egui::Panel::left("pages")
                .resizable(false)
                .exact_size(SIDEBAR_WIDTH)
                .frame(
                    egui::Frame::NONE
                        .fill(p.bg)
                        .inner_margin(egui::Margin::same(8)),
                )
                .show(root, |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    tab_list(ui);
                    for page in Page::ALL {
                        sidebar_item(ui, page, &mut self.page);
                    }
                });
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(p.bg)
                    // No right margin: the scroll bar belongs on the window's edge, and the
                    // content keeps its own gap from it below.
                    .inner_margin(egui::Margin {
                        left: 24,
                        right: 0,
                        top: 4,
                        bottom: 4,
                    }),
            )
            .show(root, |ui| {
                // A solid bar, always drawn: the floating one only shows on hover, so a page
                // that runs past the window gave no sign there was more below.
                ui.spacing_mut().scroll = egui::style::ScrollStyle {
                    bar_width: 10.0,
                    ..egui::style::ScrollStyle::solid()
                };
                let scrolled = egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
                    .show(ui, |ui| {
                        ui.set_max_width((ui.available_width() - SCROLL_GAP).min(FORM_MAX_WIDTH));
                        if let Some(notice) = self.notice.clone() {
                            ui.add_space(10.0);
                            ui.horizontal_wrapped(|ui| {
                                let shown = ui.label(
                                    RichText::new(notice).color(ui.visuals().error_fg_color),
                                );
                                // Read out when it appears, wherever focus is.
                                ui.ctx().accesskit_node_builder(shown.id, |node| {
                                    node.set_live(accesskit::Live::Assertive)
                                });
                                if ui.small_button("Dismiss").clicked() {
                                    self.notice = None;
                                }
                            });
                        }
                        let status = shared.status.as_ref();
                        match self.page {
                            Page::Index => self.index(ui, status, shared.largest.as_ref()),
                            Page::Folders => self.folders(ui, status),
                            Page::Search => self.search(ui, status),
                            Page::Model => self.model(ui, status),
                            Page::Hardware => self.hardware(ui),
                            Page::Integration => self.integration(ui),
                        }
                        ui.add_space(12.0);
                    });
                let view = scrolled.inner_rect;
                name_scrolling(ui, &scrolled, self.page.title());
                // The bar alone is easy to miss: fade the last lines out while more is below.
                if scrolled.state.clamped_offset().y + view.height() < scrolled.content_size.y - 1.0
                {
                    let fade = egui::Rect::from_min_max(
                        egui::pos2(view.left(), view.bottom() - 48.0),
                        view.right_bottom(),
                    );
                    let mut mesh = egui::Mesh::default();
                    mesh.colored_vertex(fade.left_top(), Color32::TRANSPARENT);
                    mesh.colored_vertex(fade.right_top(), Color32::TRANSPARENT);
                    mesh.colored_vertex(fade.right_bottom(), p.bg);
                    mesh.colored_vertex(fade.left_bottom(), p.bg);
                    mesh.add_triangle(0, 1, 2);
                    mesh.add_triangle(0, 2, 3);
                    ui.painter().add(mesh);
                }
            });

        // Typing or dragging a number changes it every frame; write the file once it settles.
        if self.idle_dirty && ctx.dragged_id().is_none() && !ctx.egui_wants_keyboard_input() {
            self.idle_dirty = false;
            self.save_config();
        }
    }
}

pub fn run(app_id: &str, page: Option<&str>) -> ExitCode {
    let page = page.and_then(Page::parse);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_app_id(app_id)
            .with_inner_size([820.0, 640.0])
            .with_min_inner_size([340.0, 320.0]),
        // As in vinoWhisper: with vsync, a swap waits on a frame callback KWin holds back from
        // a window it considers hidden, and the window stops answering pings.
        glow_options: eframe::egui_glow::GlowConfiguration {
            vsync: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let result = eframe::run_native(
        TITLE,
        options,
        Box::new(|cc| {
            theme::install(&cc.egui_ctx);
            theme::visible_scroll_handles(&cc.egui_ctx);
            Ok(Box::new(SettingsWindow::new(&cc.egui_ctx, page)))
        }),
    );
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("vinosearch-settings: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod ui_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_get_thousands_separators() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1000), "1,000");
        assert_eq!(count(41924), "41,924");
        assert_eq!(count(1234567), "1,234,567");
        assert_eq!(count(-1234), "-1,234");
    }

    #[test]
    fn times_read_as_ago() {
        let now = 1_000_000;
        assert_eq!(ago_from(now - 5, now), "just now");
        assert_eq!(ago_from(now + 5, now), "just now");
        assert_eq!(ago_from(now - 12 * 60, now), "12 min ago");
        assert_eq!(ago_from(now - 3 * 3600 - 59, now), "3 h ago");
        assert_eq!(ago_from(now - 30 * 3600, now), "1 day ago");
        assert_eq!(ago_from(now - 5 * 86400, now), "5 days ago");
    }

    #[test]
    fn sizes_use_decimal_units() {
        assert_eq!(size(512), "512 B");
        assert_eq!(size(176_577_856), "177 MB");
        assert_eq!(size(307_607_341), "308 MB");
        assert_eq!(size(1_500_000_000), "1.5 GB");
    }

    #[test]
    fn pages_parse_by_title() {
        assert!(Page::parse("model") == Some(Page::Model));
        assert!(Page::parse("Hardware") == Some(Page::Hardware));
        assert!(Page::parse("folders") == Some(Page::Folders));
        assert!(Page::parse("search") == Some(Page::Search));
        assert!(Page::parse("nope").is_none());
    }
}
