//! Quantities as people write them, for messages.

use std::time::Duration;

/// A duration: `1 h`, `90 s`, `1500 ms`.
pub(crate) fn duration(duration: Duration) -> String {
    let ms = duration.as_millis();
    match ms {
        0 => "0 s".into(),
        ms if ms % 3_600_000 == 0 => format!("{} h", ms / 3_600_000),
        ms if ms % 60_000 == 0 => format!("{} min", ms / 60_000),
        ms if ms % 1_000 == 0 => format!("{} s", ms / 1_000),
        ms => format!("{ms} ms"),
    }
}

/// A size: `900 bytes`, `1.5 KiB`, `64 GiB`.
pub(crate) fn bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    let unit = UNITS[unit];
    if value >= 100.0 || value.fract() == 0.0 {
        format!("{value:.0} {unit}")
    } else {
        format!("{value:.1} {unit}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(bytes(900), "900 bytes");
        assert_eq!(bytes(1536), "1.5 KiB");
        assert_eq!(bytes(64 << 30), "64 GiB");
        assert_eq!(bytes(2_684_354_560), "2.5 GiB");
        assert_eq!(bytes(200 << 20), "200 MiB");
    }
}
