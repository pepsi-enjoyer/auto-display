use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

const MAX_LOG_BYTES: u64 = 1024 * 1024;

pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("auto-display")
}

pub fn path() -> PathBuf {
    data_dir().join("auto-display.log")
}

/// Writes a timestamped line to the console (when there is one) and to the log file.
pub fn line(msg: &str) {
    let mut t: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    let text = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} {msg}\n",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    );
    print!("{text}");

    let path = path();
    let _ = fs::create_dir_all(data_dir());
    let too_big = fs::metadata(&path).map(|m| m.len() > MAX_LOG_BYTES).unwrap_or(false);
    let file = OpenOptions::new().create(true).write(true).append(!too_big).truncate(too_big).open(&path);
    if let Ok(mut f) = file {
        let _ = f.write_all(text.as_bytes());
    }
}
