use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use eframe::egui::{self, Align, Layout, RichText};
use vino_ui::a11y;
use vino_ui::icons::{self, Icon};
use vino_ui::theme;
use vino_ui::widgets::{self, Button, ChipKind, Row, Tone};
use vinosearch_shared::apps::{self, App};
use vinosearch_shared::config::{
    self, Config, DEFAULT_CODE_MIN_SCORE, DEFAULT_DEVICE, DEFAULT_IDLE_UNLOAD_MINUTES,
    DEFAULT_IMAGE_EXTENSIONS, DEFAULT_IMAGE_MIN_SCORE, DEFAULT_INDEX_BUSY_PERCENT,
    DEFAULT_TEXT_MIN_SCORE,
};

use vinosearch_shared::dbus::{hold, seal};

use crate::control::{Link, Partial, Request, Status};
use crate::{doctor, integration, portal};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Index,
    Folders,
    Search,
    Security,
    Model,
    Hardware,
    Integration,
}

impl Page {
    const ALL: [Page; 7] = [
        Page::Index,
        Page::Folders,
        Page::Search,
        Page::Security,
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
            Page::Security => "Security",
            Page::Model => "Model",
            Page::Hardware => "Hardware",
            Page::Integration => "Integration",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Page::Index => Icon::Index,
            Page::Folders => Icon::Folder,
            Page::Search => Icon::Search,
            Page::Security => Icon::Lock,
            Page::Model => Icon::Model,
            Page::Hardware => Icon::Hardware,
            Page::Integration => Icon::Integration,
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

/// What the Editor dropdown shows picked.
#[derive(Clone, PartialEq)]
enum Pick {
    Default,
    /// A desktop entry id.
    App(String),
    Custom,
}

/// A destructive action waiting for a second click.
#[derive(Clone, PartialEq)]
enum Confirm {
    Forget(String),
    DeleteModel,
    Rekey,
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
    /// The editor as typed, until Enter or leaving the field.
    editor: Option<String>,
    /// Custom command picked, so its field shows while still empty.
    custom_editor: bool,
    /// The apps the editor can be, read when the Search page first shows.
    editors: Option<Vec<App>>,
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
    /// A folder chosen to add, shown with its estimate until confirmed (#112).
    adding: Option<estimate::Pending>,
    estimator: estimate::Estimator,
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
            editor: None,
            custom_editor: false,
            editors: None,
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
            adding: None,
            estimator: estimate::run,
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
            Target::Root => self.propose_root(path),
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

    /// Beside the Index page's title: what the service is doing, in words.
    fn index_status(ui: &mut egui::Ui, status: Option<&Result<Status, String>>) {
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
                    // The Now row says what for.
                    widgets::chip(ui, ChipKind::Warn, "On hold");
                } else if s.indexing {
                    widgets::chip(ui, ChipKind::Ok, "Indexing");
                } else {
                    widgets::chip(ui, ChipKind::Ok, "Up to date");
                }
            }
        }
    }

    fn index(
        &mut self,
        ui: &mut egui::Ui,
        status: Option<&Result<Status, String>>,
        largest: Option<&Vec<(String, u64)>>,
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
                    ];
                    let bar: Vec<(f32, egui::Color32)> = shares
                        .iter()
                        .zip(p.kinds)
                        .map(|((n, _), color)| (*n as f32 / total, color))
                        .collect();
                    ui.add_space(4.0);
                    widgets::share_bar(ui, &bar, 8.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 16.0;
                        for ((n, name), color) in shares.iter().zip(p.kinds) {
                            if *n > 0 {
                                let pct = (*n as f64 * 100.0 / f64::from(total)).round();
                                widgets::legend(ui, color, &format!("{name} {pct:.0}%"));
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
            // Why it is indexing after a start: a model changed (#80).
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
                    .show(ui, |ui, _| {
                        ui.with_layout(Layout::top_down(Align::Max), |ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            for d in s.partial.iter().take(PARTIAL_SHOWN) {
                                ui.label(RichText::new(partial_line(d)).size(12.5));
                            }
                            let more = s.partial.len().saturating_sub(PARTIAL_SHOWN);
                            if more > 0 {
                                widgets::small(ui, format!("and {} more", count(more as i64)));
                            }
                        });
                    });
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
                widgets::banner(ui, Tone::Error, why, |_| {});
                None
            }
            None => None,
        };
        widgets::group_label(ui, "Included");
        widgets::group_help(ui, "Indexed and watched for changes.");
        let mut roots = reachable.map(|s| s.roots.clone()).unwrap_or_default();
        self.forgetting.retain(|r| roots.contains(r));
        roots.retain(|r| !self.forgetting.contains(r));
        let indexing = reachable.is_some_and(|s| s.indexing);
        widgets::group(ui, |ui| {
            if reachable.is_some() && roots.is_empty() {
                widgets::group_section(ui, |ui| {
                    widgets::muted(ui, "Nothing is indexed yet.");
                });
            }
            for root in &roots {
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
                        ui.horizontal(|ui| {
                            widgets::mono(ui, &entry);
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                widgets::small(
                                    ui,
                                    format!(
                                        "{} chunks · {:.0}%",
                                        count(*chunks as i64),
                                        share * 100.0
                                    ),
                                );
                            });
                        });
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

    fn cancel_button(&mut self, ui: &mut egui::Ui) -> egui::Response {
        let cancel = ui.add(Button::ghost("Cancel").small());
        if std::mem::take(&mut self.focus_cancel) {
            cancel.request_focus();
        }
        cancel
    }

    /// "Add folder…" through the desktop's chooser, or a typed path, at the
    /// bottom of a group.
    fn adder(&mut self, ui: &mut egui::Ui, target: Target) {
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

    fn model(&mut self, ui: &mut egui::Ui, status: Option<&Result<Status, String>>) {
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
        if let Some(s) = s {
            widgets::group_label(ui, "EmbeddingGemma 2");
            let deleting = self.confirm == Some(Confirm::DeleteModel);
            widgets::group(ui, |ui| {
                let row = Row::new("Download").help("Needed for search and indexing.");
                let row = if deleting {
                    row.tint(p.error_tint)
                } else {
                    row
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
                    let deletable = s.model_present && !s.fetching;
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
                        let fraction = s.fetch_done as f32 / s.fetch_total.max(1) as f32;
                        widgets::small(
                            ui,
                            format!("{} of {}", size(s.fetch_done), size(s.fetch_total)),
                        );
                        widgets::progress(ui, fraction, 120.0);
                    } else if s.model_present {
                        widgets::chip(
                            ui,
                            ChipKind::Ok,
                            &format!("Downloaded, {}", size(s.model_bytes)),
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

        widgets::group_label(ui, "Running");
        if self.config_broken.is_some() {
            widgets::group_help(ui, "The settings file doesn't parse; see the Folders page.");
            return;
        }
        let mut changed = false;
        widgets::group(ui, |ui| {
            Row::new("Device")
                .help("The NPU is fastest and uses the least power.")
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

    /// What gets indexed, and how close a result must be to show.
    fn search(&mut self, ui: &mut egui::Ui, status: Option<&Result<Status, String>>) {
        if let Some(why) = &self.config_broken {
            ui.add_space(10.0);
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
        let p = theme::palette(ui.ctx());
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
                Some(_) => format!("{} files", count(files)),
                None => format!("{} photos", count(files)),
            };
            let pct = chunks.unwrap_or(files) as f64 * 100.0 / s.chunks.max(1) as f64;
            Some(format!("{what} · {pct:.0}%"))
        };
        widgets::group_label(ui, "What to index");
        widgets::group(ui, |ui| {
            Row::new("Text")
                .help("Notes, Markdown and web pages.")
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
                .help("By what's in them. 671 MB download.")
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
                && !s.model_present
            {
                Row::new("").tint(p.warn_tint).custom(
                    ui,
                    |ui| {
                        ui.horizontal(|ui| {
                            icons::icon(ui, Icon::Warning, 16.0, p.warn);
                            let shown = ui.add(
                                egui::Label::new("The vision model isn't downloaded yet.").wrap(),
                            );
                            a11y::live(&shown);
                        });
                        None
                    },
                    |ui, _| {
                        if ui
                            .add_enabled(!s.fetching, Button::primary("Download").small())
                            .clicked()
                        {
                            self.link.send(Request::FetchModel);
                        }
                    },
                );
            }
            if images {
                Row::new("Image types")
                    .help("Empty restores the defaults.")
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
                "PDF, Office, EPUB, subtitles, CSV."
            } else {
                "PDF, Office, EPUB, subtitles, CSV. Stored unencrypted unless the index is \
                 encrypted (Security)."
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
                .help("Found with “code:” searches. Secrets and ignored files skipped.")
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
                    "Turn on encryption (Security) to keep transcripts encrypted.",
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
            if let Some(s) = s
                && s.sessions_unread > 0
            {
                let n = s.sessions_unread;
                Row::new("Not read")
                    .help(&format!(
                        "{n} {}: made by a newer vinoWhisper, or damaged.",
                        if n == 1 { "session" } else { "sessions" }
                    ))
                    .tone(Tone::Error)
                    .show(ui, |_, _| {});
            }
        });

        widgets::group_label(ui, "Opening results");
        let editors = self.editors.get_or_insert_with(apps::editors).clone();
        let current = self.config.editor.clone();
        let mut pick = match &current {
            Some(e) if apps::is_entry_id(e) && !self.custom_editor => {
                Pick::App(e.trim().to_owned())
            }
            Some(_) => Pick::Custom,
            None if self.custom_editor => Pick::Custom,
            None => Pick::Default,
        };
        widgets::group(ui, |ui| {
            Row::new("Editor")
                .help("Opens at the matching line.")
                .show(ui, |ui, named| {
                    let before = pick.clone();
                    let label = |pick: &Pick| match pick {
                        Pick::Default => "Default app".to_owned(),
                        Pick::App(id) => editors
                            .iter()
                            .find(|a| &a.id == id)
                            .map_or_else(|| id.clone(), |a| a.name.clone()),
                        Pick::Custom => "Custom command…".to_owned(),
                    };
                    let combo =
                        widgets::combo(ui, "editor", label(&pick), 200.0).show_ui(ui, |ui| {
                            ui.selectable_value(&mut pick, Pick::Default, label(&Pick::Default));
                            for a in editors.iter() {
                                ui.selectable_value(&mut pick, Pick::App(a.id.clone()), &a.name);
                            }
                            ui.separator();
                            ui.selectable_value(&mut pick, Pick::Custom, label(&Pick::Custom));
                        });
                    named(combo.response);
                    if pick != before {
                        self.custom_editor = pick == Pick::Custom;
                        self.editor = None;
                        let next = match &pick {
                            Pick::Default => None,
                            Pick::App(id) => Some(id.clone()),
                            // Kept until a command is typed, unless it was an app.
                            Pick::Custom => current.clone().filter(|e| !apps::is_entry_id(e)),
                        };
                        if next != self.config.editor {
                            self.config.editor = next;
                            changed = true;
                        }
                    }
                });
            if pick == Pick::Custom {
                Row::new("Command")
                    .help("With {file} and {line} in it, run without a shell.")
                    .show(ui, |ui, named| {
                        let current = self.config.editor.clone().unwrap_or_default();
                        let text = self.editor.get_or_insert(current);
                        let width = ui.available_width().min(260.0);
                        let response = named(ui.add(widgets::field(
                            text,
                            "foot -e nvim +{line} {file}",
                            width,
                        )));
                        if response.lost_focus() {
                            let typed = text.trim();
                            let next = (!typed.is_empty()).then(|| typed.to_owned());
                            if next != self.config.editor {
                                self.config.editor = next;
                                changed = true;
                            }
                            self.editor = None;
                        }
                    });
            }
        });

        widgets::group_label(ui, "How close results must be");
        widgets::group_help(ui, "Higher means fewer, better results.");
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
        });
        if changed {
            self.save_config();
        }
    }

    /// Encrypting the index at rest (#4): the switch, what the key is sealed with, and a new
    /// key on demand.
    fn security(&mut self, ui: &mut egui::Ui, status: Option<&Result<Status, String>>) {
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
            "The index holds the text of what it covers. Encrypted, it reads only on this \
             machine, as you: not from a backup, a copy of your home folder or a disk taken out.",
        );
        if let Some(why) = &self.config_broken {
            widgets::group_help(
                ui,
                &format!("The settings file doesn't parse, so it isn't changed from here: {why}"),
            );
            return;
        }
        let no_tpm = s.is_some_and(|s| !s.tpm);
        let mut changed = false;
        let rekeying = self.confirm == Some(Confirm::Rekey);
        widgets::group(ui, |ui| {
            Row::new("Encrypt the index")
                .help("Key sealed to the TPM. Nothing is re-indexed.")
                .show(ui, |ui, _| {
                    let mut on = self.config.encryption.on();
                    if widgets::switch(ui, &mut on, "Encrypt the index").changed() {
                        if on && no_tpm && !self.config.encryption.host_key_allowed() {
                            // Refused here as the runner would refuse it: never weaker than asked.
                            self.notice = Some(
                                "This machine has no TPM that systemd can use, so the key can't \
                                 be sealed to it. Allow the host key below, or leave encryption \
                                 off."
                                    .into(),
                            );
                        } else {
                            self.config.encryption.enabled = on.then_some(true);
                            changed = true;
                        }
                    }
                });
            if no_tpm || self.config.encryption.host_key_allowed() {
                Row::new("Without a TPM")
                    .help(
                        "Seals the key with systemd's host key, which root can read: safe in a \
                         backup, not on a disk taken out with the system.",
                    )
                    .show(ui, |ui, _| {
                        let mut allow = self.config.encryption.host_key_allowed();
                        if widgets::switch(ui, &mut allow, "Use the host key instead (weaker)")
                            .changed()
                        {
                            self.config.encryption.allow_host_key = allow.then_some(true);
                            changed = true;
                        }
                    });
            }
            let Some(s) = s else {
                return;
            };
            let error = (!s.encryption_error.is_empty()).then_some(s.encryption_error.as_str());
            Row::new("Now")
                .help(error.unwrap_or(""))
                .tone(Tone::Error)
                .show(ui, |ui, _| {
                    let (kind, text) = match (s.encrypted, s.seal.as_str()) {
                        (true, seal::TPM) => (
                            ChipKind::Ok,
                            "Encrypted, the key sealed to this machine's TPM",
                        ),
                        (true, seal::HOST) => (
                            ChipKind::Ok,
                            "Encrypted, the key sealed with systemd's host key",
                        ),
                        (true, _) => (
                            ChipKind::Warn,
                            "Encrypted with a key held in memory only: nothing could seal it, so \
                             the index is rebuilt at the next start",
                        ),
                        (false, _)
                            if self.config.encryption.on() && s.encryption_error.is_empty() =>
                        {
                            (ChipKind::Neutral, "Encrypting…")
                        }
                        (false, _) => (ChipKind::Neutral, "Not encrypted"),
                    };
                    a11y::live(&widgets::chip(ui, kind, text));
                });
            if !s.seal_broken.is_empty() {
                Row::new("Rebuilt")
                    .help(&format!(
                        "The index's key stopped working ({}), so the index is being built again \
                         from your files.",
                        s.seal_broken
                    ))
                    .tone(Tone::Warn)
                    .show(ui, |_, _| {});
            }
            if s.encrypted {
                let row = Row::new("Key").help("Re-encrypt under a new key.");
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
        ui.add_space(8.0);
        widgets::small(
            ui,
            "Old plaintext may linger in free disk space until overwritten; full-disk \
             encryption covers it, and swap.",
        );
        if changed {
            self.save_config();
            if self.config_broken.is_none() {
                self.link.send(Request::ApplyEncryption);
            }
        }
    }

    /// What `vinosearch doctor` reports, run the first time the page shows.
    /// The device it probes is the one picked on the Model page.
    fn hardware(&mut self, ui: &mut egui::Ui) {
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
                .help("Same as vinosearch doctor.")
                .show(ui, |ui, _| {
                    if state.running {
                        widgets::chip(ui, ChipKind::Neutral, "Checking…");
                    } else if ui.add(Button::secondary("Check again")).clicked() {
                        doctor::run(Arc::clone(&self.doctor), ui.ctx());
                    }
                });
        });

        widgets::group_label(ui, "Power");
        if self.config_broken.is_some() {
            widgets::group_help(ui, "The settings file doesn't parse; see the Folders page.");
            return;
        }
        widgets::group(ui, |ui| {
            Row::new("Index on battery")
                .help("Off waits for a charger. Power saver pauses indexing too.")
                .show(ui, |ui, _| {
                    let mut on = self.config.index_on_battery();
                    if widgets::switch(ui, &mut on, "Index on battery").changed() {
                        self.config.index_on_battery = (!on).then_some(false);
                        self.save_config();
                    }
                });
        });
    }

    fn integration(&mut self, ui: &mut egui::Ui) {
        let state = self.integration.lock().unwrap().clone();
        let (kde, gnome) = integration::desktops();
        widgets::group_label(ui, "Search places");
        widgets::group_help(
            ui,
            "Each place has its own switch; only what's on is installed.",
        );
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
                let button = if on { "Turn off" } else { "Turn on" };
                let button = ui.add_enabled(!state.running, Button::secondary(button));
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
        widgets::group(ui, |ui| {
            switch_row(
                ui,
                "KRunner",
                "Alt+Space, and the search in the Plasma app launcher.",
                integration::Switch::Kde,
                (!kde).then_some("Plasma isn't running in this session."),
            );
            switch_row(
                ui,
                "GNOME Shell",
                "The overview's search, from the next login.",
                integration::Switch::Gnome,
                (!gnome).then_some("GNOME isn't running in this session."),
            );
            switch_row(
                ui,
                "App menu",
                "The search window as an app of its own.",
                integration::Switch::Menu,
                None,
            );
        });
        widgets::group_label(ui, "Service");
        widgets::group(ui, |ui| {
            switch_row(
                ui,
                "Search service",
                "Runs at login. Turning it off removes every search place too.",
                integration::Switch::Service,
                None,
            );
            if let Some(output) = &state.output {
                widgets::group_section(ui, |ui| {
                    ui.add(egui::Label::new(RichText::new(output).monospace()).wrap());
                });
            }
        });
        if let Some((switch, on)) = flip {
            integration::run(Arc::clone(&self.integration), ui.ctx(), switch, on);
        }
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
}

/// A folder in a list: its icon and path, the path in full on hover.
fn path_label(ui: &mut egui::Ui, icon: Icon, path: &str, alarmed: bool) {
    let p = theme::palette(ui.ctx());
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        icons::icon(ui, icon, 16.0, if alarmed { p.error } else { p.muted });
        widgets::mono(ui, path);
    });
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

/// Documents indexed in part listed by name on the Index page; the rest are counted.
const PARTIAL_SHOWN: usize = 3;

/// "book.pdf, 120 of 640 pages".
fn partial_line(d: &Partial) -> String {
    let name = Path::new(&d.path)
        .file_name()
        .map_or_else(|| d.path.clone(), |n| n.to_string_lossy().into_owned());
    let unit = if d.total == 1 {
        d.unit.clone()
    } else {
        format!("{}s", d.unit)
    };
    format!(
        "{name}, {} of {} {unit}",
        count(d.done as i64),
        count(d.total as i64)
    )
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

/// A minimum score as a slider, strict on the left and loose on the right,
/// with a way back to the default. Some(new value) when it changed, None
/// meaning the default.
fn score_row(
    ui: &mut egui::Ui,
    label: &str,
    current: Option<f32>,
    default: f32,
    range: std::ops::RangeInclusive<f32>,
) -> Option<Option<f32>> {
    let mut out = None;
    Row::new(label).show(ui, |ui, named| {
        // Left to right as the number rises, so dragging, the number and
        // the end labels all agree: further right, closer matches only.
        let p = theme::palette(ui.ctx());
        let mut v = current.unwrap_or(default);
        let text = |ui: &egui::Ui, t: &str| {
            egui::WidgetText::from(RichText::new(t).size(12.5).color(p.muted)).into_galley(
                ui,
                Some(egui::TextWrapMode::Extend),
                f32::INFINITY,
                egui::TextStyle::Small,
            )
        };
        // Right to left: Default, the number, Closer, the bar, Looser.
        if current.is_some()
            && a11y::for_what(ui.add(Button::ghost("Default").small()), label).clicked()
        {
            out = Some(None);
        }
        let number = egui::DragValue::new(&mut v)
            .range(range.clone())
            .speed(0.01)
            .fixed_decimals(2);
        let number = named(ui.add(number));
        ui.label(text(ui, "Closer"));
        // The bar takes what "Looser" leaves of the row.
        let looser = text(ui, "Looser");
        let room = ui.available_width() - looser.size().x - 2.0 * ui.spacing().item_spacing.x;
        ui.spacing_mut().slider_width = room.clamp(48.0, 240.0);
        let bar = egui::Slider::new(&mut v, range)
            .show_value(false)
            .step_by(0.01);
        let bar = named(ui.add(bar));
        ui.label(looser);
        let response = bar | number;
        if response.changed() {
            // Stored rounded, as the slider shows it; the default is left unwritten.
            let v = (v * 100.0).round() / 100.0;
            out = Some(((v - default).abs() > 0.001).then_some(v));
        }
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
    // Right to left: the number box, then the bar.
    let number = egui::DragValue::new(value)
        .range(range.clone())
        .speed(step)
        .fixed_decimals(decimals)
        .suffix(suffix);
    let number = named(ui.add(number));
    ui.spacing_mut().slider_width = (ui.available_width() - 8.0).clamp(48.0, 200.0);
    let bar = egui::Slider::new(value, range)
        .show_value(false)
        .step_by(step);
    let bar = named(ui.add(bar));
    bar | number
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
        let wide = ctx.content_rect().width() >= widgets::SIDEBAR_FROM;
        let status = shared.status.as_ref();

        if wide {
            egui::Panel::left("pages")
                .resizable(false)
                .show_separator_line(false)
                .exact_size(widgets::SIDEBAR_WIDTH)
                .frame(widgets::sidebar_frame(p))
                .show(root, |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.horizontal(|ui| {
                        ui.add_space(8.0);
                        vino_ui::logo::search(ui, 28.0);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            ui.label(RichText::new("vinoSearch").size(15.0).family(theme::bold()));
                            widgets::small(ui, "Settings");
                        });
                    });
                    ui.add_space(14.0);
                    a11y::tab_list(ui, "Pages");
                    for page in Page::ALL {
                        if widgets::nav_item(
                            ui,
                            page.icon(),
                            page.title(),
                            page.title(),
                            self.page == page,
                            None,
                        )
                        .clicked()
                        {
                            self.page = page;
                        }
                    }
                    ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                        ui.add_space(2.0);
                        ui.horizontal(|ui| {
                            ui.add_space(4.0);
                            let shown = match status {
                                None => widgets::chip(ui, ChipKind::Neutral, "Connecting…"),
                                Some(Err(_)) => widgets::chip(ui, ChipKind::Err, "Service stopped"),
                                Some(Ok(_)) => widgets::chip(ui, ChipKind::Ok, "Service running"),
                            };
                            a11y::live(&shown);
                        });
                    });
                    widgets::panel_rule(ui, widgets::Edge::Left);
                });
        } else {
            egui::Panel::top("header")
                .show_separator_line(false)
                .frame(widgets::header_frame(header, p).inner_margin(egui::Margin {
                    left: 16,
                    right: 16,
                    top: 8,
                    bottom: 0,
                }))
                .show(root, |ui| {
                    ui.horizontal(|ui| {
                        vino_ui::logo::search(ui, 22.0);
                        ui.label(
                            RichText::new("vinoSearch")
                                .size(15.0)
                                .family(theme::bold())
                                .color(header.text),
                        );
                    });
                    // Seven tabs: wrap rather than clip at the narrowest widths.
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(18.0, 0.0);
                        a11y::tab_list(ui, "Pages");
                        for page in Page::ALL {
                            if widgets::tab(ui, page.title(), self.page == page, true, header)
                                .clicked()
                            {
                                self.page = page;
                            }
                        }
                    });
                });
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(p.bg)
                    // No right margin: the scroll bar belongs on the window's edge, and the
                    // content keeps its own gap from it below.
                    .inner_margin(egui::Margin {
                        left: if wide { widgets::PAGE_INSET as i8 } else { 16 },
                        right: 0,
                        top: if wide { 20 } else { 12 },
                        bottom: 0,
                    }),
            )
            .show(root, |ui| {
                widgets::page_scroll(ui, self.page.title(), widgets::CONTENT_MAX, |ui| {
                    ui.horizontal(|ui| {
                        widgets::title(ui, self.page.title());
                        if self.page == Page::Index {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                Self::index_status(ui, status);
                            });
                        }
                    });
                    if let Some(notice) = self.notice.clone() {
                        ui.add_space(12.0);
                        if widgets::notice(ui, Tone::Error, &notice) {
                            self.notice = None;
                        }
                    }
                    match self.page {
                        Page::Index => self.index(ui, status, shared.largest.as_ref()),
                        Page::Folders => self.folders(ui, status),
                        Page::Search => self.search(ui, status),
                        Page::Security => self.security(ui, status),
                        Page::Model => self.model(ui, status),
                        Page::Hardware => self.hardware(ui),
                        Page::Integration => self.integration(ui),
                    }
                });
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
            .with_inner_size([820.0, 676.0])
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

mod estimate;
pub mod search;
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
        assert!(Page::parse("Security") == Some(Page::Security));
        assert!(Page::parse("nope").is_none());
    }
}
