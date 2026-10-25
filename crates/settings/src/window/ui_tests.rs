//! The window as a screen reader sees it: every page rendered headless through egui_kittest,
//! which reads egui's AccessKit tree, then found and driven by role and name.

use std::sync::mpsc::Receiver;

use eframe::egui::accesskit::Role;
use eframe::egui::{Key, Theme};
use egui_kittest::kittest::{NodeT, Queryable};
use egui_kittest::{Harness, Node};

use super::*;
use crate::control::Request;

/// Nothing in these tests may reach the bus, run `vinosearch` or open a portal: the link is
/// detached, the doctor already answered and no test presses Add folder…, Turn on/off or
/// Check again.
fn window(page: Page, name: &str) -> (SettingsWindow, Receiver<Request>) {
    let dir =
        std::env::temp_dir().join(format!("vinosearch-settings-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (link, requests) = Link::detached();
    link.shared.lock().unwrap().status = Some(Ok(Status {
        model_loaded: true,
        model_present: true,
        indexing: true,
        indexed_files: 12,
        device: "CPU".into(),
        files: 40,
        chunks: 400,
        text_files: 30,
        text_chunks: 300,
        roots: vec!["/srv/notes".into()],
        model_bytes: 300_000_000,
        cache_bytes: 1_000_000,
        ..Status::default()
    }));
    link.shared.lock().unwrap().largest = Some(vec![("/srv/notes/build".into(), 200)]);
    let mut w = SettingsWindow::with(Some(page), link, dir.join("config.toml"));
    w.estimator = estimated;
    w.integration = Arc::new(Mutex::new(integration::State::default()));
    w.doctor = Arc::new(Mutex::new(doctor::State {
        running: false,
        report: Some(doctor::Report {
            ok: false,
            lines: vec![
                ("NPU:".into(), "none".into()),
                ("Probe:".into(), "failed on NPU: no such device".into()),
            ],
            error: None,
        }),
    }));
    (w, requests)
}

/// What `vinosearch estimate` would say about any folder, without running it.
fn estimated(path: &Path) -> Result<estimate::Answer, String> {
    Ok(estimate::Answer {
        summary: "~1,000 files, ~10 min, ~40 MB".into(),
        note: Some("Times are at NPU rates; on the CPU indexing takes about 5x longer.".into()),
        folders: vec![estimate::Folder {
            path: path.join("raw").display().to_string(),
            summary: "~8 min, 600 files".into(),
        }],
    })
}

/// Frames until the estimate, which lands from another thread, is shown.
fn until_estimated(h: &mut Harness<'_, SettingsWindow>) {
    for _ in 0..100 {
        settle(h);
        if h.query_by_label("~1,000 files, ~10 min, ~40 MB").is_some() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("the estimate never showed");
}

fn harness(w: SettingsWindow, size: [f32; 2], theme: Theme) -> Harness<'static, SettingsWindow> {
    let mut styled = false;
    let mut h = Harness::builder()
        .with_size(size)
        .with_theme(theme)
        .build_ui_state(
            move |ui, w: &mut SettingsWindow| {
                // Fonts set in a frame arrive with the next one, and the window needs its bold.
                if !styled {
                    theme::apply(ui.ctx());
                    theme::visible_scroll_handles(ui.ctx());
                    styled = true;
                    ui.ctx().request_repaint();
                    return;
                }
                w.show(ui);
            },
            w,
        );
    settle(&mut h);
    h
}

const WIDE: [f32; 2] = [820.0, 640.0];
const NARROW: [f32; 2] = [400.0, 640.0];

/// The spinner keeps asking for frames, so a fixed few rather than `run`.
fn settle(h: &mut Harness<'_, SettingsWindow>) {
    h.run_steps(3);
}

fn sent(requests: &Receiver<Request>) -> Vec<Request> {
    requests.try_iter().collect()
}

/// Roles a user operates, and anything that takes focus; each needs a name of its own.
const CONTROLS: [Role; 9] = [
    Role::ScrollBar,
    Role::Button,
    Role::CheckBox,
    Role::TextInput,
    Role::Slider,
    Role::SpinButton,
    Role::ComboBox,
    Role::Tab,
    Role::Link,
];

fn unnamed_controls(h: &Harness<'_, SettingsWindow>) -> Vec<String> {
    h.query_all_by(|node| {
        let focusable = node.data().supports_action(accesskit::Action::Focus);
        (focusable || CONTROLS.contains(&node.role())) && !node.is_hidden()
    })
    .filter(|node: &Node<'_>| {
        node.accesskit_node()
            .label()
            .is_none_or(|label| label.trim().is_empty())
    })
    .map(|node| format!("{node:?} at {:?}", node.rect()))
    .collect()
}

#[test]
fn every_control_on_every_page_has_a_name() {
    for size in [WIDE, NARROW] {
        for page in Page::ALL {
            let (w, _requests) = window(page, "names");
            let h = harness(w, size, Theme::Light);
            let unnamed = unnamed_controls(&h);
            assert!(
                unnamed.is_empty(),
                "{} at {size:?}: unnamed controls {unnamed:#?}",
                page.title()
            );
            let mut seen = std::collections::HashSet::new();
            for node in h.query_all_by(|node| CONTROLS.contains(&node.role())) {
                let node = node.accesskit_node();
                let name = (node.role(), node.label());
                assert!(seen.insert(name.clone()), "{}: two {name:?}", page.title());
            }
        }
    }
}

#[test]
fn the_pages_are_tabs_and_clicking_one_shows_it() {
    let (w, _requests) = window(Page::Index, "tabs");
    let mut h = harness(w, WIDE, Theme::Dark);
    assert_eq!(h.root().accesskit_node().label().as_deref(), Some(TITLE));
    h.get_by_role_and_label(Role::TabList, "Pages");
    for page in Page::ALL {
        h.get_by_role_and_label(Role::Tab, page.title()).click();
        settle(&mut h);
        assert!(h.state().page == page, "{} didn't open", page.title());
        let tab = h.get_by_role_and_label(Role::Tab, page.title());
        assert_eq!(tab.accesskit_node().is_selected(), Some(true));
        let title = h.get_by_role_and_label(Role::Heading, page.title());
        assert_eq!(title.accesskit_node().data().level(), Some(1));
    }
    let others = h
        .query_all_by_role(Role::Tab)
        .filter(|tab| tab.accesskit_node().is_selected() == Some(true));
    assert_eq!(others.count(), 1, "one tab is the current page");
}

#[test]
fn tab_reaches_the_page_switcher_and_enter_or_space_switches() {
    for size in [WIDE, NARROW] {
        let (w, _requests) = window(Page::Index, "keys");
        let mut h = harness(w, size, Theme::Light);
        // Tab goes first to the switcher, in page order.
        h.key_press(Key::Tab);
        settle(&mut h);
        assert!(h.get_by_role_and_label(Role::Tab, "Index").is_focused());
        h.key_press(Key::Tab);
        settle(&mut h);
        assert!(h.get_by_role_and_label(Role::Tab, "Folders").is_focused());
        h.key_press(Key::Enter);
        settle(&mut h);
        assert!(h.state().page == Page::Folders, "Enter at {size:?}");
        h.key_press(Key::Tab);
        settle(&mut h);
        h.key_press(Key::Space);
        settle(&mut h);
        assert!(h.state().page == Page::Search, "Space at {size:?}");
    }
}

#[test]
fn index_page_buttons_reach_the_runner() {
    let (w, requests) = window(Page::Index, "index");
    let mut h = harness(w, WIDE, Theme::Light);
    for section in ["Status", "Largest folders"] {
        let heading = h.get_by_role_and_label(Role::Heading, section);
        assert_eq!(heading.accesskit_node().data().level(), Some(2));
    }
    // The spinner is decoration: the text beside it says the same.
    h.get_by_label("Indexing, 12 files so far");
    for button in ["Stop", "Check now", "Exclude /srv/notes/build"] {
        h.get_by_role_and_label(Role::Button, button).click();
        settle(&mut h);
    }
    let sent = sent(&requests);
    assert!(sent.iter().any(|r| matches!(r, Request::PauseIndexing)));
    assert!(sent.iter().any(|r| matches!(r, Request::CheckNow)));
    assert_eq!(
        h.state().config.exclude.last().map(String::as_str),
        Some("/srv/notes/build")
    );
    h.get_by_role_and_label(Role::Button, "Excluded /srv/notes/build");
}

/// Each folder's buttons say which folder they are for.
fn first_remove<'h>(h: &'h Harness<'_, SettingsWindow>) -> Node<'h> {
    h.get_by_role_and_label(Role::Button, "Remove /srv/notes")
}

#[test]
fn removing_a_folder_confirms_moves_focus_and_escape_backs_out() {
    let (w, requests) = window(Page::Folders, "folders");
    let mut h = harness(w, WIDE, Theme::Light);
    first_remove(&h).click();
    settle(&mut h);
    // The button that was clicked is gone: focus lands on Cancel, not nowhere.
    assert!(h.get_by_role_and_label(Role::Button, "Cancel").is_focused());
    h.key_press(Key::Escape);
    settle(&mut h);
    assert!(h.state().confirm.is_none());
    first_remove(&h).click();
    settle(&mut h);
    h.get_by_role_and_label(Role::Button, "Remove and drop its index /srv/notes")
        .click();
    settle(&mut h);
    let sent = sent(&requests);
    assert!(
        sent.iter()
            .any(|r| matches!(r, Request::Forget(p) if p == "/srv/notes"))
    );
}

#[test]
fn typed_folders_are_named_fields_that_take_enter() {
    let (w, requests) = window(Page::Folders, "typed");
    let mut h = harness(w, WIDE, Theme::Light);
    let add = h.get_by_role_and_label(Role::TextInput, "Folder to add");
    add.focus();
    settle(&mut h);
    h.get_by_role_and_label(Role::TextInput, "Folder to add")
        .type_text("/srv/photos");
    settle(&mut h);
    h.key_press(Key::Enter);
    until_estimated(&mut h);
    // Nothing is indexed until the estimate is confirmed.
    assert!(sent(&requests).is_empty());
    h.get_by_role_and_label(Role::Button, "Add and index /srv/photos")
        .click();
    settle(&mut h);
    let sent = sent(&requests);
    assert!(
        sent.iter()
            .any(|r| matches!(r, Request::Reindex(p) if p == "/srv/photos")),
        "{sent:?}"
    );
    assert!(h.state().adding.is_none());

    h.get_by_role_and_label(Role::TextInput, "Folder to exclude")
        .focus();
    settle(&mut h);
    h.get_by_role_and_label(Role::TextInput, "Folder to exclude")
        .type_text("/srv/notes/tmp");
    settle(&mut h);
    h.key_press(Key::Enter);
    settle(&mut h);
    assert_eq!(
        h.state().config.exclude.last().map(String::as_str),
        Some("/srv/notes/tmp")
    );
    let saved = std::fs::read_to_string(&h.state().config_path).unwrap();
    assert!(saved.contains("/srv/notes/tmp"), "{saved}");
}

#[test]
fn a_folder_to_add_shows_its_estimate_and_its_longest_subfolders() {
    let (w, requests) = window(Page::Folders, "estimate");
    let mut h = harness(w, WIDE, Theme::Light);
    h.state_mut().add(Target::Root, Path::new("/srv/photos"));
    until_estimated(&mut h);
    assert!(h.query_by_label("Add /srv/photos?").is_some());
    assert!(h.query_by_label("~8 min, 600 files").is_some());
    assert!(h.query_by_label_contains("at NPU rates").is_some());
    let unnamed = unnamed_controls(&h);
    assert!(unnamed.is_empty(), "{unnamed:#?}");

    // Excluding a subfolder saves it and estimates again.
    h.get_by_role_and_label(Role::Button, "Exclude /srv/photos/raw")
        .click();
    settle(&mut h);
    assert_eq!(
        h.state().config.exclude.last().map(String::as_str),
        Some("/srv/photos/raw")
    );
    until_estimated(&mut h);
    assert!(
        h.get_by_role_and_label(Role::Button, "Excluded /srv/photos/raw")
            .accesskit_node()
            .is_disabled()
    );

    h.get_by_role_and_label(Role::Button, "Cancel adding /srv/photos")
        .click();
    settle(&mut h);
    assert!(h.state().adding.is_none());
    assert!(
        !sent(&requests)
            .iter()
            .any(|r| matches!(r, Request::Reindex(_)))
    );
}

#[test]
fn search_page_controls_are_named_by_their_row_labels() {
    let (w, _requests) = window(Page::Search, "search");
    let mut h = harness(w, WIDE, Theme::Dark);
    let images = h.get_by_role_and_label(Role::CheckBox, "Index images");
    assert_eq!(
        images.accesskit_node().toggled(),
        Some(accesskit::Toggled::False)
    );
    images.click();
    settle(&mut h);
    assert!(h.state().config.formats.images_enabled());
    // Shown once images are on, named by its row label rather than only a placeholder.
    h.get_by_role_and_label(Role::TextInput, "Image types:");
    // Code is opt-in too (#110), and saved as it's turned on.
    let code = h.get_by_role_and_label(Role::CheckBox, "Index code");
    assert_eq!(
        code.accesskit_node().toggled(),
        Some(accesskit::Toggled::False)
    );
    code.click();
    settle(&mut h);
    assert!(h.state().config.formats.code_enabled());
    let saved = std::fs::read_to_string(&h.state().config_path).unwrap();
    assert!(saved.contains("[formats.code]\nenabled = true"), "{saved}");
    for row in ["Text:", "Images:", "Code:"] {
        h.get_by_role_and_label(Role::Slider, row);
        h.get_by_role_and_label(Role::SpinButton, row);
    }
}

#[test]
fn the_editor_is_saved_when_the_field_is_left() {
    let (w, _requests) = window(Page::Search, "editor");
    let mut h = harness(w, WIDE, Theme::Light);
    h.get_by_role_and_label(Role::TextInput, "Editor:").focus();
    settle(&mut h);
    h.get_by_role_and_label(Role::TextInput, "Editor:")
        .type_text("kate");
    settle(&mut h);
    h.key_press(Key::Enter);
    settle(&mut h);
    assert_eq!(h.state().config.editor.as_deref(), Some("kate"));
    let saved = std::fs::read_to_string(&h.state().config_path).unwrap();
    assert!(saved.contains("editor = \"kate\""), "{saved}");
}

#[test]
fn a_score_slider_moves_from_the_keyboard() {
    let (w, _requests) = window(Page::Search, "slider");
    let mut h = harness(w, WIDE, Theme::Light);
    h.get_by_role_and_label(Role::Slider, "Text:").focus();
    settle(&mut h);
    h.key_press(Key::ArrowRight);
    settle(&mut h);
    // Right is closer: the bar the score has to clear goes up.
    let moved = h.state().config.formats.text.min_score;
    assert!(
        moved.is_some_and(|v| v > DEFAULT_TEXT_MIN_SCORE + 0.001),
        "{moved:?}"
    );
}

#[test]
fn model_page_controls_are_named_and_delete_asks_first() {
    let (w, requests) = window(Page::Model, "model");
    let mut h = harness(w, WIDE, Theme::Light);
    h.get_by_role_and_label(Role::ComboBox, "Device:");
    h.get_by_role_and_label(Role::SpinButton, "Unload after:");
    h.get_by_role_and_label(Role::Slider, "Indexing share:");
    h.get_by_role_and_label(Role::SpinButton, "Indexing share:");
    h.get_by_role_and_label(Role::Button, "Verify");
    h.get_by_role_and_label(Role::Button, "Unload now").click();
    settle(&mut h);
    h.get_by_role_and_label(Role::Button, "Delete…").click();
    settle(&mut h);
    assert!(h.get_by_role_and_label(Role::Button, "Cancel").is_focused());
    h.get_by_role_and_label(Role::Button, "Delete the model")
        .click();
    settle(&mut h);
    let sent = sent(&requests);
    assert!(sent.iter().any(|r| matches!(r, Request::Unload)));
    assert!(sent.iter().any(|r| matches!(r, Request::DeleteModel)));
}

#[test]
fn the_device_menu_opens_and_picks_from_the_keyboard() {
    let (w, _requests) = window(Page::Model, "device");
    let mut h = harness(w, WIDE, Theme::Light);
    h.get_by_role_and_label(Role::ComboBox, "Device:").focus();
    settle(&mut h);
    h.key_press(Key::Enter);
    settle(&mut h);
    h.get_by_role_and_label(Role::Button, "CPU").click();
    settle(&mut h);
    assert_eq!(h.state().config.device.as_deref(), Some("CPU"));
}

#[test]
fn a_failed_probe_says_so_in_words() {
    let (w, _requests) = window(Page::Hardware, "doctor");
    let h = harness(w, WIDE, Theme::Light);
    h.get_by_label("This machine");
    // Red too, but the words carry it.
    h.get_by_label("failed on NPU: no such device");
    h.get_by_role_and_label(Role::Button, "Check again");
}

#[test]
fn index_on_battery_is_a_checkbox_saved_only_when_off() {
    let (w, _requests) = window(Page::Hardware, "battery");
    let path = w.config_path.clone();
    let mut h = harness(w, WIDE, Theme::Light);
    h.get_by_role_and_label(Role::Heading, "Power");
    let battery = h.get_by_role_and_label(Role::CheckBox, "Index on battery");
    assert_eq!(
        battery.accesskit_node().toggled(),
        Some(accesskit::Toggled::True)
    );
    battery.click();
    settle(&mut h);
    assert!(!h.state().config.index_on_battery());
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("index_on_battery = false"), "{text}");
    h.get_by_role_and_label(Role::CheckBox, "Index on battery")
        .click();
    settle(&mut h);
    assert_eq!(h.state().config.index_on_battery, None);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("index_on_battery"), "{text}");
}

#[test]
fn the_index_page_says_why_power_holds_indexing() {
    for (key, says) in [
        (hold::CHARGER, "Waiting for a charger"),
        (hold::POWER_SAVER, "Paused: power saver"),
    ] {
        let (w, _requests) = window(Page::Index, key);
        if let Some(Ok(s)) = &mut w.link.shared.lock().unwrap().status {
            s.indexing = false;
            s.power_hold = key.into();
        }
        let h = harness(w, WIDE, Theme::Light);
        h.get_by_label(says);
        assert!(h.query_by_role_and_label(Role::Button, "Stop").is_none());
    }
}

#[test]
fn the_index_page_lists_documents_indexed_in_part() {
    let (w, _requests) = window(Page::Index, "partial");
    if let Some(Ok(s)) = &mut w.link.shared.lock().unwrap().status {
        s.indexing_rest = true;
        s.partial = (0..5)
            .map(|i| Partial {
                path: format!("/srv/books/book{i}.pdf"),
                unit: "page".into(),
                done: 120,
                total: 1640,
            })
            .collect();
        s.partial[1].unit = "chapter".into();
        s.partial[1].done = 30;
        s.partial[1].total = 41;
    }
    let h = harness(w, WIDE, Theme::Light);
    h.get_by_label("Indexing the rest of long documents");
    h.get_by_label("book0.pdf, 120 of 1,640 pages");
    h.get_by_label("book1.pdf, 30 of 41 chapters");
    h.get_by_label("and 2 more");
    assert!(h.query_by_label("book3.pdf, 120 of 1,640 pages").is_none());
}

#[test]
fn integration_switches_say_their_state_in_words() {
    let (w, _requests) = window(Page::Integration, "integration");
    let h = harness(w, WIDE, Theme::Dark);
    assert_eq!(h.get_all_by_label("Off").count(), 3);
    for place in ["KRunner", "GNOME Shell", "Search service"] {
        h.get_by_role_and_label(Role::Button, &format!("Turn on {place}"));
    }
    h.get_by_label("vinosearch doctor");
}

#[test]
fn a_notice_is_announced_when_it_appears() {
    let (mut w, _requests) = window(Page::Index, "notice");
    w.notice = Some("Couldn't save the settings".into());
    let mut h = harness(w, WIDE, Theme::Light);
    let notice = h.get_by_label("Couldn't save the settings");
    assert_eq!(notice.accesskit_node().live(), accesskit::Live::Assertive);
    h.get_by_role_and_label(Role::Button, "Dismiss").click();
    settle(&mut h);
    assert!(h.state().notice.is_none());
}

#[test]
fn a_greyed_out_button_says_it_is_disabled() {
    let (w, _requests) = window(Page::Folders, "disabled");
    let mut h = harness(w, WIDE, Theme::Light);
    let disabled = |h: &Harness<'_, SettingsWindow>, name: &str| {
        h.get_by_role_and_label(Role::Button, name)
            .accesskit_node()
            .is_disabled()
    };
    // The window's status is mid-index.
    assert!(disabled(&h, "Re-index /srv/notes"));
    assert!(disabled(&h, "Add to included"));
    h.get_by_role_and_label(Role::TextInput, "Folder to add")
        .click();
    settle(&mut h);
    h.get_by_role_and_label(Role::TextInput, "Folder to add")
        .type_text("/srv/more");
    settle(&mut h);
    assert!(!disabled(&h, "Add to included"));
}

#[test]
fn reduced_motion_stops_the_spinner() {
    let (w, _requests) = window(Page::Index, "still");
    let mut h = harness(w, WIDE, Theme::Light);
    h.ctx.all_styles_mut(|style| style.animation_time = 0.0);
    h.run_steps(2);
    // With nothing changing, the frames stop: run() would panic if the spinner kept asking.
    h.run();
}

#[test]
fn large_text_clips_nothing_at_either_width() {
    for size in [WIDE, NARROW] {
        for page in Page::ALL {
            let (w, _requests) = window(page, "large");
            let mut h = harness(w, size, Theme::Light);
            h.ctx.set_zoom_factor(1.5);
            settle(&mut h);
            let right = h.ctx.content_rect().right();
            let clipped: Vec<_> = h
                .query_all_by(|node| CONTROLS.contains(&node.role()) && !node.is_hidden())
                .filter(|node| node.rect().max.x > right + 0.5)
                .map(|node| format!("{node:?} at {:?}", node.rect()))
                .collect();
            assert!(
                clipped.is_empty(),
                "{} at {size:?}: past the right edge {clipped:#?}",
                page.title()
            );
        }
    }
}

#[test]
fn zoom_keeps_every_page_usable() {
    for page in Page::ALL {
        let (w, _requests) = window(page, "zoom");
        let mut h = harness(w, WIDE, Theme::Light);
        h.ctx.set_zoom_factor(2.0);
        settle(&mut h);
        // Twice the size is past the sidebar's breakpoint: the tabs take over.
        h.get_by_role_and_label(Role::Tab, page.title());
        assert!(unnamed_controls(&h).is_empty(), "{}", page.title());
    }
}

/// The Security page over a runner that says whether there's a TPM, and how the index is
/// encrypted.
fn security(name: &str, edit: impl FnOnce(&mut Status)) -> (SettingsWindow, Receiver<Request>) {
    let (w, requests) = window(Page::Security, name);
    if let Some(Ok(s)) = w.link.shared.lock().unwrap().status.as_mut() {
        edit(s);
    }
    (w, requests)
}

#[test]
fn encryption_is_saved_and_applied_at_once() {
    let (w, requests) = security("encrypt", |s| s.tpm = true);
    let path = w.config_path.clone();
    let mut h = harness(w, WIDE, Theme::Light);
    h.get_by_label("Not encrypted");
    // A TPM: nothing to say about doing without one.
    assert!(
        h.query_by_label("Use the host key instead (weaker)")
            .is_none()
    );
    h.get_by_role_and_label(Role::CheckBox, "Encrypt the index")
        .click();
    settle(&mut h);
    assert!(h.state().config.encryption.on());
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("[encryption]\nenabled = true"), "{text}");
    assert!(
        sent(&requests)
            .iter()
            .any(|r| matches!(r, Request::ApplyEncryption))
    );
    h.get_by_label("Encrypting…");
}

#[test]
fn without_a_tpm_turning_it_on_is_refused_until_the_host_key_is_allowed() {
    let (w, requests) = security("notpm", |_| {});
    let mut h = harness(w, WIDE, Theme::Light);
    h.get_by_role_and_label(Role::CheckBox, "Encrypt the index")
        .click();
    settle(&mut h);
    assert!(!h.state().config.encryption.on());
    assert!(h.state().notice.as_ref().unwrap().contains("no TPM"));
    assert!(sent(&requests).is_empty());
    h.get_by_role_and_label(Role::CheckBox, "Use the host key instead (weaker)")
        .click();
    settle(&mut h);
    h.get_by_role_and_label(Role::CheckBox, "Encrypt the index")
        .click();
    settle(&mut h);
    let c = &h.state().config.encryption;
    assert!(c.on() && c.host_key_allowed());
}

#[test]
fn an_encrypted_index_says_what_seals_it_and_a_new_key_asks_first() {
    let (w, requests) = security("rekey", |s| {
        s.tpm = true;
        s.encrypted = true;
        s.seal = "tpm".into();
        s.seal_broken = "Failed to unseal secret using TPM2".into();
    });
    let mut h = harness(w, WIDE, Theme::Light);
    h.get_by_label("Encrypted, the key sealed to this machine's TPM");
    h.get_by_label_contains("so the index is being built again from your files");
    h.get_by_role_and_label(Role::Button, "New key…").click();
    settle(&mut h);
    assert!(h.get_by_role_and_label(Role::Button, "Cancel").is_focused());
    h.get_by_role_and_label(Role::Button, "Replace the key")
        .click();
    settle(&mut h);
    assert!(sent(&requests).iter().any(|r| matches!(r, Request::Rekey)));
}
