use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use eframe::egui::{self, Align, Layout, RichText};
use vino_ui::a11y;
use vino_ui::icons::{self, Icon};
use vino_ui::theme;
use vino_ui::widgets::{self, Button, ChipKind, Column, Row, Tone};
use vinosearch_shared::apps::{self, App};
use vinosearch_shared::config::{
    self, Config, DEFAULT_CODE_MIN_SCORE, DEFAULT_DEVICE, DEFAULT_IDLE_UNLOAD_MINUTES,
    DEFAULT_IMAGE_EXTENSIONS, DEFAULT_IMAGE_MIN_SCORE, DEFAULT_INDEX_BUSY_PERCENT,
    DEFAULT_MEDIA_MIN_SCORE, DEFAULT_TEXT_MIN_SCORE,
};

use vinosearch_shared::dbus::{hold, seal};
use vinosearch_shared::models;
use vinosearch_shared::sandbox;

use crate::control::{Link, NOT_INSTALLED, Request, Status};
use crate::{doctor, integration, portal};
use labels::{ago, count, home, middle_cut, partial_line, size, tilde};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Index,
    Folders,
    Search,
    Security,
    Model,
    Hardware,
    Integration,
    About,
}

impl Page {
    const ALL: [Page; 8] = [
        Page::Index,
        Page::Folders,
        Page::Search,
        Page::Security,
        Page::Model,
        Page::Hardware,
        Page::Integration,
        Page::About,
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
            Page::About => vino_ui::about::TITLE,
        }
    }

    fn max_width(self) -> f32 {
        match self {
            Page::Security | Page::About => widgets::CONTENT_MAX,
            Page::Index
            | Page::Folders
            | Page::Search
            | Page::Model
            | Page::Hardware
            | Page::Integration => widgets::WIDE_MAX,
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
            Page::About => Icon::Info,
        }
    }
}

const TITLE: &str = "vinoSearch settings";

const DEVICES: [&str; 3] = ["NPU", "GPU", "CPU"];

fn power_hold(key: &str) -> Option<&'static str> {
    match key {
        hold::CHARGER => Some("Waiting for a charger"),
        hold::POWER_SAVER => Some("Paused: power saver"),
        _ => None,
    }
}

const LARGEST: u32 = 8;

#[derive(Clone, Copy, PartialEq)]
enum Target {
    Root,
    Exclude,
}

type Picked = (Target, Result<Option<PathBuf>, String>);

#[derive(Clone, PartialEq)]
enum Pick {
    Default,
    App(String),
    Custom,
}

struct AppPicker<'a> {
    label: &'a str,
    help: &'a str,
    command_help: &'a str,
    hint: &'a str,
    apps: &'a [App],
}

/// A dropdown of the default app, installed `apps` and a custom command, with the command's field
/// under it when picked. `draft` is the command as typed until Enter or leaving the field; `custom`
/// keeps the field showing while empty. Whether `setting` changed.
fn app_picker(
    ui: &mut egui::Ui,
    picker: &AppPicker,
    setting: &mut Option<String>,
    draft: &mut Option<String>,
    custom: &mut bool,
) -> bool {
    let mut changed = false;
    let current = setting.clone();
    let mut pick = match &current {
        Some(e) if apps::is_entry_id(e) && !*custom => Pick::App(e.trim().to_owned()),
        Some(_) => Pick::Custom,
        None if *custom => Pick::Custom,
        None => Pick::Default,
    };
    Row::new(picker.label)
        .help(picker.help)
        .show(ui, |ui, named| {
            let before = pick.clone();
            let label = |pick: &Pick| match pick {
                Pick::Default => "Default app".to_owned(),
                Pick::App(id) => picker
                    .apps
                    .iter()
                    .find(|a| &a.id == id)
                    .map_or_else(|| id.clone(), |a| a.name.clone()),
                Pick::Custom => "Custom command…".to_owned(),
            };
            let id = picker.label.to_lowercase();
            let combo = widgets::combo(ui, &id, label(&pick), 200.0).show_ui(ui, |ui| {
                ui.selectable_value(&mut pick, Pick::Default, label(&Pick::Default));
                for a in picker.apps {
                    ui.selectable_value(&mut pick, Pick::App(a.id.clone()), &a.name);
                }
                ui.separator();
                ui.selectable_value(&mut pick, Pick::Custom, label(&Pick::Custom));
            });
            named(combo.response);
            if pick != before {
                *custom = pick == Pick::Custom;
                *draft = None;
                let next = match &pick {
                    Pick::Default => None,
                    Pick::App(id) => Some(id.clone()),
                    Pick::Custom => command_kept(current.as_ref()),
                };
                if next != *setting {
                    *setting = next;
                    changed = true;
                }
            }
        });
    if pick == Pick::Custom {
        Row::new(&format!("{} command", picker.label))
            .help(picker.command_help)
            .show(ui, |ui, named| {
                let text = draft.get_or_insert(setting.clone().unwrap_or_default());
                let width = ui.available_width().min(260.0);
                let response = named(ui.add(widgets::field(text, picker.hint, width)));
                if response.lost_focus() {
                    let typed = text.trim();
                    let next = (!typed.is_empty()).then(|| typed.to_owned());
                    if next != *setting {
                        *setting = next;
                        changed = true;
                    }
                    *draft = None;
                }
            });
    }
    changed
}

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
    config_broken: Option<String>,
    idle_dirty: bool,
    new_root: String,
    new_exclude: String,
    image_extensions: Option<String>,
    editor: Option<String>,
    custom_editor: bool,
    editors: Option<Vec<App>>,
    player: Option<String>,
    custom_player: bool,
    players: Option<Vec<App>>,
    picked: Arc<Mutex<Option<Picked>>>,
    picking: bool,
    confirm: Option<Confirm>,
    /// Move focus to Cancel when a confirmation replaces the button that asked, so a keyboard user
    /// isn't left on nothing.
    focus_cancel: bool,
    check_asked: Option<u64>,
    forgetting: HashSet<String>,
    /// Folders added that Status doesn't list yet: the service registers one only once its worker
    /// takes it, behind changes under the other roots.
    joining: HashSet<String>,
    notice: Option<String>,
    integration: Arc<Mutex<integration::State>>,
    doctor: Arc<Mutex<doctor::State>>,
    largest_for: Option<i64>,
    adding: Option<estimate::Pending>,
    estimator: estimate::Estimator,
    focused: bool,
    kde_header: Option<theme::KdeHeader>,
    app_title_bar: bool,
    /// The desktop's text scale as last applied; only a change is reapplied so a Ctrl+ zoom isn't
    /// undone on focus.
    text_scale: f32,
    updater: vino_ui::update::Updater,
    models_dir: Option<PathBuf>,
}

/// The release installer leaves `~/.local/bin/vinosearch-upgrade`: About's Update button installs a
/// release with it. A release's Flatpak bundle installs the next with `vinosearch flatpak-update`.
/// A build from source has neither and links to the release.
fn updater() -> vino_ui::update::Updater {
    let updater =
        vino_ui::update::Updater::new(vino_ui::App::Search, vinosearch_shared::version::VERSION);
    if sandbox::flatpak_id().is_some() {
        let release =
            sandbox::info_value("Instance", "branch").as_deref() == Some(sandbox::RELEASE_BRANCH);
        return if release {
            updater.with_command([
                integration::vinosearch().to_string_lossy().into_owned(),
                "flatpak-update".into(),
            ])
        } else {
            updater
        };
    }
    match std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".local/bin/vinosearch-upgrade"))
    {
        Some(upgrade) if upgrade.is_file() => {
            updater.with_command([upgrade.to_string_lossy().into_owned()])
        }
        _ => updater,
    }
}

impl SettingsWindow {
    fn new(ctx: &egui::Context, page: Option<Page>) -> Self {
        let config_path = config::default_path().unwrap_or_default();
        let mut window = SettingsWindow::with(page, Link::start(ctx), config_path);
        window.follow_text_scale(ctx);
        integration::first_run(&window.integration, ctx);
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
            player: None,
            custom_player: false,
            players: None,
            picked: Arc::new(Mutex::new(None)),
            picking: false,
            confirm: None,
            focus_cancel: false,
            check_asked: None,
            forgetting: HashSet::new(),
            joining: HashSet::new(),
            notice: None,
            integration: Arc::new(Mutex::new(integration::State::read())),
            doctor: Arc::default(),
            largest_for: None,
            adding: None,
            estimator: estimate::run,
            focused: true,
            kde_header: theme::kde_header(),
            app_title_bar: false,
            text_scale: 1.0,
            updater: updater(),
            models_dir: models::dir().ok(),
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
}

fn path_label(ui: &mut egui::Ui, icon: Icon, path: &str, alarmed: bool) {
    let p = theme::palette(ui.ctx());
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        icons::icon(ui, icon, 16.0, if alarmed { p.error } else { p.muted });
        widgets::mono(ui, path);
    });
}

fn fetch_progress(ui: &mut egui::Ui, s: &Status) {
    let fraction = s.fetch_done as f32 / s.fetch_total.max(1) as f32;
    widgets::small(
        ui,
        format!("{} of {}", size(s.fetch_done), size(s.fetch_total)),
    );
    widgets::progress(ui, fraction, 120.0);
}

const LEGEND_SWATCH: f32 = 8.0 + 6.0;

/// The share bar's colours, in Status's order: the palette's four, then sessions (teal) and media
/// (amber), which the palette has no colour for yet, shaded dark against the light theme's card and
/// light against the dark one's.
fn kind_colors(p: &theme::Palette, dark: bool) -> [egui::Color32; 6] {
    let [text, documents, images, code] = p.kinds;
    let (sessions, media) = if dark {
        (
            egui::Color32::from_rgb(0x4f, 0xc8, 0xc0),
            egui::Color32::from_rgb(0xe8, 0xa8, 0x4a),
        )
    } else {
        (
            egui::Color32::from_rgb(0x16, 0x84, 0x7c),
            egui::Color32::from_rgb(0xa8, 0x62, 0x0c),
        )
    };
    [text, documents, images, code, sessions, media]
}

/// Switching to a custom command keeps the one set until another is typed; an app's entry id isn't
/// a command.
fn command_kept(current: Option<&String>) -> Option<String> {
    current.filter(|e| !apps::is_entry_id(e)).cloned()
}

/// A minimum score as a slider, strict on the left and loose on the right, with a way back to the
/// default. Some(new value) when it changed, None meaning the default.
///
/// Laid out right to left: Default, the number, Closer, the bar, Looser. Default keeps its space
/// when hidden, so every row's bar lines up.
fn score_row(
    ui: &mut egui::Ui,
    label: &str,
    current: Option<f32>,
    default: f32,
    range: std::ops::RangeInclusive<f32>,
) -> Option<Option<f32>> {
    let mut out = None;
    Row::new(label).show(ui, |ui, named| {
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
        let reset = ui.add_visible(current.is_some(), Button::ghost("Default").small());
        if current.is_some() && a11y::for_what(reset, label).clicked() {
            out = Some(None);
        }
        let number = egui::DragValue::new(&mut v)
            .range(range.clone())
            .speed(0.01)
            .fixed_decimals(2);
        let number = named(ui.add(number));
        ui.label(text(ui, "Closer"));
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
            out = Some(stored_score(v, default));
        }
    });
    out
}

/// Rounded to hundredths, as the slider shows it; None for the default, which
/// is left unwritten (pinned by `tests::a_score_is_stored_rounded_to_hundredths`
/// and `tests::a_score_that_rounds_to_the_default_is_left_unwritten`).
fn stored_score(v: f32, default: f32) -> Option<f32> {
    let v = (v * 100.0).round() / 100.0;
    ((v - default).abs() > 0.001).then_some(v)
}

/// A slider and its number box, both named by the row's label. egui's Slider leaves its box
/// unnamed, and while the box has focus the response returned is the box's, so naming that response
/// would name the slider only some of the time.
fn slider<N: egui::emath::Numeric>(
    ui: &mut egui::Ui,
    named: &dyn Fn(egui::Response) -> egui::Response,
    value: &mut N,
    range: std::ops::RangeInclusive<N>,
    step: f64,
    decimals: usize,
    suffix: &str,
) -> egui::Response {
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

    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        vino_ui::input::faster_touchpad(raw);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
        if self.app_title_bar {
            vino_ui::window::edges(ui);
        }
    }
}

impl SettingsWindow {
    fn show(&mut self, root: &mut egui::Ui) {
        let ctx = &root.ctx().clone();
        let focused = ctx.input(|input| input.focused);
        if focused && !self.focused {
            self.catch_up_on_focus(ctx);
        }
        self.focused = focused;
        self.take_picked_folder();
        let shared = self.link.shared.lock().unwrap().clone();
        if let Some(refused) = shared.refused {
            self.link.shared.lock().unwrap().refused = None;
            self.show_refusal(refused);
        }
        name_like_the_title_bar(ctx);
        if self.confirm.is_some() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.confirm = None;
        }

        let p = theme::palette(ctx);
        let window = ctx.content_rect().width();
        let wide = window >= widgets::SIDEBAR_FROM;
        let status = shared.status.as_ref();
        if wide {
            self.sidebar(root, p, window, status);
        } else {
            let header = theme::header(ctx, self.kde_header.as_ref(), focused);
            self.header(root, p, header);
        }
        let bar = wide && self.app_title_bar;
        if bar {
            self.page_title_bar(root, window, status);
        }
        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(p.bg)
                    .inner_margin(page_margin(window, wide, bar)),
            )
            .show(root, |ui| {
                self.page(ui, bar, status, shared.largest.as_ref());
            });
        self.save_once_numbers_settle(ctx);
    }

    /// A refusal doesn't say which request it was, so every folder still joining is dropped.
    fn show_refusal(&mut self, refused: String) {
        self.joining.clear();
        self.notice = Some(refused);
    }

    fn sidebar(
        &mut self,
        root: &mut egui::Ui,
        p: &theme::Palette,
        window: f32,
        status: Option<&Result<Status, String>>,
    ) {
        egui::Panel::left("pages")
            .resizable(false)
            .show_separator_line(false)
            .exact_size(widgets::sidebar_width(window))
            .frame(widgets::sidebar_frame(p))
            .show(root, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                let brand = brand(ui);
                if self.app_title_bar {
                    drag_by_brand(ui, brand);
                }
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
                        service_chip(ui, status);
                    });
                });
                widgets::panel_rule(ui, widgets::Edge::Left);
            });
    }

    fn header(&mut self, root: &mut egui::Ui, p: &theme::Palette, header: theme::Header) {
        egui::Panel::top("header")
            .show_separator_line(false)
            .frame(widgets::header_frame(header, p).inner_margin(egui::Margin {
                left: 16,
                right: 16,
                top: 8,
                bottom: 0,
            }))
            .show(root, |ui| {
                if self.app_title_bar {
                    vino_ui::window::drag(ui, ui.clip_rect(), "header");
                }
                ui.horizontal(|ui| {
                    vino_ui::logo::search(ui, 22.0);
                    ui.add(
                        egui::Label::new(
                            RichText::new("vinoSearch")
                                .size(15.0)
                                .family(theme::bold())
                                .color(header.text),
                        )
                        .selectable(false),
                    );
                    if self.app_title_bar {
                        vino_ui::window::controls(ui);
                    }
                });
                self.page_tabs(ui, header);
            });
    }

    fn page_tabs(&mut self, ui: &mut egui::Ui, header: theme::Header) {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(18.0, 0.0);
            a11y::tab_list(ui, "Pages");
            for page in Page::ALL {
                if widgets::tab(ui, page.title(), self.page == page, true, header).clicked() {
                    self.page = page;
                }
            }
        });
    }

    /// With the app's title bar a wide window's page title moves out of the scroll area into a bar
    /// of its own, which the window is dragged by.
    fn page_title_bar(
        &self,
        root: &mut egui::Ui,
        window: f32,
        status: Option<&Result<Status, String>>,
    ) {
        let title = self.page.title();
        let index = self.page == Page::Index;
        vino_ui::window::bar(
            root,
            widgets::page_inset(window),
            |ui| {
                widgets::title(ui, title);
            },
            |ui| {
                if index {
                    Self::index_status(ui, status);
                }
            },
        );
    }

    fn page(
        &mut self,
        ui: &mut egui::Ui,
        bar: bool,
        status: Option<&Result<Status, String>>,
        largest: Option<&Vec<(String, u64)>>,
    ) {
        widgets::page_scroll(ui, self.page.title(), self.page.max_width(), |ui| {
            if !bar {
                ui.horizontal(|ui| {
                    widgets::title(ui, self.page.title());
                    if self.page == Page::Index {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            Self::index_status(ui, status);
                        });
                    }
                });
            }
            if let Some(notice) = self.notice.clone() {
                ui.add_space(12.0);
                if widgets::notice(ui, Tone::Error, &notice) {
                    self.notice = None;
                }
            }
            match self.page {
                Page::Index => self.index(ui, status, largest),
                Page::Folders => self.folders(ui, status),
                Page::Search => self.search(ui, status),
                Page::Security => self.security(ui, status),
                Page::Model => self.model(ui, status),
                Page::Hardware => self.hardware(ui),
                Page::Integration => self.integration(ui),
                Page::About => vino_ui::about::page_with_updates(ui, &mut self.updater),
            }
        });
    }

    /// Typing or dragging a number changes it every frame: the file is written once it settles.
    fn save_once_numbers_settle(&mut self, ctx: &egui::Context) {
        if self.idle_dirty && ctx.dragged_id().is_none() && !ctx.egui_wants_keyboard_input() {
            self.idle_dirty = false;
            self.save_config();
        }
    }

    /// Back from the background: the desktop theme or text size, the config file or the KRunner
    /// registration may have changed.
    fn catch_up_on_focus(&mut self, ctx: &egui::Context) {
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

    fn take_picked_folder(&mut self) {
        let picked = self.picked.lock().unwrap().take();
        if let Some((target, answer)) = picked {
            self.picking = false;
            match answer {
                Ok(Some(path)) => self.add(target, &path),
                Ok(None) => {}
                Err(e) => self.notice = Some(format!("No folder chooser: {e}")),
            }
        }
    }
}

pub fn run(app_id: &str, page: Option<&str>) -> ExitCode {
    let page = page.and_then(Page::parse);
    let app_title_bar = vino_ui::window::app_title_bar();
    let options = eframe::NativeOptions {
        viewport: vino_ui::window::viewport(
            egui::ViewportBuilder::default()
                .with_title(TITLE)
                .with_app_id(app_id)
                .with_inner_size([820.0, 676.0])
                .with_min_inner_size([340.0, 320.0]),
            app_title_bar,
        ),
        glow_options: without_vsync(),
        ..Default::default()
    };
    let result = eframe::run_native(
        TITLE,
        options,
        Box::new(|cc| {
            theme::install(&cc.egui_ctx);
            let mut window = SettingsWindow::new(&cc.egui_ctx, page);
            window.app_title_bar = app_title_bar;
            Ok(Box::new(window))
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

/// With vsync, a swap waits on a frame callback KWin holds back from a window
/// it considers hidden, and the window stops answering pings.
fn without_vsync() -> eframe::egui_glow::GlowConfiguration {
    eframe::egui_glow::GlowConfiguration {
        vsync: false,
        ..Default::default()
    }
}

fn name_like_the_title_bar(ctx: &egui::Context) {
    ctx.accesskit_node_builder(egui::accesskit_root_id(), |node| node.set_label(TITLE));
}

/// vino-ui's brand, which the window bar's row is centred on.
fn brand(ui: &mut egui::Ui) -> egui::Rect {
    ui.scope(|ui| {
        ui.style_mut().interaction.selectable_labels = false;
        widgets::sidebar_brand(ui, Some(vino_ui::logo::search), "vinoSearch", "Settings");
    })
    .response
    .rect
}

fn drag_by_brand(ui: &mut egui::Ui, brand: egui::Rect) {
    let clip = ui.clip_rect();
    vino_ui::window::drag(
        ui,
        egui::Rect::from_x_y_ranges(clip.x_range(), clip.top()..=brand.bottom()),
        "brand",
    );
}

fn service_chip(ui: &mut egui::Ui, status: Option<&Result<Status, String>>) {
    let shown = match status {
        None => widgets::chip(ui, ChipKind::Neutral, "Connecting…"),
        Some(Err(_)) => widgets::chip(ui, ChipKind::Err, "Service stopped"),
        Some(Ok(_)) => widgets::chip(ui, ChipKind::Ok, "Service running"),
    };
    a11y::live(&shown);
}

/// No right margin: the scroll bar belongs on the window's edge, and the
/// content keeps its own gap from it. Under a title bar, 2 keeps the same 15
/// from title to content as in the scroll area.
fn page_margin(window: f32, wide: bool, bar: bool) -> egui::Margin {
    egui::Margin {
        left: if wide {
            widgets::page_inset(window) as i8
        } else {
            16
        },
        right: 0,
        top: match (wide, bar) {
            (_, true) => 2,
            (true, false) => 20,
            (false, false) => 12,
        },
        bottom: 0,
    }
}

mod estimate;
mod labels;
mod pages;
pub mod search;
#[cfg(test)]
mod ui_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_parse_by_title() {
        assert!(Page::parse("model") == Some(Page::Model));
        assert!(Page::parse("Hardware") == Some(Page::Hardware));
        assert!(Page::parse("folders") == Some(Page::Folders));
        assert!(Page::parse("search") == Some(Page::Search));
        assert!(Page::parse("Security") == Some(Page::Security));
        assert!(Page::parse("nope").is_none());
    }

    #[test]
    fn a_score_is_stored_rounded_to_hundredths() {
        assert_eq!(stored_score(0.7049, 0.65), Some(0.7));
        assert_eq!(stored_score(0.706, 0.65), Some(0.71));
    }

    #[test]
    fn a_score_that_rounds_to_the_default_is_left_unwritten() {
        assert_eq!(stored_score(0.6501, 0.65), None);
        assert_eq!(stored_score(0.66, 0.65), Some(0.66));
    }
}
