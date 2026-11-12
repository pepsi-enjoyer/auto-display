//! Background mode: a hidden window that hears display and device broadcasts and re-checks the
//! layout whenever the set of connected monitors changes (including at sign-in).

use std::cell::RefCell;
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, ERROR_INVALID_PARAMETER, GetLastError, HWND, LPARAM, LRESULT, WAIT_OBJECT_0, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::{CreateMutexW, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, FindWindowW, GetMessageW, GetWindowThreadProcessId, KillTimer, MSG,
    PostMessageW, PostQuitMessage, RegisterClassW, SetTimer, TranslateMessage, WM_CLOSE, WM_DESTROY,
    WM_DEVICECHANGE, WM_DISPLAYCHANGE, WM_POWERBROADCAST, WM_TIMER, WNDCLASSW, WS_OVERLAPPED,
};

use crate::{display, fix, install, log, update};

const CLASS_NAME: &str = "auto-display-watcher";
const MUTEX_NAME: &str = "Local\\auto-display";

const TIMER_CHECK: usize = 1;
const TIMER_POLL: usize = 2;
/// Wait for Windows to settle after a change before looking.
const SETTLE_MS: u32 = 2000;
/// Windows sometimes applies duplicate a few seconds after a monitor appears, so look a few times.
const RECHECKS: u32 = 4;
const RECHECK_MS: u32 = 3000;
/// Safety net in case a broadcast is missed.
const POLL_MS: u32 = 30_000;

#[derive(Default)]
struct State {
    monitors: Option<Vec<String>>,
    checks_left: u32,
}

impl State {
    fn observe(&mut self, monitors: Vec<String>) -> bool {
        if self.monitors.as_ref() != Some(&monitors) {
            self.monitors = Some(monitors);
            self.checks_left = RECHECKS;
        }
        if self.checks_left == 0 {
            return false;
        }
        self.checks_left -= 1;
        true
    }
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn run() -> Result<(), String> {
    run_with_initial_check(true)
}

/// Wait for the old watcher to release its mutex, then resume without changing the display layout.
pub fn resume_after_update(parent: u32) -> Result<(), String> {
    if parent != 0 {
        let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, parent) };
        if process.is_null() {
            if unsafe { GetLastError() } != ERROR_INVALID_PARAMETER {
                return Err("could not wait for the previous watcher".into());
            }
        } else {
            let status = unsafe { WaitForSingleObject(process, 60_000) };
            unsafe { CloseHandle(process) };
            if status != WAIT_OBJECT_0 {
                return Err("previous watcher did not exit in time".into());
            }
        }
    }
    run_with_initial_check(false)
}

fn run_with_initial_check(initial_check: bool) -> Result<(), String> {
    let mutex_name = wide(MUTEX_NAME);
    let mutex = unsafe { CreateMutexW(null(), 0, mutex_name.as_ptr()) };
    if mutex.is_null() {
        return Err("could not create the single-instance mutex".into());
    }
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe { CloseHandle(mutex) };
        return Ok(()); // Already running.
    }

    if !initial_check {
        let monitors = display::connected_monitors()?;
        STATE.with(|s| *s.borrow_mut() = State { monitors: Some(monitors), checks_left: 0 });
    }
    update::cleanup();

    let class = wide(CLASS_NAME);
    unsafe {
        let instance = GetModuleHandleW(null());
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: null_mut(),
            hCursor: null_mut(),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: class.as_ptr(),
        };
        if RegisterClassW(&wc) == 0 {
            return Err("RegisterClassW failed".into());
        }
        // A real top-level window (never shown): message-only windows do not get broadcasts.
        let hwnd = CreateWindowExW(
            0, class.as_ptr(), class.as_ptr(), WS_OVERLAPPED, 0, 0, 0, 0, null_mut(), null_mut(), instance, null(),
        );
        if hwnd.is_null() {
            return Err("CreateWindowExW failed".into());
        }
        log::line(&format!("Watching for display changes ({}).", update::CURRENT));
        update::start();
        SetTimer(hwnd, TIMER_CHECK, SETTLE_MS, None);
        SetTimer(hwnd, TIMER_POLL, POLL_MS, None);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    log::line("Stopped.");
    unsafe { CloseHandle(mutex) };
    Ok(())
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_DISPLAYCHANGE | WM_DEVICECHANGE | WM_POWERBROADCAST => {
                SetTimer(hwnd, TIMER_CHECK, SETTLE_MS, None);
            }
            WM_TIMER if wparam == TIMER_POLL => {
                check(hwnd);
                return 0;
            }
            WM_TIMER if wparam == TIMER_CHECK => {
                KillTimer(hwnd, TIMER_CHECK);
                check(hwnd);
                return 0;
            }
            WM_CLOSE => {
                DestroyWindow(hwnd);
                return 0;
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                return 0;
            }
            _ => {}
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

/// Only acts in the moments after the connected monitors change, so choosing duplicate yourself
/// later (for example to present) is left alone.
fn check(hwnd: HWND) {
    let monitors = match display::connected_monitors() {
        Ok(m) => m,
        Err(e) => return log::line(&format!("error: {e}")),
    };
    let act = STATE.with(|s| {
        let mut s = s.borrow_mut();
        if s.monitors.as_ref().is_some_and(|old| old != &monitors) {
            log::line(&format!("Connected displays changed ({} now).", monitors.len()));
        }
        if !s.observe(monitors) {
            return false;
        }
        if s.checks_left > 0 {
            unsafe { SetTimer(hwnd, TIMER_CHECK, RECHECK_MS, None) };
        }
        true
    });
    if act {
        match display::active() {
            Ok(cfg) if display::is_duplicate(&cfg.paths) => start_setup(),
            Ok(_) => {}
            Err(e) => log::line(&format!("error: {e}")),
        }
    }
}

/// The fix runs in its own process so it can show the on-screen card. If it cannot be started, fix
/// the displays here without the card.
fn start_setup() {
    let started = std::env::current_exe()
        .map_err(|e| e.to_string())
        .and_then(|exe| install::start_detached(&format!("\"{}\" setup", exe.display())));
    if let Err(e) = started {
        log::line(&format!("Could not start the setup card ({e}); fixing without it."));
        if let Err(e) = fix::if_duplicate() {
            log::line(&format!("error: {e}"));
        }
    }
}

/// Asks a running watcher to exit and waits for it. Returns true if one was running.
pub fn stop_running() -> bool {
    let class = wide(CLASS_NAME);
    let hwnd = unsafe { FindWindowW(class.as_ptr(), null()) };
    if hwnd.is_null() {
        return false;
    }
    unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) };
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !unsafe { FindWindowW(class.as_ptr(), null()) }.is_null() {
        std::thread::sleep(Duration::from_millis(100));
    }
    // Give the process a moment to release its executable after the window goes.
    std::thread::sleep(Duration::from_millis(300));
    true
}

pub fn is_running() -> bool {
    let class = wide(CLASS_NAME);
    !unsafe { FindWindowW(class.as_ptr(), null()) }.is_null()
}

fn running_window() -> Option<(HWND, u32)> {
    let class = wide(CLASS_NAME);
    let hwnd = unsafe { FindWindowW(class.as_ptr(), null()) };
    if hwnd.is_null() { return None; }
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    (pid != 0).then_some((hwnd, pid))
}

pub fn running_pid() -> Option<u32> {
    running_window().map(|(_, pid)| pid)
}

pub fn request_stop(expected_pid: u32) {
    if let Some((hwnd, pid)) = running_window() && pid == expected_pid {
        unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_restart_preserves_layout_until_monitors_change() {
        let monitors = vec!["laptop".into(), "external".into()];
        let mut resumed = State { monitors: Some(monitors.clone()), checks_left: 0 };
        // An update must leave a user's duplicate/presentation mode alone.
        assert!(!resumed.observe(monitors.clone()));
        assert!(!resumed.observe(monitors.clone()));
        assert!(resumed.observe(vec!["laptop".into(), "new external".into()]));
        assert_eq!(resumed.checks_left, RECHECKS - 1);
        let mut signed_in = State::default();
        assert!(signed_in.observe(monitors.clone()));
        for _ in 1..RECHECKS { assert!(signed_in.observe(monitors.clone())); }
        assert!(!signed_in.observe(monitors));
    }
}
