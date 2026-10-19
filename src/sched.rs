//! Idle priority and pacing for background indexing.
//!
//! The worker thread both indexes and answers queries, so it drops to idle
//! CPU scheduling and idle I/O priority for an indexing job and returns to
//! normal for each query it answers in between. Walking and reading files
//! then loses to anything the user is doing.

use std::sync::atomic::{AtomicBool, Ordering};

const IOPRIO_WHO_PROCESS: libc::c_int = 1;
const IOPRIO_CLASS_SHIFT: libc::c_int = 13;
const IOPRIO_CLASS_IDLE: libc::c_int = 3;
/// Class "none": priority follows the CPU nice value, the default.
const IOPRIO_DEFAULT: libc::c_int = 0;

/// Whether leaving SCHED_IDLE is allowed here. An unprivileged thread may
/// switch back to SCHED_OTHER only while its nice value is inside
/// RLIMIT_NICE (20 - limit); a distribution whose limit is 0 would leave the
/// worker stuck at idle, answering queries behind every busy process. So
/// demote only when the way back is known to work.
fn can_return() -> bool {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes one rlimit through a valid pointer.
    if unsafe { libc::getrlimit(libc::RLIMIT_NICE, &mut lim) } != 0 {
        return false;
    }
    // SAFETY: getpriority has no memory effects. The calling thread's nice
    // is 0 unless something lowered it; errno is not needed to tell -1 apart
    // because -1 makes the bound stricter, never looser.
    let nice = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
    lim.rlim_cur >= (20 - nice) as libc::rlim_t
}

fn set_cpu(policy: libc::c_int) -> bool {
    let param = libc::sched_param { sched_priority: 0 };
    // SAFETY: pid 0 is the calling thread; param is a valid sched_param.
    unsafe { libc::sched_setscheduler(0, policy, &param) == 0 }
}

fn set_io(prio: libc::c_int) -> bool {
    // SAFETY: ioprio_set takes three integers; 0 is the calling thread.
    unsafe { libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, prio) == 0 }
}

/// On battery: a battery is present and no charger (mains or USB-C) is
/// online. A machine with no battery is never on it.
pub fn on_battery() -> bool {
    on_battery_in(std::path::Path::new("/sys/class/power_supply"))
}

fn on_battery_in(dir: &std::path::Path) -> bool {
    let read = |p: std::path::PathBuf| std::fs::read_to_string(p).unwrap_or_default();
    let mut battery = false;
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        match read(e.path().join("type")).trim() {
            "Battery" => battery = true,
            "Mains" | "USB" if read(e.path().join("online")).trim() == "1" => return false,
            _ => {}
        }
    }
    battery
}

/// Share of the device's time background indexing may take: `share`, or
/// half of it on battery. Rechecks the power supply every 10 s.
pub struct Pace {
    share: f64,
    battery: bool,
    checked: Option<std::time::Instant>,
}

impl Pace {
    pub fn new(share: f64) -> Self {
        Self {
            share,
            battery: false,
            checked: None,
        }
    }

    pub fn set(&mut self, share: f64) {
        self.share = share;
    }

    pub fn share(&mut self) -> f64 {
        if self
            .checked
            .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(10))
        {
            self.battery = on_battery();
            self.checked = Some(std::time::Instant::now());
        }
        if self.battery {
            self.share / 2.0
        } else {
            self.share
        }
    }
}

static WARNED: AtomicBool = AtomicBool::new(false);

/// The calling thread at idle priority until dropped.
pub struct Idle {
    cpu: bool,
    io: bool,
}

impl Idle {
    pub fn enter() -> Self {
        let cpu = can_return() && set_cpu(libc::SCHED_IDLE);
        let io = set_io(IOPRIO_CLASS_IDLE << IOPRIO_CLASS_SHIFT);
        if !cpu && !WARNED.swap(true, Ordering::Relaxed) {
            eprintln!(
                "indexing at normal CPU priority: RLIMIT_NICE would not allow returning from SCHED_IDLE"
            );
        }
        Self { cpu, io }
    }

    /// Back to normal for the length of `f` (a query answered mid-index).
    pub fn normal<T>(&self, f: impl FnOnce() -> T) -> T {
        self.restore();
        let out = f();
        if self.cpu {
            set_cpu(libc::SCHED_IDLE);
        }
        if self.io {
            set_io(IOPRIO_CLASS_IDLE << IOPRIO_CLASS_SHIFT);
        }
        out
    }

    fn restore(&self) {
        if self.cpu && !set_cpu(libc::SCHED_OTHER) {
            eprintln!("could not leave SCHED_IDLE");
        }
        if self.io {
            set_io(IOPRIO_DEFAULT);
        }
    }
}

impl Drop for Idle {
    fn drop(&mut self) {
        self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> libc::c_int {
        // SAFETY: pid 0 is the calling thread.
        unsafe { libc::sched_getscheduler(0) }
    }

    #[test]
    fn battery_only_without_a_charger_online() {
        let dir = std::env::temp_dir().join(format!("vinosearch-psy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let supply = |name: &str, kind: &str, online: Option<&str>| {
            let d = dir.join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("type"), format!("{kind}\n")).unwrap();
            if let Some(o) = online {
                std::fs::write(d.join("online"), format!("{o}\n")).unwrap();
            }
        };
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!on_battery_in(&dir));
        supply("BAT0", "Battery", None);
        supply("ADP1", "Mains", Some("0"));
        supply("ucsi", "USB", Some("0"));
        assert!(on_battery_in(&dir));
        supply("ucsi", "USB", Some("1"));
        assert!(!on_battery_in(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn idle_is_entered_and_left() {
        // Its own thread, so the test harness's thread is never demoted.
        std::thread::spawn(|| {
            assert_eq!(policy(), libc::SCHED_OTHER);
            {
                let idle = Idle::enter();
                if idle.cpu {
                    assert_eq!(policy(), libc::SCHED_IDLE);
                    idle.normal(|| assert_eq!(policy(), libc::SCHED_OTHER));
                    assert_eq!(policy(), libc::SCHED_IDLE);
                }
            }
            assert_eq!(policy(), libc::SCHED_OTHER);
        })
        .join()
        .unwrap();
    }
}
