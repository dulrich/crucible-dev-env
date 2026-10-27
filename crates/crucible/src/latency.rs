//! Overlay first-usable-frame latency: the compositor exports `CDE_SPAWN_EPOCH_MS` to overlay
//! children; after its first populated frame is flushed, an overlay appends one line to
//! [`LOG_PATH`]. Without the variable (CLI use outside CDE) nothing is written, and every IO
//! error is ignored so the TUI is never disturbed.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Environment variable the compositor sets on overlay spawns (wall-clock ms since the epoch).
const SPAWN_EPOCH_ENV: &str = "CDE_SPAWN_EPOCH_MS";
/// Directory the latency log lives in.
const LOG_DIR: &str = "/tmp/cde";
/// File the latency lines are appended to.
const LOG_PATH: &str = "/tmp/cde/overlay-latency.log";

/// Parses the spawn epoch (ms); anything but a plain unsigned integer is rejected.
fn parse_spawn_epoch(raw: Option<&str>) -> Option<u128> {
    raw?.parse().ok()
}

/// The log line: `<epoch_ms_now> overlay=<name> spawn_to_usable_ms=<n>` (n is 0 when the clock
/// stepped backwards).
fn format_line(overlay: &str, spawn_epoch_ms: u128, now_ms: u128) -> String {
    format!(
        "{now_ms} overlay={overlay} spawn_to_usable_ms={}",
        now_ms.saturating_sub(spawn_epoch_ms)
    )
}

/// Call right after the overlay's frame is flushed; logs only the first call per process, and
/// only when `CDE_SPAWN_EPOCH_MS` parses. Callers reach it after candidate discovery, so the
/// first frame already shows usable content.
pub fn first_frame(overlay: &str) {
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Ordering::Relaxed) {
        return;
    }
    let Some(spawn) = parse_spawn_epoch(std::env::var(SPAWN_EPOCH_ENV).ok().as_deref()) else {
        return;
    };
    let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return;
    };
    let line = format_line(overlay, spawn, now.as_millis());
    let _ = std::fs::create_dir_all(LOG_DIR);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(LOG_PATH)
    {
        let _ = writeln!(f, "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_has_the_documented_shape() {
        assert_eq!(
            format_line("pick", 1000, 1234),
            "1234 overlay=pick spawn_to_usable_ms=234"
        );
    }

    #[test]
    fn backwards_clock_saturates_to_zero() {
        assert_eq!(
            format_line("keys", 2000, 1000),
            "1000 overlay=keys spawn_to_usable_ms=0"
        );
    }

    #[test]
    fn spawn_epoch_parse_rejects_non_integers() {
        assert_eq!(
            parse_spawn_epoch(Some("1700000000123")),
            Some(1_700_000_000_123)
        );
        assert_eq!(parse_spawn_epoch(None), None);
        for bad in ["", "abc", "-5", "1.5", "12x", " 5"] {
            assert_eq!(parse_spawn_epoch(Some(bad)), None, "{bad:?}");
        }
    }
}
