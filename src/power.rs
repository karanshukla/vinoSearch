//! Holding indexing for power (#109): on battery while `index_on_battery` is
//! false, and while power-profiles-daemon (or tuned-ppd, which serves the
//! same interface) has the power-saver profile on.
//!
//! The profile is watched on the system bus, by its PropertiesChanged signal
//! and by the name's owner coming and going, so nothing is polled and a
//! daemon started or restarted later is picked up. Without one on the bus,
//! or without a system bus at all, power saver never holds anything. The
//! battery is read from sysfs every BATTERY_EVERY, as `sched::Pace` does.
//!
//! Held work waits in the worker with what a PauseIndexing holds, and a
//! `Job::Retry` runs it once nothing holds it any more.

use crate::sched;
use crate::worker::{Job, Status};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;
use vinosearch_shared::dbus::hold;
use zbus::zvariant::OwnedValue;

const PPD: &str = "net.hadess.PowerProfiles";
const PPD_PATH: &str = "/net/hadess/PowerProfiles";
const PROFILE: &str = "ActiveProfile";
const POWER_SAVER: &str = "power-saver";
const BATTERY_EVERY: Duration = Duration::from_secs(10);

/// Why indexing is held for power.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hold {
    /// On battery, and `index_on_battery = false`.
    Charger,
    /// The power-saver profile is on.
    PowerSaver,
}

impl Hold {
    /// The value of `power_hold` in Control1's Status.
    pub fn key(self) -> &'static str {
        match self {
            Hold::Charger => hold::CHARGER,
            Hold::PowerSaver => hold::POWER_SAVER,
        }
    }

    fn says(self) -> &'static str {
        match self {
            Hold::Charger => "waiting for a charger",
            Hold::PowerSaver => "power saver is on",
        }
    }
}

/// Whether indexing waits, and why. Waiting for a charger comes first: it
/// outlasts power saver, which a charger usually turns off.
pub fn hold(index_on_battery: bool, on_battery: bool, power_saver: bool) -> Option<Hold> {
    if on_battery && !index_on_battery {
        Some(Hold::Charger)
    } else if power_saver {
        Some(Hold::PowerSaver)
    } else {
        None
    }
}

/// What the hold is decided from, kept in `Status`.
#[derive(Default)]
pub struct State {
    /// `index_on_battery = false`, so false is the default.
    charger_only: AtomicBool,
    on_battery: AtomicBool,
    power_saver: AtomicBool,
    /// Taken by `update`, so two changes at once can't both miss a release.
    lock: Mutex<()>,
}

impl State {
    pub fn hold(&self) -> Option<Hold> {
        hold(
            !self.charger_only.load(Ordering::Relaxed),
            self.on_battery.load(Ordering::Relaxed),
            self.power_saver.load(Ordering::Relaxed),
        )
    }

    /// Apply `change`; true when that released held indexing, which then
    /// needs a Retry.
    fn update(&self, change: impl FnOnce(&Self)) -> bool {
        let _guard = self.lock.lock().unwrap();
        let before = self.hold();
        change(self);
        let after = self.hold();
        if before != after {
            match after {
                Some(h) => eprintln!("indexing held: {}", h.says()),
                None => eprintln!("indexing resumed"),
            }
        }
        before.is_some() && after.is_none()
    }

    pub fn set_index_on_battery(&self, on: bool) -> bool {
        self.update(|s| s.charger_only.store(!on, Ordering::Relaxed))
    }

    pub fn set_on_battery(&self, on: bool) -> bool {
        self.update(|s| s.on_battery.store(on, Ordering::Relaxed))
    }

    pub fn set_power_saver(&self, on: bool) -> bool {
        self.update(|s| s.power_saver.store(on, Ordering::Relaxed))
    }
}

/// Watch the battery and the power profile for as long as the process runs,
/// sending `Job::Retry` to `jobs` when indexing may go on again.
pub fn spawn(jobs: mpsc::Sender<Job>, status: Arc<Status>) {
    // Read now, so a job started before the first poll is held already.
    status.power.set_on_battery(sched::on_battery());
    let battery = (jobs.clone(), status.clone());
    std::thread::spawn(move || {
        let (jobs, status) = battery;
        loop {
            std::thread::sleep(BATTERY_EVERY);
            if status.power.set_on_battery(sched::on_battery()) && jobs.send(Job::Retry).is_err() {
                return;
            }
        }
    });
    std::thread::spawn(move || {
        if let Err(e) = watch_profile(&jobs, &status) {
            eprintln!("power profile not watched: {e}");
        }
    });
}

fn set_power_saver(jobs: &mpsc::Sender<Job>, status: &Status, on: bool) {
    if status.power.set_power_saver(on) {
        let _ = jobs.send(Job::Retry);
    }
}

/// The active profile is power saver; false when it can't be read (no
/// daemon on the bus).
fn read_profile(conn: &zbus::blocking::Connection) -> bool {
    conn.call_method(
        Some(PPD),
        PPD_PATH,
        Some("org.freedesktop.DBus.Properties"),
        "Get",
        &(PPD, PROFILE),
    )
    .and_then(|reply| reply.body().deserialize::<OwnedValue>())
    .ok()
    .and_then(|v| String::try_from(v).ok())
    .is_some_and(|p| p == POWER_SAVER)
}

/// The profile in a PropertiesChanged body: Some(power saver or not) when it
/// changed, None when the signal is about something else.
fn changed_profile(
    interface: &str,
    changed: &HashMap<String, OwnedValue>,
    invalidated: &[String],
    read: impl FnOnce() -> bool,
) -> Option<bool> {
    if interface != PPD {
        return None;
    }
    if let Some(v) = changed.get(PROFILE) {
        return Some(
            v.try_clone()
                .ok()
                .and_then(|v| String::try_from(v).ok())
                .is_some_and(|p| p == POWER_SAVER),
        );
    }
    invalidated.iter().any(|p| p == PROFILE).then(read)
}

fn watch_profile(jobs: &mpsc::Sender<Job>, status: &Arc<Status>) -> zbus::Result<()> {
    let conn = zbus::blocking::Connection::system()?;
    // The daemon starting, stopping or restarting. Subscribed before the
    // first read, so a change between the two isn't missed.
    let owner = zbus::blocking::MessageIterator::for_match_rule(
        zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender("org.freedesktop.DBus")?
            .interface("org.freedesktop.DBus")?
            .member("NameOwnerChanged")?
            .add_arg(PPD)?
            .build(),
        &conn,
        Some(8),
    )?;
    let changes = zbus::blocking::MessageIterator::for_match_rule(
        zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(PPD)?
            .path(PPD_PATH)?
            .interface("org.freedesktop.DBus.Properties")?
            .member("PropertiesChanged")?
            .add_arg(PPD)?
            .build(),
        &conn,
        Some(8),
    )?;
    set_power_saver(jobs, status, read_profile(&conn));
    {
        let (conn, jobs, status) = (conn.clone(), jobs.clone(), status.clone());
        std::thread::spawn(move || {
            for msg in owner.flatten() {
                let Ok((_, _, new)) = msg.body().deserialize::<(String, String, String)>() else {
                    continue;
                };
                // Gone: no daemon, no power saver. A new owner: read what it says.
                let on = !new.is_empty() && read_profile(&conn);
                set_power_saver(&jobs, &status, on);
            }
        });
    }
    for msg in changes.flatten() {
        type Body = (String, HashMap<String, OwnedValue>, Vec<String>);
        let Ok((interface, changed, invalidated)) = msg.body().deserialize::<Body>() else {
            continue;
        };
        if let Some(on) =
            changed_profile(&interface, &changed, &invalidated, || read_profile(&conn))
        {
            set_power_saver(jobs, status, on);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Value;

    #[test]
    fn what_holds_indexing() {
        // (index_on_battery, on_battery, power_saver)
        assert_eq!(hold(true, false, false), None);
        assert_eq!(hold(true, true, false), None);
        assert_eq!(hold(false, false, false), None);
        assert_eq!(hold(false, true, false), Some(Hold::Charger));
        assert_eq!(hold(true, false, true), Some(Hold::PowerSaver));
        assert_eq!(hold(true, true, true), Some(Hold::PowerSaver));
        assert_eq!(hold(false, true, true), Some(Hold::Charger));
        assert_eq!(hold(false, false, true), Some(Hold::PowerSaver));
    }

    #[test]
    fn only_the_last_release_asks_for_a_retry() {
        let s = State::default();
        assert_eq!(s.hold(), None);
        assert!(!s.set_on_battery(true));
        assert_eq!(s.hold(), None, "index_on_battery defaults to true");
        assert!(!s.set_index_on_battery(false));
        assert_eq!(s.hold(), Some(Hold::Charger));
        assert!(!s.set_power_saver(true));
        // Plugged in: power saver still holds it.
        assert!(!s.set_on_battery(false));
        assert_eq!(s.hold(), Some(Hold::PowerSaver));
        assert!(s.set_power_saver(false));
        assert_eq!(s.hold(), None);
        // Setting what is already so releases nothing.
        assert!(!s.set_power_saver(false));
        // Unplugged again, then the setting turned back on.
        assert!(!s.set_on_battery(true));
        assert!(s.set_index_on_battery(true));
    }

    #[test]
    fn reads_the_profile_from_properties_changed() {
        let profile = |p: &str| {
            HashMap::from([(
                PROFILE.to_owned(),
                OwnedValue::try_from(Value::from(p)).unwrap(),
            )])
        };
        let unread = || panic!("read when the value was in the signal");
        assert_eq!(
            changed_profile(PPD, &profile("power-saver"), &[], unread),
            Some(true)
        );
        assert_eq!(
            changed_profile(PPD, &profile("balanced"), &[], unread),
            Some(false)
        );
        assert_eq!(
            changed_profile(PPD, &profile("performance"), &[], unread),
            Some(false)
        );
        // Another property, or another interface.
        let other = HashMap::from([(
            "PerformanceDegraded".to_owned(),
            OwnedValue::try_from(Value::from("")).unwrap(),
        )]);
        assert_eq!(changed_profile(PPD, &other, &[], unread), None);
        assert_eq!(
            changed_profile("org.example", &profile("power-saver"), &[], unread),
            None
        );
        // Invalidated: read again.
        assert_eq!(
            changed_profile(PPD, &HashMap::new(), &[PROFILE.to_owned()], || true),
            Some(true)
        );
    }
}
