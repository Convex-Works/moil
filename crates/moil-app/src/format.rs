//! Numbers and times as the owner reads them.

use std::time::{Duration, SystemTime};

/// A download size in decimal units, as Finder shows them: `483.5 MB`.
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if bytes < 1000 {
        return format!("{bytes} bytes");
    }
    let mut value = bytes as f64 / 1000.0;
    let mut unit = 0;
    while value >= 999.95 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// An amount of memory in binary units, as manifests usually mean it:
/// `8 GiB`, `1.5 GiB`.
pub fn memory(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    let rounded = format!("{value:.1}");
    let rounded = rounded.strip_suffix(".0").unwrap_or(&rounded);
    format!("{rounded} {}", UNITS[unit])
}

/// A job's progress as a whole percentage, rounded down so that it never
/// reads 100% before the job is done. `None` for a meaningless fraction.
pub fn percent(fraction: f64) -> Option<u8> {
    fraction
        .is_finite()
        .then(|| (fraction.clamp(0.0, 1.0) * 100.0).floor() as u8)
}

/// A percentage padded with figure spaces to the width of `100%`:
/// `\u{2007}\u{2007}7%`, `\u{2007}42%`. In a font with tabular digits,
/// where a figure space is as wide as a digit, its width never changes.
pub fn fixed_percent(percent: u8) -> String {
    format!("{percent:\u{2007}>3}%")
}

const MINUTE: u64 = 60;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

/// How long ago `then` was, coarsely: `just now`, `5 minutes ago`,
/// `3 weeks ago`. A time in the future (a skewed clock) is `just now`.
pub fn ago(then: SystemTime, now: SystemTime) -> String {
    let secs = now.duration_since(then).unwrap_or_default().as_secs();
    let (count, unit) = match secs {
        s if s < MINUTE => return "just now".to_owned(),
        s if s < HOUR => (s / MINUTE, "minute"),
        s if s < DAY => (s / HOUR, "hour"),
        s if s < 2 * DAY => return "yesterday".to_owned(),
        s if s < 14 * DAY => (s / DAY, "day"),
        s if s < 60 * DAY => (s / (7 * DAY), "week"),
        s if s < 365 * DAY => (s / (30 * DAY), "month"),
        s => (s / (365 * DAY), "year"),
    };
    format!("{count} {unit}{} ago", if count == 1 { "" } else { "s" })
}

/// How long until [`ago`] may read differently: it only changes a whole
/// number of minutes after `then`.
pub fn ago_changes_in(then: SystemTime, now: SystemTime) -> Duration {
    let elapsed = now.duration_since(then).unwrap_or_default();
    let minute = Duration::from_secs(MINUTE);
    let into_minute = elapsed.as_nanos() % minute.as_nanos();
    minute - Duration::from_nanos(into_minute as u64)
}

/// A short wait, rounded up so that it never reads zero before it's over:
/// `12s`, `2 min`.
pub fn wait(duration: Duration) -> String {
    let secs = duration.as_secs() + u64::from(duration.subsec_nanos() > 0);
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{} min", secs.div_ceil(60))
    }
}

/// `n` things, with the noun pluralized by adding `s`: `1 package`,
/// `3 packages`.
pub fn count(n: usize, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

/// Text from a service, made fit for a menu or a window title: one line of
/// at most `max` characters, without the characters that are invisible or
/// reorder text (see [`crate::page`]).
pub fn one_line(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut chars = text
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .filter(|&c| !crate::page::is_hidden(c));
    for c in chars.by_ref().take(max) {
        out.push(c);
    }
    if chars.next().is_some() {
        out.truncate(out.trim_end().len());
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_like_finder() {
        assert_eq!(size(5), "5 bytes");
        assert_eq!(size(2370), "2.4 KB");
        assert_eq!(size(483_546_902), "483.5 MB");
        assert_eq!(size(999_999), "1.0 MB", "never `1000.0 KB`");
        assert_eq!(size(532_462_568), "532.5 MB");
        assert_eq!(size(u64::MAX), "18446.7 PB");
    }

    #[test]
    fn memory_uses_binary_units_without_needless_decimals() {
        assert_eq!(memory(8 << 30), "8 GiB");
        assert_eq!(memory(6_442_450_944), "6 GiB");
        assert_eq!(memory(1536 << 20), "1.5 GiB");
        assert_eq!(memory(512 << 20), "512 MiB");
        assert_eq!(memory(100), "100 bytes");
    }

    #[test]
    fn percent_never_claims_completion_early() {
        assert_eq!(percent(0.42), Some(42));
        assert_eq!(percent(0.999), Some(99));
        assert_eq!(percent(1.0), Some(100));
        assert_eq!(percent(-0.5), Some(0));
        assert_eq!(percent(7.0), Some(100));
        assert_eq!(percent(f64::NAN), None);
    }

    #[test]
    fn ago_is_coarse_and_tolerates_skew() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        let before = |secs| now - Duration::from_secs(secs);
        assert_eq!(ago(before(5), now), "just now");
        assert_eq!(ago(now + Duration::from_secs(90), now), "just now");
        assert_eq!(ago(before(60), now), "1 minute ago");
        assert_eq!(ago(before(5 * 60), now), "5 minutes ago");
        assert_eq!(ago(before(3 * 3600), now), "3 hours ago");
        assert_eq!(ago(before(30 * 3600), now), "yesterday");
        assert_eq!(ago(before(5 * 86400), now), "5 days ago");
        assert_eq!(ago(before(21 * 86400), now), "3 weeks ago");
        assert_eq!(ago(before(100 * 86400), now), "3 months ago");
        assert_eq!(ago(before(800 * 86400), now), "2 years ago");
    }

    #[test]
    fn fixed_percentages_are_all_as_wide_as_100() {
        assert_eq!(fixed_percent(7), "\u{2007}\u{2007}7%");
        assert_eq!(fixed_percent(42), "\u{2007}42%");
        assert_eq!(fixed_percent(100), "100%");
        for percent in 0..=100 {
            let text = fixed_percent(percent);
            assert_eq!(text.chars().count(), 4, "{text:?}");
            let digits: String = text.chars().filter(char::is_ascii_digit).collect();
            assert_eq!(
                digits,
                percent.to_string(),
                "padded with figure spaces only"
            );
            assert!(
                text.trim_start_matches('\u{2007}')
                    .starts_with(|c: char| c.is_ascii_digit())
            );
        }
    }

    #[test]
    fn ago_is_refreshed_on_the_minutes_after_the_moment() {
        let then = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        let changes_in = |elapsed: Duration| ago_changes_in(then, then + elapsed);
        assert_eq!(changes_in(Duration::ZERO), Duration::from_secs(60));
        assert_eq!(
            changes_in(Duration::from_millis(59_500)),
            Duration::from_millis(500)
        );
        assert_eq!(changes_in(Duration::from_secs(61)), Duration::from_secs(59));
        assert_eq!(
            changes_in(Duration::from_secs(3 * 3600 + 10)),
            Duration::from_secs(50)
        );
        assert_eq!(
            ago_changes_in(then + Duration::from_secs(90), then),
            Duration::from_secs(60),
            "a skewed clock still refreshes"
        );
        // Every change of `ago` falls on one of those refreshes.
        let mut at = Duration::ZERO;
        while at < Duration::from_secs(3 * DAY) {
            let next = at + changes_in(at);
            let later = |d: Duration| ago(then, then + d);
            assert_eq!(
                later(at),
                later(next - Duration::from_millis(1)),
                "at {at:?}"
            );
            at = next;
        }
    }

    #[test]
    fn waits_round_up() {
        assert_eq!(wait(Duration::from_millis(11_200)), "12s");
        assert_eq!(wait(Duration::from_millis(1)), "1s");
        assert_eq!(wait(Duration::from_secs(60)), "1 min");
        assert_eq!(wait(Duration::from_secs(61)), "2 min");
    }
}
