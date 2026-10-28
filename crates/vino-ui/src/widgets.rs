//! The parts every window is made of, one function each. Everything painted
//! here says what it is to a screen reader: a switch is a switch with the
//! row's title for its name, a nav item is a tab, a chip reads as its text.

use std::cell::RefCell;

use egui::{
    Align, Color32, CornerRadius, FontId, Id, Layout, Rect, Response, RichText, Role, Sense, Shape,
    Stroke, StrokeKind, Ui, UiBuilder, Vec2, WidgetInfo, pos2, vec2,
};

use crate::a11y;
use crate::icons::{self, Icon};
use crate::theme::{self, Palette};

/// A button, field or combo box is this tall.
pub const CONTROL_HEIGHT: f32 = 30.0;
/// A small button, as in a list row.
pub const SMALL_HEIGHT: f32 = 26.0;
/// Below this width a row's control goes under its label.
pub const NARROW: f32 = 440.0;
/// Below this window width the sidebar folds into underline tabs.
pub const SIDEBAR_FROM: f32 = 620.0;
pub const SIDEBAR_WIDTH: f32 = 200.0;
/// The content column's widest.
pub const CONTENT_MAX: f32 = 620.0;
/// Between the content and the scroll bar on the window's edge.
pub const SCROLL_GAP: f32 = 24.0;
const GROUP_RADIUS: f32 = 12.0;
const ROW_PADDING: Vec2 = vec2(16.0, 12.0);

fn palette(ui: &Ui) -> &'static Palette {
    theme::palette(ui.ctx())
}

fn faded(color: Color32, enabled: bool) -> Color32 {
    if enabled {
        color
    } else {
        color.gamma_multiply(0.45)
    }
}

// Text

/// The page title: 22 bold, a level-1 heading.
pub fn title(ui: &mut Ui, text: &str) -> Response {
    let response =
        ui.add(egui::Label::new(RichText::new(text).size(22.0).family(theme::bold())).truncate());
    a11y::heading(&response, text, 1);
    response
}

/// A window's title in its header: 16 semibold, a level-1 heading.
pub fn window_title(ui: &mut Ui, text: &str, color: Color32) -> Response {
    let response = ui.add(
        egui::Label::new(
            RichText::new(text)
                .size(16.0)
                .family(theme::semibold())
                .color(color),
        )
        .truncate(),
    );
    a11y::heading(&response, text, 1);
    response
}

/// A group's label above it: small caps, muted, a level-2 heading. With a
/// count, the count follows in the same size at regular weight.
pub fn group_label(ui: &mut Ui, text: &str) -> Response {
    ui.add_space(14.0);
    let p = palette(ui);
    let response = ui.add(
        egui::Label::new(
            RichText::new(text.to_uppercase())
                .size(12.0)
                .family(theme::semibold())
                .extra_letter_spacing(0.7)
                .color(p.muted),
        )
        .truncate(),
    );
    a11y::heading(&response, text, 2);
    response
}

/// A short line under a group label.
pub fn group_help(ui: &mut Ui, text: &str) -> Response {
    let p = palette(ui);
    ui.add(egui::Label::new(RichText::new(text).size(12.5).color(p.muted)).wrap())
}

/// Muted body text.
pub fn muted(ui: &mut Ui, text: impl Into<String>) -> Response {
    let p = palette(ui);
    ui.add(egui::Label::new(RichText::new(text).color(p.muted)).wrap())
}

/// Small muted text: help, facts, captions.
pub fn small(ui: &mut Ui, text: impl Into<String>) -> Response {
    let p = palette(ui);
    ui.add(egui::Label::new(RichText::new(text).size(12.5).color(p.muted)).wrap())
}

/// A path or command, in the monospace face, on one line.
pub fn mono(ui: &mut Ui, text: &str) -> Response {
    ui.add(egui::Label::new(RichText::new(text).monospace()).truncate())
        .on_hover_text(text)
}

/// A big number with what it counts under it.
pub fn stat(ui: &mut Ui, value: &str, label: &str) -> Response {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        let shown = ui.label(RichText::new(value).size(22.0).family(theme::bold()));
        small(ui, label);
        shown
    })
    .inner
}

// Groups and rows

/// A surface with a 1 px rule and 12 px corners, holding [`Row`]s separated
/// by hairlines. The corners stay round whatever a row paints.
pub fn group<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let p = palette(ui);
    let outer = ui.available_rect_before_wrap();
    let fill = ui.painter().add(Shape::Noop);
    let inner_rect = Rect::from_min_size(
        outer.min + vec2(1.0, 1.0),
        vec2(outer.width() - 2.0, f32::INFINITY),
    );
    let inner = ui.scope_builder(
        UiBuilder::new()
            .max_rect(inner_rect)
            .layout(Layout::top_down(Align::Min)),
        |ui| {
            ui.set_width(inner_rect.width());
            ui.spacing_mut().item_spacing.y = 0.0;
            add(ui)
        },
    );
    let bottom = inner.response.rect.bottom().max(outer.top() + 2.0) + 1.0;
    let rect = Rect::from_min_max(outer.min, pos2(outer.right(), bottom));
    let painter = ui.painter();
    painter.set(fill, Shape::rect_filled(rect, GROUP_RADIUS, p.surface));
    // Mask what a row painted over the corners, then draw the edge on top.
    for (corner, dx, dy) in [
        (rect.left_top(), 1.0, 1.0),
        (rect.right_top(), -1.0, 1.0),
        (rect.right_bottom(), -1.0, -1.0),
        (rect.left_bottom(), 1.0, -1.0),
    ] {
        let r = GROUP_RADIUS;
        let center = corner + vec2(dx * r, dy * r);
        let mut points = vec![corner];
        for i in 0..=8 {
            let a = i as f32 / 8.0 * std::f32::consts::FRAC_PI_2;
            points.push(center - vec2(dx * r * a.cos(), dy * r * a.sin()));
        }
        painter.add(Shape::convex_polygon(points, p.bg, Stroke::NONE));
    }
    painter.rect_stroke(
        rect,
        GROUP_RADIUS,
        Stroke::new(1.0_f32, p.rule),
        StrokeKind::Inside,
    );
    ui.allocate_rect(rect, Sense::hover());
    inner.inner
}

/// A row in a [`group`]: title and help on the left, the control on the
/// right (under the label below [`NARROW`]), a hairline above it unless it
/// is the first. The control is named by the title for screen readers when
/// it is passed through the function the closure is given.
pub struct Row<'a> {
    title: &'a str,
    help: &'a str,
    tint: Option<Color32>,
    /// The help's colour: muted, or the warning or error colour when the
    /// row needs attention.
    tone: Tone,
}

impl<'a> Row<'a> {
    pub fn new(title: &'a str) -> Self {
        Row {
            title,
            help: "",
            tint: None,
            tone: Tone::Neutral,
        }
    }

    pub fn help(mut self, help: &'a str) -> Self {
        self.help = help;
        self
    }

    /// Paint the row in a colour: the error tint while a confirmation waits.
    pub fn tint(mut self, tint: Color32) -> Self {
        self.tint = Some(tint);
        self
    }

    /// Colour the help: `Tone::Warn` for something that needs the user,
    /// `Tone::Error` for something that failed.
    pub fn tone(mut self, tone: Tone) -> Self {
        self.tone = tone;
        self
    }

    pub fn show<R>(
        self,
        ui: &mut Ui,
        control: impl FnOnce(&mut Ui, &dyn Fn(Response) -> Response) -> R,
    ) -> R {
        let p = palette(ui);
        let title = self.title;
        let help = self.help;
        let help_color = match self.tone {
            Tone::Warn => p.warn,
            Tone::Error => p.error,
            Tone::Info | Tone::Neutral => p.muted,
        };
        let estimate = |ui: &Ui, width: f32| -> f32 {
            let title_h = ui
                .painter()
                .layout(
                    title.to_owned(),
                    FontId::new(14.0, theme::medium()),
                    p.text,
                    width,
                )
                .size()
                .y;
            if help.is_empty() {
                title_h
            } else {
                let help_h = ui
                    .painter()
                    .layout(help.to_owned(), FontId::proportional(12.5), p.muted, width)
                    .size()
                    .y;
                title_h + 2.0 + help_h
            }
        };
        let label = move |ui: &mut Ui| -> Option<Id> {
            ui.spacing_mut().item_spacing.y = 2.0;
            let shown = ui.add(
                egui::Label::new(RichText::new(title).size(14.0).family(theme::medium())).wrap(),
            );
            if !help.is_empty() {
                ui.add(egui::Label::new(RichText::new(help).size(12.5).color(help_color)).wrap());
            }
            Some(shown.id)
        };
        show_row(ui, self.tint, &estimate, label, control)
    }

    /// A row whose left side is drawn by `label` instead of a title: a path
    /// with an icon, a stat strip. Controls go unnamed unless `label`
    /// returns an id to name them by.
    pub fn custom<R>(
        self,
        ui: &mut Ui,
        label: impl FnOnce(&mut Ui) -> Option<Id>,
        control: impl FnOnce(&mut Ui, &dyn Fn(Response) -> Response) -> R,
    ) -> R {
        show_row(ui, self.tint, &|_, _| CONTROL_HEIGHT, label, control)
    }
}

fn show_row<R>(
    ui: &mut Ui,
    tint: Option<Color32>,
    estimate: &dyn Fn(&Ui, f32) -> f32,
    label: impl FnOnce(&mut Ui) -> Option<Id>,
    control: impl FnOnce(&mut Ui, &dyn Fn(Response) -> Response) -> R,
) -> R {
    let p = palette(ui);
    let full = ui.available_rect_before_wrap();
    let top = ui.cursor().top();
    let first = top <= ui.max_rect().top() + 0.5;
    let bg = ui.painter().add(Shape::Noop);
    let line = ui.painter().add(Shape::Noop);
    let content_w = (full.width() - 2.0 * ROW_PADDING.x).max(40.0);
    let left = full.left() + ROW_PADDING.x;
    let narrow = full.width() < NARROW;
    let controls: RefCell<Vec<Id>> = RefCell::new(Vec::new());
    let named = |response: Response| -> Response {
        controls.borrow_mut().push(response.id);
        response
    };
    let salt = ui.next_auto_id();
    let (out, label_id, content_h) = if narrow {
        let label_rect = Rect::from_min_size(
            pos2(left, top + ROW_PADDING.y),
            vec2(content_w, f32::INFINITY),
        );
        let mut label_ui = ui.new_child(
            UiBuilder::new()
                .max_rect(label_rect)
                .layout(Layout::top_down(Align::Min))
                .id_salt(salt.with("label")),
        );
        let label_id = label(&mut label_ui);
        let label_h = label_ui.min_rect().height().max(0.0);
        let control_top = top + ROW_PADDING.y + label_h + if label_h > 0.0 { 10.0 } else { 0.0 };
        let control_rect =
            Rect::from_min_size(pos2(left, control_top), vec2(content_w, CONTROL_HEIGHT));
        let mut control_ui = ui.new_child(
            UiBuilder::new()
                .max_rect(control_rect)
                .layout(Layout::left_to_right(Align::Center).with_main_wrap(true))
                .id_salt(salt.with("control")),
        );
        let out = control(&mut control_ui, &named);
        let control_h = control_ui.min_rect().height().max(0.0);
        let bottom = if control_h > 0.0 {
            control_top + control_h.max(CONTROL_HEIGHT)
        } else {
            top + ROW_PADDING.y + label_h
        };
        (out, label_id, bottom - top - ROW_PADDING.y)
    } else {
        // The control first, centred on the label's likely height; then the
        // label in what is left.
        let est_h = estimate(ui, (content_w - 120.0).max(120.0));
        let row_h = est_h.max(CONTROL_HEIGHT);
        let content_rect =
            Rect::from_min_size(pos2(left, top + ROW_PADDING.y), vec2(content_w, row_h));
        let mut control_ui = ui.new_child(
            UiBuilder::new()
                .max_rect(content_rect)
                .layout(Layout::right_to_left(Align::Center).with_main_wrap(true))
                .id_salt(salt.with("control")),
        );
        let out = control(&mut control_ui, &named);
        let control_rect = control_ui.min_rect();
        let control_w = if control_rect.is_positive() {
            control_rect.width()
        } else {
            0.0
        };
        let gap = if control_w > 0.0 { 16.0 } else { 0.0 };
        let label_w = (content_w - control_w - gap).max(80.0);
        let offset = ((row_h - est_h) / 2.0).max(0.0);
        let label_rect = Rect::from_min_size(
            pos2(left, content_rect.top() + offset),
            vec2(label_w, f32::INFINITY),
        );
        let mut label_ui = ui.new_child(
            UiBuilder::new()
                .max_rect(label_rect)
                .layout(Layout::top_down(Align::Min))
                .id_salt(salt.with("label")),
        );
        let label_id = label(&mut label_ui);
        let label_h = label_ui.min_rect().height().max(0.0);
        let control_h = if control_rect.is_positive() {
            control_rect.height()
        } else {
            0.0
        };
        (out, label_id, row_h.max(offset + label_h).max(control_h))
    };
    if let Some(label_id) = label_id {
        for id in controls.borrow().iter() {
            // As Response::labelled_by: the label wins over the widget's own text.
            ui.ctx().accesskit_node_builder(*id, |node| {
                node.clear_label();
                node.push_labelled_by(label_id.accesskit_id());
            });
        }
    }
    let rect = Rect::from_min_size(
        full.min,
        vec2(full.width(), content_h + 2.0 * ROW_PADDING.y),
    );
    let painter = ui.painter();
    if let Some(tint) = tint {
        painter.set(bg, Shape::rect_filled(rect, 0.0, tint));
    }
    if !first {
        painter.set(
            line,
            Shape::line_segment(
                [rect.left_top(), rect.right_top()],
                Stroke::new(1.0_f32, p.rule),
            ),
        );
    }
    ui.allocate_rect(rect, Sense::hover());
    out
}

/// Free-form content inside a [`group`], padded like a row: a stats strip, a
/// preview, an adder line.
pub fn group_section<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let p = palette(ui);
    let full = ui.available_rect_before_wrap();
    let top = ui.cursor().top();
    let first = top <= ui.max_rect().top() + 0.5;
    if !first {
        ui.painter().hline(
            full.left()..=full.right(),
            top,
            Stroke::new(1.0_f32, p.rule),
        );
    }
    let inner = ui.scope_builder(
        UiBuilder::new()
            .max_rect(Rect::from_min_size(
                full.min + ROW_PADDING,
                vec2(full.width() - 2.0 * ROW_PADDING.x, f32::INFINITY),
            ))
            .layout(Layout::top_down(Align::Min)),
        |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            add(ui)
        },
    );
    let bottom = inner.response.rect.bottom() + ROW_PADDING.y;
    ui.allocate_rect(
        Rect::from_min_max(full.min, pos2(full.right(), bottom)),
        Sense::hover(),
    );
    inner.inner
}

// Choices

/// A switch: 38 × 22, green when on. `name` is what a screen reader calls it,
/// usually the row's title; nothing is painted beside it.
pub fn switch(ui: &mut Ui, on: &mut bool, name: &str) -> Response {
    let p = palette(ui);
    let enabled = ui.is_enabled();
    let (rect, mut response) = ui.allocate_exact_size(vec2(38.0, 22.0), Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    let is_on = *on;
    response.widget_info(|| WidgetInfo::selected(Role::Switch, enabled, is_on, name));
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let t = ui.ctx().animate_bool_responsive(response.id, is_on);
    let track = p.pressed.lerp_to_gamma(p.primary_fill, t);
    let edge = p.edge.lerp_to_gamma(p.primary_fill, t);
    let knob = p.muted.lerp_to_gamma(p.switch_knob, t);
    let painter = ui.painter();
    painter.rect(
        rect,
        11.0,
        faded(track, enabled),
        Stroke::new(1.0_f32, faded(edge, enabled)),
        StrokeKind::Inside,
    );
    let x = rect.left() + 11.0 + 16.0 * t;
    painter.circle_filled(pos2(x, rect.center().y), 7.0, faded(knob, enabled));
    if response.has_focus() {
        painter.rect_stroke(
            rect.expand(3.0),
            14.0,
            Stroke::new(2.0_f32, p.accent),
            StrokeKind::Inside,
        );
    }
    response
}

/// Two to four short options in one control; a radio group for screen
/// readers, with Left and Right moving the choice. True when it changed.
pub fn segmented<T: Copy + PartialEq>(
    ui: &mut Ui,
    name: &str,
    current: &mut T,
    options: &[(T, &str)],
) -> bool {
    let p = palette(ui);
    let enabled = ui.is_enabled();
    let before = *current;
    let galleys: Vec<_> = options
        .iter()
        .map(|(value, label)| {
            let selected = *current == *value;
            ui.painter().layout_no_wrap(
                (*label).to_owned(),
                FontId::new(
                    13.0,
                    if selected {
                        theme::semibold()
                    } else {
                        theme::medium()
                    },
                ),
                Color32::PLACEHOLDER,
            )
        })
        .collect();
    // Sized up front and laid out left to right whatever the row's layout: a
    // row lays its controls out from the right.
    let total = galleys.iter().map(|g| g.size().x + 20.0).sum::<f32>()
        + 2.0 * options.len().saturating_sub(1) as f32;
    let frame = egui::Frame::new()
        .fill(faded(p.field, enabled))
        .stroke(Stroke::new(1.0_f32, faded(p.rule_strong, enabled)))
        .corner_radius(8.0)
        .inner_margin(2.0);
    frame.show(ui, |ui| {
        ui.ctx().accesskit_node_builder(ui.unique_id(), |node| {
            node.set_role(Role::RadioGroup);
            node.set_label(name);
        });
        ui.spacing_mut().item_spacing.x = 2.0;
        ui.allocate_ui_with_layout(
            vec2(total, 24.0),
            Layout::left_to_right(Align::Center),
            |ui| {
                for (i, ((value, label), galley)) in options.iter().zip(galleys).enumerate() {
                    let selected = *current == *value;
                    let size = vec2(galley.size().x + 20.0, 24.0);
                    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
                    response.widget_info(|| {
                        WidgetInfo::selected(Role::RadioButton, enabled, selected, *label)
                    });
                    if response.clicked() {
                        *current = *value;
                    }
                    if response.has_focus() {
                        let step = ui.input(|input| {
                            if input.key_pressed(egui::Key::ArrowRight)
                                || input.key_pressed(egui::Key::ArrowDown)
                            {
                                1
                            } else if input.key_pressed(egui::Key::ArrowLeft)
                                || input.key_pressed(egui::Key::ArrowUp)
                            {
                                -1
                            } else {
                                0
                            }
                        });
                        if step != 0 {
                            let to = (i as i32 + step).rem_euclid(options.len() as i32) as usize;
                            *current = options[to].0;
                        }
                    }
                    let painter = ui.painter();
                    if selected {
                        painter.rect(
                            rect,
                            6.0,
                            faded(p.surface, enabled),
                            Stroke::new(1.0_f32, faded(p.rule_strong, enabled)),
                            StrokeKind::Inside,
                        );
                    } else if response.hovered() && enabled {
                        painter.rect_filled(rect, 6.0, p.hovered());
                    }
                    let color = if selected { p.text } else { p.muted };
                    painter.galley(
                        rect.center() - galley.size() / 2.0,
                        galley,
                        faded(color, enabled),
                    );
                    if response.has_focus() {
                        painter.rect_stroke(
                            rect.expand(2.0),
                            8.0,
                            Stroke::new(2.0_f32, p.accent),
                            StrokeKind::Inside,
                        );
                    }
                }
            },
        );
    });
    *current != before
}

/// A filter chip: a pill that is on or off, with an optional count.
pub fn toggle_chip(ui: &mut Ui, on: &mut bool, label: &str, count: Option<usize>) -> Response {
    let p = palette(ui);
    let enabled = ui.is_enabled();
    let font = FontId::new(
        13.0,
        if *on {
            theme::semibold()
        } else {
            theme::medium()
        },
    );
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font, Color32::PLACEHOLDER);
    let count_galley = count.map(|n| {
        ui.painter().layout_no_wrap(
            n.to_string(),
            FontId::new(13.0, theme::semibold()),
            Color32::PLACEHOLDER,
        )
    });
    let count_w = count_galley.as_ref().map_or(0.0, |g| g.size().x + 6.0);
    let size = vec2(galley.size().x + count_w + 24.0, 28.0);
    let (rect, mut response) = ui.allocate_exact_size(size, Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    let is_on = *on;
    let name = match count {
        Some(n) => format!("{label} ({n})"),
        None => label.to_owned(),
    };
    response.widget_info(|| WidgetInfo::selected(Role::CheckBox, enabled, is_on, &name));
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let painter = ui.painter();
    let (fill, stroke, color) = if is_on {
        (p.tint, Stroke::NONE, p.accent)
    } else if response.hovered() && enabled {
        (p.hovered(), Stroke::new(1.0_f32, p.rule_strong), p.text)
    } else {
        (
            Color32::TRANSPARENT,
            Stroke::new(1.0_f32, p.rule_strong),
            p.muted,
        )
    };
    painter.rect(rect, 14.0, faded(fill, enabled), stroke, StrokeKind::Inside);
    let mut x = rect.left() + 12.0;
    let y = rect.center().y - galley.size().y / 2.0;
    painter.galley(pos2(x, y), galley.clone(), faded(color, enabled));
    x += galley.size().x + 6.0;
    if let Some(count) = count_galley {
        painter.galley(pos2(x, y), count, faded(color, enabled));
    }
    if response.has_focus() {
        painter.rect_stroke(
            rect.expand(2.0),
            16.0,
            Stroke::new(2.0_f32, p.accent),
            StrokeKind::Inside,
        );
    }
    response
}

/// The chevron a combo box shows, in the muted colour.
pub fn combo_chevron(ui: &Ui, rect: Rect, _visuals: &egui::style::WidgetVisuals, _open: bool) {
    let p = palette(ui);
    icons::paint(
        ui.painter(),
        Rect::from_center_size(rect.center(), vec2(14.0, 14.0)),
        Icon::ChevronDown,
        p.muted,
    );
}

/// A combo box at the row's height, with the chevron and a width that never
/// passes the edge.
pub fn combo(ui: &Ui, id_salt: &str, selected: String, width: f32) -> egui::ComboBox<'static> {
    egui::ComboBox::from_id_salt(id_salt)
        .width(width.min(ui.available_width()))
        .height(260.0)
        .selected_text(selected)
        .icon(combo_chevron)
        .truncate()
}

/// A text field at the row's height.
pub fn field<'t>(text: &'t mut String, hint: &str, width: f32) -> egui::TextEdit<'t> {
    egui::TextEdit::singleline(text)
        .hint_text(hint)
        .desired_width(width)
        .min_size(vec2(0.0, CONTROL_HEIGHT))
        .margin(egui::Margin::symmetric(10, 5))
        .align(egui::Align2::LEFT_CENTER)
}

/// A larger search field with a search icon and an accent ring while it has
/// focus. `name` is its accessible name.
pub fn search_field(
    ui: &mut Ui,
    text: &mut String,
    hint: &str,
    name: &str,
    width: f32,
) -> Response {
    let p = palette(ui);
    let id = ui.scope_id().with(("search-field", name));
    let focused = ui.memory(|m| m.has_focus(id));
    let stroke = if focused {
        Stroke::new(2.0_f32, p.accent)
    } else {
        Stroke::new(1.0_f32, p.edge)
    };
    let frame = egui::Frame::new()
        .fill(p.field)
        .stroke(stroke)
        .corner_radius(10.0)
        .inner_margin(egui::Margin::symmetric(12, 0));
    frame
        .show(ui, |ui| {
            ui.set_width(width - 24.0);
            ui.set_height(38.0);
            ui.horizontal_centered(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                icons::icon(ui, Icon::Search, 16.0, p.muted);
                let edit = ui.add(
                    egui::TextEdit::singleline(text)
                        .id(id)
                        .frame(egui::Frame::NONE)
                        .font(FontId::proportional(15.0))
                        .hint_text(hint)
                        .desired_width(f32::INFINITY)
                        .margin(egui::Margin::symmetric(0, 8))
                        .align(egui::Align2::LEFT_CENTER),
                );
                ui.ctx()
                    .accesskit_node_builder(edit.id, |node| node.set_label(name));
                edit
            })
            .inner
        })
        .inner
}

// Buttons

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    /// Green on dark, ink on light: one per view.
    Primary,
    /// Field-coloured with a rule.
    Secondary,
    /// Bare text until hovered.
    Ghost,
    /// Red: only inside a confirmation.
    Destructive,
}

/// A button in one of the four kinds, 30 tall (26 small), with an optional
/// icon before its text.
pub struct Button {
    kind: ButtonKind,
    text: String,
    icon: Option<Icon>,
    small: bool,
    min_width: f32,
}

impl Button {
    pub fn new(kind: ButtonKind, text: impl Into<String>) -> Self {
        Button {
            kind,
            text: text.into(),
            icon: None,
            small: false,
            min_width: 0.0,
        }
    }

    pub fn primary(text: impl Into<String>) -> Self {
        Button::new(ButtonKind::Primary, text)
    }

    pub fn secondary(text: impl Into<String>) -> Self {
        Button::new(ButtonKind::Secondary, text)
    }

    pub fn ghost(text: impl Into<String>) -> Self {
        Button::new(ButtonKind::Ghost, text)
    }

    pub fn destructive(text: impl Into<String>) -> Self {
        Button::new(ButtonKind::Destructive, text)
    }

    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }

    pub fn small(mut self) -> Self {
        self.small = true;
        self
    }

    pub fn min_width(mut self, width: f32) -> Self {
        self.min_width = width;
        self
    }
}

impl egui::Widget for Button {
    fn ui(self, ui: &mut Ui) -> Response {
        let p = palette(ui);
        let enabled = ui.is_enabled();
        let (height, pad, size) = if self.small {
            (SMALL_HEIGHT, 10.0, 12.5)
        } else {
            (CONTROL_HEIGHT, 12.0, 13.5)
        };
        let family = match self.kind {
            ButtonKind::Primary | ButtonKind::Destructive => theme::semibold(),
            _ => theme::medium(),
        };
        let galley = ui.painter().layout_no_wrap(
            self.text.clone(),
            FontId::new(size, family),
            Color32::PLACEHOLDER,
        );
        let icon_w = if self.icon.is_some() { 16.0 + 6.0 } else { 0.0 };
        let text_w = if self.text.is_empty() {
            0.0
        } else {
            galley.size().x
        };
        let width = (text_w + icon_w + 2.0 * pad).max(self.min_width);
        let width = if self.text.is_empty() && self.icon.is_some() {
            height
        } else {
            width
        };
        let (rect, response) = ui.allocate_exact_size(
            vec2(width, height),
            if enabled {
                Sense::click()
            } else {
                Sense::hover()
            },
        );
        let text = self.text.clone();
        response.widget_info(|| WidgetInfo::labeled(Role::Button, enabled, &text));
        if !ui.is_rect_visible(rect) {
            return response;
        }
        let pressed = response.is_pointer_button_down_on();
        let hovered = response.hovered() && enabled;
        let (fill, stroke, color) = match self.kind {
            ButtonKind::Primary => (
                if pressed {
                    p.primary_fill.lerp_to_gamma(p.text, 0.18)
                } else if hovered {
                    p.primary_fill.lerp_to_gamma(p.text, 0.09)
                } else {
                    p.primary_fill
                },
                Stroke::NONE,
                p.primary_text,
            ),
            ButtonKind::Secondary => (
                if pressed {
                    p.pressed
                } else if hovered {
                    p.hovered()
                } else {
                    p.field
                },
                Stroke::new(1.0_f32, p.rule_strong),
                p.text,
            ),
            ButtonKind::Ghost => (
                if pressed {
                    p.pressed
                } else if hovered {
                    p.field
                } else {
                    Color32::TRANSPARENT
                },
                Stroke::NONE,
                p.text,
            ),
            ButtonKind::Destructive => (
                if pressed || hovered {
                    p.error.lerp_to_gamma(p.text, 0.12)
                } else {
                    p.error
                },
                Stroke::NONE,
                p.bg,
            ),
        };
        let painter = ui.painter();
        painter.rect(
            rect,
            8.0,
            faded(fill, enabled),
            Stroke::new(stroke.width, faded(stroke.color, enabled)),
            StrokeKind::Inside,
        );
        let color = faded(color, enabled);
        let content_w = text_w + icon_w;
        let mut x = rect.center().x - content_w / 2.0;
        if let Some(icon) = self.icon {
            icons::paint(
                painter,
                Rect::from_center_size(pos2(x + 8.0, rect.center().y), vec2(16.0, 16.0)),
                icon,
                color,
            );
            x += 22.0;
        }
        if text_w > 0.0 {
            painter.galley(
                pos2(x, rect.center().y - galley.size().y / 2.0),
                galley,
                color,
            );
        }
        if response.has_focus() {
            painter.rect_stroke(
                rect.expand(2.0),
                10.0,
                Stroke::new(2.0_f32, p.accent),
                StrokeKind::Inside,
            );
        }
        response
    }
}

/// An icon-only button, square, named for a screen reader.
pub fn icon_button(ui: &mut Ui, kind: ButtonKind, icon: Icon, name: &str) -> Response {
    let response = ui.add(Button::new(kind, "").icon(icon));
    let enabled = ui.is_enabled();
    response.widget_info(|| WidgetInfo::labeled(Role::Button, enabled, name));
    response.on_hover_text(name)
}

// Status

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipKind {
    Ok,
    Warn,
    Err,
    Neutral,
}

fn chip_colors(p: &Palette, kind: ChipKind) -> (Color32, Color32) {
    match kind {
        ChipKind::Ok => (p.tint, p.accent),
        ChipKind::Warn => (p.warn_tint, p.warn),
        ChipKind::Err => (p.error_tint, p.error),
        ChipKind::Neutral => (p.field, p.muted),
    }
}

/// A status at a glance: a pill with a dot and a few words, read as its text.
pub fn chip(ui: &mut Ui, kind: ChipKind, text: &str) -> Response {
    chip_inner(ui, kind, text, false)
}

/// A chip whose dot turns: something is under way.
pub fn chip_busy(ui: &mut Ui, kind: ChipKind, text: &str) -> Response {
    chip_inner(ui, kind, text, true)
}

fn chip_inner(ui: &mut Ui, kind: ChipKind, text: &str, busy: bool) -> Response {
    let p = palette(ui);
    let (fill, color) = chip_colors(p, kind);
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        FontId::new(12.5, theme::medium()),
        Color32::PLACEHOLDER,
    );
    let mark = if busy { 14.0 } else { 7.0 };
    let size = vec2(galley.size().x + mark + 6.0 + 20.0, 24.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    response.widget_info(|| WidgetInfo::labeled(Role::Label, true, text));
    if !ui.is_rect_visible(rect) {
        return response;
    }
    ui.painter().rect_filled(rect, 12.0, fill);
    let mark_center = pos2(rect.left() + 10.0 + mark / 2.0, rect.center().y);
    if busy {
        paint_spinner(
            ui,
            Rect::from_center_size(mark_center, vec2(mark, mark)),
            color,
        );
    } else {
        ui.painter().circle_filled(mark_center, 3.5, color);
    }
    ui.painter().galley(
        pos2(
            rect.left() + 10.0 + mark + 6.0,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        color,
    );
    response
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Info,
    Warn,
    Error,
    Neutral,
}

/// A small caps tag: "Beta" in amber, a kind in grey.
pub fn tag(ui: &mut Ui, tone: Tone, text: &str) -> Response {
    let p = palette(ui);
    let (fill, color) = match tone {
        Tone::Warn => (p.warn_tint, p.warn),
        Tone::Error => (p.error_tint, p.error),
        Tone::Info | Tone::Neutral => (p.pressed, p.muted),
    };
    let galley = ui.painter().layout_no_wrap(
        text.to_uppercase(),
        FontId::new(10.5, theme::bold()),
        Color32::PLACEHOLDER,
    );
    let size = vec2(galley.size().x + 12.0 + 1.0, 16.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    response.widget_info(|| WidgetInfo::labeled(Role::Label, true, text));
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.rect_filled(rect, 4.0, fill);
        painter.galley(
            pos2(rect.left() + 6.0, rect.center().y - galley.size().y / 2.0),
            galley,
            color,
        );
    }
    response
}

/// One key as a cap.
pub fn keycap(ui: &mut Ui, text: &str) -> Response {
    let p = palette(ui);
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        FontId::new(12.5, theme::semibold()),
        Color32::PLACEHOLDER,
    );
    let size = vec2(galley.size().x + 16.0, 24.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    response.widget_info(|| WidgetInfo::labeled(Role::Label, true, text));
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.rect(
            rect,
            5.0,
            p.field,
            Stroke::new(1.0_f32, p.rule_strong),
            StrokeKind::Inside,
        );
        painter.hline(
            rect.left() + 1.0..=rect.right() - 1.0,
            rect.bottom() - 1.5,
            Stroke::new(1.0_f32, p.rule_strong),
        );
        painter.galley(
            pos2(
                rect.left() + 8.0,
                rect.center().y - galley.size().y / 2.0 - 0.5,
            ),
            galley,
            p.text,
        );
    }
    response
}

/// A shortcut as key caps: "Meta+Alt+C" is three caps with pluses between.
/// Read as the whole trigger.
pub fn keys(ui: &mut Ui, trigger: &str) -> Response {
    let p = palette(ui);
    let keys: Vec<&str> = trigger.split('+').map(str::trim).collect();
    let measure = |text: &str, font: FontId| {
        ui.painter()
            .layout_no_wrap(text.to_owned(), font, Color32::PLACEHOLDER)
            .size()
            .x
    };
    let plus = measure("+", FontId::proportional(12.5));
    let total = keys
        .iter()
        .map(|key| measure(key, FontId::new(12.5, theme::semibold())) + 16.0)
        .sum::<f32>()
        + keys.len().saturating_sub(1) as f32 * (plus + 8.0);
    // Sized up front and left to right, as a segmented control is.
    let response = ui
        .allocate_ui_with_layout(
            vec2(total, 24.0),
            Layout::left_to_right(Align::Center),
            |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                for (i, key) in keys.iter().enumerate() {
                    if i > 0 {
                        let plus = ui.label(RichText::new("+").size(12.5).color(p.muted));
                        a11y::decorative(ui, plus.id);
                    }
                    let cap = keycap(ui, key);
                    a11y::decorative(ui, cap.id);
                }
            },
        )
        .response;
    response.widget_info(|| WidgetInfo::labeled(Role::Label, true, trigger));
    response
}

/// A thin bar of progress, `width` wide.
pub fn progress(ui: &mut Ui, fraction: f32, width: f32) -> Response {
    let p = palette(ui);
    let (rect, response) = ui.allocate_exact_size(vec2(width, 6.0), Sense::hover());
    a11y::decorative(ui, response.id);
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.rect_filled(rect, 3.0, p.pressed);
        let done = Rect::from_min_size(
            rect.min,
            vec2(rect.width() * fraction.clamp(0.0, 1.0), rect.height()),
        );
        painter.rect_filled(done, 3.0, p.primary_fill);
    }
    response
}

/// Shares of a whole as one bar: (fraction, colour) each, 2 px apart.
pub fn share_bar(ui: &mut Ui, shares: &[(f32, Color32)], height: f32) -> Response {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    a11y::decorative(ui, response.id);
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let total: f32 = shares.iter().map(|(f, _)| f.max(0.0)).sum();
    if total <= 0.0 {
        ui.painter()
            .rect_filled(rect, height / 2.0, palette(ui).pressed);
        return response;
    }
    let gaps = 2.0
        * (shares
            .iter()
            .filter(|(f, _)| *f > 0.0)
            .count()
            .saturating_sub(1)) as f32;
    let mut x = rect.left();
    for (fraction, color) in shares {
        if *fraction <= 0.0 {
            continue;
        }
        let w = (rect.width() - gaps) * fraction / total;
        let piece = Rect::from_min_size(pos2(x, rect.top()), vec2(w, height));
        ui.painter().rect_filled(piece, height / 2.0, *color);
        x += w + 2.0;
    }
    response
}

/// A legend entry: a small square in a colour, then text.
pub fn legend(ui: &mut Ui, color: Color32, text: &str) -> Response {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let (rect, swatch) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
        a11y::decorative(ui, swatch.id);
        ui.painter().rect_filled(rect, 2.0, color);
        small(ui, text)
    })
    .inner
}

/// A banner across the column: a tinted surface with an icon, text that wraps
/// and whatever `actions` adds at the right. The text is read out when it
/// appears.
pub fn banner<R>(ui: &mut Ui, tone: Tone, text: &str, actions: impl FnOnce(&mut Ui) -> R) -> R {
    let p = palette(ui);
    let (fill, icon, color) = match tone {
        Tone::Warn => (p.warn_tint, Icon::Warning, p.warn),
        Tone::Error => (p.error_tint, Icon::Error, p.error),
        Tone::Info | Tone::Neutral => (p.field, Icon::Info, p.muted),
    };
    egui::Frame::new()
        .fill(fill)
        .corner_radius(GROUP_RADIUS)
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                ui.set_min_height(CONTROL_HEIGHT);
                icons::icon(ui, icon, 16.0, color);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let out = actions(ui);
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        let shown = ui.add(egui::Label::new(RichText::new(text)).wrap());
                        a11y::live(&shown);
                    });
                    out
                })
                .inner
            })
            .inner
        })
        .inner
}

/// A notice that just appeared, read out at once: an error banner with a
/// Dismiss button. True when dismissed.
pub fn notice(ui: &mut Ui, tone: Tone, text: &str) -> bool {
    let p = palette(ui);
    let (fill, icon, color) = match tone {
        Tone::Warn => (p.warn_tint, Icon::Warning, p.warn),
        Tone::Error => (p.error_tint, Icon::Error, p.error),
        Tone::Info | Tone::Neutral => (p.field, Icon::Info, p.muted),
    };
    egui::Frame::new()
        .fill(fill)
        .corner_radius(GROUP_RADIUS)
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                ui.set_min_height(SMALL_HEIGHT);
                icons::icon(ui, icon, 16.0, color);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let dismissed = ui.add(Button::ghost("Dismiss").small()).clicked();
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        let shown = ui.add(egui::Label::new(RichText::new(text)).wrap());
                        a11y::assertive(&shown);
                    });
                    dismissed
                })
                .inner
            })
            .inner
        })
        .inner
}

// Navigation

/// A sidebar entry: icon, text and an optional tag, tinted when it is the
/// current page. A tab for screen readers, named `name`.
pub fn nav_item(
    ui: &mut Ui,
    icon: Icon,
    text: &str,
    name: &str,
    selected: bool,
    badge: Option<&str>,
) -> Response {
    let p = palette(ui);
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::click());
    a11y::tab(&response, name, selected);
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let (fill, color, family) = if selected {
        (p.tint, p.accent, theme::semibold())
    } else if response.hovered() {
        (p.field, p.text, theme::medium())
    } else {
        (Color32::TRANSPARENT, p.muted, theme::medium())
    };
    let painter = ui.painter();
    painter.rect_filled(rect, 8.0, fill);
    icons::paint(
        painter,
        Rect::from_center_size(pos2(rect.left() + 18.0, rect.center().y), vec2(16.0, 16.0)),
        icon,
        color,
    );
    painter.text(
        pos2(rect.left() + 36.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        text,
        FontId::new(14.0, family),
        color,
    );
    if response.has_focus() {
        painter.rect_stroke(
            rect,
            8.0,
            Stroke::new(2.0_f32, p.accent),
            StrokeKind::Inside,
        );
    }
    if let Some(badge) = badge {
        let mut badge_ui = ui.new_child(
            UiBuilder::new()
                .max_rect(Rect::from_min_max(
                    pos2(rect.left(), rect.top()),
                    pos2(rect.right() - 10.0, rect.bottom()),
                ))
                .layout(Layout::right_to_left(Align::Center)),
        );
        let shown = tag(&mut badge_ui, Tone::Warn, badge);
        a11y::decorative(ui, shown.id);
    }
    response
}

/// An underlined tab, for a row of them under a narrow window's header or
/// in a viewer. Painted in `colors`, the header's.
pub fn tab(
    ui: &mut Ui,
    name: &str,
    active: bool,
    enabled: bool,
    colors: theme::Header,
) -> Response {
    let p = palette(ui);
    let galley = ui.painter().layout_no_wrap(
        name.to_owned(),
        FontId::new(
            14.0,
            if active {
                theme::semibold()
            } else {
                theme::medium()
            },
        ),
        Color32::PLACEHOLDER,
    );
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(galley.size() + vec2(0.0, 14.0), sense);
    let response = if enabled {
        response
    } else {
        response.on_hover_cursor(egui::CursorIcon::NotAllowed)
    };
    let lit = enabled && (active || response.hovered() || response.has_focus());
    let color = if lit {
        colors.text
    } else if enabled {
        colors.muted
    } else {
        colors.muted.gamma_multiply(0.5)
    };
    let painter = ui.painter();
    painter.galley(rect.left_top() + vec2(0.0, 5.0), galley, color);
    if active {
        painter.hline(
            rect.left()..=rect.right(),
            rect.bottom() - 1.0,
            Stroke::new(2.0_f32, p.accent),
        );
    }
    if response.has_focus() {
        painter.rect_stroke(
            rect.expand2(vec2(4.0, 0.0)),
            4.0,
            Stroke::new(2.0_f32, p.accent),
            StrokeKind::Inside,
        );
    }
    let mut response = response;
    if !enabled {
        response.flags.remove(egui::response::Flags::ENABLED);
    }
    a11y::tab(&response, name, active);
    response
}

/// The header strip's frame: the surface (or KDE's header colour) with a
/// hairline under it.
pub fn header_frame(colors: theme::Header, p: &Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(colors.bg)
        .stroke(Stroke::new(1.0_f32, p.rule))
        .inner_margin(egui::Margin {
            left: 20,
            right: 20,
            top: 10,
            bottom: 12,
        })
}

/// The sidebar's frame: the surface, 14 × 10 inside.
pub fn sidebar_frame(p: &Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(p.surface)
        .inner_margin(egui::Margin {
            left: 10,
            right: 10,
            top: 14,
            bottom: 12,
        })
}

/// Which edge of the window a side panel sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
}

/// The hairline on a panel's inner edge, drawn after its content.
pub fn panel_rule(ui: &Ui, side: Edge) {
    let p = palette(ui);
    let rect = ui.max_rect();
    let x = match side {
        Edge::Left => rect.right() + ui.spacing().item_spacing.x - 0.5,
        Edge::Right => rect.left() - 0.5,
    };
    ui.painter()
        .vline(x, rect.y_range(), Stroke::new(1.0_f32, p.rule));
}

/// A selected list row's look: the tint, with a 1 px accent inside.
pub fn paint_selected(ui: &Ui, rect: Rect, radius: f32) {
    let p = palette(ui);
    let painter = ui.painter();
    painter.rect(
        rect,
        radius,
        p.tint,
        Stroke::new(1.0_f32, p.accent),
        StrokeKind::Inside,
    );
}

/// A selected tile's look: a 2 px accent ring with a gap of page colour
/// between it and the tile.
pub fn paint_ring(ui: &Ui, rect: Rect, radius: f32) {
    let p = palette(ui);
    let painter = ui.painter();
    painter.rect_stroke(
        rect.expand(2.0),
        radius + 2.0,
        Stroke::new(2.0_f32, p.bg),
        StrokeKind::Inside,
    );
    painter.rect_stroke(
        rect.expand(4.0),
        radius + 4.0,
        Stroke::new(2.0_f32, p.accent),
        StrokeKind::Inside,
    );
}

/// A square tile for a file icon, as a list row starts.
pub fn file_tile(ui: &mut Ui, icon: Icon, size: f32) -> Response {
    let p = palette(ui);
    let (rect, response) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    a11y::decorative(ui, response.id);
    if ui.is_rect_visible(rect) {
        ui.painter().rect_filled(rect, 8.0, p.field);
        icons::paint(
            ui.painter(),
            Rect::from_center_size(rect.center(), vec2(16.0, 16.0)),
            icon,
            p.muted,
        );
    }
    response
}

/// A spinner, 14 px, drawn at 10 fps rather than every frame: egui's own asks
/// for a repaint as fast as the GPU goes. It stands still when the desktop
/// asks for less motion, and a screen reader skips it: the text beside it
/// says what is going on.
pub fn spinner(ui: &mut Ui) -> Response {
    let color = ui.visuals().strong_text_color();
    spinner_colored(ui, 14.0, color)
}

pub fn spinner_colored(ui: &mut Ui, size: f32, color: Color32) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    a11y::decorative(ui, response.id);
    if ui.is_rect_visible(rect) {
        paint_spinner(ui, rect, color);
    }
    response
}

fn paint_spinner(ui: &Ui, rect: Rect, color: Color32) {
    let still = theme::still(ui.ctx());
    let t = if still { 0.0 } else { ui.input(|i| i.time) };
    let start = (t * 6.0) % std::f64::consts::TAU;
    let radius = rect.width() / 2.0 - 1.5;
    let points: Vec<egui::Pos2> = (0..=16)
        .map(|i| {
            let a = start + i as f64 / 16.0 * std::f64::consts::PI * 1.5;
            rect.center() + vec2(a.cos() as f32, a.sin() as f32) * radius
        })
        .collect();
    ui.painter()
        .add(Shape::line(points, Stroke::new(2.0_f32, color)));
    if !still {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(100));
    }
}

/// Fades the last lines of a scroll area out while more is below: the bar
/// alone is easy to miss.
pub fn fade_below<R>(ui: &Ui, scrolled: &egui::scroll_area::ScrollAreaOutput<R>, bg: Color32) {
    let view = scrolled.inner_rect;
    if scrolled.state.clamped_offset().y + view.height() >= scrolled.content_size.y - 1.0 {
        return;
    }
    let fade = Rect::from_min_max(pos2(view.left(), view.bottom() - 48.0), view.right_bottom());
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(fade.left_top(), Color32::TRANSPARENT);
    mesh.colored_vertex(fade.right_top(), Color32::TRANSPARENT);
    mesh.colored_vertex(fade.right_bottom(), bg);
    mesh.colored_vertex(fade.left_bottom(), bg);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    ui.painter().add(mesh);
}

/// The vertical scroll area every page uses: a solid bar always shown, the
/// drag-to-scroll area (for touch) off since it took a Tab stop, and the
/// content kept off the bar.
pub fn page_scroll<R>(
    ui: &mut Ui,
    what: &str,
    max_width: f32,
    add: impl FnOnce(&mut Ui) -> R,
) -> R {
    let p = palette(ui);
    let scrolled = egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
        .scroll_source(egui::scroll_area::ScrollSource {
            drag: egui::scroll_area::DragScroll::Never,
            ..Default::default()
        })
        .show(ui, |ui| {
            ui.set_max_width((ui.available_width() - SCROLL_GAP).min(max_width));
            let out = add(ui);
            ui.add_space(16.0);
            out
        });
    a11y::name_scrolling(ui, &scrolled, what);
    fade_below(ui, &scrolled, p.bg);
    scrolled.inner
}

/// Whatever `CornerRadius` a group's own corner needs.
pub fn corner(radius: f32) -> CornerRadius {
    CornerRadius::same(radius as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::accesskit::Toggled;
    use egui_kittest::Harness;
    use egui_kittest::kittest::{NodeT, Queryable};

    #[derive(Clone, Copy, PartialEq)]
    enum Device {
        Npu,
        Gpu,
        Cpu,
    }

    struct State {
        styled: bool,
        on: bool,
        minutes: u32,
        device: Device,
        images: bool,
        clicks: u32,
    }

    fn harness(width: f32) -> Harness<'static, State> {
        Harness::builder().with_size((width, 600.0)).build_ui_state(
            |ui, state: &mut State| {
                if !state.styled {
                    theme::apply(ui.ctx());
                    state.styled = true;
                    ui.ctx().request_repaint();
                    return;
                }
                group_label(ui, "What to index");
                group(ui, |ui| {
                    Row::new("Images")
                        .help("By what's in them.")
                        .show(ui, |ui, _| {
                            switch(ui, &mut state.on, "Index images");
                        });
                    Row::new("Unload after")
                        .help("0 keeps it loaded.")
                        .show(ui, |ui, named| {
                            named(ui.add(egui::DragValue::new(&mut state.minutes)));
                        });
                    Row::new("Device").show(ui, |ui, _| {
                        segmented(
                            ui,
                            "Device",
                            &mut state.device,
                            &[
                                (Device::Npu, "NPU"),
                                (Device::Gpu, "GPU"),
                                (Device::Cpu, "CPU"),
                            ],
                        );
                    });
                    Row::new("Download")
                        .help("Checked against pinned sha256s.")
                        .show(ui, |ui, _| {
                            if ui.add(Button::secondary("Verify")).clicked() {
                                state.clicks += 1;
                            }
                            chip(ui, ChipKind::Ok, "Downloaded, 1.7 GB");
                        });
                });
                toggle_chip(ui, &mut state.images, "Images", Some(8));
                banner(
                    ui,
                    Tone::Warn,
                    "The vision model isn't downloaded yet.",
                    |ui| {
                        ui.add(Button::primary("Download").small());
                    },
                );
                nav_item(
                    ui,
                    Icon::Cleanup,
                    "Cleanup",
                    "Cleanup (beta)",
                    false,
                    Some("Beta"),
                );
            },
            State {
                styled: false,
                on: false,
                minutes: 15,
                device: Device::Npu,
                images: true,
                clicks: 0,
            },
        )
    }

    #[test]
    fn a_switch_is_a_named_switch_that_flips_from_the_keyboard() {
        let mut h = harness(820.0);
        h.run();
        let switch = h.get_by_role_and_label(Role::Switch, "Index images");
        assert_eq!(switch.accesskit_node().toggled(), Some(Toggled::False));
        switch.focus();
        h.run();
        h.key_press(egui::Key::Space);
        h.run();
        assert!(h.state().on);
        let switch = h.get_by_role_and_label(Role::Switch, "Index images");
        assert_eq!(switch.accesskit_node().toggled(), Some(Toggled::True));
        switch.click();
        h.run();
        assert!(!h.state().on);
    }

    #[test]
    fn a_segmented_control_is_a_radio_group_that_arrows_move() {
        let mut h = harness(820.0);
        h.run();
        h.get_by_role_and_label(Role::RadioGroup, "Device");
        let gpu = h.get_by_role_and_label(Role::RadioButton, "GPU");
        assert_eq!(gpu.accesskit_node().toggled(), Some(Toggled::False));
        gpu.click();
        h.run();
        assert!(h.state().device == Device::Gpu);
        h.get_by_role_and_label(Role::RadioButton, "GPU").focus();
        h.run();
        h.key_press(egui::Key::ArrowRight);
        h.run();
        assert!(h.state().device == Device::Cpu);
    }

    #[test]
    fn buttons_chips_tags_and_banners_read_as_their_text() {
        let mut h = harness(820.0);
        h.run();
        h.get_by_role_and_label(Role::Button, "Verify").click();
        h.run();
        assert_eq!(h.state().clicks, 1);
        h.get_by_label("Downloaded, 1.7 GB");
        h.get_by_label("The vision model isn't downloaded yet.");
        h.get_by_role_and_label(Role::Button, "Download");
        h.get_by_role_and_label(Role::Heading, "What to index");
        // Passed through `named`, a control takes the row's title as its name.
        h.get_by_role_and_label(Role::SpinButton, "Unload after");
        let chip = h.get_by_role_and_label(Role::CheckBox, "Images (8)");
        assert_eq!(chip.accesskit_node().toggled(), Some(Toggled::True));
        chip.click();
        h.run();
        assert!(!h.state().images);
        let tab = h.get_by_role_and_label(Role::Tab, "Cleanup (beta)");
        assert_eq!(tab.accesskit_node().is_selected(), Some(false));
    }

    #[test]
    fn rows_name_their_controls_at_both_widths() {
        for width in [820.0, 380.0] {
            let mut h = harness(width);
            h.run();
            let switch = h.get_by_role_and_label(Role::Switch, "Index images");
            assert!(
                switch.rect().max.x <= width + 0.5,
                "{width}: the switch is inside the window"
            );
            let verify = h.get_by_role_and_label(Role::Button, "Verify");
            let title = h.get_by_role_and_label(Role::Label, "Download");
            if width >= NARROW {
                assert!(
                    (verify.rect().center().y - title.rect().center().y).abs() < 14.0,
                    "{width}: the control sits beside its label"
                );
            } else {
                assert!(
                    verify.rect().min.y >= title.rect().max.y,
                    "{width}: the control sits under its label"
                );
            }
        }
    }
}
