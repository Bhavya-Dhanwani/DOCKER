//! Pure display helpers.

pub fn fmt_bytes(b: u64) -> String {
    const KB: f64 = 1024.0;
    let f = b as f64;
    if f >= KB * KB * KB {
        format!("{:.2} GB", f / (KB * KB * KB))
    } else if f >= KB * KB {
        format!("{:.1} MB", f / (KB * KB))
    } else if f >= KB {
        format!("{:.0} KB", f / KB)
    } else {
        format!("{b} B")
    }
}

pub fn fmt_duration(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m {}s", secs / 60, secs % 60),
        3600..=86399 => format!("{}h {}m", secs / 3600, secs / 60 % 60),
        _ => format!("{}d {}h", secs / 86400, secs / 3600 % 24),
    }
}

/// "HH:MM:SS" (as printed by `docker ps`) to seconds.
pub fn parse_hms(s: &str) -> Option<u64> {
    let mut it = s.split(':');
    let (h, m, sec) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() {
        return None;
    }
    Some(h.parse::<u64>().ok()? * 3600 + m.parse::<u64>().ok()? * 60 + sec.parse::<u64>().ok()?)
}

/// "4.0MB" / "1.5GB" / "-" (as printed by the CLI) to bytes.
pub fn parse_size(s: &str) -> u64 {
    let s = s.trim();
    for (suffix, mul) in [("GB", 1024.0 * 1024.0 * 1024.0), ("MB", 1024.0 * 1024.0), ("KB", 1024.0), ("B", 1.0)] {
        if let Some(n) = s.strip_suffix(suffix) {
            return n.trim().parse::<f64>().map_or(0, |v| (v * mul) as u64);
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(2048), "2 KB");
        assert_eq!(fmt_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(fmt_bytes(3 * 1024 * 1024 * 1024), "3.00 GB");
    }

    #[test]
    fn durations() {
        assert_eq!(fmt_duration(5), "5s");
        assert_eq!(fmt_duration(125), "2m 5s");
        assert_eq!(fmt_duration(7384), "2h 3m");
        assert_eq!(fmt_duration(90_000), "1d 1h");
    }

    #[test]
    fn hms_and_sizes() {
        assert_eq!(parse_hms("01:02:03"), Some(3723));
        assert_eq!(parse_hms("1:2"), None);
        assert_eq!(parse_size("4.0MB"), 4 * 1024 * 1024);
        assert_eq!(parse_size("1.5GB"), 1610612736);
        assert_eq!(parse_size("-"), 0);
    }
}
