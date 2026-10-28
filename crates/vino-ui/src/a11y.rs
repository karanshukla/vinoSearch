//! What a screen reader needs that egui's painted widgets don't give by
//! themselves: names, roles, headings and live regions.

use egui::{Id, Response, Role, Ui, accesskit};

/// Painted only for the eye: kept out of the accessibility tree.
pub fn decorative(ui: &Ui, id: Id) {
    ui.ctx()
        .accesskit_node_builder(id, |node| node.set_hidden());
}

/// A heading for screen readers to list and jump between; egui only has labels.
pub fn heading(response: &Response, text: &str, level: usize) {
    response.ctx.accesskit_node_builder(response.id, |node| {
        node.set_role(Role::Heading);
        node.set_level(level);
        node.clear_value();
        node.set_label(text);
    });
}

/// A status line read out when it changes, without moving focus.
pub fn live(response: &Response) {
    response
        .ctx
        .accesskit_node_builder(response.id, |node| node.set_live(accesskit::Live::Polite));
}

/// Read out at once, wherever focus is: a notice that just appeared.
pub fn assertive(response: &Response) {
    response.ctx.accesskit_node_builder(response.id, |node| {
        node.set_live(accesskit::Live::Assertive)
    });
}

/// A button's name with what it acts on after its text, where the same text
/// shows more than once: "Remove ~/Downloads", not one "Remove" after another.
pub fn for_what(button: Response, what: &str) -> Response {
    button.ctx.accesskit_node_builder(button.id, |node| {
        let text = node.label().unwrap_or_default();
        node.set_label(format!("{text} {what}"));
    });
    button
}

/// Named by a label when there is one.
pub fn labelled(response: Response, label: Option<Id>) -> Response {
    match label {
        Some(id) => response.labelled_by(id),
        None => response,
    }
}

/// The page switcher, sidebar or tabs, as a tab list.
pub fn tab_list(ui: &Ui, name: &str) {
    ui.ctx().accesskit_node_builder(ui.unique_id(), |node| {
        node.set_role(Role::TabList);
        node.set_label(name);
    });
}

/// One tab in a `tab_list`: its title is painted, so name it, and say whether
/// it is the one showing.
pub fn tab(response: &Response, name: &str, selected: bool) {
    let enabled = response.enabled();
    response.widget_info(|| egui::WidgetInfo::selected(Role::Tab, enabled, selected, name));
    response.ctx.accesskit_node_builder(response.id, |node| {
        node.set_role(Role::Tab);
        node.clear_toggled();
        node.set_selected(selected);
    });
}

/// A list of options for a screen reader to step through.
pub fn list_box(ui: &Ui, name: &str) {
    ui.ctx().accesskit_node_builder(ui.unique_id(), |node| {
        node.set_role(Role::ListBox);
        node.set_label(name);
    });
}

/// One option in a `list_box`, with its state.
pub fn list_box_option(response: &Response, name: &str, selected: bool) {
    response.widget_info(|| egui::WidgetInfo::selected(Role::ListBoxOption, true, selected, name));
    response.ctx.accesskit_node_builder(response.id, |node| {
        node.set_role(Role::ListBoxOption);
        node.clear_toggled();
        node.set_selected(selected);
    });
}

/// egui's scroll bar, and the drag-to-scroll area it puts behind an
/// overflowing page, both take keyboard focus but have no role or name of
/// their own.
pub fn name_scrolling<R>(ui: &Ui, scrolled: &egui::scroll_area::ScrollAreaOutput<R>, what: &str) {
    let view = scrolled.inner_rect;
    let overflow = (scrolled.content_size.y - view.height()).max(0.0);
    ui.ctx()
        .accesskit_node_builder(scrolled.id.with(1_usize), |node| {
            node.set_role(Role::ScrollBar);
            node.set_label(format!("Scroll {what}"));
            node.set_orientation(accesskit::Orientation::Vertical);
            node.set_min_numeric_value(0.0);
            node.set_max_numeric_value(f64::from(overflow));
            node.set_numeric_value(f64::from(scrolled.state.clamped_offset().y));
        });
    if overflow > 0.0 {
        ui.ctx()
            .accesskit_node_builder(scrolled.id.with("area"), |node| {
                node.set_role(Role::ScrollView);
                node.set_label(what);
            });
    }
}
