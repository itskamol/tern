//! How the Files tab words things: a size, a date, a share done, and where an arrow key lands.

use std::time::SystemTime;

/// `1536` → `1.5 KB`; plain bytes below 1 KB.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit + 1 < UNITS.len() && format!("{value:.0}") == "1024" {
        unit += 1;
        value /= 1024.0;
    }
    if value >= 10.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        let text = format!("{value:.1}");
        if text == "10.0" {
            format!("10 {}", UNITS[unit])
        } else {
            format!("{text} {}", UNITS[unit])
        }
    }
}

/// `Oct  6 12:35` in local time, with the year instead of the clock for older files.
pub fn modified(time: SystemTime, now: SystemTime) -> String {
    use chrono::{DateTime, Local};
    let at = DateTime::<Local>::from(time);
    let recent = now
        .duration_since(time)
        .is_ok_and(|age| age.as_secs() < 180 * 24 * 3600);
    if recent {
        at.format("%b %e %H:%M").to_string()
    } else {
        at.format("%b %e  %Y").to_string()
    }
}

/// The row picked after moving by `delta`: the first when nothing was picked, never past
/// either end, `None` for an empty list.
pub fn step(current: Option<usize>, delta: isize, len: usize) -> Option<usize> {
    let last = len.checked_sub(1)?;
    Some(match current {
        None => 0,
        Some(at) => at.saturating_add_signed(delta).min(last),
    })
}

/// Whole percent, 0 when the size is unknown.
pub fn percent(done: u64, total: u64) -> u64 {
    (done.min(total) * 100).checked_div(total).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn sizes_read_in_the_unit_a_person_would_use() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(10 * 1024 - 1), "10 KB");
        assert_eq!(human_size(10 * 1024), "10 KB");
        assert_eq!(human_size(1024 * 1024 - 1), "1.0 MB");
        assert_eq!(human_size(5 * 1024 * 1024 + 200 * 1024), "5.2 MB");
        assert_eq!(human_size(1024 * 1024 * 1024 - 1), "1.0 GB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn the_arrow_keys_stay_inside_the_list() {
        assert_eq!(step(None, 1, 3), Some(0));
        assert_eq!(step(None, -1, 3), Some(0));
        assert_eq!(step(Some(0), 1, 3), Some(1));
        assert_eq!(step(Some(2), 1, 3), Some(2));
        assert_eq!(step(Some(0), -1, 3), Some(0));
        assert_eq!(step(Some(1), -1, 3), Some(0));
        assert_eq!(step(Some(0), 1, 0), None);
    }

    #[test]
    fn progress_is_a_whole_percent_and_never_over_a_hundred() {
        assert_eq!(percent(0, 200), 0);
        assert_eq!(percent(50, 200), 25);
        assert_eq!(percent(199, 200), 99);
        assert_eq!(percent(300, 200), 100);
        assert_eq!(percent(5, 0), 0);
    }

    #[test]
    fn recent_files_show_the_clock_and_old_ones_the_year() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let recent = modified(now - Duration::from_secs(3600), now);
        let old = modified(now - Duration::from_secs(400 * 24 * 3600), now);
        assert!(recent.contains(':'), "{recent}");
        assert!(!old.contains(':') && old.contains("202"), "{old}");
    }
}
