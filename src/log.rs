//! Logging to the journal: stderr when run by systemd, syslog (/dev/log) otherwise.
//!
//! Everything logged may contain data from X or /proc, so control characters are
//! neutralised (no forged log lines) and lines are length-capped.
//! Secrets (X cookies) must never be passed here.

use std::os::unix::net::UnixDatagram;
use std::sync::atomic::{AtomicBool, Ordering};

static VERBOSE: AtomicBool = AtomicBool::new(false);
const MAX_LEN: usize = 512;

pub fn set_verbose(v: bool) {
    VERBOSE.store(v, Ordering::Relaxed);
}

/// Replace control characters and cap the length of untrusted text.
pub fn clean(s: &str) -> String {
    s.chars()
        .take(MAX_LEN)
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

pub fn log(msg: &str) {
    let msg = clean(msg);
    let under_systemd = std::env::var_os("JOURNAL_STREAM").is_some();
    if under_systemd || VERBOSE.load(Ordering::Relaxed) {
        eprintln!("{msg}");
    }
    if !under_systemd {
        // <14> = facility user, severity info.
        if let Ok(sock) = UnixDatagram::unbound() {
            let line = format!("<14>kvm-autofit[{}]: {msg}", std::process::id());
            let _ = sock.send_to(line.as_bytes(), "/dev/log");
        }
    }
}

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => { $crate::log::log(&format!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    use super::clean;

    #[test]
    fn strips_control_chars_and_caps_length() {
        assert_eq!(clean("a\nb\x1b[31mc"), "a?b?[31mc");
        assert_eq!(clean(&"x".repeat(2000)).len(), 512);
    }
}
