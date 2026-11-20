//! The on-screen card shown while auto-display sets up the displays.
//!
//! Runs in its own short-lived process (`auto-display`) so the watcher stays a plain Win32
//! message loop. A worker thread does the display changes and publishes progress; the card shows it
//! on every monitor, then lets the user adjust the displays and choose whether to keep the changes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{sleep, spawn};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, FontData, FontDefinitions, FontFamily, FontId, Id, Key, Painter,
    Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Vec2, ViewportBuilder, ViewportCommand, ViewportId, pos2,
    vec2,
};
use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HWND, LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, FindWindowW, GWL_EXSTYLE, GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId,
    LWA_ALPHA, MONITORINFOF_PRIMARY, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER,
    SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPos, WS_EX_LAYERED,
};
use windows_sys::core::BOOL;

use crate::display::{self, Area, Rotation, Screen, Side, Snapshot};
use crate::{config, fix, install, log, watch};

mod diagram;
mod pointer;
use diagram::{constrained_seam, corner_rect, is_clear, nearest_clear_rect, offset_in_mm, offset_in_pixels, physical_rects};
use pointer::{PointerAnchor, PointerSettlement};

const WIDTH: f32 = 584.0;
const HEIGHT: f32 = 414.0;
const PAD: f32 = 24.0;
const KEEP_AFTER: Duration = Duration::from_secs(10);
const DONE_FOR: Duration = Duration::from_secs(3);
const FAILED_FOR: Duration = Duration::from_secs(10);
const FADE_IN: f32 = 0.22;
const FADE_OUT: f32 = 0.25;
const CARD_ALPHA: f32 = 234.0;
const MONITOR_POLL: Duration = Duration::from_millis(250);
const ANIMATION_TICK: Duration = Duration::from_millis(33);
const COUNTDOWN_TICK: Duration = Duration::from_millis(75);
const SNAP_DISTANCE: f32 = 10.0;
const MIN_SEAM: f32 = 8.0;
const BG: Color32 = Color32::from_rgb(0x1b, 0x1c, 0x1f);
const SURFACE: Color32 = Color32::from_rgb(0x15, 0x16, 0x18);
const SCREEN: Color32 = Color32::from_rgb(0x2d, 0x30, 0x36);
const SCREEN_EDGE: Color32 = Color32::from_rgb(0x47, 0x4a, 0x52);
const BUTTON: Color32 = Color32::from_rgb(0x2b, 0x2d, 0x32);
const BUTTON_HOVER: Color32 = Color32::from_rgb(0x36, 0x39, 0x40);
const TEXT: Color32 = Color32::from_rgb(0xf2, 0xf3, 0xf5);
const MUTED: Color32 = Color32::from_rgb(0x93, 0x97, 0xa1);
const ACCENT: Color32 = Color32::from_rgb(0x3d, 0x7e, 0xea);
const ACCENT_HOVER: Color32 = Color32::from_rgb(0x5a, 0x94, 0xf0);
/// The accent, lightened to read as text on the dark card.
const ACCENT_TEXT: Color32 = Color32::from_rgb(0x8a, 0xb4, 0xf8);
const GOOD: Color32 = Color32::from_rgb(0x4c, 0xc3, 0x8a);
const BAD: Color32 = Color32::from_rgb(0xf0, 0x6a, 0x6a);

#[derive(Clone, PartialEq)]
enum Phase {
    Working,
    Prompt,
    Applying,
    Kept,
    Done(String),
    Failed(String),
}

struct Shared {
    phase: Phase,
    since: Instant,
    screens: Vec<Screen>,
    needs_taskbar_refresh: bool,
    pointer_settlement: Option<PointerSettlement>,
}

type State = Arc<Mutex<Shared>>;

fn capture_pointer(state: &State) -> Option<PointerAnchor> {
    PointerAnchor::capture(&state.lock().unwrap().screens)
}

fn finish_display_change(state: &State, ctx: &egui::Context, phase: Phase, pointer: Option<PointerAnchor>) {
    let actual = display::screens().ok();
    if let (Some(pointer), Some(screens)) = (pointer, actual.as_ref()) {
        pointer.restore(screens);
    }
    set_phase_with(state, ctx, phase, actual);
}

fn set_phase_with(state: &State, ctx: &egui::Context, phase: Phase, screens: Option<Vec<Screen>>) {
    let mut s = state.lock().unwrap();
    s.phase = phase;
    s.since = Instant::now();
    if let Some(screens) = screens {
        s.screens = screens;
    }
    drop(s);
    ctx.request_repaint_of(ViewportId::ROOT);
}

/// Opens the card with the Windows layout for the connected displays.
pub fn run() -> Result<(), String> {
    let name = watch::wide("Local\\auto-display-setup");
    let mutex = unsafe { CreateMutexW(null(), 0, name.as_ptr()) };
    if !mutex.is_null() && unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Ok(()); // Another setup is already on it.
    }
    let snapshot = Snapshot::capture()?;
    let duplicated = snapshot.duplicated();
    let started = show(snapshot.screens(), None, Some(snapshot), move |state, ctx| {
        worker(state, ctx, duplicated)
    })?;
    if !started {
        fix::setup()?;
    }
    Ok(())
}

fn worker(state: State, ctx: egui::Context, duplicated: bool) {
    let result = (|| -> Result<(), String> {
        if duplicated {
            log::line("Displays are duplicated; switching to extend.");
            let pointer = capture_pointer(&state);
            let result = display::extend();
            finish_display_change(&state, &ctx, Phase::Working, pointer);
            result?;
        }
        let layout = {
            let screens = state.lock().unwrap().screens.clone();
            display::remembered_layout(&screens)
        };
        match layout {
            display::StartupLayout::Current => {}
            display::StartupLayout::Remembered => {
                let pointer = capture_pointer(&state);
                let result = display::restore_remembered_layout();
                finish_display_change(&state, &ctx, Phase::Working, pointer);
                result?;
                log::line("Restored the Windows layout for these displays.");
            }
            display::StartupLayout::Defaults => {
                let pointer = capture_pointer(&state);
                let result = fix::arrange(None, false);
                finish_display_change(&state, &ctx, Phase::Working, pointer);
                result?;
                let pointer = capture_pointer(&state);
                let scaling = fix::scale();
                finish_display_change(&state, &ctx, Phase::Working, pointer);
                if scaling? {
                    fix::refresh_taskbar();
                }
            }
        }
        Ok(())
    })();
    let phase = match result {
        Ok(()) if !state.lock().unwrap().screens.is_empty() => Phase::Prompt,
        Ok(()) => Phase::Done("Your displays are extended.".into()),
        Err(e) => {
            log::line(&format!("error: {e}"));
            Phase::Failed(e)
        }
    };
    let ready = phase == Phase::Prompt;
    set_phase_with(&state, &ctx, phase, None);
    if ready {
        log::line("Display preview is ready.");
    }
}

// ---------------------------------------------------------------------------------------------
// Preview: the same card with made-up screens; nothing on the real displays changes.

#[derive(Clone)]
struct Preview {
    kind: String,
}

impl Preview {
    fn dual(&self) -> bool {
        !matches!(self.kind.as_str(), "single" | "done")
    }

    fn screens(&self) -> Vec<Screen> {
        let mut screens = if self.dual() {
            let (w, h) = if self.kind == "portrait" { (2160, 3840) } else { (3840, 2160) };
            vec![
                fake("DELL U2723QE", false, false, (-w, 0, w, h), (597.0, 336.0)),
                fake("LG 27UL850", false, true, (0, 0, 3840, 2160), (597.0, 336.0)),
            ]
        } else {
            vec![fake("DELL U4021QW", false, true, (0, 0, 5120, 2160), (977.0, 412.0))]
        };
        let areas: Vec<Area> = screens.iter().map(Screen::area).collect();
        let side = if self.dual() { Side::Below } else { Side::Left };
        let (x, y) = display::laptop_spot(&areas, (3840, 2400), side);
        screens.push(fake("Laptop", true, false, (x, y, 3840, 2400), (345.0, 215.0)));
        screens
    }
}

fn fake(name: &str, internal: bool, primary: bool, area: (i32, i32, i32, i32), mm: (f32, f32)) -> Screen {
    let (x, y, width, height) = area;
    Screen {
        id: name.into(), name: name.into(), internal, primary, x, y, width, height, mm: Some(mm),
        rotation: if width < height { Rotation::Portrait } else { Rotation::Landscape },
        scale: Some(display::Scale::sample(if internal { 250 } else { 150 })),
    }
}

/// Plays the card with made-up screens and changes nothing. `kind` is "dual" (two monitors),
/// "portrait" (one monitor turned vertically), "single" (one ultrawide), "done" or "error".
pub fn preview(kind: &str) -> Result<(), String> {
    let preview = Preview { kind: kind.to_owned() };
    let names = if preview.dual() { "Laptop + DELL U2723QE + LG 27UL850" } else { "Laptop + DELL U4021QW" };
    let start = vec![fake(names, false, true, (0, 0, 3840, 2400), (597.0, 336.0))];
    let worker_preview = preview.clone();
    show(start, Some(preview), None, move |state, ctx| {
        sleep(Duration::from_millis(1400));
        let arranged = worker_preview.screens();
        set_phase_with(&state, &ctx, Phase::Working, Some(arranged.clone()));
        sleep(Duration::from_millis(1800));
        let last = match worker_preview.kind.as_str() {
            "error" => Phase::Failed("SetDisplayConfig (arrange) failed with error 87".into()),
            "done" => Phase::Done("DELL U4021QW is your main display.".into()),
            _ => Phase::Prompt,
        };
        set_phase_with(&state, &ctx, last, Some(arranged));
    })
    .map(|_| ())
}

// ---------------------------------------------------------------------------------------------
// The window(s).

/// Shows the card and runs `work` alongside it. Returns false if the window never started.
fn show(
    screens: Vec<Screen>,
    preview: Option<Preview>,
    snapshot: Option<Snapshot>,
    work: impl FnOnce(State, egui::Context) + Send + 'static,
) -> Result<bool, String> {
    let state: State = Arc::new(Mutex::new(Shared {
        phase: Phase::Working,
        since: Instant::now(),
        screens,
        needs_taskbar_refresh: false,
        pointer_settlement: None,
    }));
    let started = Arc::new(AtomicBool::new(false));
    let options = eframe::NativeOptions { viewport: card_viewport("auto-display").with_app_id("auto-display"),
        glow_options: egui_glow_config(),
        ..Default::default()
    };
    let (app_state, app_started) = (state.clone(), started.clone());
    let result = eframe::run_native(
        "auto-display",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            install_fonts(&cc.egui_ctx);
            app_started.store(true, Ordering::SeqCst);
            let (worker_state, ctx) = (app_state.clone(), cc.egui_ctx.clone());
            spawn(move || work(worker_state, ctx));
            Ok(Box::new(Card::new(app_state, preview, snapshot)))
        }),
    );
    if let Err(e) = result {
        log::line(&format!("Could not show the setup card: {e}"));
    }
    Ok(started.load(Ordering::SeqCst))
}

/// Each immediate viewport presents on the UI thread. Waiting on each one would make input wait
/// for several display refreshes. Events and repaint timers schedule the frames instead.
fn egui_glow_config() -> eframe::egui_glow::GlowConfiguration {
    eframe::egui_glow::GlowConfiguration { vsync: false, ..Default::default() }
}

fn card_viewport(title: &str) -> ViewportBuilder {
    ViewportBuilder::default()
        .with_title(title)
        .with_inner_size([WIDTH, HEIGHT])
        .with_decorations(false)
        .with_always_on_top()
        .with_taskbar(false)
        .with_resizable(false)
        .with_active(false)
        .with_visible(false)
}

/// Segoe UI for text and Segoe UI Semibold for headings, falling back to egui's fonts.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let dir = std::env::var_os("WINDIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows")).join("Fonts");
    let mut regular = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mut semibold = regular.clone();
    if let Ok(bytes) = std::fs::read(dir.join("segoeui.ttf")) {
        fonts.font_data.insert("segoe".into(), Arc::new(FontData::from_owned(bytes)));
        regular.insert(0, "segoe".into());
        semibold.insert(0, "segoe".into());
    }
    if let Ok(bytes) = std::fs::read(dir.join("seguisb.ttf")) {
        fonts.font_data.insert("segoe-semibold".into(), Arc::new(FontData::from_owned(bytes)));
        semibold.insert(0, "segoe-semibold".into());
    }
    fonts.families.insert(FontFamily::Proportional, regular);
    fonts.families.insert(FontFamily::Name("semibold".into()), semibold);
    ctx.set_fonts(fonts);
}

fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}

fn regular(size: f32) -> FontId {
    FontId::proportional(size)
}

enum Action {
    Keep,
    Revert,
    Close,
    MoveScreen { index: usize, position: (i32, i32), rect: Rect },
    SetScale(usize, u32),
    SetMain(usize),
}

#[derive(Default)]
struct LayoutEdit {
    drag: Option<Drag>,
    hold_open: bool,
}

impl LayoutEdit {
    fn auto_keep(&self) -> bool {
        !self.hold_open && self.drag.is_none()
    }
}

struct Drag {
    viewport: ViewportId,
    screen: usize,
    start: Pos2,
    rect: Rect,
    scale: f32,
    origin_mm: Pos2,
}

struct LayoutDraft {
    screens: Vec<Screen>,
    // Keep stationary tiles in place when a drag breaks their desktop connections.
    rects: Vec<Rect>,
}

/// One native window showing the card. The main one is eframe's root window; every other monitor
/// gets an extra egui viewport.
#[derive(Default)]
struct CardWindow {
    hwnd: Option<isize>,
    alpha: Option<u8>,
    shown: bool,
}

struct Card {
    state: State,
    shown: Option<Instant>,
    closing: Option<Instant>,
    /// Monitors in card order. The first screen keeps the root window when desktop origins change.
    monitors: Vec<DesktopMonitor>,
    last_monitor_poll: Option<Instant>,
    recenter: bool,
    /// Opacity for this frame, shared by every window.
    alpha: u8,
    windows: HashMap<ViewportId, CardWindow>,
    preview: Option<Preview>,
    snapshot: Option<Arc<Snapshot>>,
    mock_original: Option<Vec<Screen>>,
    draft: Option<LayoutDraft>,
    edit: LayoutEdit,
}

impl Card {
    fn new(state: State, preview: Option<Preview>, snapshot: Option<Snapshot>) -> Self {
        let edit = LayoutEdit { hold_open: preview.is_some(), ..Default::default() };
        let mock_original = preview.as_ref().map(|_| state.lock().unwrap().screens.clone());
        Card {
            state, shown: None, closing: None, monitors: Vec::new(), last_monitor_poll: None,
            recenter: false, alpha: 0, windows: HashMap::new(),
            preview, snapshot: snapshot.map(Arc::new), mock_original, draft: None, edit,
        }
    }

    /// Styles, fades and centres one card window on its monitor, and shows it once it is ready.
    fn manage(&mut self, ctx: &egui::Context, id: ViewportId, hwnd: Option<isize>, monitor: usize) {
        let alpha = self.alpha;
        let work = self.monitors.get(monitor).map(|m| m.work);
        let win = self.windows.entry(id).or_default();
        if let Some(h) = hwnd.filter(|&h| win.hwnd != Some(h)) {
            *win = CardWindow { hwnd: Some(h), ..Default::default() };
            style_window(h);
            set_alpha(h, 0);
            win.alpha = Some(0);
        }
        let Some(h) = win.hwnd else { return };
        if (self.recenter || !win.shown) && let Some(work) = work {
            centre_in(h, work);
        }
        if !win.shown {
            ctx.send_viewport_cmd_to(id, ViewportCommand::Visible(true));
            win.shown = true;
        }
        if win.alpha != Some(alpha) || (self.recenter && !is_layered(h)) {
            set_alpha(h, alpha);
            win.alpha = Some(alpha);
        }
    }

    /// The id of the screen that card window number `monitor` sits on, which that card highlights.
    fn screen_under(&self, screens: &[Screen], monitor: usize) -> Option<String> {
        if self.preview.is_some() {
            // Made-up screens: pair them with the real monitors in the same order, main display first.
            let mut order: Vec<&Screen> = screens.iter().collect();
            order.sort_by_key(|s| (!s.primary, s.x, s.y));
            return order.get(monitor).map(|s| s.id.clone());
        }
        let desktop = self.monitors.get(monitor)?.desktop;
        let (x, y) = ((desktop.left + desktop.right) / 2, (desktop.top + desktop.bottom) / 2);
        screens
            .iter()
            .find(|s| x >= s.x && x < s.x + s.width && y >= s.y && y < s.y + s.height)
            .map(|s| s.id.clone())
    }

    fn close(&mut self) {
        self.closing.get_or_insert_with(Instant::now);
    }

    fn act(&mut self, action: Action, ctx: &egui::Context) {
        if self.state.lock().unwrap().phase == Phase::Applying {
            return;
        }
        match action {
            Action::Keep => self.keep(ctx),
            Action::Revert => self.revert(ctx),
            Action::Close => self.close(),
            Action::SetScale(index, percent) => self.change_scale(index, percent, ctx),
            Action::MoveScreen { .. } | Action::SetMain(..) => {
                self.edit_layout(action);
                ctx.request_repaint_of(ViewportId::ROOT);
            }
        }
    }

    fn keep(&mut self, ctx: &egui::Context) {
        let (before, needs_refresh) = {
            let state = self.state.lock().unwrap();
            (state.screens.clone(), state.needs_taskbar_refresh)
        };
        let Some(draft) = &self.draft else {
            if self.preview.is_none() {
                log::line("Kept the display settings.");
                if needs_refresh {
                    refresh_taskbar_detached();
                }
            }
            set_phase_with(&self.state, ctx, Phase::Kept, None);
            return;
        };
        if !display::valid_layout(&draft.screens.iter().map(Screen::area).collect::<Vec<_>>()) {
            return;
        }
        let mut screens = draft.screens.clone();
        for (draft, live) in screens.iter_mut().zip(&before) {
            draft.scale = live.scale;
        }
        if self.preview.is_some() {
            set_phase_with(&self.state, ctx, Phase::Kept, Some(screens));
            return;
        }

        let (state, ctx) = (self.state.clone(), ctx.clone());
        set_phase_with(&state, &ctx, Phase::Applying, None);
        spawn(move || {
            let pointer = PointerAnchor::capture(&before);
            match display::apply_positions(&before, &screens) {
                Ok(()) => {
                    state.lock().unwrap().pointer_settlement = pointer.and_then(|anchor| anchor.restore_now(&screens));
                    log::line("Kept the display layout.");
                    if needs_refresh {
                        refresh_taskbar_detached();
                    }
                    set_phase_with(&state, &ctx, Phase::Kept, Some(screens));
                }
                Err(error) => {
                    log::line(&format!("error: {error}"));
                    finish_display_change(&state, &ctx, Phase::Failed(error), pointer);
                }
            }
        });
    }

    fn revert(&mut self, ctx: &egui::Context) {
        self.draft = None;
        let restored = Phase::Done("Previous display settings restored.".into());
        if let Some(screens) = self.mock_original.take() {
            set_phase_with(&self.state, ctx, restored, Some(screens));
            return;
        }
        let Some(snapshot) = self.snapshot.clone() else { return self.close() };
        let (state, ctx) = (self.state.clone(), ctx.clone());
        let needs_refresh = state.lock().unwrap().needs_taskbar_refresh;
        set_phase_with(&state, &ctx, Phase::Applying, None);
        spawn(move || {
            let pointer = capture_pointer(&state);
            let phase = match snapshot.restore() {
                Ok(scaling_changed) => {
                    if scaling_changed || needs_refresh {
                        refresh_taskbar_detached();
                    }
                    log::line("Restored the previous display settings.");
                    restored
                }
                Err(error) => {
                    log::line(&format!("error: {error}"));
                    Phase::Failed(error)
                }
            };
            finish_display_change(&state, &ctx, phase, pointer);
        });
    }

    fn edit_layout(&mut self, action: Action) {
        self.edit.hold_open = true;
        let state = self.state.lock().unwrap();
        let mut draft = self.draft.take().unwrap_or_else(|| LayoutDraft {
            screens: state.screens.clone(),
            rects: physical_rects(&state.screens),
        });
        match action {
            Action::MoveScreen { index, position, rect } => {
                (draft.screens[index].x, draft.screens[index].y) = position;
                draft.rects[index] = rect;
            }
            Action::SetMain(index) => {
                for (i, screen) in draft.screens.iter_mut().enumerate() {
                    screen.primary = i == index;
                }
            }
            _ => unreachable!(),
        }
        anchor_primary(&mut draft.screens);
        self.draft = draft.screens.iter().zip(&state.screens)
            .any(|(screen, current)| screen.area() != current.area() || screen.primary != current.primary)
            .then_some(draft);
    }

    fn change_scale(&mut self, index: usize, percent: u32, ctx: &egui::Context) {
        self.edit.hold_open = true;
        let mut screens = self.state.lock().unwrap().screens.clone();
        let before = screens[index].clone();
        let Some(scale) = &mut screens[index].scale else { return };
        if scale.current == percent {
            return;
        }
        scale.current = percent;
        if self.preview.is_some() {
            set_phase_with(&self.state, ctx, Phase::Prompt, Some(screens));
            return;
        }

        let (state, ctx) = (self.state.clone(), ctx.clone());
        set_phase_with(&state, &ctx, Phase::Applying, None);
        spawn(move || {
            let pointer = PointerAnchor::capture(&screens);
            match display::apply_scale(&before, percent) {
                Ok(changed) => {
                    {
                        let settlement = pointer.and_then(|anchor| anchor.restore_now(&screens));
                        let mut shared = state.lock().unwrap();
                        shared.needs_taskbar_refresh |= changed;
                        shared.pointer_settlement = settlement;
                    }
                    set_phase_with(&state, &ctx, Phase::Prompt, Some(screens));
                }
                Err(error) => {
                    log::line(&format!("error: {error}"));
                    finish_display_change(&state, &ctx, Phase::Failed(error), pointer);
                }
            }
        });
    }
}

fn refresh_taskbar_detached() {
    if !config::load().restart_explorer {
        return;
    }
    let result = std::env::current_exe().map_err(|e| e.to_string()).and_then(|exe| {
        install::start_detached(&format!("\"{}\" taskbar", exe.display()))
    });
    if let Err(error) = result {
        log::line(&format!("Could not start the taskbar refresh: {error}"));
    }
}

impl eframe::App for Card {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        BG.to_normalized_gamma_f32()
    }

    /// Window management lives here because `ui` does not run while the window is hidden.
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        let now = Instant::now();
        let (phase, since) = {
            let s = self.state.lock().unwrap();
            (s.phase.clone(), s.since)
        };
        // Ignore transient monitor lists during an edit. The completed phase triggers a fresh
        // poll so all the existing windows move directly to their final work areas.
        self.recenter = phase != Phase::Applying && self.last_monitor_poll.is_none_or(|last| {
            since > last || now - last >= MONITOR_POLL
        });
        if self.recenter {
            if let Some(monitors) = monitors(&self.monitors) {
                self.windows.retain(|id, win| {
                    *id == ViewportId::ROOT || (monitors.iter().skip(1).any(|m| m.viewport_id() == *id)
                        && win.hwnd.is_none_or(window_belongs_to_this_process))
                });
                let root_changed = self.monitors.first().map(|m| &m.device) != monitors.first().map(|m| &m.device);
                if root_changed || monitors.len() != self.monitors.len()
                    || self.edit.drag.as_ref().is_some_and(|drag| !self.windows.contains_key(&drag.viewport))
                {
                    self.edit.drag = None;
                }
                self.monitors = monitors;
            }
            self.last_monitor_poll = Some(now);
        }
        ctx.request_repaint_after(MONITOR_POLL);
        let fade_in = self.shown.map_or(0.0, |t| (now - t).as_secs_f32() / FADE_IN).clamp(0.0, 1.0);
        let fade_out = self.closing.map_or(1.0, |t| 1.0 - (now - t).as_secs_f32() / FADE_OUT).clamp(0.0, 1.0);
        self.alpha = (ease_out(fade_in) * fade_out * CARD_ALPHA).round() as u8;

        self.manage(ctx, ViewportId::ROOT, hwnd(frame), 0);
        if self.windows.get(&ViewportId::ROOT).is_none_or(|w| w.hwnd.is_none()) {
            ctx.request_repaint();
            return;
        }
        self.shown.get_or_insert(now);
        if self.closing.is_some_and(|t| now - t > Duration::from_secs_f32(FADE_OUT + 0.02)) {
            ctx.send_viewport_cmd(ViewportCommand::Close);
            return;
        }

        let elapsed = now.saturating_duration_since(since);
        match phase {
            Phase::Working => ctx.request_repaint_after(ANIMATION_TICK),
            Phase::Kept | Phase::Done(_) if elapsed >= DONE_FOR => self.close(),
            Phase::Kept | Phase::Done(_) if elapsed < Duration::from_millis(350) => {
                ctx.request_repaint_after(ANIMATION_TICK);
            }
            Phase::Kept | Phase::Done(_) => ctx.request_repaint_after(DONE_FOR - elapsed),
            Phase::Failed(_) if elapsed >= FAILED_FOR => self.close(),
            Phase::Prompt if self.edit.auto_keep() && self.closing.is_none() => {
                if elapsed >= KEEP_AFTER {
                    self.act(Action::Keep, ctx);
                } else {
                    ctx.request_repaint_after(COUNTDOWN_TICK);
                }
            }
            _ => {}
        }
        if fade_in < 1.0 || self.closing.is_some() {
            ctx.request_repaint_after(ANIMATION_TICK);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let (phase, since, screens, settlement) = {
            let mut s = self.state.lock().unwrap();
            let settlement = (s.phase != Phase::Applying).then(|| s.pointer_settlement.take()).flatten();
            (s.phase.clone(), s.since, s.screens.clone(), settlement)
        };
        let ctx = ui.ctx().clone();
        let here: Vec<Option<String>> =
            (0..self.monitors.len().max(1)).map(|m| self.screen_under(&screens, m)).collect();
        let (preview, rects) = if let Some(draft) = &self.draft && matches!(phase, Phase::Prompt | Phase::Applying) {
            let mut preview = draft.screens.clone();
            for (screen, actual) in preview.iter_mut().zip(&screens) {
                screen.scale = actual.scale;
            }
            (preview, draft.rects.clone())
        } else {
            let rects = physical_rects(&screens);
            (screens, rects)
        };
        let mut action = draw(ui, &phase, since, &preview, &rects, here[0].as_deref(), &mut self.edit);

        // The same card on every other monitor; any of them can be clicked.
        for (monitor, here) in here.iter().enumerate().skip(1) {
            let display = &self.monitors[monitor];
            let id = display.viewport_id();
            let title = display.card_title();
            let clicked =
                ctx.show_viewport_immediate(id, card_viewport(&title), |ui, _| {
                    draw(ui, &phase, since, &preview, &rects, here.as_deref(), &mut self.edit)
                });
            action = action.or(clicked);
            let hwnd = if self.windows.get(&id).is_none_or(|win| win.hwnd.is_none()) {
                find_card_window(&title)
            } else {
                None
            };
            self.manage(&ctx, id, hwnd, monitor);
        }

        if let Some(settlement) = settlement {
            settlement.finish();
        }
        if let Some(action) = action && self.closing.is_none() {
            self.act(action, &ctx);
        }
    }
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

// ---------------------------------------------------------------------------------------------
// Drawing.

/// Find the closest drop where the screens have a shared edge and do not overlap.
#[cfg(test)]
fn snap_position(screens: &[Screen], moving: usize, desired: (i32, i32)) -> Option<(i32, i32)> {
    let rects: Vec<_> = screens.iter().map(desktop_rect).collect();
    snap_to_edge(screens, &rects, moving, pos2(desired.0 as f32, desired.1 as f32), 1.0)
        .map(|snap| snap.position)
}

#[derive(Clone, Copy)]
struct Snap {
    position: (i32, i32),
    rect: Rect,
    distance_squared: f32,
}

impl Snap {
    fn prefer(self, best: &mut Option<Self>) {
        if best.is_none_or(|old| self.distance_squared < old.distance_squared) {
            *best = Some(self);
        }
    }

    fn near(self, points_per_mm: f32) -> bool {
        self.distance_squared * points_per_mm.powi(2) <= SNAP_DISTANCE.powi(2)
    }
}

/// Find an edge contact, then prefer a nearby alignment visible on the diagram.
fn snap_to_edge(
    screens: &[Screen], rects: &[Rect], moving: usize, desired: Pos2, points_per_mm: f32,
) -> Option<Snap> {
    let screen = screens[moving].area();
    let size = rects[moving].size();
    let panels: Vec<_> = screens.iter().zip(rects).enumerate()
        .filter_map(|(index, (screen, &rect))| (index != moving).then_some((screen.area(), rect)))
        .collect();
    let snap_distance = SNAP_DISTANCE / points_per_mm;
    let mut nearest = None;
    let mut clear_seam = None;
    let mut corner = None;
    for vertical in [false, true] {
        let axes = |a: Area| if vertical { (a.y, a.x, a.h, a.w) } else { (a.x, a.y, a.w, a.h) };
        let (_, _, width, height) = axes(screen);
        let diagram_axes = |r: Rect| {
            if vertical { (r.top(), r.left(), r.height(), r.width()) } else { (r.left(), r.top(), r.width(), r.height()) }
        };
        let (_, _, moving_w, moving_h) = diagram_axes(rects[moving]);
        let desired_y = if vertical { desired.x } else { desired.y };
        let neighbours: Vec<_> = panels.iter().map(|&(area, rect)| (axes(area), diagram_axes(rect))).collect();
        for (&anchor, &((x, y, w, h), (rx, ry, rw, rh))) in panels.iter().zip(&neighbours) {
            let range = (y - height + 1)..=(y + h - 1);
            let to_pixels = |mm| y + offset_in_pixels(mm - ry, height, h, moving_h, rh);
            let min_overlap = (MIN_SEAM / points_per_mm).min(moving_h.min(rh) * 0.25);
            let edges = [(x - width, rx - moving_w), (x + w, rx + rw)];
            for (edge, physical_edge) in edges {
                let position = |offset| if vertical { (offset, edge) } else { (edge, offset) };
                let (x, y) = position(to_pixels(desired_y));
                let proposed = Area { x, y, ..screen };
                let min = if vertical { pos2(desired_y, physical_edge) } else { pos2(physical_edge, desired_y) };
                let rect = Rect::from_min_size(min, size);
                let Some(span) = constrained_seam((proposed, rect), anchor, &panels, vertical, false) else { continue };
                let Some(offset) = span.pixel_at(desired_y) else { continue };
                let (x, y) = position(offset);
                let proposed = Area { x, y, ..screen };
                if panels.iter().any(|&(area, _)| proposed.overlaps(area)) || !is_clear(rect, &panels) {
                    continue;
                }
                let snap = Snap { position: (x, y), rect, distance_squared: (min - desired).length_sq() };
                snap.prefer(&mut nearest);
                let overlap = (desired_y + moving_h).min(ry + rh) - desired_y.max(ry);
                if (overlap - min_overlap) * points_per_mm >= -0.25 {
                    snap.prefer(&mut clear_seam);
                }
            }
            let offsets = [
                (to_pixels(desired_y), false),
                (to_pixels(ry + min_overlap - moving_h), false),
                (to_pixels(ry + rh - min_overlap), false),
                (y, true),
                (to_pixels(ry + rh - moving_h), true),
            ].into_iter().chain(neighbours.iter().flat_map(|&((_, y, _, h), (_, other_y, _, other_h))| {
                // Check both desktop and physical boundaries for nearby contacts. Panels can
                // have different proportions in the two coordinate spaces.
                [
                    (y - height, false), (y + h, false),
                    (y - height + 1, false), (y + h - 1, false),
                    (to_pixels(other_y - moving_h), false), (to_pixels(other_y + other_h), false),
                ]
            }));
            for (requested, aligned) in offsets {
                let offset = requested.clamp(*range.start(), *range.end());
                let diagram_y = ry + offset_in_mm(offset - y, height, h, moving_h, rh);
                for (edge, diagram_x) in edges {
                    let (x, y) = if vertical { (offset, edge) } else { (edge, offset) };
                    let proposed = Area { x, y, ..screen };
                    if panels.iter().any(|&(area, _)| proposed.overlaps(area)) {
                        continue;
                    }
                    if let Some(rect) = corner_rect(proposed, size, &panels) {
                        let snap = Snap { position: (x, y), rect, distance_squared: (rect.min - desired).length_sq() };
                        snap.prefer(&mut nearest);
                        snap.prefer(&mut clear_seam);
                        if snap.near(points_per_mm) {
                            snap.prefer(&mut corner);
                        }
                        continue;
                    }
                    let rect = Rect::from_min_size(
                        if vertical { pos2(diagram_y, diagram_x) } else { pos2(diagram_x, diagram_y) },
                        size,
                    );
                    if !is_clear(rect, &panels) {
                        continue;
                    }
                    let snap = Snap { position: (x, y), rect, distance_squared: (rect.min - desired).length_sq() };
                    snap.prefer(&mut nearest);

                    let overlap = (diagram_y + moving_h).min(ry + rh) - diagram_y.max(ry);
                    if (overlap - min_overlap) * points_per_mm >= -0.25 {
                        snap.prefer(&mut clear_seam);
                        if snap.near(points_per_mm) {
                            let at_three_way_corner = neighbours.iter().any(|&(_, (nx, ny, nw, nh))| {
                                let crosses = (diagram_x + moving_w).min(nx + nw) > diagram_x.max(nx) + 0.001;
                                let gap = (diagram_y + moving_h - ny).abs().min((diagram_y - ny - nh).abs());
                                crosses && gap * points_per_mm <= 0.25
                            });
                            if (aligned && offset == requested) || at_three_way_corner {
                                snap.prefer(&mut corner);
                            }
                        }
                    }
                }
            }
        }
    }
    let nearest = nearest?;
    let clear_seam = clear_seam.filter(|snap| {
        snap.distance_squared.sqrt() <= nearest.distance_squared.sqrt() + snap_distance
    });
    Some(corner.or(clear_seam).unwrap_or(nearest))
}

fn drop_tile(screens: &[Screen], rects: &[Rect], index: usize, rect: Rect, snap: Option<Snap>, scale: f32) -> Action {
    let others: Vec<_> = screens.iter().zip(rects).enumerate()
        .filter_map(|(i, (screen, &rect))| (i != index).then_some((screen.area(), rect)))
        .collect();
    if let Some(snap) = snap.filter(|snap| snap.near(scale) || !is_clear(rect, &others)) {
        return Action::MoveScreen { index, position: snap.position, rect: snap.rect };
    }

    let rect = nearest_clear_rect(rect, &others);
    let desktop = desktop_rect(&screens[index]);
    let delta = (rect.min - rects[index].min) * desktop.size() / rect.size();
    let target = desktop.translate(delta.round());
    // A free drop is detached in the diagram. Give it at least one pixel of clearance in
    // desktop coordinates too, where displays with different pixel densities can overlap.
    let blocked: Vec<_> = screens.iter().enumerate().filter(|(i, _)| *i != index)
        .map(|(_, screen)| (screen.area(), desktop_rect(screen).expand(1.0)))
        .collect();
    let desktop = nearest_clear_rect(target, &blocked).min;
    Action::MoveScreen { index, position: (desktop.x.round() as i32, desktop.y.round() as i32), rect }
}

fn connected_to_main(screens: &[Screen]) -> Vec<bool> {
    let areas: Vec<_> = screens.iter().map(Screen::area).collect();
    let main = screens.iter().position(|s| s.primary).unwrap_or(0);
    display::connected_to(&areas, main)
}

fn anchor_primary(screens: &mut [Screen]) {
    if let Some(main) = screens.iter().find(|s| s.primary) {
        let (x, y) = (main.x, main.y);
        for screen in screens {
            screen.x -= x;
            screen.y -= y;
        }
    }
}

/// Draws the card. `here` is the id of the screen this card is on, which is highlighted.
fn draw(ui: &mut egui::Ui, phase: &Phase, since: Instant, screens: &[Screen], rects: &[Rect], here: Option<&str>, edit: &mut LayoutEdit) -> Option<Action> {
    let rect = ui.max_rect();
    let painter = ui.painter().clone();
    let t = ui.input(|i| i.time) as f32;
    let interactive = *phase == Phase::Prompt;
    let menu_open = egui::Popup::is_any_open(ui.ctx());

    painter.rect_filled(rect, CornerRadius::ZERO, BG);
    draw_icon(&painter, rect.min + vec2(PAD + 22.0, 48.0), phase, since, t);

    // Title and subtitle.
    let text_x = rect.left() + PAD + 64.0;
    let text_w = rect.right() - PAD - text_x;
    let (title, subtitle) = match phase {
        Phase::Working => ("Setting up your screens...", None),
        Phase::Prompt | Phase::Applying => (
            "Display Preview",
            Some("Drag displays until correct. Right-click to set main display and scaling".into()),
        ),
        Phase::Done(summary) => ("Displays ready", Some(summary.clone())),
        Phase::Kept => ("Displays ready", None),
        Phase::Failed(e) => ("Couldn't set up your displays", Some(e.clone())),
    };
    let (title_font, sub_font) = (semibold(19.0), regular(14.0));
    let title_y = if subtitle.is_some() { 36.0 } else { 48.0 };
    let title = fit(&painter, title, &title_font, text_w);
    painter.text(pos2(text_x, rect.top() + title_y), Align2::LEFT_CENTER, title, title_font, TEXT);
    if let Some(subtitle) = subtitle {
        let subtitle = fit(&painter, &subtitle, &sub_font, text_w);
        painter.text(pos2(text_x, rect.top() + 61.0), Align2::LEFT_CENTER, subtitle, sub_font, MUTED);
    }

    let area = Rect::from_min_max(pos2(rect.left() + PAD, rect.top() + 84.0), pos2(rect.right() - PAD, rect.bottom() - 74.0));
    painter.rect_filled(area, CornerRadius::same(12), SURFACE);
    let connected = connected_to_main(screens);
    let all_connected = connected.iter().all(|&attached| attached);
    let mut action = draw_screens(ui, area, screens, rects, &connected, here, phase, edit);
    if edit.drag.is_some() || !all_connected {
        edit.hold_open = true;
    }

    // Footer.
    let row = Rect::from_min_max(pos2(rect.left() + PAD, rect.bottom() - 54.0), pos2(rect.right() - PAD, rect.bottom() - 20.0));
    let footer_font = regular(13.0);
    match phase {
        Phase::Prompt | Phase::Applying => {
            let can_keep = interactive && all_connected && edit.drag.is_none();
            let counting = can_keep && edit.auto_keep() && !matches!(action, Some(Action::MoveScreen { .. }));
            let keep = Rect::from_min_size(pos2(row.right() - 112.0, row.top()), vec2(112.0, row.height()));
            let revert = Rect::from_min_size(pos2(keep.left() - 122.0, row.top()), vec2(112.0, row.height()));
            let keep = button(ui, &painter, keep, "keep", "Keep", true, can_keep);
            if !all_connected {
                keep.clone().on_hover_text("Reconnect the detached displays to keep this layout");
            }
            if keep.clicked() {
                action = action.or(Some(Action::Keep));
            }
            if button(ui, &painter, revert, "revert", "Revert", false, interactive && edit.drag.is_none()).clicked() {
                action = action.or(Some(Action::Revert));
            }
            if counting {
                let left = KEEP_AFTER.saturating_sub(since.elapsed()).as_secs_f32();
                let note = fit(&painter, &format!("Keeping in {}s", left.ceil() as u32), &footer_font, revert.left() - row.left() - 12.0);
                painter.text(pos2(row.left(), row.center().y), Align2::LEFT_CENTER, note, footer_font, MUTED);
                let track = Rect::from_min_max(pos2(rect.left(), rect.bottom() - 3.0), rect.max);
                painter.rect_filled(track, CornerRadius::ZERO, BUTTON);
                let fill = Rect::from_min_size(track.min, vec2(track.width() * left / KEEP_AFTER.as_secs_f32(), track.height()));
                painter.rect_filled(fill, CornerRadius::ZERO, ACCENT);
            }
            if interactive && !menu_open && !egui::Popup::is_any_open(ui.ctx()) {
                ui.input(|i| {
                    if i.key_pressed(Key::Escape) {
                        action = Some(Action::Revert);
                    } else if action.is_none() && can_keep && i.key_pressed(Key::Enter) {
                        action = Some(Action::Keep);
                    }
                });
            }
        }
        Phase::Failed(_) => {
            let close = Rect::from_min_size(pos2(row.right() - 104.0, row.top()), vec2(104.0, row.height()));
            let revert = Rect::from_min_size(pos2(close.left() - 110.0, row.top()), vec2(100.0, row.height()));
            let note = fit(&painter, "Details in auto-display.log", &footer_font, revert.left() - row.left() - 12.0);
            painter.text(pos2(row.left(), row.center().y), Align2::LEFT_CENTER, note, footer_font, MUTED);
            if button(ui, &painter, revert, "revert", "Revert", false, true).clicked() {
                action = Some(Action::Revert);
            }
            if button(ui, &painter, close, "close", "Close", false, true).clicked() {
                action = Some(Action::Close);
            }
        }
        _ => {}
    }
    action
}

fn draw_icon(painter: &Painter, c: Pos2, phase: &Phase, since: Instant, t: f32) {
    let r = 20.0;
    match phase {
        Phase::Working => {
            painter.circle_stroke(c, r, Stroke::new(3.0, BUTTON));
            let start = t * 4.2;
            let sweep = 1.6 + 0.9 * (t * 2.1).sin();
            let points: Vec<_> = (0..=36)
                .map(|i| {
                    let a = start + sweep * i as f32 / 36.0;
                    c + vec2(a.cos(), a.sin()) * r
                })
                .collect();
            painter.add(Shape::line(points, Stroke::new(3.0, ACCENT)));
        }
        Phase::Prompt | Phase::Applying => {
            painter.circle_filled(c, r + 2.0, ACCENT.gamma_multiply(0.18));
            // A small screen with a laptop beside it.
            let s = Stroke::new(2.0, ACCENT_TEXT);
            let screen = Rect::from_center_size(c + vec2(3.0, -3.0), vec2(16.0, 10.0));
            painter.rect_stroke(screen, CornerRadius::same(2), s, StrokeKind::Middle);
            painter.line_segment([c + vec2(3.0, 2.0), c + vec2(3.0, 5.0)], s);
            let laptop = Rect::from_center_size(c + vec2(-8.0, 5.0), vec2(8.0, 5.0));
            painter.rect_stroke(laptop, CornerRadius::same(1), s, StrokeKind::Middle);
            painter.line_segment([c + vec2(-13.0, 9.0), c + vec2(-3.0, 9.0)], s);
        }
        Phase::Done(_) | Phase::Kept => {
            painter.circle_filled(c, r + 2.0, GOOD.gamma_multiply(0.18));
            let pop = ease_out((since.elapsed().as_secs_f32() / 0.35).min(1.0));
            let s = Stroke::new(3.0, GOOD);
            let (a, b, e) = (c + vec2(-8.0, 0.5), c + vec2(-2.5, 6.0), c + vec2(9.0, -6.0));
            if pop < 0.4 {
                painter.line_segment([a, a + (b - a) * (pop / 0.4)], s);
            } else {
                painter.line_segment([a, b], s);
                painter.line_segment([b, b + (e - b) * ((pop - 0.4) / 0.6)], s);
            }
        }
        Phase::Failed(_) => {
            painter.circle_filled(c, r + 2.0, BAD.gamma_multiply(0.18));
            painter.text(c, Align2::CENTER_CENTER, "!", semibold(22.0), BAD);
        }
    }
}

fn desktop_rect(screen: &Screen) -> Rect {
    Rect::from_min_size(pos2(screen.x as f32, screen.y as f32), vec2(screen.width as f32, screen.height as f32))
}

/// Draws the physical display sizes and their controls.
fn draw_screens(
    ui: &mut egui::Ui,
    area: Rect,
    screens: &[Screen],
    rects: &[Rect],
    connected: &[bool],
    here: Option<&str>,
    phase: &Phase,
    edit: &mut LayoutEdit,
) -> Option<Action> {
    if screens.is_empty() {
        return None;
    }
    let painter = ui.painter_at(area);
    let area = area.shrink2(vec2(22.0, 20.0));
    let bounds = rects.iter().fold(Rect::NOTHING, |b, &r| b.union(r));
    let room = area.size();
    let scale = (room.x / bounds.width().max(1.0)).min(room.y / bounds.height().max(1.0));
    let offset = area.min + (room - bounds.size() * scale) / 2.0;
    let to_card = |r: Rect| {
        Rect::from_min_size(offset + (r.min - bounds.min) * scale, r.size() * scale)
    };

    let ctx = ui.ctx().clone();
    let animate = |key: &str, target: Rect| {
        // Viewports share an animation manager, but DPI rounding can give each a different target.
        let id = Id::new(("screen", ctx.viewport_id(), key));
        let a = |k: &str, v: f32| ctx.animate_value_with_time(id.with(k), v, 0.15);
        Rect::from_min_size(pos2(a("x", target.left()), a("y", target.top())), vec2(a("w", target.width()), a("h", target.height())))
    };

    let viewport = ctx.viewport_id();
    let editable = *phase == Phase::Prompt;
    let multiple_displays = screens.len() > 1;
    let drag = &mut edit.drag;
    let mut action = None;
    let mut moving = None;
    for (index, screen) in screens.iter().enumerate() {
        let target = to_card(rects[index]);
        let rect = if *phase == Phase::Working { animate(&screen.id, target) } else { target };
        let detached = matches!(phase, Phase::Prompt | Phase::Applying) && !connected[index];
        let sense = if !editable {
            Sense::hover()
        } else if multiple_displays {
            Sense::click_and_drag()
        } else {
            Sense::click()
        };
        let response = ui.interact(rect, ui.id().with((&screen.id, "display")), sense);
        let response = if detached { response.on_hover_text("Detached from the main display") } else { response };
        if editable && response.is_pointer_button_down_on() {
            edit.hold_open = true;
        }
        if editable && multiple_displays {
            let button = egui::PointerButton::Primary;
            response.clone().on_hover_cursor(CursorIcon::Grab);
            if response.drag_started_by(button) && drag.is_none()
                && let Some(start) = ui.input(|i| i.pointer.press_origin())
                && rect.contains(start)
            {
                *drag = Some(Drag { viewport, screen: index, start, rect, scale, origin_mm: rects[index].min });
            }
            if let Some(active) = drag.as_ref().filter(|d| d.viewport == viewport && d.screen == index) {
                let delta = response.interact_pointer_pos().unwrap_or(active.start) - active.start;
                let desired = active.origin_mm + delta / active.scale;
                let moved = delta.length_sq() >= 1.0;
                let snap = moved.then(|| snap_to_edge(screens, rects, index, desired, active.scale)).flatten();
                if response.dragged_by(button) {
                    ctx.set_cursor_icon(CursorIcon::Grabbing);
                    let snapped = snap.filter(|target| target.near(active.scale));
                    let rect = snapped.map_or_else(|| active.rect.translate(delta), |target| to_card(target.rect));
                    moving = Some((index, rect, snapped.is_some()));
                    continue;
                }
                if response.drag_stopped_by(button) && moved {
                    let tile = Rect::from_min_size(desired, rects[index].size());
                    action = Some(drop_tile(screens, rects, index, tile, snap, active.scale));
                }
                *drag = None;
            }
        }
        draw_screen(&painter, screen, rect, here == Some(screen.id.as_str()), detached, false);
        if editable && drag.is_none() {
            let (opened, selected) = display_menu(&response, screen, index, multiple_displays);
            edit.hold_open |= opened;
            action = action.or(selected);
        }
    }
    if let Some((index, rect, snapped)) = moving {
        let screen = &screens[index];
        draw_screen(&painter, screen, rect, here == Some(screen.id.as_str()), false, true);
        if snapped {
            for (i, neighbour) in rects.iter().enumerate() {
                if i != index {
                    draw_snap_seam(&painter, rect, to_card(*neighbour));
                }
            }
        }
    }
    action
}

fn draw_snap_seam(painter: &Painter, tile: Rect, neighbour: Rect) {
    let stroke = Stroke::new(2.0, ACCENT_TEXT);
    let shared = tile.intersect(neighbour);
    // The projections can differ by a fraction of a point after scaling.
    if shared.width().abs() < 0.1 && shared.height() > 1.0 {
        painter.line_segment([shared.left_top(), shared.left_bottom()], stroke);
    }
    if shared.height().abs() < 0.1 && shared.width() > 1.0 {
        painter.line_segment([shared.left_top(), shared.right_top()], stroke);
    }
}

fn display_menu(response: &egui::Response, screen: &Screen, index: usize, multiple_displays: bool) -> (bool, Option<Action>) {
    let popup = egui::Popup::context_menu(response).width(150.0).show(|ui| {
        let main_label = if screen.primary { "Main display" } else { "Set to main display" };
        if ui.add_enabled(multiple_displays && !screen.primary, egui::Button::selectable(screen.primary, main_label)).clicked() {
            ui.close();
            return Some(Action::SetMain(index));
        }
        let mut chosen = None;
        ui.add_enabled_ui(screen.scale.is_some(), |ui| {
            ui.menu_button("Adjust scaling", |ui| {
                let options = [50, 75].into_iter().chain(display::SCALES.into_iter().take_while(|&p| p <= 300));
                for percent in options {
                    let allowed = screen.scale.is_some_and(|s| s.relative_for(percent).is_ok());
                    let selected = screen.scale.is_some_and(|s| s.current == percent);
                    if ui.add_enabled(allowed, egui::Button::selectable(selected, format!("{percent}%"))).clicked() {
                        chosen = Some((percent, selected));
                        ui.close();
                    }
                }
            });
        });
        if let Some((percent, selected)) = chosen {
            ui.close();
            return (!selected).then_some(Action::SetScale(index, percent));
        }
        None
    });
    (popup.is_some(), popup.and_then(|p| p.inner))
}

/// Draws one screen. `here` highlights it as the screen this card is on.
fn draw_screen(painter: &Painter, s: &Screen, r: Rect, here: bool, detached: bool, dragging: bool) {
    let radius = CornerRadius::same(5);
    let highlighted = here || dragging;
    painter.rect_filled(r, radius, SCREEN);
    let edge = if detached {
        painter.rect_filled(r, radius, BAD.gamma_multiply(0.16));
        Stroke::new(1.5, BAD)
    } else if highlighted {
        painter.rect_filled(r, radius, ACCENT.gamma_multiply(0.14));
        Stroke::new(1.5, ACCENT)
    } else {
        Stroke::new(1.0, SCREEN_EDGE)
    };
    painter.rect_stroke(r, radius, edge, StrokeKind::Inside);
    if detached {
        let radius = (r.width().min(r.height()) * 0.15).min(8.0);
        let center = r.right_top() + vec2(-1.5, 1.5) * radius;
        painter.circle_filled(center, radius, BAD);
        let mark = Stroke::new(radius * 0.24, BG);
        painter.line_segment([center - vec2(0.0, radius * 0.52), center], mark);
        painter.circle_filled(center + vec2(0.0, radius * 0.43), mark.width * 0.56, BG);
    }

    let text_width = (r.width() - 12.0).max(1.0);
    let name_font = regular(if here { (r.height() * 0.32).clamp(1.0, 12.5) } else { 12.5 });
    let name = label(painter, &s.name, &name_font, text_width);
    let name_color = if highlighted { TEXT } else { MUTED };
    let inches = s.mm.map(|(w, h)| (w * w + h * h).sqrt() / 25.4).filter(|d| *d > 5.0 && !s.name.contains(" + "));
    let detail = match (here, s.primary, inches) {
        (true, true, Some(d)) => Some((format!("This Display · Main · {d:.0}\""), Color32::LIGHT_GRAY)),
        (true, true, None) => Some(("This Display · Main".into(), Color32::LIGHT_GRAY)),
        (true, false, Some(d)) => Some((format!("This Display · {d:.0}\""), Color32::LIGHT_GRAY)),
        (true, false, None) => Some(("This Display".into(), Color32::LIGHT_GRAY)),
        (false, true, Some(d)) => Some((format!("Main · {d:.0}\""), if dragging { ACCENT_TEXT } else { MUTED })),
        (false, true, None) => Some(("Main".into(), if dragging { ACCENT_TEXT } else { MUTED })),
        (false, false, Some(d)) => Some((format!("{d:.0}\""), MUTED)),
        (false, false, None) => None,
    };
    match detail {
        Some((text, colour)) if here || r.height() > 40.0 => {
            let spacing = (r.height() * 0.22).min(8.5);
            painter.text(r.center() - vec2(0.0, spacing), Align2::CENTER_CENTER, name, name_font, name_color);
            let detail_font = if here {
                let width = painter.layout_no_wrap(text.clone(), regular(11.0), colour).size().x;
                regular((11.0 * (text_width / width).min(1.0)).min(r.height() * 0.3).max(1.0))
            } else {
                semibold(11.0)
            };
            let text = fit(painter, &text, &detail_font, text_width);
            painter.text(r.center() + vec2(0.0, spacing), Align2::CENTER_CENTER, text, detail_font, colour);
        }
        _ => {
            painter.text(r.center(), Align2::CENTER_CENTER, name, name_font, name_color);
        }
    }
}

fn button(ui: &egui::Ui, painter: &Painter, rect: Rect, id: &str, label: &str, primary: bool, enabled: bool) -> egui::Response {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let response = ui.interact(rect, ui.id().with(id), sense);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    let response = if enabled { response.on_hover_cursor(CursorIcon::PointingHand) } else { response };
    let hovered = response.hovered();
    let radius = CornerRadius::same(10);
    if primary && enabled {
        painter.rect_filled(rect, radius, if hovered { ACCENT_HOVER } else { ACCENT });
    } else {
        painter.rect_filled(rect, radius, if hovered && enabled { BUTTON_HOVER } else { BUTTON });
        painter.rect_stroke(rect, radius, Stroke::new(1.0, SCREEN_EDGE), StrokeKind::Inside);
    }
    centered_text(painter, rect.center(), label.into(), semibold(14.0), if enabled { TEXT } else { MUTED });
    response
}

fn centered_text(painter: &Painter, center: Pos2, label: String, font: FontId, color: Color32) {
    let galley = painter.layout_no_wrap(label, font, color);
    if !galley.is_empty() {
        painter.galley(center - galley.mesh_bounds.center().to_vec2(), galley, color);
    }
}

/// A screen's name if it fits, else just the brand ("DELL"), else as much of the name as fits.
fn label(painter: &Painter, name: &str, font: &FontId, max: f32) -> String {
    let width = |s: &str| painter.layout_no_wrap(s.to_owned(), font.clone(), TEXT).size().x;
    if width(name) <= max {
        return name.to_owned();
    }
    if name.contains(" + ") {
        return fit(painter, "Duplicated", font, max);
    }
    match name.split_whitespace().next() {
        Some(brand) if brand != name && width(brand) <= max => brand.to_owned(),
        _ => fit(painter, name, font, max),
    }
}

/// Shortens `text` with an ellipsis until it fits in `max` points.
fn fit(painter: &Painter, text: &str, font: &FontId, max: f32) -> String {
    let width = |s: &str| painter.layout_no_wrap(s.to_owned(), font.clone(), TEXT).size().x;
    if width(text) <= max {
        return text.to_owned();
    }
    let mut chars: Vec<char> = text.chars().collect();
    while chars.pop().is_some() {
        let candidate = format!("{}…", chars.iter().collect::<String>().trim_end());
        if width(&candidate) <= max {
            return candidate;
        }
    }
    String::new()
}

// ---------------------------------------------------------------------------------------------
// Win32 window helpers.

fn hwnd(frame: &eframe::Frame) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match frame.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(win) => Some(win.hwnd.get()),
        _ => None,
    }
}

/// Windows 11 rounded corners with the native outline suppressed.
fn style_window(h: isize) {
    #[link(name = "dwmapi")]
    unsafe extern "system" {
        fn DwmSetWindowAttribute(hwnd: isize, attr: u32, value: *const core::ffi::c_void, size: u32) -> i32;
    }
    const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
    const DWMWA_BORDER_COLOR: u32 = 34;
    const DWMWA_CAPTION_COLOR: u32 = 35;
    const DWMWCP_ROUND: u32 = 2;
    const DWMWA_COLOR_NONE: u32 = 0xffff_fffe;
    let colorref = |c: Color32| u32::from_le_bytes([c.r(), c.g(), c.b(), 0]);
    let caption = colorref(BG);
    unsafe {
        DwmSetWindowAttribute(h, DWMWA_WINDOW_CORNER_PREFERENCE, (&DWMWCP_ROUND as *const u32).cast(), 4);
        DwmSetWindowAttribute(h, DWMWA_BORDER_COLOR, (&DWMWA_COLOR_NONE as *const u32).cast(), 4);
        DwmSetWindowAttribute(h, DWMWA_CAPTION_COLOR, (&caption as *const u32).cast(), 4);
    }
}

fn set_alpha(h: isize, alpha: u8) {
    let hwnd = h as HWND;
    unsafe {
        // winit rewrites the extended style when its own flags change, so re-add layering each time.
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        if ex & WS_EX_LAYERED as isize == 0 {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_LAYERED as isize);
        }
        SetLayeredWindowAttributes(hwnd, 0, alpha, LWA_ALPHA);
    }
}

fn is_layered(h: isize) -> bool {
    let style = unsafe { GetWindowLongPtrW(h as HWND, GWL_EXSTYLE) };
    style & WS_EX_LAYERED as isize != 0
}

struct DesktopMonitor {
    device: String,
    desktop: RECT,
    work: RECT,
    primary: bool,
}

impl DesktopMonitor {
    fn viewport_id(&self) -> ViewportId {
        ViewportId::from_hash_of(("card", &self.device))
    }

    fn card_title(&self) -> String {
        let name = self.device.strip_prefix(r"\\.\").unwrap_or(&self.device);
        format!("auto-display ({name})")
    }
}

/// Match existing cards to the same devices before assigning any newly connected displays.
fn monitors(previous: &[DesktopMonitor]) -> Option<Vec<DesktopMonitor>> {
    unsafe extern "system" fn collect(monitor: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let list = unsafe { &mut *(data as *mut Vec<DesktopMonitor>) };
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if unsafe { GetMonitorInfoW(monitor, std::ptr::from_mut(&mut info).cast()) } != 0 {
            let end = info.szDevice.iter().position(|&c| c == 0).unwrap_or(info.szDevice.len());
            list.push(DesktopMonitor {
                device: String::from_utf16_lossy(&info.szDevice[..end]),
                desktop: info.monitorInfo.rcMonitor,
                work: info.monitorInfo.rcWork,
                primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            });
        }
        1
    }
    let mut list: Vec<DesktopMonitor> = Vec::new();
    let ok = unsafe { EnumDisplayMonitors(null_mut(), null(), Some(collect), &mut list as *mut _ as LPARAM) };
    if ok == 0 || list.is_empty() {
        return None;
    }
    order_monitors(&mut list, previous);
    Some(list)
}

fn order_monitors(monitors: &mut [DesktopMonitor], previous: &[DesktopMonitor]) {
    monitors.sort_by_key(|monitor| {
        let previous_index = previous.iter().position(|old| old.device == monitor.device);
        (previous_index.unwrap_or(previous.len()), !monitor.primary, monitor.desktop.left, monitor.desktop.top)
    });
}

fn window_belongs_to_this_process(h: isize) -> bool {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(h as HWND, &mut pid) };
    pid == std::process::id()
}

fn find_card_window(title: &str) -> Option<isize> {
    let title = watch::wide(title);
    let mut after = null_mut();
    loop {
        let h = unsafe { FindWindowExW(null_mut(), after, null(), title.as_ptr()) };
        if h.is_null() {
            return None;
        }
        if window_belongs_to_this_process(h as isize) {
            return Some(h as isize);
        }
        after = h;
    }
}

fn find_window(title: &str) -> Option<isize> {
    let title = watch::wide(title);
    let h = unsafe { FindWindowW(null(), title.as_ptr()) };
    (!h.is_null()).then_some(h as isize)
}

pub fn is_open() -> bool {
    find_window("auto-display").is_some()
}

fn centre_in(h: isize, work: RECT) {
    let hwnd = h as HWND;
    unsafe {
        let mut wr: RECT = std::mem::zeroed();
        if GetWindowRect(hwnd, &mut wr) == 0 {
            return;
        }
        let size = Vec2::new((wr.right - wr.left) as f32, (wr.bottom - wr.top) as f32);
        let x = work.left + ((work.right - work.left) as f32 - size.x) as i32 / 2;
        let y = work.top + ((work.bottom - work.top) as f32 - size.y) as i32 / 2;
        if (x, y) != (wr.left, wr.top) {
            SetWindowPos(hwnd, null_mut(), x, y, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOOWNERZORDER);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::fake as screen;

    #[test]
    fn changing_the_primary_keeps_each_preview_card_on_its_panel() {
        let monitor = |name: &str, primary, left| {
            let desktop = RECT { left, top: 0, right: left + 1920, bottom: 1080 };
            DesktopMonitor { device: name.into(), desktop, work: desktop, primary }
        };
        let before = [monitor("right", true, 0), monitor("left", false, -1920)];
        let mut after = [monitor("left", true, 0), monitor("right", false, 1920)];
        order_monitors(&mut after, &before);
        assert_eq!(after[0].device, "right");
        assert_eq!(after[0].desktop.left, 1920);
        assert_eq!(after[1].device, "left");
    }

    #[test]
    fn portrait_preview_uses_physical_sizes_and_keeps_the_seam() {
        let screens = [
            screen("portrait", false, false, (-1440, -2300, 1440, 2560), (597.0, 336.0)),
            screen("main", false, true, (0, 0, 1920, 1080), (530.0, 300.0)),
        ];
        let ctx = egui::Context::default();
        install_fonts(&ctx);
        let output = paint_frame(&ctx, ViewportId::ROOT, vec2(WIDTH, HEIGHT), 0.0, vec![], |ui| {
            draw(ui, &Phase::Prompt, Instant::now(), &screens, &physical_rects(&screens), None, &mut LayoutEdit::default());
        });
        let rects = screen_rects(&output);
        let portrait = rects[0];
        let main = rects[1];
        let scale = main.width() / 530.0;
        assert!((portrait.width() - 336.0 * scale).abs() < 0.01);
        assert!((portrait.height() - 597.0 * scale).abs() < 0.01);
        assert!((portrait.right() - main.left()).abs() < 0.01);
        assert!((portrait.bottom() - main.top() - (260.0 / 2560.0 * 597.0) * scale).abs() < 0.01);
    }

    fn paint_frame(
        ctx: &egui::Context,
        viewport: ViewportId,
        size: Vec2,
        time: f64,
        events: Vec<egui::Event>,
        paint: impl FnMut(&mut egui::Ui),
    ) -> egui::FullOutput {
        let input = egui::RawInput {
            viewport_id: viewport,
            viewports: [(viewport, egui::ViewportInfo::default())].into_iter().collect(),
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
            time: Some(time),
            events,
            ..Default::default()
        };
        let mut paint = paint;
        let mut output = ctx.run_ui(input, |ui| paint(ui));
        output.textures_delta.clear();
        output
    }

    fn screen_rects(output: &egui::FullOutput) -> Vec<Rect> {
        output.shapes.iter().filter_map(|s| match &s.shape {
            Shape::Rect(r) if r.fill == SCREEN => Some(r.rect),
            _ => None,
        }).collect()
    }

    #[test]
    fn screen_animations_are_independent_on_each_monitor() {
        let ctx = egui::Context::default();
        install_fonts(&ctx);
        let screens = Preview { kind: "dual".into() }.screens();
        // Different DPI rounding/window sizes give each viewport different diagram coordinates.
        let viewports = [
            (ViewportId::ROOT, vec2(WIDTH, HEIGHT)),
            (ViewportId::from_hash_of("second monitor"), vec2(WIDTH - 1.0, HEIGHT - 1.0)),
        ];
        let mut expected = Vec::new();
        for (viewport, size) in viewports {
            let isolated = egui::Context::default();
            install_fonts(&isolated);
            let output = paint_frame(&isolated, viewport, size, 0.0, vec![], |ui| {
                draw(ui, &Phase::Prompt, Instant::now(), &screens, &physical_rects(&screens), None, &mut LayoutEdit::default());
            });
            let rects = screen_rects(&output);
            assert_eq!(rects.len(), screens.len());
            expected.push(rects);
        }
        for frame in 0..120 {
            for (i, (viewport, size)) in viewports.into_iter().enumerate() {
                let output = paint_frame(&ctx, viewport, size, frame as f64 / 60.0, vec![], |ui| {
                    draw(ui, &Phase::Prompt, Instant::now(), &screens, &physical_rects(&screens), None, &mut LayoutEdit::default());
                });
                assert_eq!(screen_rects(&output), expected[i], "frame {frame}, monitor {i}");
            }
        }
    }

    #[test]
    fn dropping_a_physically_sized_tile_saves_its_desktop_position_on_keep() {
        use egui::{Event, Modifiers, PointerButton};

        let ctx = egui::Context::default();
        install_fonts(&ctx);
        let screens = vec![
            screen("portrait", false, false, (-1080, 0, 1080, 1920), (597.0, 336.0)),
            screen("main", false, true, (0, 0, 1920, 1080), (530.0, 300.0)),
        ];
        let since = Instant::now();
        let state = Arc::new(Mutex::new(Shared { phase: Phase::Prompt, since, screens: screens.clone(), needs_taskbar_refresh: false, pointer_settlement: None }));
        let mut card = Card::new(state.clone(), Some(Preview { kind: "portrait".into() }), None);
        let mut time = 0.0;
        let mut frame = |events, edit: &mut LayoutEdit| {
            let mut action = None;
            let output = paint_frame(&ctx, ViewportId::ROOT, vec2(WIDTH, HEIGHT), time, events, |ui| {
                action = draw(ui, &Phase::Prompt, since, &screens, &physical_rects(&screens), None, edit);
            });
            time += 0.1;
            (screen_rects(&output), action)
        };
        let (rects, _) = frame(vec![], &mut card.edit);
        let start = rects[0].center();
        let scale = rects[1].width() / 530.0;
        let end = start + vec2(530.0 + 336.0 + 20.0, 300.0 * 300.0 / 1080.0) * scale;
        let mouse = |pos, pressed| Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        frame(vec![Event::PointerMoved(start), mouse(start, true)], &mut card.edit);
        let far = end + vec2(30.0, 0.0);
        let (dragged, action) = frame(vec![Event::PointerMoved(far)], &mut card.edit);
        assert!(card.edit.drag.is_some());
        assert!(action.is_none());
        assert!((dragged[1].center() - far).length() < 0.01);
        let (snapped, action) = frame(vec![Event::PointerMoved(end)], &mut card.edit);
        assert!(action.is_none());
        assert!((snapped[1].left() - snapped[0].right()).abs() < 0.01);
        assert_eq!(state.lock().unwrap().screens, screens);
        let (_, drop) = frame(vec![mouse(end, false)], &mut card.edit);
        card.act(drop.expect("drop action"), &ctx);
        assert_eq!(state.lock().unwrap().screens, screens);
        let moved = card.draft.as_ref().unwrap().screens.clone();
        assert_eq!((moved[0].x, moved[0].y), (1920, 300));

        let keep = pos2(WIDTH - PAD - 50.0, HEIGHT - 37.0);
        frame(vec![Event::PointerMoved(keep), mouse(keep, true)], &mut card.edit);
        let (_, action) = frame(vec![mouse(keep, false)], &mut card.edit);
        assert!(matches!(action, Some(Action::Keep)));
        card.act(action.unwrap(), &ctx);
        let saved = state.lock().unwrap();
        assert_eq!(saved.screens, moved);
        assert!(matches!(saved.phase, Phase::Kept));
        assert!(card.closing.is_none());
    }

    #[test]
    fn snaps_to_aligned_corners_from_either_axis() {
        let screens = [
            screen("main", false, true, (0, 0, 200, 120), (200.0, 120.0)),
            screen("other", false, false, (-80, 0, 80, 60), (80.0, 60.0)),
        ];

        assert_eq!(snap_position(&screens, 1, (204, 6)), Some((200, 0)));
        assert_eq!(snap_position(&screens, 1, (202, 57)), Some((200, 60)));
        assert_eq!(snap_position(&screens, 1, (4, 121)), Some((0, 120)));
        assert_eq!(snap_position(&screens, 1, (118, 122)), Some((120, 120)));
        assert_eq!(snap_position(&screens, 1, (202, 22)), Some((200, 22)));
    }

    #[test]
    fn snaps_to_the_corner_shared_by_two_neighbours() {
        let screens = [
            screen("main", false, true, (0, 0, 200, 180), (200.0, 180.0)),
            screen("upper", false, false, (-100, 0, 100, 90), (100.0, 90.0)),
            screen("moving", false, false, (200, 0, 100, 60), (100.0, 60.0)),
        ];

        assert_eq!(snap_position(&screens, 2, (-103, 95)), Some((-100, 90)));
    }

    #[test]
    fn laptop_fits_below_main_and_beside_portrait_with_different_pixel_densities() {
        let mut screens = [
            screen("laptop", true, false, (-3840, 890, 3840, 2400), (344.0, 215.0)),
            screen("portrait", false, false, (2560, -443, 1080, 1920), (530.0, 280.0)),
            screen("main", false, true, (0, 0, 2560, 1440), (597.0, 336.0)),
        ];
        let rects = physical_rects(&screens);
        let desired = pos2(rects[1].left() - rects[0].width() + 4.0, rects[2].bottom() + 3.0);

        let (x, y) = snap_to_edge(&screens, &rects, 0, desired, 0.31).unwrap().position;
        assert_eq!((x, y), (-1280, 1440));
        (screens[0].x, screens[0].y) = (x, y);
        assert!(display::valid_layout(&screens.iter().map(Screen::area).collect::<Vec<_>>()));

        let [laptop, portrait, main]: [Rect; 3] = physical_rects(&screens).try_into().unwrap();
        assert_eq!(laptop.size(), vec2(344.0, 215.0));
        assert_eq!(laptop.top(), main.bottom());
        assert_eq!(laptop.right(), portrait.left());
    }

    #[test]
    fn laptop_slides_lower_beside_portrait_and_above_main() {
        let mut screens = [
            screen("main", false, true, (0, 0, 2560, 1440), (597.0, 336.0)),
            screen("portrait", false, false, (-1080, -1480, 1080, 1920), (530.0, 280.0)),
            screen("laptop", true, false, (0, -3250, 3840, 2400), (344.0, 215.0)),
        ];
        let rects = physical_rects(&screens);
        let desired = pos2(-3.0, -342.0);
        assert!(rects[2].top() < desired.y);
        let snap = snap_to_edge(&screens, &rects, 2, desired, 0.258).unwrap();
        assert!(snap.near(0.258));
        assert_eq!(snap.position.0, 0);
        assert!((screens[2].y + 1..-2400).contains(&snap.position.1));
        assert_eq!(snap.rect.top(), desired.y);

        screens[2].y = snap.position.1;
        assert!(display::valid_layout(&screens.iter().map(Screen::area).collect::<Vec<_>>()));
        let reopened = physical_rects(&screens);
        assert!((reopened[2].top() - desired.y).abs() < 0.3);

        let corner = snap_to_edge(&screens, &reopened, 2, pos2(-3.0, -220.0), 0.258).unwrap();
        assert_eq!(corner.position, (0, -2400));
        assert_eq!(corner.rect.bottom(), reopened[0].top());
    }

    #[test]
    fn drags_laptop_straight_from_the_inside_corner_to_the_left_of_main() {
        use egui::{Event, Modifiers, PointerButton};

        let ctx = egui::Context::default();
        install_fonts(&ctx);
        let screens = vec![
            screen("laptop", true, false, (-1280, 1440, 3840, 2400), (344.0, 215.0)),
            screen("portrait", false, false, (2560, -443, 1080, 1920), (530.0, 280.0)),
            screen("main", false, true, (0, 0, 2560, 1440), (597.0, 336.0)),
        ];
        let state = Arc::new(Mutex::new(Shared {
            phase: Phase::Prompt, since: Instant::now(), screens: screens.clone(),
            needs_taskbar_refresh: false, pointer_settlement: None,
        }));
        let mut card = Card::new(state.clone(), Some(Preview { kind: "portrait".into() }), None);
        let diagram = physical_rects(&screens);
        let mut time = 0.0;
        let mut frame = |events, edit: &mut LayoutEdit| {
            let mut action = None;
            let output = paint_frame(&ctx, ViewportId::ROOT, vec2(WIDTH, HEIGHT), time, events, |ui| {
                action = draw(ui, &Phase::Prompt, Instant::now(), &screens, &diagram, None, edit);
            });
            time += 0.1;
            (screen_rects(&output), action)
        };
        let (tiles, _) = frame(vec![], &mut card.edit);
        let [laptop, _, main]: [Rect; 3] = tiles.try_into().unwrap();
        // The full tile is a drag target.
        let start = laptop.left_top() + vec2(3.0, laptop.height() * 0.72);
        let desired = pos2(main.left() - laptop.width() + 4.0, main.top() + 5.0);
        let end = start + (desired - laptop.min);
        let mouse = |pos, pressed| Event::PointerButton {
            pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE,
        };

        frame(vec![Event::PointerMoved(start), mouse(start, true)], &mut card.edit);
        let (snapped, action) = frame(vec![Event::PointerMoved(end)], &mut card.edit);
        assert!(card.edit.drag.is_some(), "The laptop tile must drag from its edge.");
        assert!(action.is_none());
        let tile = snapped.last().unwrap();
        assert!((tile.right() - main.left()).abs() < 0.01);
        assert!((tile.top() - main.top()).abs() < 0.01);
        let (_, drop) = frame(vec![mouse(end, false)], &mut card.edit);
        card.act(drop.expect("laptop drop"), &ctx);

        let staged = &card.draft.as_ref().unwrap().screens;
        assert_eq!((staged[0].x, staged[0].y), (-3840, 0));
        assert_eq!(staged[1..], screens[1..]);
        assert_eq!(state.lock().unwrap().screens, screens);
    }

    #[test]
    fn bottom_corner_alignment_uses_physical_sizes_at_mixed_resolutions() {
        let screens = [
            screen("main", false, true, (0, 0, 1920, 1080), (530.0, 300.0)),
            screen("portrait", false, false, (-1080, 0, 1080, 1920), (597.0, 336.0)),
        ];
        let rects = physical_rects(&screens);
        let wanted = pos2(536.0, 300.0 - 597.0 + 14.0);
        let (x, y) = snap_to_edge(&screens, &rects, 1, wanted, 0.4).unwrap().position;
        assert_eq!(x, 1920);
        let top = offset_in_mm(y, 1920, 1080, 597.0, 300.0);
        assert!((top + 597.0 - 300.0).abs() < 0.2);
    }

    #[test]
    fn snapping_allows_a_bridge_display_to_move_to_another_edge() {
        let screens = [
            screen("left", false, false, (-200, 0, 100, 100), (1.0, 1.0)),
            screen("main", false, true, (0, 0, 100, 100), (1.0, 1.0)),
            screen("laptop", true, false, (-100, 0, 100, 100), (1.0, 1.0)),
        ];
        assert_eq!(snap_position(&screens, 2, (102, 4)), Some((100, 0)));
    }

    #[test]
    fn moving_the_laptop_keeps_the_detached_display_in_place_until_reconnected() {
        use egui::{Event, Modifiers, PointerButton};

        let ctx = egui::Context::default();
        install_fonts(&ctx);
        let screens = vec![
            screen("left", false, false, (-200, 0, 100, 100), (100.0, 100.0)),
            screen("laptop", true, false, (-100, 0, 100, 100), (100.0, 100.0)),
            screen("main", false, true, (0, 0, 100, 100), (100.0, 100.0)),
        ];
        let diagram = physical_rects(&screens);
        let since = Instant::now();
        let state = Arc::new(Mutex::new(Shared {
            phase: Phase::Prompt, since, screens: screens.clone(), needs_taskbar_refresh: false, pointer_settlement: None,
        }));
        let mut card = Card::new(state.clone(), Some(Preview { kind: "portrait".into() }), None);
        let mut time = 0.0;
        let mut frame = |events, card: &mut Card| {
            let (screens, rects) = card.draft.as_ref().map_or((&screens, &diagram), |draft| (&draft.screens, &draft.rects));
            let mut action = None;
            let output = paint_frame(&ctx, ViewportId::ROOT, vec2(WIDTH, HEIGHT), time, events, |ui| {
                action = draw(ui, &Phase::Prompt, since, screens, rects, None, &mut card.edit);
            });
            time += 0.1;
            (output, action)
        };
        let mouse = |pos, pressed| Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        let (initial, _) = frame(vec![], &mut card);
        let tiles = screen_rects(&initial);
        let start = tiles[1].center();
        let end = tiles[2].center() - vec2(0.0, tiles[1].height());
        frame(vec![Event::PointerMoved(start), mouse(start, true)], &mut card);
        frame(vec![Event::PointerMoved(end)], &mut card);
        let (_, action) = frame(vec![mouse(end, false)], &mut card);
        card.act(action.expect("laptop drop"), &ctx);

        let draft = card.draft.as_ref().unwrap();
        assert_eq!(draft.screens[0], screens[0]);
        assert_eq!(draft.rects[0], diagram[0]);
        assert_eq!(draft.rects[2], diagram[2]);
        assert_eq!(connected_to_main(&draft.screens), [false, true, true]);

        let (output, _) = frame(vec![], &mut card);
        let detached = screen_rects(&output)[0];
        assert!(output.shapes.iter().any(|shape| {
            matches!(&shape.shape, Shape::Circle(icon) if icon.fill == BAD && detached.contains(icon.center))
        }));
        frame(vec![Event::PointerMoved(detached.center())], &mut card);
        for _ in 0..7 {
            frame(vec![], &mut card);
        }
        let (hovered, _) = frame(vec![], &mut card);
        text_center(&hovered, "Detached from the main display");

        let keep = pos2(WIDTH - PAD - 50.0, HEIGHT - 37.0);
        frame(vec![Event::PointerMoved(keep), mouse(keep, true)], &mut card);
        assert!(frame(vec![mouse(keep, false)], &mut card).1.is_none());
        let enter = Event::Key { key: Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE };
        assert!(frame(vec![enter], &mut card).1.is_none());
        card.act(Action::Keep, &ctx);
        assert_eq!(state.lock().unwrap().screens, screens);

        let (output, _) = frame(vec![], &mut card);
        let tiles = screen_rects(&output);
        let start = tiles[0].center();
        let end = tiles[2].center() - vec2(tiles[0].width(), 0.0);
        frame(vec![Event::PointerMoved(start), mouse(start, true)], &mut card);
        frame(vec![Event::PointerMoved(end)], &mut card);
        let (_, action) = frame(vec![mouse(end, false)], &mut card);
        card.act(action.expect("reattach the left display"), &ctx);
        assert_eq!(connected_to_main(&card.draft.as_ref().unwrap().screens), [true, true, true]);

        let (reattached, _) = frame(vec![], &mut card);
        assert!(!reattached.shapes.iter().any(|shape| matches!(&shape.shape, Shape::Circle(icon) if icon.fill == BAD)));
        frame(vec![Event::PointerMoved(keep), mouse(keep, true)], &mut card);
        let (_, action) = frame(vec![mouse(keep, false)], &mut card);
        assert!(matches!(action, Some(Action::Keep)));
        card.act(action.unwrap(), &ctx);
        assert!(matches!(state.lock().unwrap().phase, Phase::Kept));
    }

    #[test]
    fn dropping_clear_of_an_edge_preserves_the_gap() {
        let mut screens = [
            screen("main", false, true, (0, 0, 200, 100), (200.0, 100.0)),
            screen("laptop", true, false, (200, 0, 400, 200), (130.0, 65.0)),
        ];
        let rects = physical_rects(&screens);
        let target = rects[1].translate(vec2(30.0, 0.0));
        let snap = snap_to_edge(&screens, &rects, 1, target.min, 1.0);
        let Action::MoveScreen { position, rect, .. } = drop_tile(&screens, &rects, 1, target, snap, 1.0) else {
            panic!("expected a move");
        };
        (screens[1].x, screens[1].y) = position;
        assert_eq!(rect, target);
        assert_eq!(connected_to_main(&screens), [true, false]);
    }

    fn text_center(output: &egui::FullOutput, label: &str) -> Pos2 {
        output.shapes.iter().find_map(|shape| match &shape.shape {
            Shape::Text(text) if text.galley.text() == label => Some(text.pos + text.galley.size() / 2.0),
            _ => None,
        }).unwrap_or_else(|| panic!("could not find {label}"))
    }

    #[test]
    fn scaling_menu_applies_to_one_display_and_revert_restores_it() {
        use egui::{Event, Modifiers, PointerButton};

        let ctx = egui::Context::default();
        install_fonts(&ctx);
        let preview = Preview { kind: "portrait".into() };
        let screens = preview.screens();
        let since = Instant::now();
        let state = Arc::new(Mutex::new(Shared { phase: Phase::Prompt, since, screens: screens.clone(), needs_taskbar_refresh: false, pointer_settlement: None }));
        let mut card = Card::new(state.clone(), Some(preview), None);
        card.act(Action::SetMain(0), &ctx);
        assert_eq!(state.lock().unwrap().screens, screens);
        let mut time = 0.0;
        let mut frame = |events, edit: &mut LayoutEdit| {
            let mut action = None;
            let current = state.lock().unwrap().screens.clone();
            let output = paint_frame(&ctx, ViewportId::ROOT, vec2(WIDTH, HEIGHT), time, events, |ui| {
                action = draw(ui, &Phase::Prompt, since, &current, &physical_rects(&current), Some(&current[1].id), edit);
            });
            time += 0.1;
            (output, action)
        };
        let mouse = |pos, button, pressed| Event::PointerButton { pos, button, pressed, modifiers: Modifiers::NONE };
        let (output, _) = frame(vec![], &mut card.edit);
        text_center(&output, "Display Preview");
        text_center(&output, "Drag displays until correct. Right-click to set main display and scaling");
        assert!(text_center(&output, "This Display · Main · 27\"").y > text_center(&output, &screens[1].name).y);
        let tile = screen_rects(&output)[1].center();
        frame(vec![Event::PointerMoved(tile), mouse(tile, PointerButton::Secondary, true)], &mut card.edit);
        frame(vec![mouse(tile, PointerButton::Secondary, false)], &mut card.edit);
        let (menu, _) = frame(vec![], &mut card.edit);
        let scale = text_center(&menu, "Adjust scaling");
        frame(vec![Event::PointerMoved(scale), mouse(scale, PointerButton::Primary, true)], &mut card.edit);
        frame(vec![mouse(scale, PointerButton::Primary, false)], &mut card.edit);
        let (popup, _) = frame(vec![], &mut card.edit);
        let option = text_center(&popup, "125%");
        frame(vec![Event::PointerMoved(option), mouse(option, PointerButton::Primary, true)], &mut card.edit);
        let (_, action) = frame(vec![mouse(option, PointerButton::Primary, false)], &mut card.edit);
        assert!(matches!(action, Some(Action::SetScale(1, 125))));
        card.act(action.unwrap(), &ctx);
        let applied = state.lock().unwrap().screens.clone();
        assert_eq!(applied[1].scale.unwrap().current, 125);
        assert_eq!(applied[0].scale, screens[0].scale);
        assert!(card.draft.as_ref().unwrap().screens[0].primary);
        assert_eq!(applied[0].area(), screens[0].area());
        assert!(!applied[0].primary);

        let revert = pos2(WIDTH - PAD - 168.0, HEIGHT - 37.0);
        frame(vec![Event::PointerMoved(revert), mouse(revert, PointerButton::Primary, true)], &mut card.edit);
        let (_, action) = frame(vec![mouse(revert, PointerButton::Primary, false)], &mut card.edit);
        assert!(matches!(action, Some(Action::Revert)));
        card.act(action.unwrap(), &ctx);
        assert!(card.draft.is_none());
        assert_eq!(state.lock().unwrap().screens, screens);
    }

    #[test]
    fn main_menu_stages_the_primary_without_changing_relative_positions() {
        use egui::{Event, Modifiers, PointerButton};

        let ctx = egui::Context::default();
        install_fonts(&ctx);
        let preview = Preview { kind: "portrait".into() };
        let screens = preview.screens();
        let since = Instant::now();
        let state = Arc::new(Mutex::new(Shared {
            phase: Phase::Prompt, since, screens: screens.clone(), needs_taskbar_refresh: false, pointer_settlement: None,
        }));
        let mut card = Card::new(state.clone(), Some(preview), None);
        let portrait = screens.iter().position(|s| s.rotation.swaps_axes()).unwrap();
        assert!(!screens[portrait].primary);

        let mut time = 0.0;
        let mut frame = |events, edit: &mut LayoutEdit| {
            let mut action = None;
            let output = paint_frame(&ctx, ViewportId::ROOT, vec2(WIDTH, HEIGHT), time, events, |ui| {
                action = draw(ui, &Phase::Prompt, since, &screens, &physical_rects(&screens), None, edit);
            });
            time += 0.1;
            (output, action)
        };
        let (output, _) = frame(vec![], &mut card.edit);
        let tile = screen_rects(&output)[portrait].center();
        let mouse = |pos, button, pressed| Event::PointerButton { pos, button, pressed, modifiers: Modifiers::NONE };
        frame(vec![Event::PointerMoved(tile), mouse(tile, PointerButton::Secondary, true)], &mut card.edit);
        frame(vec![mouse(tile, PointerButton::Secondary, false)], &mut card.edit);
        let (menu, _) = frame(vec![], &mut card.edit);
        let main = text_center(&menu, "Set to main display");
        frame(vec![Event::PointerMoved(main), mouse(main, PointerButton::Primary, true)], &mut card.edit);
        let (_, action) = frame(vec![mouse(main, PointerButton::Primary, false)], &mut card.edit);
        assert!(matches!(action, Some(Action::SetMain(i)) if i == portrait));
        card.act(action.unwrap(), &ctx);

        assert_eq!(state.lock().unwrap().screens, screens);
        let current = &card.draft.as_ref().unwrap().screens;
        assert_eq!(current.iter().filter(|s| s.primary).count(), 1);
        assert!(current[portrait].primary);
        assert_eq!((current[portrait].x, current[portrait].y), (0, 0));
        for (before, after) in screens.iter().zip(current) {
            assert_eq!((after.x, after.y), (before.x - screens[portrait].x, before.y - screens[portrait].y));
            assert_eq!(after.scale, before.scale);
        }
    }

    #[test]
    fn button_fill_stays_inside_rounded_corners() {
        let ctx = egui::Context::default();
        install_fonts(&ctx);
        ctx.tessellation_options_mut(|o| o.prerasterized_discs = false);
        let rect = Rect::from_min_size(pos2(20.0, 20.0), vec2(118.0, 34.0));
        for primary in [false, true] {
            let output = paint_frame(&ctx, ViewportId::ROOT, vec2(WIDTH, HEIGHT), 0.0, vec![], |ui| {
                button(ui, ui.painter(), rect, "test button", "Keep", primary, true);
            });
            let glyph_center = output.shapes.iter().find_map(|shape| match &shape.shape {
                Shape::Text(text) if text.galley.text() == "Keep" => Some(text.pos + text.galley.mesh_bounds.center().to_vec2()),
                _ => None,
            }).unwrap();
            assert!((glyph_center - rect.center()).length() < 0.01);
            let meshes = ctx.tessellate(output.shapes, output.pixels_per_point);
            let covered = |point: Pos2| meshes.iter().any(|p| {
                let egui::epaint::Primitive::Mesh(mesh) = &p.primitive else { return false };
                mesh.indices.chunks_exact(3).any(|t| {
                    let vertices = [t[0], t[1], t[2]].map(|i| mesh.vertices[i as usize]);
                    if vertices.iter().all(|v| v.color.a() == 0) {
                        return false;
                    }
                    let [a, b, c] = vertices.map(|v| v.pos);
                    let cross = |u: Vec2, v: Vec2| u.x * v.y - u.y * v.x;
                    let sides = [cross(b - a, point - a), cross(c - b, point - b), cross(a - c, point - c)];
                    sides.iter().all(|s| *s > 0.0) || sides.iter().all(|s| *s < 0.0)
                })
            });
            assert!(covered(rect.center() + vec2(35.0, 0.5)), "button fill is missing");
            for corner in [rect.left_top(), rect.right_top(), rect.left_bottom(), rect.right_bottom()] {
                let direction = rect.center() - corner;
                let point = corner + vec2(direction.x.signum(), direction.y.signum()) * 2.0;
                assert!(!covered(point), "square fill at {point:?}, primary = {primary}");
            }
        }
    }
}
