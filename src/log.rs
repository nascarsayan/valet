//! Timestamped stderr logging: `2026-11-05T12:00:00Z INFO message`.
//! Use for progress in long-running commands; plain results go to stdout.

use std::time::{SystemTime, UNIX_EPOCH};

/// Current UTC time as ISO-8601 (`YYYY-MM-DDTHH:MM:SSZ`).
pub fn now_iso() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    iso_from_unix(secs as i64)
}

/// Format UNIX seconds as ISO-8601 UTC.
pub fn iso_from_unix(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

pub fn emit(level: &str, msg: std::fmt::Arguments) {
    eprintln!("{} {level} {msg}", now_iso());
}

#[macro_export]
macro_rules! info { ($($t:tt)*) => { $crate::log::emit("INFO", format_args!($($t)*)) } }
#[macro_export]
macro_rules! warn { ($($t:tt)*) => { $crate::log::emit("WARN", format_args!($($t)*)) } }
#[macro_export]
macro_rules! error { ($($t:tt)*) => { $crate::log::emit("ERROR", format_args!($($t)*)) } }

#[cfg(test)]
mod tests {
    use super::iso_from_unix;

    #[test]
    fn known_instants() {
        assert_eq!(iso_from_unix(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_from_unix(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(iso_from_unix(1_798_761_599), "2026-12-31T23:59:59Z");
    }
}
