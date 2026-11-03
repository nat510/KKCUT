#![windows_subsystem = "windows"]

mod autostart;
mod config;
mod kkbox;
mod net;
mod tray;
mod ui;

/// A NUL-terminated UTF-16 string for Windows.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Appends one line to %APPDATA%\KKCUT\kkcut.log. Failures are ignored:
/// logging must never stop KKCUT.
pub fn log(text: &str) {
    use std::io::Write;
    let Some(path) = config::data_dir().map(|d| d.join("kkcut.log")) else {
        return;
    };
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 256 * 1024) {
        let _ = std::fs::rename(&path, path.with_file_name("kkcut.old.log"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let text = text.replace(['\r', '\n'], " ");
        let _ = writeln!(f, "{} {text}", kkbox::now());
    }
}

fn main() {
    // Autostart starts KKCUT in the notification area, without its window.
    let hidden = std::env::args().any(|a| a == "--tray");
    if tray::single_instance(hidden) {
        ui::run(hidden);
    }
}
