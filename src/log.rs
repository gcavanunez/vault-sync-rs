//! Logfmt-style lines on stderr, captured by journald or launchd.

use crate::clock;
use std::io::Write;

fn line(level: &str, message: &str, detail: Option<&str>) {
    let mut text = format!(
        "timestamp={} level={level} message={message:?}",
        clock::now_iso()
    );

    if let Some(detail) = detail.filter(|detail| !detail.is_empty()) {
        text.push_str(&format!(" detail={detail:?}"));
    }

    let _ = writeln!(std::io::stderr().lock(), "{text}");
}

pub fn info(message: &str) {
    line("INFO", message, None);
}

pub fn warn(message: &str, detail: &str) {
    line("WARN", message, Some(detail));
}
