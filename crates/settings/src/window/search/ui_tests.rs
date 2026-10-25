//! The search window as a screen reader sees it, against a fake source: no
//! bus, no runner, no wl-copy. Its photos are real PNGs in a temp folder, so
//! the grid shows decoded thumbnails.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use eframe::egui::Theme;
use eframe::egui::accesskit::Role;
use egui_kittest::kittest::{NodeT, Queryable};
use egui_kittest::{Harness, Node};

use super::*;

const NOW: i64 = 1_800_000_000;

/// Answers every query with the same files, and notes what it was asked.
struct Fake {
    results: Vec<Found>,
    asked: Arc<Mutex<Vec<Ask>>>,
}

impl Source for Fake {
    fn search(&mut self, query: &str) -> Result<Vec<Found>, String> {
        self.asked
            .lock()
            .unwrap()
            .push(Ask::Search(0, query.to_owned()));
        if query == "fail" {
            return Err("the worker has stopped".into());
        }
        Ok(self.results.clone())
    }

    fn roots(&mut self) -> Result<Vec<String>, String> {
        Ok(vec!["/srv/notes".into(), "/srv/photos".into()])
    }

    fn open(&mut self, path: &str, at: u32) -> Result<(), String> {
        self.asked
            .lock()
            .unwrap()
            .push(Ask::Open(path.to_owned(), at));
        Ok(())
    }

    fn open_folder(&mut self, path: &str) -> Result<(), String> {
        self.asked
            .lock()
            .unwrap()
            .push(Ask::OpenFolder(path.to_owned()));
        Ok(())
    }

    fn copy(&mut self, path: &str) -> Result<(), String> {
        self.asked.lock().unwrap().push(Ask::Copy(path.to_owned()));
        Ok(())
    }
}

fn photos(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vinosearch-search-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (file, colour) in [("beach.png", [40, 120, 220]), ("dog.png", [180, 120, 40])] {
        image::RgbImage::from_pixel(400, 300, image::Rgb(colour))
            .save(dir.join(file))
            .unwrap();
    }
    dir
}

fn found(path: &str, kind: Kind, snippet: &str, unit: &str, at: u32, age_days: i64) -> Found {
    Found {
        path: path.to_owned(),
        kind,
        score: 0.7,
        snippet: snippet.to_owned(),
        unit: unit.to_owned(),
        at,
        mtime: NOW - age_days * DAY,
    }
}

fn results(photos: &Path) -> Vec<Found> {
    let photo = |name: &str| photos.join(name).display().to_string();
    vec![
        found(
            "/srv/notes/trip.md",
            Kind::Text,
            "Packed the tent and drove to the coast",
            "line",
            12,
            2,
        ),
        found(
            "/srv/notes/budget.pdf",
            Kind::Document,
            "Camping gear: 240 dollars",
            "page",
            3,
            40,
        ),
        found(&photo("beach.png"), Kind::Image, "", "", 0, 1),
        found(&photo("dog.png"), Kind::Image, "", "", 0, 400),
    ]
}

type Asked = Arc<Mutex<Vec<Ask>>>;

fn window(query: &str, name: &str) -> (SearchWindow, Asked, PathBuf) {
    let dir = photos(name);
    let asked = Asked::default();
    let fake = Fake {
        results: results(&dir),
        asked: Arc::clone(&asked),
    };
    let mut w = SearchWindow::with(query, fake);
    w.now = || NOW;
    (w, asked, dir)
}

fn harness(w: SearchWindow, size: [f32; 2]) -> Harness<'static, SearchWindow> {
    let mut styled = false;
    let mut h = Harness::builder()
        .with_size(size)
        .with_theme(Theme::Light)
        .build_ui_state(
            move |ui, w: &mut SearchWindow| {
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

const WIDE: [f32; 2] = [960.0, 720.0];
const NARROW: [f32; 2] = [380.0, 720.0];

fn settle(h: &mut Harness<'_, SearchWindow>) {
    h.run_steps(3);
}

/// Frames until `done`, which waits on another thread.
fn until(h: &mut Harness<'_, SearchWindow>, what: &str, done: impl Fn(&SearchWindow) -> bool) {
    for _ in 0..200 {
        settle(h);
        if done(h.state()) {
            settle(h);
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("{what} never happened");
}

fn answered(h: &mut Harness<'_, SearchWindow>) {
    until(h, "the search", |w| !w.searching);
}

fn asked(asked: &Asked, h: &mut Harness<'_, SearchWindow>, want: &Ask) {
    until(h, &format!("{want:?}"), |_| {
        asked.lock().unwrap().contains(want)
    });
}

fn options<'h>(h: &'h Harness<'_, SearchWindow>, list: &str) -> Vec<Node<'h>> {
    h.query_all_by_role(Role::ListBoxOption)
        .filter(|n| {
            let mut up = n.accesskit_node().parent();
            while let Some(p) = up {
                if p.role() == Role::ListBox {
                    return p.label().as_deref() == Some(list);
                }
                up = p.parent();
            }
            false
        })
        .collect()
}

const CONTROLS: [Role; 7] = [
    Role::Button,
    Role::CheckBox,
    Role::TextInput,
    Role::ComboBox,
    Role::ListBoxOption,
    Role::ScrollBar,
    Role::Link,
];

#[test]
fn the_query_from_the_command_line_is_searched_and_shown_by_kind() {
    let (w, asked, dir) = window("camping trip", "shown");
    let mut h = harness(w, WIDE);
    answered(&mut h);
    assert_eq!(
        asked.lock().unwrap()[0],
        Ask::Search(0, "camping trip".into())
    );
    assert_eq!(h.root().accesskit_node().label().as_deref(), Some(TITLE));
    h.get_by_role_and_label(Role::Heading, "Images (2)");
    h.get_by_role_and_label(Role::Heading, "Text and documents (2)");
    let photos = options(&h, "Images");
    assert_eq!(photos.len(), 2);
    assert!(
        photos[0]
            .accesskit_node()
            .label()
            .unwrap()
            .starts_with("beach.png, in /")
    );
    let texts: Vec<String> = options(&h, "Text and documents")
        .iter()
        .map(|n| n.accesskit_node().label().unwrap_or_default())
        .collect();
    assert_eq!(
        texts,
        [
            "trip.md, line 12, in /srv/notes: Packed the tent and drove to the coast",
            "budget.pdf, page 3, in /srv/notes: Camping gear: 240 dollars",
        ]
    );
    // The photos decode off the UI thread and land as textures.
    until(&mut h, "the thumbnails", |w| {
        w.results.iter().filter(|f| f.kind == Kind::Image).all(|f| {
            w.thumbs
                .shown
                .get(&f.path)
                .is_some_and(|t| matches!(t, Thumb::Ready(_)))
        })
    });
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn every_control_has_a_name_wide_or_narrow() {
    for size in [WIDE, NARROW] {
        let (w, _asked, dir) = window("camping", "names");
        let mut h = harness(w, size);
        answered(&mut h);
        // Select one, so the action bar shows too.
        options(&h, "Text and documents")[0].click();
        settle(&mut h);
        let unnamed: Vec<String> = h
            .query_all_by(|node| {
                let focusable = node.data().supports_action(accesskit::Action::Focus);
                (focusable || CONTROLS.contains(&node.role())) && !node.is_hidden()
            })
            .filter(|node: &Node<'_>| {
                node.accesskit_node()
                    .label()
                    .is_none_or(|label| label.trim().is_empty())
            })
            .map(|node| format!("{node:?}"))
            .collect();
        assert!(unnamed.is_empty(), "{size:?}: unnamed {unnamed:#?}");
        h.get_by_role_and_label(Role::TextInput, "Search");
        for combo in ["Folder:", "Modified:", "File type:"] {
            h.get_by_role_and_label(Role::ComboBox, combo);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn filters_narrow_by_kind_folder_date_and_type() {
    let (w, _asked, dir) = window("camping", "filters");
    let mut h = harness(w, WIDE);
    answered(&mut h);
    let shown = |h: &Harness<'_, SearchWindow>| {
        h.state()
            .shown()
            .iter()
            .map(|f| f.name().to_owned())
            .collect::<Vec<_>>()
    };

    h.get_by_role_and_label(Role::CheckBox, "Images (2)")
        .click();
    settle(&mut h);
    assert!(h.query_by_role_and_label(Role::ListBox, "Images").is_none());
    assert_eq!(shown(&h), ["trip.md", "budget.pdf"]);
    h.get_by_role_and_label(Role::Button, "Clear filters")
        .click();
    settle(&mut h);
    assert_eq!(shown(&h).len(), 4);

    h.state_mut().filters.folder = Some("/srv/notes".into());
    settle(&mut h);
    assert_eq!(shown(&h), ["trip.md", "budget.pdf"]);
    h.state_mut().filters = Filters::default();

    h.state_mut().filters.modified = Modified::Week;
    settle(&mut h);
    assert_eq!(shown(&h), ["trip.md", "beach.png"]);
    h.state_mut().filters.modified = Modified::Year;
    settle(&mut h);
    assert_eq!(shown(&h), ["trip.md", "budget.pdf", "beach.png"]);
    h.state_mut().filters = Filters::default();

    h.state_mut().filters.extension = Some("pdf".into());
    settle(&mut h);
    assert_eq!(shown(&h), ["budget.pdf"]);

    // A filter that hides everything says so, and Clear filters is there.
    h.state_mut().filters.text = false;
    h.state_mut().filters.documents = false;
    settle(&mut h);
    h.get_by_label("No results match the filters.");
    h.get_by_role_and_label(Role::Button, "Clear filters");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn the_filter_menus_open_and_pick_from_the_keyboard() {
    let (w, _asked, dir) = window("camping", "menus");
    let mut h = harness(w, WIDE);
    answered(&mut h);
    h.get_by_role_and_label(Role::ComboBox, "File type:")
        .click();
    settle(&mut h);
    h.get_by_role_and_label(Role::Button, "PDF (1)").click();
    settle(&mut h);
    assert_eq!(h.state().filters.extension.as_deref(), Some("pdf"));

    h.get_by_role_and_label(Role::ComboBox, "Folder:").click();
    settle(&mut h);
    h.get_by_role_and_label(Role::Button, "/srv/photos").click();
    settle(&mut h);
    assert_eq!(h.state().filters.folder.as_deref(), Some("/srv/photos"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_selected_result_opens_at_its_match_its_folder_or_copies() {
    let (w, asked_, dir) = window("camping", "actions");
    let mut h = harness(w, WIDE);
    answered(&mut h);
    options(&h, "Text and documents")[1].click();
    settle(&mut h);
    assert_eq!(h.state().selected.as_deref(), Some("/srv/notes/budget.pdf"));
    let selected = &options(&h, "Text and documents")[1];
    assert_eq!(selected.accesskit_node().is_selected(), Some(true));

    h.get_by_role_and_label(Role::Button, "Open at page 3 budget.pdf")
        .click();
    asked(
        &asked_,
        &mut h,
        &Ask::Open("/srv/notes/budget.pdf".into(), 3),
    );
    h.get_by_role_and_label(Role::Button, "Open folder budget.pdf")
        .click();
    asked(
        &asked_,
        &mut h,
        &Ask::OpenFolder("/srv/notes/budget.pdf".into()),
    );
    h.get_by_role_and_label(Role::Button, "Copy file budget.pdf")
        .click();
    asked(&asked_, &mut h, &Ask::Copy("/srv/notes/budget.pdf".into()));
    until(&mut h, "the copy notice", |w| {
        w.notice.as_deref() == Some("Copied budget.pdf")
    });

    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn the_keyboard_goes_from_the_query_to_the_results_and_opens_one() {
    let (w, asked_, dir) = window("camping", "keys");
    let mut h = harness(w, WIDE);
    answered(&mut h);
    // The search field has focus from the start.
    assert!(
        h.get_by_role_and_label(Role::TextInput, "Search")
            .is_focused()
    );
    h.key_press(Key::ArrowDown);
    settle(&mut h);
    let first = &options(&h, "Images")[0];
    assert!(first.is_focused());
    let beach = dir.join("beach.png").display().to_string();
    assert_eq!(h.state().selected.as_deref(), Some(beach.as_str()));
    // The arrows move about the grid, and Tab on to the list, selecting as
    // they go.
    h.key_press(Key::ArrowRight);
    settle(&mut h);
    let dog = dir.join("dog.png").display().to_string();
    assert_eq!(h.state().selected.as_deref(), Some(dog.as_str()));
    h.key_press(Key::Enter);
    asked(&asked_, &mut h, &Ask::Open(dog.clone(), 0));
    h.key_press(Key::Tab);
    settle(&mut h);
    assert_eq!(h.state().selected.as_deref(), Some("/srv/notes/trip.md"));
    h.key_press(Key::Enter);
    asked(&asked_, &mut h, &Ask::Open("/srv/notes/trip.md".into(), 12));
    // Ctrl+C copies it.
    h.event(egui::Event::Copy);
    asked(&asked_, &mut h, &Ask::Copy("/srv/notes/trip.md".into()));
    // Ctrl+F goes back to the query.
    h.key_press_modifiers(Modifiers::COMMAND, Key::F);
    settle(&mut h);
    assert!(
        h.get_by_role_and_label(Role::TextInput, "Search")
            .is_focused()
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn typing_searches_once_it_pauses_and_errors_are_said() {
    let (w, asked_, dir) = window("", "typing");
    let mut h = harness(w, WIDE);
    h.get_by_label_contains("a sentence works better");
    h.get_by_role_and_label(Role::TextInput, "Search")
        .type_text("fail");
    h.step();
    // Nothing yet: the typing hasn't paused.
    assert!(h.state().typed_at.is_some());
    assert!(asked_.lock().unwrap().is_empty());
    asked(&asked_, &mut h, &Ask::Search(0, "fail".into()));
    answered(&mut h);
    h.get_by_label("Couldn't search: the worker has stopped");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_older_answer_never_replaces_a_newer_one() {
    let (mut w, _asked, dir) = window("", "stale");
    w.asked = 2;
    w.searching = true;
    w.link.shared.lock().unwrap().results = Some((1, Ok(Vec::new())));
    w.take_answers();
    assert!(w.searching, "the answer to search 1 was taken for search 2");
    std::fs::remove_dir_all(dir).unwrap();
}
