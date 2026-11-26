//! The search window: `vinosearch-settings --search "query"`, opened
//! from KRunner's Show all results and GNOME Shell's provider icon. Every file
//! a query finds, photos and media as grids of thumbnails and text, documents,
//! code and sessions as a list with their snippet, narrowed by kind, folder,
//! date and file type.
//!
//! A window of its own rather than a settings page: it opens per query and wants the room a grid
//! needs. It shares the settings' theme, header and accessibility helpers, and asks the runner over
//! D-Bus; it never loads the model.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use eframe::egui::{self, Align, Color32, FontId, Key, Layout, Modifiers, RichText, Sense};
use vino_ui::a11y;
use vino_ui::file_tile::{self, Format};
use vino_ui::icons::{self, Icon};
use vino_ui::logo;
use vino_ui::theme;
use vino_ui::widgets::{self, Button};

use super::tilde;

mod source;
mod thumbs;

use source::{Ask, Found, Kind, Link, Source};
use thumbs::{Thumb, Thumbs};

const TITLE: &str = "vinoSearch";
/// As KRunner's plugin and GNOME Shell's provider: fewer letters say too little.
const MIN_QUERY_CHARS: usize = 3;
const TYPING_PAUSE: f64 = 0.4;
const TILE: f32 = 150.0;
const TILE_LABEL: f32 = 24.0;
const TILE_RADIUS: f32 = 10.0;
const GRID_TILE: f32 = 60.0;
const DAY: i64 = 86_400;
const MENU_WIDTH: f32 = 180.0;
const MENU_MIN: f32 = 140.0;
const MENU_ICON: f32 = 16.0 + 6.0;
const HEADER_INSET: f32 = 20.0;
/// Sections shown as grids: a video's thumbnail is its picture.
const GRIDS: [(&str, [Kind; 1]); 2] = [("Images", [Kind::Image]), ("Media", [Kind::Media])];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Modified {
    Any,
    Day,
    Week,
    Month,
    Year,
}

impl Modified {
    const ALL: [Modified; 5] = [
        Modified::Any,
        Modified::Day,
        Modified::Week,
        Modified::Month,
        Modified::Year,
    ];

    fn label(self) -> &'static str {
        match self {
            Modified::Any => "Any time",
            Modified::Day => "Past day",
            Modified::Week => "Past week",
            Modified::Month => "Past month",
            Modified::Year => "Past year",
        }
    }

    fn since(self, now: i64) -> i64 {
        now - match self {
            Modified::Any => return i64::MIN,
            Modified::Day => DAY,
            Modified::Week => 7 * DAY,
            Modified::Month => 30 * DAY,
            Modified::Year => 365 * DAY,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Filters {
    text: bool,
    documents: bool,
    images: bool,
    code: bool,
    sessions: bool,
    media: bool,
    folder: Option<String>,
    modified: Modified,
    extension: Option<String>,
}

impl Default for Filters {
    fn default() -> Self {
        Filters {
            text: true,
            documents: true,
            images: true,
            code: true,
            sessions: true,
            media: true,
            folder: None,
            modified: Modified::Any,
            extension: None,
        }
    }
}

impl Filters {
    fn keeps(&self, f: &Found, now: i64) -> bool {
        let kind = match f.kind {
            Kind::Text => self.text,
            Kind::Document => self.documents,
            Kind::Image => self.images,
            Kind::Code => self.code,
            Kind::Session => self.sessions,
            Kind::Media => self.media,
        };
        kind && self
            .folder
            .as_ref()
            .is_none_or(|root| Path::new(&f.path).starts_with(root))
            && f.mtime >= self.modified.since(now)
            && self.extension.as_ref().is_none_or(|e| *e == f.extension())
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Act {
    Open,
    OpenTranscript,
    OpenFolder,
    Copy,
}

pub struct SearchWindow {
    query: String,
    searched: String,
    typed_at: Option<f64>,
    /// Numbers each search, so an answer to an older one is dropped.
    asked: u64,
    searching: bool,
    results: Vec<Found>,
    error: Option<String>,
    filters: Filters,
    selected: Option<String>,
    notice: Option<String>,
    link: Link,
    thumbs: Thumbs,
    focus_query: bool,
    focus_first: bool,
    now: fn() -> i64,
    focused: bool,
    kde_header: Option<theme::KdeHeader>,
    app_title_bar: bool,
    text_scale: f32,
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

impl SearchWindow {
    fn with(query: &str, source: impl Source) -> Self {
        let mut window = SearchWindow {
            query: query.trim().to_owned(),
            searched: String::new(),
            typed_at: None,
            asked: 0,
            searching: false,
            results: Vec::new(),
            error: None,
            filters: Filters::default(),
            selected: None,
            notice: None,
            link: Link::start(source),
            thumbs: Thumbs::default(),
            focus_query: true,
            focus_first: false,
            now: unix_now,
            focused: true,
            kde_header: theme::kde_header(),
            app_title_bar: false,
            text_scale: 1.0,
        };
        if !window.query.is_empty() {
            window.search();
        }
        window
    }

    fn search(&mut self) {
        self.typed_at = None;
        let query = self.query.trim().to_owned();
        if query == self.searched && (self.searching || self.error.is_none()) {
            return;
        }
        self.asked += 1;
        self.searched = query.clone();
        self.searching = true;
        self.link.send(Ask::Search(self.asked, query));
    }

    fn act(&mut self, act: Act, f: &Found) {
        self.link.send(match act {
            Act::Open if f.timed() => Ask::Play(f.path.clone(), f.at),
            Act::Open => Ask::Open(f.path.clone(), f.at),
            Act::OpenTranscript => Ask::Open(f.path.clone(), 0),
            Act::OpenFolder => Ask::OpenFolder(f.path.clone()),
            Act::Copy => Ask::Copy(f.path.clone()),
        });
    }

    fn selected(&self) -> Option<&Found> {
        let path = self.selected.as_ref()?;
        self.results.iter().find(|f| &f.path == path)
    }

    fn shown(&self) -> Vec<Found> {
        let now = (self.now)();
        self.results
            .iter()
            .filter(|f| self.filters.keeps(f, now))
            .cloned()
            .collect()
    }

    fn take_answers(&mut self) {
        let (results, refused) = {
            let mut shared = self.link.shared.lock().unwrap();
            (shared.results.take(), shared.refused.take())
        };
        if let Some((n, answer)) = results
            && n == self.asked
        {
            self.searching = false;
            match answer {
                Ok(results) => {
                    self.results = results;
                    self.error = None;
                    self.drop_a_type_filter_these_results_lack();
                    if self.selected().is_none() {
                        self.selected = None;
                    }
                }
                Err(e) => {
                    self.results.clear();
                    self.error = Some(e);
                }
            }
        }
        if let Some(refused) = refused {
            self.notice = Some(refused);
        } else if let Some(done) = self.link.shared.lock().unwrap().done.take() {
            self.notice = Some(done);
        }
    }

    fn drop_a_type_filter_these_results_lack(&mut self) {
        if let Some(e) = &self.filters.extension
            && !self.results.iter().any(|f| f.extension() == *e)
        {
            self.filters.extension = None;
        }
    }

    pub fn show(&mut self, root: &mut egui::Ui) {
        let ctx = &root.ctx().clone();
        self.link.attach(ctx);
        let focused = ctx.input(|i| i.focused);
        if focused && !self.focused {
            self.catch_up_on_focus(ctx);
        }
        self.focused = focused;
        self.take_answers();
        ctx.accesskit_node_builder(egui::accesskit_root_id(), |node| node.set_label(TITLE));
        self.search_once_typing_pauses(ctx);
        self.shortcuts(ctx);

        let p = theme::palette(ctx);
        let header = theme::header(ctx, self.kde_header.as_ref(), focused);
        let shown = self.shown();
        self.header(root, p, header);
        if let Some(f) = self.selected().cloned() {
            self.actions(root, p, &f);
        }
        let inset = widgets::content_inset(ctx.content_rect().width()) as i8;
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(p.bg).inner_margin(egui::Margin {
                left: inset,
                right: 0,
                top: 4,
                bottom: 0,
            }))
            .show(root, |ui| {
                widgets::page_scroll(ui, "Results", f32::INFINITY, |ui| {
                    self.notice_line(ui);
                    self.results(ui, &shown);
                });
            });
    }

    fn catch_up_on_focus(&mut self, ctx: &egui::Context) {
        theme::follow_desktop(ctx);
        self.kde_header = theme::kde_header();
        let scale = theme::text_scale();
        if scale != self.text_scale {
            self.text_scale = scale;
            ctx.set_zoom_factor(scale);
        }
    }

    fn search_once_typing_pauses(&mut self, ctx: &egui::Context) {
        let Some(at) = self.typed_at else {
            return;
        };
        let left = TYPING_PAUSE - (ctx.input(|i| i.time) - at);
        if left > 0.0 {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(left));
        } else if self.query.trim().chars().count() >= MIN_QUERY_CHARS {
            self.search();
        } else {
            self.typed_at = None;
        }
    }

    /// Ctrl+F or Ctrl+L goes back to the search field. Ctrl+C copies the
    /// selected file while it, or nothing, has focus: not from the field
    /// (pinned by `window::search::ui_tests::the_keyboard_reaches_the_results_and_opens_one`).
    fn shortcuts(&mut self, ctx: &egui::Context) {
        let (find, copy) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::COMMAND, Key::F)
                    || i.consume_key(Modifiers::COMMAND, Key::L),
                i.events.iter().any(|e| matches!(e, egui::Event::Copy)),
            )
        });
        if find {
            self.focus_query = true;
        }
        if copy
            && let Some(f) = self.selected().cloned()
            && ctx
                .memory(|m| m.focused())
                .is_none_or(|id| id == result_id(&f))
        {
            self.act(Act::Copy, &f);
        }
    }

    /// The query and the filters. With the app's title bar the window buttons go up in the corner
    /// and the filters keep the inset they gave up (pinned by
    /// `window::search::ui_tests::the_window_buttons_sit_in_the_top_right_corner`).
    fn header(&mut self, root: &mut egui::Ui, p: &theme::Palette, header: theme::Header) {
        egui::Panel::top("search-header")
            .show_separator_line(false)
            .frame(widgets::header_frame(header, p).inner_margin(egui::Margin {
                left: HEADER_INSET as i8,
                right: if self.app_title_bar {
                    vino_ui::window::BAR_GAP as i8
                } else {
                    HEADER_INSET as i8
                },
                top: 8,
                bottom: 14,
            }))
            .show(root, |ui| {
                let right_inset = if self.app_title_bar {
                    HEADER_INSET - vino_ui::window::BAR_GAP
                } else {
                    0.0
                };
                if self.app_title_bar {
                    vino_ui::window::drag(ui, ui.clip_rect(), "header");
                }
                ui.horizontal(|ui| self.query_row(ui, right_inset));
                ui.add_space(4.0);
                let width = ui.max_rect().right() - right_inset - ui.cursor().left();
                ui.allocate_ui(egui::vec2(width, 0.0), |ui| self.filter_bar(ui));
            });
    }

    /// Enter searches and keeps the field focused, so Enter again goes to the results.
    fn query_row(&mut self, ui: &mut egui::Ui, right_inset: f32) {
        ui.spacing_mut().item_spacing.x = 12.0;
        vino_ui::logo::search(ui, 28.0);
        let window_buttons = if self.app_title_bar {
            vino_ui::window::controls_width() + 12.0
        } else {
            0.0
        };
        let width =
            (ui.available_width() - 24.0 - right_inset - window_buttons).clamp(120.0, 640.0);
        let field = widgets::search_field(
            ui,
            &mut self.query,
            "Describe what you're looking for",
            "Search",
            width,
        );
        if self.focus_query {
            self.focus_query = false;
            field.request_focus();
        }
        if field.changed() {
            self.typed_at = Some(ui.input(|i| i.time));
        }
        if field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
            if !self.query.trim().is_empty() {
                self.search();
            }
            field.request_focus();
        }
        if field.has_focus() && ui.input(|i| i.key_pressed(Key::ArrowDown)) {
            self.focus_first = true;
        }
        if self.searching {
            widgets::spinner(ui);
        }
        if self.app_title_bar {
            window_buttons_at_the_top(ui);
        }
    }

    fn actions(&mut self, root: &mut egui::Ui, p: &theme::Palette, f: &Found) {
        let inset = widgets::content_inset(root.ctx().content_rect().width()).min(20.0) as i8;
        let mut chosen = None;
        egui::Panel::bottom("actions")
            .show_separator_line(false)
            .frame(
                egui::Frame::NONE
                    .fill(p.surface)
                    .stroke(egui::Stroke::new(1.0_f32, p.rule))
                    .inner_margin(egui::Margin::symmetric(inset, 10)),
            )
            .show(root, |ui| {
                let open = if f.timed() {
                    format!("Play at {}", f.place())
                } else if f.place().is_empty() {
                    "Open".to_owned()
                } else {
                    format!("Open at {}", f.place())
                };
                let has_transcript = f.timed() && format_of(&f.path).is_none();
                let wide = ui.available_width() >= widgets::COMPACT_MAX;
                let draw_path = |ui: &mut egui::Ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(tilde(Path::new(&f.path)))
                                .monospace()
                                .color(p.muted),
                        )
                        .truncate(),
                    )
                    .on_hover_text(&f.path);
                };
                let named = |r| a11y::for_what(r, f.name());
                let draw_open = |ui: &mut egui::Ui, chosen: &mut Option<Act>| {
                    if named(ui.add(Button::primary(open.clone())))
                        .on_hover_text("Enter")
                        .clicked()
                    {
                        *chosen = Some(Act::Open);
                    }
                };
                let draw_transcript = |ui: &mut egui::Ui, chosen: &mut Option<Act>| {
                    if has_transcript && named(ui.add(Button::tonal("Open transcript"))).clicked() {
                        *chosen = Some(Act::OpenTranscript);
                    }
                };
                let draw_folder = |ui: &mut egui::Ui, chosen: &mut Option<Act>| {
                    if named(ui.add(Button::secondary("Open folder"))).clicked() {
                        *chosen = Some(Act::OpenFolder);
                    }
                };
                let draw_copy = |ui: &mut egui::Ui, chosen: &mut Option<Act>| {
                    if named(ui.add(Button::secondary("Copy file")))
                        .on_hover_text("Ctrl+C")
                        .clicked()
                    {
                        *chosen = Some(Act::Copy);
                    }
                };
                if wide {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 12.0;
                        tile(ui, f, 20.0);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            draw_open(ui, &mut chosen);
                            draw_transcript(ui, &mut chosen);
                            draw_folder(ui, &mut chosen);
                            draw_copy(ui, &mut chosen);
                            ui.with_layout(Layout::left_to_right(Align::Center), draw_path);
                        });
                    });
                } else {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 10.0;
                        tile(ui, f, 20.0);
                        draw_path(ui);
                    });
                    ui.horizontal_wrapped(|ui| {
                        draw_open(ui, &mut chosen);
                        draw_transcript(ui, &mut chosen);
                        draw_folder(ui, &mut chosen);
                        draw_copy(ui, &mut chosen);
                    });
                }
            });
        if let Some(act) = chosen {
            self.act(act, f);
        }
    }

    fn notice_line(&mut self, ui: &mut egui::Ui) {
        let Some(notice) = self.notice.clone() else {
            return;
        };
        ui.add_space(8.0);
        if widgets::notice(ui, widgets::Tone::Info, &notice) {
            self.notice = None;
        }
    }

    fn filter_bar(&mut self, ui: &mut egui::Ui) {
        let p = theme::palette(ui.ctx());
        let count = |kind| self.results.iter().filter(|f| f.kind == kind).count();
        let roots = self.link.shared.lock().unwrap().roots.clone();
        let mut extensions: BTreeMap<String, usize> = BTreeMap::new();
        for f in &self.results {
            *extensions.entry(f.extension()).or_default() += 1;
        }
        let filters = &mut self.filters;
        let wrap_at = ui.max_rect().right();
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
            for (on, label, kind) in [
                (&mut filters.images, "Images", Kind::Image),
                (&mut filters.text, "Text", Kind::Text),
                (&mut filters.documents, "Documents", Kind::Document),
                (&mut filters.code, "Code", Kind::Code),
                (&mut filters.sessions, "Sessions", Kind::Session),
                (&mut filters.media, "Media", Kind::Media),
            ] {
                widgets::toggle_chip(ui, on, label, Some(count(kind)));
            }
            keep_the_menus_together(ui, p, wrap_at);

            let folder = filters
                .folder
                .as_deref()
                .map_or("All folders".to_owned(), |r| tilde(Path::new(r)));
            menu(ui, Icon::Folder, "Folder", folder, wrap_at, |ui| {
                ui.selectable_value(&mut filters.folder, None, "All folders");
                for root in &roots {
                    ui.selectable_value(
                        &mut filters.folder,
                        Some(root.clone()),
                        tilde(Path::new(root)),
                    );
                }
            });
            menu(
                ui,
                Icon::Clock,
                "Modified",
                filters.modified.label().to_owned(),
                wrap_at,
                |ui| {
                    for m in Modified::ALL {
                        ui.selectable_value(&mut filters.modified, m, m.label());
                    }
                },
            );
            let selected = filters
                .extension
                .as_deref()
                .map_or("All types".to_owned(), type_name);
            menu(ui, Icon::File, "File type", selected, wrap_at, |ui| {
                ui.selectable_value(&mut filters.extension, None, "All types");
                for (e, n) in &extensions {
                    let name = format!("{} ({n})", type_name(e));
                    ui.selectable_value(&mut filters.extension, Some(e.clone()), name);
                }
            });

            if *filters != Filters::default()
                && ui.add(Button::ghost("Clear filters").small()).clicked()
            {
                *filters = Filters::default();
            }
        });
    }

    fn results(&mut self, ui: &mut egui::Ui, shown: &[Found]) {
        let p = theme::palette(ui.ctx());
        ui.add_space(8.0);
        let status = if let Some(e) = &self.error {
            Some(RichText::new(format!("Search failed: {e}")).color(p.error))
        } else if self.searching && self.results.is_empty() {
            Some(RichText::new("Searching…").color(p.muted))
        } else if self.searched.is_empty() {
            let hint = if self.query.trim().is_empty() {
                "Type a query to search your files (a descriptive phrase works better than a \
                 filename)."
            } else {
                "Press Enter to search."
            };
            Some(RichText::new(hint).color(p.muted))
        } else if self.results.is_empty() {
            Some(RichText::new(format!("No results found for “{}”.", self.searched)).color(p.muted))
        } else if shown.is_empty() {
            Some(RichText::new("No results match the filters.").color(p.muted))
        } else {
            None
        };
        if let Some(status) = status {
            let label = ui.add(egui::Label::new(status).wrap());
            a11y::live(&label);
            return;
        }
        let mut first = std::mem::take(&mut self.focus_first);
        let of = |kinds: &[Kind]| -> Vec<&Found> {
            shown.iter().filter(|f| kinds.contains(&f.kind)).collect()
        };
        let mut any = false;
        for (title, kinds) in GRIDS {
            let tiles = of(&kinds);
            if !tiles.is_empty() {
                if std::mem::replace(&mut any, true) {
                    ui.add_space(10.0);
                }
                section(ui, title, tiles.len());
                self.grid(ui, title, &tiles, &mut first);
            }
        }
        let texts = of(&[Kind::Text, Kind::Document, Kind::Code, Kind::Session]);
        if !texts.is_empty() {
            if any {
                ui.add_space(10.0);
            }
            section(ui, "Text and documents", texts.len());
            self.list(ui, &texts, &mut first);
        }
    }

    fn grid(&mut self, ui: &mut egui::Ui, title: &str, images: &[&Found], first: &mut bool) {
        ui.horizontal_wrapped(|ui| {
            a11y::list_box(ui, title);
            ui.spacing_mut().item_spacing = egui::vec2(12.0, 12.0);
            let columns = ((ui.available_width() + 12.0) / (TILE + 12.0)).max(1.0) as usize;
            let mut to = None;
            for (i, f) in images.iter().enumerate() {
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(TILE, TILE + TILE_LABEL), Sense::hover());
                let response = ui.interact(rect, result_id(f), Sense::click());
                if ui.is_rect_visible(rect) {
                    self.tile(ui, rect, &response, f);
                }
                self.respond(ui, &response, f, first);
                to = to.or_else(|| arrows(ui, &response, i, images.len(), columns, 1));
            }
            self.move_to(ui, to.map(|i| images[i]));
        });
    }

    fn tile(&mut self, ui: &egui::Ui, rect: egui::Rect, response: &egui::Response, f: &Found) {
        let p = theme::palette(ui.ctx());
        let selected = self.selected.as_deref() == Some(f.path.as_str());
        let picture = egui::Rect::from_min_size(rect.min, egui::vec2(TILE, TILE));
        let painter = ui.painter();
        painter.rect_filled(picture, TILE_RADIUS, p.field);
        match self.thumbs.get(ui.ctx(), &f.path) {
            Thumb::Ready(texture) => {
                let size = texture.size_vec2();
                let fit = (picture.size() / size).min_elem();
                let at = egui::Rect::from_center_size(picture.center(), size * fit);
                painter.image(
                    texture.id(),
                    at,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            Thumb::Loading => {}
            Thumb::Failed => file_tile::paint(
                painter,
                egui::Rect::from_center_size(picture.center(), egui::vec2(GRID_TILE, GRID_TILE)),
                format_of(&f.path).unwrap_or(Format::Image),
            ),
        }
        let name = egui::Rect::from_min_max(egui::pos2(rect.left(), picture.bottom()), rect.max);
        let galley = painter.layout(
            f.name().to_owned(),
            FontId::proportional(12.5),
            p.text,
            TILE,
        );
        let clipped = painter.with_clip_rect(name);
        clipped.galley(
            egui::pos2(
                name.center().x - galley.size().x.min(TILE) / 2.0,
                name.top() + 6.0,
            ),
            galley,
            p.text,
        );
        if selected || response.has_focus() {
            widgets::paint_ring(ui, picture, TILE_RADIUS);
        } else if response.hovered() {
            painter.rect_stroke(
                picture,
                TILE_RADIUS,
                egui::Stroke::new(1.0_f32, p.edge),
                egui::StrokeKind::Inside,
            );
        }
    }

    fn list(&mut self, ui: &mut egui::Ui, texts: &[&Found], first: &mut bool) {
        let p = theme::palette(ui.ctx());
        ui.vertical(|ui| {
            a11y::list_box(ui, "Text and documents");
            ui.set_max_width(ui.available_width().min(760.0));
            ui.spacing_mut().item_spacing.y = 0.0;
            let mut to = None;
            let mut last_rect = None;
            for (i, f) in texts.iter().enumerate() {
                let response = self.row(ui, f);
                last_rect = Some(response.rect);
                self.respond(ui, &response, f, first);
                to = to.or_else(|| arrows(ui, &response, i, texts.len(), 1, 0));
            }
            if let Some(rect) = last_rect {
                ui.painter().hline(
                    rect.left()..=rect.right(),
                    rect.bottom(),
                    egui::Stroke::new(1.0_f32, p.rule),
                );
            }
            self.move_to(ui, to.map(|i| texts[i]));
        });
    }

    fn row(&mut self, ui: &mut egui::Ui, f: &Found) -> egui::Response {
        let p = theme::palette(ui.ctx());
        let selected = self.selected.as_deref() == Some(f.path.as_str());
        let width = ui.available_width();
        let bg = ui.painter().add(egui::Shape::Noop);
        let inner = egui::Frame::NONE
            .inner_margin(egui::Margin::symmetric(10, 10))
            .show(ui, |ui| {
                ui.set_width(width - 20.0);
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 12.0;
                    tile(ui, f, 30.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 3.0;
                        let place = f.place();
                        let mut detail = tilde(f.folder());
                        if !place.is_empty() {
                            detail = format!("{detail}, {place}");
                        }
                        ui.horizontal(|ui| {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&detail).color(p.muted).size(12.5),
                                    )
                                    .truncate()
                                    .selectable(false),
                                );
                                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(f.name()).family(theme::semibold()),
                                        )
                                        .truncate()
                                        .selectable(false),
                                    );
                                });
                            });
                        });
                        if !f.snippet.is_empty() {
                            ui.add(
                                egui::Label::new(RichText::new(&f.snippet).size(13.0))
                                    .wrap()
                                    .selectable(false),
                            );
                        }
                    });
                });
            });
        let rect = inner.response.rect;
        let response = ui.interact(rect, result_id(f), Sense::click());
        let painter = ui.painter();
        if selected || response.has_focus() {
            painter.set(bg, egui::Shape::rect_filled(rect, 0.0, p.tint));
            painter.rect_filled(
                egui::Rect::from_min_size(rect.min, egui::vec2(3.0, rect.height())),
                0.0,
                p.accent,
            );
        } else if response.hovered() {
            painter.set(bg, egui::Shape::rect_filled(rect, 0.0, p.field));
        }
        painter.hline(
            rect.left()..=rect.right(),
            rect.top(),
            egui::Stroke::new(1.0_f32, p.rule),
        );
        a11y::decorative(ui, inner.response.id);
        response
    }

    /// Focus and select `f`, which an arrow key moved to; egui's own arrow-key move is dropped
    /// because it goes to the nearest widget, often the wrong one in a wrapped grid.
    fn move_to(&mut self, ui: &egui::Ui, f: Option<&Found>) {
        if let Some(f) = f {
            ui.memory_mut(|m| {
                m.request_focus(result_id(f));
                m.move_focus(egui::FocusDirection::None);
            });
            self.selected = Some(f.path.clone());
        }
    }

    fn respond(&mut self, ui: &egui::Ui, response: &egui::Response, f: &Found, first: &mut bool) {
        let selected = self.selected.as_deref() == Some(f.path.as_str());
        let mut name = f.name().to_owned();
        let place = f.place();
        if !place.is_empty() {
            name = format!("{name}, {place}");
        }
        name = format!("{name}, in {}", tilde(f.folder()));
        if !f.snippet.is_empty() {
            name = format!("{name}: {}", f.snippet);
        }
        a11y::list_box_option(response, &name, selected);
        hover_preview(response, f, (self.now)());
        if std::mem::take(first) {
            response.request_focus();
            self.selected = Some(f.path.clone());
        }
        if response.gained_focus() || response.clicked() {
            self.selected = Some(f.path.clone());
        }
        let enter = response.has_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        if response.double_clicked() || enter {
            self.act(Act::Open, f);
        }
        response.context_menu(|ui| {
            self.selected = Some(f.path.clone());
            for (text, act) in [
                ("Open", Act::Open),
                ("Open folder", Act::OpenFolder),
                ("Copy file", Act::Copy),
            ] {
                if ui.button(text).clicked() {
                    self.act(act, f);
                    ui.close();
                }
            }
        });
    }
}

/// In-context hover card for a search result: full filename, path, match
/// location, relative modified time, and snippet without leaving the results.
fn hover_preview(response: &egui::Response, f: &Found, now: i64) {
    let title = f.name().to_owned();
    let full_path = tilde(Path::new(&f.path));
    let mut meta = Vec::new();
    let place = f.place();
    if !place.is_empty() {
        meta.push(place);
    }
    if f.mtime > 0 {
        meta.push(relative_age(now, f.mtime));
    }
    let meta = meta.join(" · ");
    let snippet = f.snippet.clone();
    response.clone().on_hover_ui(move |ui| {
        let p = theme::palette(ui.ctx());
        ui.set_max_width(widgets::popup_width(ui.ctx(), 280.0));
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.add(
            egui::Label::new(
                RichText::new(&title)
                    .size(13.5)
                    .family(theme::semibold())
                    .color(p.text),
            )
            .wrap(),
        );
        ui.add(
            egui::Label::new(
                RichText::new(&full_path)
                    .monospace()
                    .size(11.5)
                    .color(p.muted),
            )
            .wrap(),
        );
        if !meta.is_empty() {
            widgets::small(ui, &meta);
        }
        if !snippet.is_empty() {
            ui.add_space(2.0);
            ui.add(egui::Label::new(RichText::new(&snippet).size(12.5).color(p.text)).wrap());
        }
    });
}

fn relative_age(now: i64, mtime: i64) -> String {
    let days = ((now - mtime).max(0)) / DAY;
    match days {
        0 => "Modified today".to_owned(),
        1 => "Modified yesterday".to_owned(),
        2..=29 => format!("Modified {days} days ago"),
        30..=364 => {
            let months = (days / 30).max(1);
            let s = if months == 1 { "" } else { "s" };
            format!("Modified {months} month{s} ago")
        }
        _ => {
            let years = (days / 365).max(1);
            let s = if years == 1 { "" } else { "s" };
            format!("Modified {years} year{s} ago")
        }
    }
}

/// A vinoWhisper session bundle's extension (the indexer's `extract::session::EXTENSION`).
const SESSION: &str = "vwsession";

/// A file's tile: vino-ui's for its format, so a file looks the same here as in vinoSpeak; the
/// vinoWhisper logo for a session or a recording found in one.
fn tile(ui: &mut egui::Ui, f: &Found, size: f32) -> egui::Response {
    match format_of(&f.path) {
        Some(format) => file_tile::file(ui, format, size),
        None => logo::whisper(ui, size),
    }
}

fn format_of(path: &str) -> Option<Format> {
    let path = Path::new(path);
    let session = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case(SESSION));
    (!session).then(|| Format::of(path))
}

fn arrows(
    ui: &egui::Ui,
    response: &egui::Response,
    i: usize,
    len: usize,
    down: usize,
    across: usize,
) -> Option<usize> {
    if !response.has_focus() {
        return None;
    }
    let key = |k| ui.input(|input| input.key_pressed(k) && input.modifiers.is_none());
    let to = if key(Key::ArrowDown) {
        i.checked_add(down)
    } else if key(Key::ArrowUp) {
        i.checked_sub(down)
    } else if across > 0 && key(Key::ArrowRight) {
        i.checked_add(across)
    } else if across > 0 && key(Key::ArrowLeft) {
        i.checked_sub(across)
    } else {
        None
    };
    to.filter(|&to| to < len)
}

/// A result's own id: one from its place in the layout changes when the action bar appears, which
/// would drop its focus.
fn result_id(f: &Found) -> egui::Id {
    egui::Id::unique(("vinosearch-result", &f.path))
}

/// A filter's menu with an icon before it, named by `label` for a screen reader and kept on one
/// line with its icon when the bar wraps at `wrap_at` (a wrapping row's own width grows with what
/// overflows it; a group of its own doesn't wrap as a widget does, so the combo goes on to the next
/// row).
fn menu(
    ui: &mut egui::Ui,
    icon: Icon,
    label: &str,
    selected: String,
    wrap_at: f32,
    items: impl FnOnce(&mut egui::Ui),
) {
    let p = theme::palette(ui.ctx());
    let left = ui.cursor().left();
    if left > ui.max_rect().left() && wrap_at - left < MENU_ICON + MENU_MIN {
        ui.end_row();
    }
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        icons::icon(ui, icon, 16.0, p.muted);
        let shown = widgets::combo(ui, label, selected, MENU_WIDTH).show_ui(ui, items);
        ui.ctx()
            .accesskit_node_builder(shown.response.id, |node| node.set_label(label));
    });
}

fn keep_the_menus_together(ui: &mut egui::Ui, p: &theme::Palette, wrap_at: f32) {
    const MENUS: f32 = 3.0 * (MENU_ICON + MENU_WIDTH) + 2.0 * 8.0;
    if wrap_at - ui.cursor().left() < 1.0 + 8.0 + MENUS {
        ui.end_row();
    } else {
        let (rule, _) = ui.allocate_exact_size(egui::vec2(1.0, 20.0), Sense::hover());
        ui.painter().rect_filled(rule, 0.0, p.rule);
    }
}

/// At the row's top, not centred on the taller field (pinned by
/// `window::search::ui_tests::the_window_buttons_sit_in_the_top_right_corner`).
fn window_buttons_at_the_top(ui: &mut egui::Ui) {
    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(vino_ui::window::controls_width(), vino_ui::window::BUTTON),
            Layout::right_to_left(Align::Center),
            vino_ui::window::controls,
        );
    });
}

fn type_name(extension: &str) -> String {
    if extension.is_empty() {
        "No extension".to_owned()
    } else {
        extension.to_uppercase()
    }
}

fn section(ui: &mut egui::Ui, title: &str, n: usize) {
    let p = theme::palette(ui.ctx());
    ui.add_space(8.0);
    let name = format!("{title} ({n})");
    let shown = ui
        .horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let caps = ui.label(
                RichText::new(title.to_uppercase())
                    .size(12.0)
                    .family(theme::semibold())
                    .extra_letter_spacing(0.7)
                    .color(p.muted),
            );
            a11y::decorative(ui, caps.id);
            let count = ui.label(RichText::new(n.to_string()).size(12.0).color(p.muted));
            a11y::decorative(ui, count.id);
        })
        .response;
    a11y::heading(&shown, &name, 2);
    ui.add_space(6.0);
}

impl eframe::App for SearchWindow {
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

pub fn run(app_id: &str, query: &str) -> ExitCode {
    let app_title_bar = vino_ui::window::app_title_bar();
    let options = eframe::NativeOptions {
        viewport: vino_ui::window::viewport(
            egui::ViewportBuilder::default()
                .with_title(TITLE)
                .with_app_id(app_id)
                .with_inner_size([960.0, 680.0])
                .with_min_inner_size([360.0, 320.0]),
            app_title_bar,
        ),
        glow_options: eframe::egui_glow::GlowConfiguration {
            vsync: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let query = query.to_owned();
    let result = eframe::run_native(
        TITLE,
        options,
        Box::new(move |cc| {
            theme::install(&cc.egui_ctx);
            let mut window = SearchWindow::with(&query, source::Runner::default());
            window.app_title_bar = app_title_bar;
            window.text_scale = theme::text_scale();
            cc.egui_ctx.set_zoom_factor(window.text_scale);
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

#[cfg(test)]
mod ui_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// The lists vinoSearch indexes by aren't reachable from this crate; vino-ui's own test walks
    /// them. This checks the tile for each kind, and that a session gets the logo.
    #[test]
    fn a_file_gets_the_tile_of_its_format_and_a_session_the_logo() {
        assert_eq!(format_of("/a/b.PDF"), Some(Format::Pdf));
        assert_eq!(format_of("/a/b.odt"), Some(Format::Writing));
        assert_eq!(format_of("/a/b.pptx"), Some(Format::Slides));
        assert_eq!(format_of("/a/b.epub"), Some(Format::Book));
        assert_eq!(format_of("/a/b.srt"), Some(Format::Subtitles));
        assert_eq!(format_of("/a/b.md"), Some(Format::Markdown));
        assert_eq!(format_of("/a/b.csv"), Some(Format::Sheet));
        assert_eq!(format_of("/a/b.py"), Some(Format::Code));
        assert_eq!(format_of("/a/b.jpg"), Some(Format::Image));
        assert_eq!(format_of("/a/b.txt"), Some(Format::Plain));
        assert_eq!(format_of("/a/Makefile"), Some(Format::Plain));
        assert_eq!(format_of("/t/2026-10-29-101500.vwsession"), None);
        assert_eq!(format_of("/t/a.VWSESSION"), None);
    }
}
