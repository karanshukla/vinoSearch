use std::path::{Path, PathBuf};

use crate::control::Partial;

/// `/home/u/x` as `~/x`, the way the config file writes it.
pub(super) fn tilde(path: &Path) -> String {
    match home().and_then(|h| path.strip_prefix(&h).ok().map(Path::to_owned)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

pub(super) fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Unix seconds as "just now", "12 min ago", "3 h ago" or "2 days ago".
pub(super) fn ago(at: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    ago_from(at, now)
}

pub(super) fn ago_from(at: u64, now: u64) -> String {
    let s = now.saturating_sub(at);
    match s {
        0..60 => "just now".to_owned(),
        60..3600 => format!("{} min ago", s / 60),
        3600..86400 => format!("{} h ago", s / 3600),
        _ if s < 2 * 86400 => "1 day ago".to_owned(),
        _ => format!("{} days ago", s / 86400),
    }
}

/// 41924 as "41,924".
pub(super) fn count(n: i64) -> String {
    let digits = vinosearch_shared::numbers::thousands(n.unsigned_abs());
    if n < 0 { format!("-{digits}") } else { digits }
}

/// `name` in about `room` letters: the middle cut out, as the end of a file
/// name (the track, the chapter, the extension) is what tells it from the
/// next one.
pub(super) fn middle_cut(name: &str, room: usize) -> String {
    let letters: Vec<char> = name.chars().collect();
    let room = room.max(12);
    if letters.len() <= room {
        return name.to_owned();
    }
    let tail = (room - 1) * 3 / 5;
    let head = room - 1 - tail;
    let mut cut: String = letters[..head].iter().collect();
    cut.push('…');
    cut.extend(&letters[letters.len() - tail..]);
    cut
}

/// A document indexed in part: its file name and how far it has got,
/// "book.pdf" and "120 of 640 pages".
pub(super) fn partial_line(d: &Partial) -> (String, String) {
    let name = Path::new(&d.path)
        .file_name()
        .map_or_else(|| d.path.clone(), |n| n.to_string_lossy().into_owned());
    let unit = if d.total == 1 {
        d.unit.clone()
    } else {
        format!("{}s", d.unit)
    };
    let progress = format!(
        "{} of {} {unit}",
        count(d.done as i64),
        count(d.total as i64)
    );
    (name, progress)
}

/// Decimal units, as the README and `fetch-model` give sizes.
pub(super) fn size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = "B";
    for u in UNITS {
        if value < 1000.0 {
            break;
        }
        value /= 1000.0;
        unit = u;
    }
    if value < 10.0 {
        format!("{value:.1} {unit}")
    } else {
        format!("{value:.0} {unit}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_name_keeps_both_ends() {
        assert_eq!(middle_cut("notes.md", 20), "notes.md");
        let cut = middle_cut(
            "Windwaker - HYPERVIOLENCE - 06 Fractured State of Mind.mp3",
            30,
        );
        assert_eq!(cut.chars().count(), 30);
        assert!(
            cut.starts_with("Windwaker - ") && cut.ends_with("e of Mind.mp3"),
            "{cut}"
        );
        // Never so little room that nothing of either end is left.
        assert_eq!(
            middle_cut("a-very-long-file-name.txt", 0).chars().count(),
            12
        );
    }

    #[test]
    fn counts_get_thousands_separators() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1000), "1,000");
        assert_eq!(count(41924), "41,924");
        assert_eq!(count(1234567), "1,234,567");
        assert_eq!(count(-1234), "-1,234");
    }

    #[test]
    fn times_read_as_ago() {
        let now = 1_000_000;
        assert_eq!(ago_from(now - 5, now), "just now");
        assert_eq!(ago_from(now + 5, now), "just now");
        assert_eq!(ago_from(now - 12 * 60, now), "12 min ago");
        assert_eq!(ago_from(now - 3 * 3600 - 59, now), "3 h ago");
        assert_eq!(ago_from(now - 30 * 3600, now), "1 day ago");
        assert_eq!(ago_from(now - 5 * 86400, now), "5 days ago");
    }

    #[test]
    fn sizes_use_decimal_units() {
        assert_eq!(size(512), "512 B");
        assert_eq!(size(176_577_856), "177 MB");
        assert_eq!(size(307_607_341), "308 MB");
        assert_eq!(size(1_500_000_000), "1.5 GB");
    }
}
