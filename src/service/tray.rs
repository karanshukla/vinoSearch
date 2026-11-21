//! A tray icon while indexing runs, and none otherwise (#235): the runner has
//! nothing to show between runs, so it isn't in the panel then.
//!
//! The item is registered when indexing has run for `SHOW_AFTER` and removed
//! once it has been quiet for `HIDE_AFTER`, so one changed file doesn't flash
//! an icon. It stays while paused with work left, to offer Resume.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_channel::oneshot;
use ksni::menu::StandardItem;
use ksni::{Category, MenuItem, ToolTip};
use vino_ui::tray::MenuIcon;
use vinosearch_shared::dbus::names;
use vinosearch_shared::sandbox;

use crate::service::worker::{Job, Status};

pub mod icon;

const POLL: Duration = Duration::from_secs(1);
const SHOW_AFTER: u32 = 3;
const HIDE_AFTER: u32 = 5;

pub type Quit = Arc<Mutex<Option<oneshot::Sender<()>>>>;

/// What the icon says, read from `Status` once a `POLL`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct View {
    paused: bool,
    files: u64,
    rest: bool,
}

impl View {
    fn of(status: &Status) -> View {
        View {
            paused: status.paused.load(Ordering::Relaxed),
            files: status.indexed_files.load(Ordering::Relaxed),
            rest: status.indexing_rest.load(Ordering::Relaxed),
        }
    }

    fn text(&self) -> String {
        let files = match self.files {
            0 => String::new(),
            1 => ", 1 file so far".into(),
            n => format!(", {n} files so far"),
        };
        if self.paused {
            format!("Indexing paused{files}")
        } else if self.rest {
            format!("Indexing the rest of a long document{files}")
        } else {
            format!("Indexing{files}")
        }
    }
}

/// Whether the icon is up, from one reading a `POLL`.
#[derive(Default)]
struct Shown {
    up: bool,
    /// Readings in a row that disagree with `up`.
    against: u32,
}

impl Shown {
    /// True when `up` changed.
    fn next(&mut self, wanted: bool) -> bool {
        if wanted == self.up {
            self.against = 0;
            return false;
        }
        self.against += 1;
        let needed = if wanted { SHOW_AFTER } else { HIDE_AFTER };
        if self.against < needed {
            return false;
        }
        self.up = wanted;
        self.against = 0;
        true
    }
}

fn wanted(status: &Status) -> bool {
    status.indexing() || (status.paused.load(Ordering::Relaxed) && status.work.pending())
}

struct Tray {
    status: Arc<Status>,
    jobs: std::sync::mpsc::Sender<Job>,
    quit: Quit,
    view: View,
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "vinosearch".into()
    }

    fn title(&self) -> String {
        "vinoSearch".into()
    }

    fn category(&self) -> Category {
        Category::ApplicationStatus
    }

    /// `<icon>-symbolic`, installed natively and exported by the Flatpak, as
    /// the other vinoPilot trays ask for theirs; the pixmaps are the fallback.
    fn icon_name(&self) -> String {
        format!("{}-symbolic", names().icon)
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        static ICONS: std::sync::OnceLock<Vec<ksni::Icon>> = std::sync::OnceLock::new();
        ICONS.get_or_init(icon::pixmaps).clone()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: "vinoSearch".into(),
            description: self.view.text(),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        crate::service::krunner::launch_settings(&[]);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let (label, icon) = if self.view.paused {
            ("Resume indexing", MenuIcon::Refresh)
        } else {
            ("Pause indexing", MenuIcon::Cancel)
        };
        vec![
            StandardItem {
                label: self.view.text(),
                icon_name: MenuIcon::Info.name().into(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: label.into(),
                icon_name: icon.name().into(),
                activate: Box::new(|tray: &mut Self| {
                    let pause = !tray.view.paused;
                    tray.status.paused.store(pause, Ordering::Relaxed);
                    if !pause {
                        let _ = tray.jobs.send(Job::Retry);
                    }
                    tray.view = View::of(&tray.status);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Settings…".into(),
                icon_name: MenuIcon::Settings.name().into(),
                activate: Box::new(|_: &mut Self| crate::service::krunner::launch_settings(&[])),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                icon_name: MenuIcon::Quit.name().into(),
                activate: Box::new(|tray: &mut Self| {
                    if let Some(quit) = tray.quit.lock().unwrap().take() {
                        let _ = quit.send(());
                    }
                }),
                ..Default::default()
            }
            .into(),
        ]
    }

    // No tray host (GNOME without the AppIndicator extension, a panel not up
    // yet) is no reason to stop: search works without the icon.
    fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
        true
    }
}

/// Watch `status` for the runner's life, showing and hiding the icon.
pub fn spawn(status: Arc<Status>, jobs: std::sync::mpsc::Sender<Job>, quit: Quit) {
    std::thread::spawn(move || {
        use ksni::blocking::TrayMethods;
        let mut shown = Shown::default();
        let mut handle: Option<ksni::blocking::Handle<Tray>> = None;
        loop {
            std::thread::sleep(POLL);
            let changed = shown.next(wanted(&status));
            let view = View::of(&status);
            match (&handle, changed && shown.up, changed && !shown.up) {
                (None, true, _) => {
                    let tray = Tray {
                        status: status.clone(),
                        jobs: jobs.clone(),
                        quit: quit.clone(),
                        view,
                    };
                    // A Flatpak may not own the item's well-known name.
                    match tray
                        .disable_dbus_name(sandbox::flatpak_id().is_some())
                        .assume_sni_available(true)
                        .spawn()
                    {
                        Ok(h) => handle = Some(h),
                        Err(e) => eprintln!("no tray icon: {e}"),
                    }
                }
                (Some(h), _, true) => {
                    h.shutdown().wait();
                    handle = None;
                }
                (Some(h), _, _) => {
                    h.update(|tray| {
                        if tray.view != view {
                            tray.view = view;
                        }
                    });
                }
                _ => {}
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_run_shows_no_icon_and_a_long_one_does() {
        let mut s = Shown::default();
        assert!(!s.next(true));
        assert!(!s.next(false), "one changed file came and went");
        assert!(!s.up);
        for _ in 1..SHOW_AFTER {
            assert!(!s.next(true));
        }
        assert!(s.next(true));
        assert!(s.up);
    }

    #[test]
    fn the_icon_outlasts_a_gap_between_files() {
        let mut s = Shown {
            up: true,
            against: 0,
        };
        for _ in 1..HIDE_AFTER {
            assert!(!s.next(false));
        }
        assert!(!s.next(true), "the next file started");
        for _ in 1..HIDE_AFTER {
            assert!(!s.next(false));
        }
        assert!(s.next(false));
        assert!(!s.up);
    }

    #[test]
    fn a_paused_run_with_work_left_keeps_its_icon() {
        let status = Status::new("CPU");
        assert!(!wanted(&status));
        status.paused.store(true, Ordering::Relaxed);
        assert!(!wanted(&status), "paused with nothing to do");
        status
            .work
            .changed([std::path::PathBuf::from("/home/u/a.txt")]);
        assert!(!status.indexing(), "held");
        assert!(
            wanted(&status),
            "held, with a file waiting: Resume is offered"
        );
        status.paused.store(false, Ordering::Relaxed);
        assert!(wanted(&status));
    }

    #[test]
    fn the_text_says_what_is_happening() {
        let v = |paused, files, rest| View {
            paused,
            files,
            rest,
        };
        assert_eq!(v(false, 0, false).text(), "Indexing");
        assert_eq!(v(false, 1, false).text(), "Indexing, 1 file so far");
        assert_eq!(
            v(true, 12, false).text(),
            "Indexing paused, 12 files so far"
        );
        assert_eq!(
            v(false, 0, true).text(),
            "Indexing the rest of a long document"
        );
    }
}
