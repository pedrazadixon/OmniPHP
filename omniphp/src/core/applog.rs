//! The app's own log, `logs\omniphp.log`.
//!
//! A release build has no console, so anything written to standard error is
//! lost. The few things worth keeping (a panic, a tray icon that could not be
//! created) are appended here instead.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use super::paths::Paths;

static FILE: OnceLock<PathBuf> = OnceLock::new();

/// Chooses the log file. Until this is called, [`write`] does nothing.
pub fn init(paths: &Paths) {
    let _ = FILE.set(paths.logs().join("omniphp.log"));
}

/// Appends one line. Failing to log is never an error of its own.
pub fn write(message: &str) {
    if let Some(file) = FILE.get() {
        let now = time::OffsetDateTime::now_utc();
        let stamp = format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}Z",
            now.year(),
            u8::from(now.month()),
            now.day(),
            now.hour(),
            now.minute(),
            now.second()
        );
        let _ = append(file, &format!("{stamp} {message}"));
    }
}

fn append(file: &Path, line: &str) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut log = std::fs::OpenOptions::new().create(true).append(true).open(file)?;
    writeln!(log, "{line}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_added_to_what_is_already_there() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("logs").join("omniphp.log");
        append(&file, "first").unwrap();
        append(&file, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "first\nsecond\n");
    }
}
