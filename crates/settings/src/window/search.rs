//! The search window (#111): `vinosearch-settings --search "query"`, opened
//! from KRunner's Show all results and GNOME Shell's provider icon. Every file
//! a query finds, photos as a grid of thumbnails and text and documents as a
//! list with their snippet, narrowed by kind, folder, date and file type.
//!
//! A window of its own rather than a page among the settings: it opens per
//! query, wants the room a grid needs, and the settings already have a Search
//! page (what counts as a match). It shares their theme, header and
//! accessibility helpers. Like them it asks the runner over D-Bus and never
//! loads the model itself.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use eframe::egui::{
    self, Align, Color32, FontId, Key, Layout, Modifiers, RichText, Sense, Stroke, WidgetInfo,
    accesskit,
};

use super::{decorative, for_what, heading, logo, name_scrolling, spinner, tilde};
use crate::theme;

mod source;
mod thumbs;

use source::{Ask, Found, Kind, Link, Source};
use thumbs::{Thumb, Thumbs};

const TITLE: &str = "vinoSearch";
/// As KRunner's plugin and GNOME Shell's provider: fewer letters say too little.
const MIN_QUERY_CHARS: usize = 3;
/// Typing pauses this long before the query is searched.
const TYPING_PAUSE: f64 = 0.4;
const TILE: f32 = 150.0;
/// Below a tile, its file name.
const TILE_LABEL: f32 = 22.0;
const DAY: i64 = 86_400;

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

    /// The oldest mtime that passes, in Unix seconds.
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
    /// An indexed root; None for all of them.
    folder: Option<String>,
    modified: Modified,
    /// A lowercased extension; None for any.
    extension: Option<String>,
}

impl Default for Filters {
    fn default() -> Self {
        Filters {
            text: true,
            documents: true,
            images: true,
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
        };
        kind && self
            .folder
            .as_ref()
            .is_none_or(|root| Path::new(&f.path).starts_with(root))
            && f.mtime >= self.modified.since(now)
            && self.extension.as_ref().is_none_or(|e| *e == f.extension())
    }
}

/// An action on a result.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Act {
    Open,
    OpenFolder,
    Copy,
}

pub struct SearchWindow {
    query: String,
    /// The query the results are for, or are being searched for.
    searched: String,
    /// When the query was last typed in, until it is searched.
    typed_at: Option<f64>,
    /// Numbers each search, so an answer to an older one is dropped.
    asked: u64,
    searching: bool,
    results: Vec<Found>,
    error: Option<String>,
    filters: Filters,
    /// A result's path.
    selected: Option<String>,
    notice: Option<String>,
    link: Link,
    thumbs: Thumbs,
    /// Put the cursor in the search field on the next frame.
    focus_query: bool,
    /// Move focus to the first result on the next frame.
    focus_first: bool,
    /// Unix seconds, for the date filter; the clock in the app.
    now: fn() -> i64,
    focused: bool,
    kde_header: Option<theme::KdeHeader>,
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
            Act::Open => Ask::Open(f.path.clone(), f.at),
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

    /// Answers from the link thread.
    fn take_answers(&mut self) {
        let mut shared = self.link.shared.lock().unwrap();
        if let Some((n, answer)) = shared.results.take()
            && n == self.asked
        {
            self.searching = false;
            match answer {
                Ok(results) => {
                    self.results = results;
                    self.error = None;
                    // Filters for what these results don't have would hide everything.
                    if let Some(e) = &self.filters.extension
                        && !self.results.iter().any(|f| f.extension() == *e)
                    {
                        self.filters.extension = None;
                    }
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
        if let Some(refused) = shared.refused.take() {
            self.notice = Some(refused);
        } else if let Some(done) = shared.done.take() {
            self.notice = Some(done);
        }
    }

    pub fn show(&mut self, root: &mut egui::Ui) {
        let ctx = &root.ctx().clone();
        self.link.attach(ctx);
        let focused = ctx.input(|i| i.focused);
        if focused && !self.focused {
            theme::follow_desktop(ctx);
            self.kde_header = theme::kde_header();
            let scale = theme::text_scale();
            if scale != self.text_scale {
                self.text_scale = scale;
                ctx.set_zoom_factor(scale);
            }
        }
        self.focused = focused;
        self.take_answers();
        ctx.accesskit_node_builder(egui::accesskit_root_id(), |node| node.set_label(TITLE));

        if let Some(at) = self.typed_at {
            let left = TYPING_PAUSE - (ctx.input(|i| i.time) - at);
            if left <= 0.0 {
                if self.query.trim().chars().count() >= MIN_QUERY_CHARS {
                    self.search();
                } else {
                    self.typed_at = None;
                }
            } else {
                ctx.request_repaint_after(std::time::Duration::from_secs_f64(left));
            }
        }

        // Ctrl+F or Ctrl+L: back to the search field. Ctrl+C copies the
        // selected file while it, or nothing, has focus: not from the field.
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

        let p = theme::palette(ctx);
        let header = theme::header(ctx, self.kde_header.as_ref(), focused);
        let shown = self.shown();

        egui::Panel::top("search-header")
            .show_separator_line(false)
            .frame(
                egui::Frame::NONE
                    .fill(header.bg)
                    .inner_margin(egui::Margin::symmetric(16, 10)),
            )
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    logo(ui, 22.0);
                    let label = ui.label(
                        RichText::new("Search")
                            .family(theme::bold())
                            .size(15.0)
                            .color(header.text),
                    );
                    heading(ui, &label, "Search", 1);
                    let width = (ui.available_width() - 40.0).clamp(120.0, 640.0);
                    let field = ui.add(
                        egui::TextEdit::singleline(&mut self.query)
                            .id_salt("query")
                            .hint_text("Describe what you're looking for")
                            .desired_width(width)
                            .min_size(egui::vec2(0.0, ui.spacing().interact_size.y))
                            .align(egui::Align2::LEFT_CENTER),
                    );
                    let field = field.labelled_by(label.id);
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
                        // Enter again goes to the results, as Down does.
                        field.request_focus();
                    }
                    if field.has_focus() && ui.input(|i| i.key_pressed(Key::ArrowDown)) {
                        self.focus_first = true;
                    }
                    if self.searching {
                        spinner(ui);
                    }
                });
                ui.add_space(8.0);
                self.filter_bar(ui);
            });

        if let Some(f) = self.selected().cloned() {
            egui::Panel::bottom("actions")
                .frame(
                    egui::Frame::NONE
                        .fill(p.code_bg)
                        .inner_margin(egui::Margin::symmetric(16, 8)),
                )
                .show(root, |ui| {
                    ui.horizontal(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let named = |r| for_what(r, f.name());
                            if named(ui.button("Copy file"))
                                .on_hover_text("Ctrl+C")
                                .clicked()
                            {
                                self.act(Act::Copy, &f);
                            }
                            if named(ui.button("Open folder")).clicked() {
                                self.act(Act::OpenFolder, &f);
                            }
                            let open = if f.place().is_empty() {
                                "Open".to_owned()
                            } else {
                                format!("Open at {}", f.place())
                            };
                            if named(ui.button(open)).on_hover_text("Enter").clicked() {
                                self.act(Act::Open, &f);
                            }
                            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                                ui.add(
                                    egui::Label::new(RichText::new(&f.path).monospace()).truncate(),
                                )
                                .on_hover_text(&f.path);
                            });
                        });
                    });
                });
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(p.bg).inner_margin(egui::Margin {
                left: 16,
                right: 0,
                top: 4,
                bottom: 4,
            }))
            .show(root, |ui| {
                ui.spacing_mut().scroll = egui::style::ScrollStyle {
                    bar_width: 10.0,
                    ..egui::style::ScrollStyle::solid()
                };
                let scrolled = egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_max_width(ui.available_width() - 16.0);
                        self.notice_line(ui);
                        self.results(ui, &shown);
                        ui.add_space(12.0);
                    });
                name_scrolling(ui, &scrolled, "Results");
            });
    }

    fn notice_line(&mut self, ui: &mut egui::Ui) {
        let Some(notice) = self.notice.clone() else {
            return;
        };
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            let shown = ui.label(&notice);
            ui.ctx()
                .accesskit_node_builder(shown.id, |node| node.set_live(accesskit::Live::Polite));
            if ui.small_button("Dismiss").clicked() {
                self.notice = None;
            }
        });
    }

    fn filter_bar(&mut self, ui: &mut egui::Ui) {
        let count = |kind| self.results.iter().filter(|f| f.kind == kind).count();
        let (text, documents, images) =
            (count(Kind::Text), count(Kind::Document), count(Kind::Image));
        let roots = self.link.shared.lock().unwrap().roots.clone();
        let mut extensions: BTreeMap<String, usize> = BTreeMap::new();
        for f in &self.results {
            *extensions.entry(f.extension()).or_default() += 1;
        }
        let filters = &mut self.filters;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            ui.label("Show:");
            let checks = [
                (&mut filters.text, format!("Text ({text})")),
                (&mut filters.documents, format!("Documents ({documents})")),
                (&mut filters.images, format!("Images ({images})")),
            ];
            for (on, name) in checks {
                ui.checkbox(on, name);
            }
            ui.add_space(8.0);

            let folder = filters
                .folder
                .as_deref()
                .map_or("All folders".to_owned(), |r| tilde(Path::new(r)));
            menu(ui, "Folder:", folder, |ui| {
                ui.selectable_value(&mut filters.folder, None, "All folders");
                for root in &roots {
                    ui.selectable_value(
                        &mut filters.folder,
                        Some(root.clone()),
                        tilde(Path::new(root)),
                    );
                }
            });
            menu(ui, "Modified:", filters.modified.label().to_owned(), |ui| {
                for m in Modified::ALL {
                    ui.selectable_value(&mut filters.modified, m, m.label());
                }
            });
            let selected = filters
                .extension
                .as_deref()
                .map_or("All types".to_owned(), type_name);
            menu(ui, "File type:", selected, |ui| {
                ui.selectable_value(&mut filters.extension, None, "All types");
                for (e, n) in &extensions {
                    let name = format!("{} ({n})", type_name(e));
                    ui.selectable_value(&mut filters.extension, Some(e.clone()), name);
                }
            });

            if *filters != Filters::default() && ui.button("Clear filters").clicked() {
                *filters = Filters::default();
            }
        });
    }

    fn results(&mut self, ui: &mut egui::Ui, shown: &[Found]) {
        let p = theme::palette(ui.ctx());
        ui.add_space(8.0);
        let status = if let Some(e) = &self.error {
            Some(RichText::new(format!("Couldn't search: {e}")).color(p.error))
        } else if self.searching && self.results.is_empty() {
            Some(RichText::new("Searching…").color(p.muted))
        } else if self.searched.is_empty() {
            let hint = if self.query.trim().is_empty() {
                "Type what you're looking for: a sentence works better than a file name."
            } else {
                "Press Enter to search."
            };
            Some(RichText::new(hint).color(p.muted))
        } else if self.results.is_empty() {
            Some(RichText::new(format!("Nothing found for “{}”.", self.searched)).color(p.muted))
        } else if shown.is_empty() {
            Some(RichText::new("No results match the filters.").color(p.muted))
        } else {
            None
        };
        if let Some(status) = status {
            let label = ui.add(egui::Label::new(status).wrap());
            ui.ctx()
                .accesskit_node_builder(label.id, |node| node.set_live(accesskit::Live::Polite));
            return;
        }
        let mut first = std::mem::take(&mut self.focus_first);
        let images: Vec<&Found> = shown.iter().filter(|f| f.kind == Kind::Image).collect();
        if !images.is_empty() {
            section(ui, &format!("Images ({})", images.len()));
            self.grid(ui, &images, &mut first);
        }
        let texts: Vec<&Found> = shown.iter().filter(|f| f.kind != Kind::Image).collect();
        if !texts.is_empty() {
            section(ui, &format!("Text and documents ({})", texts.len()));
            self.list(ui, &texts, &mut first);
        }
    }

    fn grid(&mut self, ui: &mut egui::Ui, images: &[&Found], first: &mut bool) {
        ui.horizontal_wrapped(|ui| {
            list_box(ui, "Images");
            ui.spacing_mut().item_spacing = egui::vec2(10.0, 10.0);
            let columns = ((ui.available_width() + 10.0) / (TILE + 10.0)).max(1.0) as usize;
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
        painter.rect_filled(picture, 6.0, p.code_bg);
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
            Thumb::Failed => {
                painter.text(
                    picture.center(),
                    egui::Align2::CENTER_CENTER,
                    f.extension().to_uppercase(),
                    FontId::proportional(18.0),
                    p.muted,
                );
            }
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
                name.top() + 3.0,
            ),
            galley,
            p.text,
        );
        if selected || response.has_focus() || response.hovered() {
            let stroke = if selected || response.has_focus() {
                Stroke::new(2.0_f32, p.link)
            } else {
                Stroke::new(1.0_f32, p.muted)
            };
            painter.rect_stroke(picture.expand(1.0), 6.0, stroke, egui::StrokeKind::Outside);
        }
    }

    fn list(&mut self, ui: &mut egui::Ui, texts: &[&Found], first: &mut bool) {
        ui.vertical(|ui| {
            list_box(ui, "Text and documents");
            ui.spacing_mut().item_spacing.y = 4.0;
            let mut to = None;
            for (i, f) in texts.iter().enumerate() {
                let response = self.row(ui, f);
                self.respond(ui, &response, f, first);
                to = to.or_else(|| arrows(ui, &response, i, texts.len(), 1, 0));
            }
            self.move_to(ui, to.map(|i| texts[i]));
        });
    }

    fn row(&mut self, ui: &mut egui::Ui, f: &Found) -> egui::Response {
        let p = theme::palette(ui.ctx());
        let selected = self.selected.as_deref() == Some(f.path.as_str());
        let width = ui.available_width();
        let inner = egui::Frame::NONE
            .inner_margin(egui::Margin::symmetric(10, 6))
            .corner_radius(6.0)
            .show(ui, |ui| {
                ui.set_width(width - 20.0);
                ui.horizontal(|ui| {
                    ui.add(
                        egui::Label::new(RichText::new(f.name()).family(theme::bold()))
                            .truncate()
                            .selectable(false),
                    );
                    let place = f.place();
                    let mut detail = tilde(f.folder());
                    if !place.is_empty() {
                        detail = format!("{detail}, {place}");
                    }
                    ui.add(
                        egui::Label::new(RichText::new(detail).color(p.muted).size(12.5))
                            .truncate()
                            .selectable(false),
                    );
                });
                if !f.snippet.is_empty() {
                    ui.add(
                        egui::Label::new(RichText::new(&f.snippet).size(13.0))
                            .wrap()
                            .selectable(false),
                    );
                }
            });
        let rect = inner.response.rect;
        let response = ui.interact(rect, result_id(f), Sense::click());
        let painter = ui.painter();
        if selected || response.has_focus() {
            painter.rect_stroke(
                rect,
                6.0,
                Stroke::new(2.0_f32, p.link),
                egui::StrokeKind::Inside,
            );
        } else if response.hovered() {
            painter.rect_stroke(
                rect,
                6.0,
                Stroke::new(1.0_f32, p.rule),
                egui::StrokeKind::Inside,
            );
        }
        // The labels inside are read as the row's name.
        decorative(ui, inner.response.id);
        response
    }

    /// Focus and select `f`, which an arrow key moved to.
    fn move_to(&mut self, ui: &egui::Ui, f: Option<&Found>) {
        if let Some(f) = f {
            ui.memory_mut(|m| {
                m.request_focus(result_id(f));
                // egui's own arrow-key move goes to the nearest widget, which
                // in a wrapped grid is often the wrong one.
                m.move_focus(egui::FocusDirection::None);
            });
            self.selected = Some(f.path.clone());
        }
    }

    /// What every result does: named for a screen reader, selected on a
    /// click or when Tab reaches it, opened on a double click or Enter, and
    /// a menu of actions on a right click.
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
        response
            .widget_info(|| WidgetInfo::selected(egui::Role::ListBoxOption, true, selected, &name));
        ui.ctx().accesskit_node_builder(response.id, |node| {
            node.set_role(accesskit::Role::ListBoxOption);
            node.clear_toggled();
            node.set_selected(selected);
        });
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

/// Where an arrow key moves from result `i` of `len`, the focused one: `down`
/// results on for Down, `across` for Right (0 where Left and Right do nothing).
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

/// A result's own id, whatever is around it: one from its place in the layout
/// changes when the action bar appears, which would drop its focus.
fn result_id(f: &Found) -> egui::Id {
    egui::Id::unique(("vinosearch-result", &f.path))
}

/// A filter's menu, named by the label before it and kept on one line with it
/// when the bar wraps.
fn menu(ui: &mut egui::Ui, label: &str, selected: String, items: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        let salt = label;
        let label = ui.label(label);
        egui::ComboBox::from_id_salt(salt)
            .selected_text(selected)
            .show_ui(ui, items)
            .response
            .labelled_by(label.id);
    });
}

/// "PDF", "JPG"; "No extension" for none.
fn type_name(extension: &str) -> String {
    if extension.is_empty() {
        "No extension".to_owned()
    } else {
        extension.to_uppercase()
    }
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(6.0);
    let label = ui.label(RichText::new(title).family(theme::bold()).size(14.0));
    heading(ui, &label, title, 2);
    ui.add_space(6.0);
}

/// The results of a kind as one list for a screen reader to step through.
fn list_box(ui: &egui::Ui, name: &str) {
    ui.ctx().accesskit_node_builder(ui.unique_id(), |node| {
        node.set_role(accesskit::Role::ListBox);
        node.set_label(name);
    });
}

impl eframe::App for SearchWindow {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }
}

pub fn run(app_id: &str, query: &str) -> ExitCode {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_app_id(app_id)
            .with_inner_size([960.0, 720.0])
            .with_min_inner_size([360.0, 320.0]),
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
            theme::visible_scroll_handles(&cc.egui_ctx);
            let mut window = SearchWindow::with(&query, source::Runner::default());
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
