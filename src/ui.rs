//! The on-screen card shown while auto-display fixes duplicated displays.
//!
//! Runs in its own short-lived process (`auto-display setup`) so the watcher stays a plain Win32
//! message loop. A worker thread does the display changes and publishes progress; the card shows it,
//! and for the two-monitor layout asks whether to swap the monitors, keeping after 10 seconds.

use std::collections::HashMap;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{sleep, spawn};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, FontData, FontDefinitions, FontFamily, FontId, Id, Key, Mesh,
    Painter, Rect, Sense, Shape, Stroke, StrokeKind, Vec2, ViewportBuilder, ViewportCommand, ViewportId, pos2, vec2,
};
use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HWND, LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GWL_EXSTYLE, GetWindowLongPtrW, MONITORINFOF_PRIMARY, GetWindowRect, HWND_TOPMOST, LWA_ALPHA, SWP_NOACTIVATE, SWP_NOSIZE,
    SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPos, WS_EX_LAYERED,
};

use crate::display::{self, Screen};
use crate::{fix, log, watch};

const WIDTH: f32 = 540.0;
const HEIGHT: f32 = 320.0;
const PAD: f32 = 28.0;
const KEEP_AFTER: Duration = Duration::from_secs(10);
const DONE_FOR: Duration = Duration::from_millis(2800);
const FAILED_FOR: Duration = Duration::from_secs(10);
const FADE_IN: f32 = 0.22;
const FADE_OUT: f32 = 0.25;

const BG: Color32 = Color32::from_rgb(0x14, 0x16, 0x1b);
const SURFACE: Color32 = Color32::from_rgb(0x1b, 0x1e, 0x25);
const SCREEN: Color32 = Color32::from_rgb(0x2a, 0x2f, 0x3b);
const SCREEN_EDGE: Color32 = Color32::from_rgb(0x3a, 0x40, 0x4e);
const BUTTON: Color32 = Color32::from_rgb(0x26, 0x2a, 0x34);
const BUTTON_HOVER: Color32 = Color32::from_rgb(0x31, 0x36, 0x43);
const TEXT: Color32 = Color32::from_rgb(0xf2, 0xf4, 0xf8);
const MUTED: Color32 = Color32::from_rgb(0x8b, 0x93, 0xa3);
const ACCENT: Color32 = Color32::from_rgb(0x7c, 0x6c, 0xff);
const ACCENT_HOVER: Color32 = Color32::from_rgb(0x8f, 0x81, 0xff);
const ACCENT_2: Color32 = Color32::from_rgb(0x22, 0xd3, 0xee);
const GOOD: Color32 = Color32::from_rgb(0x34, 0xd3, 0x99);
const BAD: Color32 = Color32::from_rgb(0xf8, 0x71, 0x71);

#[derive(Clone, PartialEq)]
enum Phase {
    Working(String),
    Prompt,
    Done(String),
    Failed(String),
}

struct Shared {
    phase: Phase,
    since: Instant,
    screens: Vec<Screen>,
}

type State = Arc<Mutex<Shared>>;

fn set_phase(state: &State, ctx: &egui::Context, phase: Phase) {
    set_phase_with(state, ctx, phase, display::screens().ok());
}

fn set_phase_with(state: &State, ctx: &egui::Context, phase: Phase, screens: Option<Vec<Screen>>) {
    let mut s = state.lock().unwrap();
    s.phase = phase;
    s.since = Instant::now();
    if let Some(screens) = screens {
        s.screens = screens;
    }
    drop(s);
    ctx.request_repaint();
}

/// Fixes duplicated displays with the card on screen. Returns false if they were not duplicated.
pub fn run() -> Result<bool, String> {
    let name = watch::wide("Local\\auto-display-setup");
    let mutex = unsafe { CreateMutexW(null(), 0, name.as_ptr()) };
    if !mutex.is_null() && unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Ok(true); // Another setup is already on it.
    }
    if !display::is_duplicate(&display::active()?.paths) {
        return Ok(false);
    }

    let started = show(display::screens().unwrap_or_default(), None, worker)?;
    if !started {
        fix::if_duplicate()?;
    }
    Ok(true)
}

/// Plays the card with made-up screens and changes nothing. `kind` is "dual" (two monitors),
/// "single" (one ultrawide), "done" (nothing to ask) or "error".
pub fn preview(kind: &str) -> Result<(), String> {
    let kind = kind.to_owned();
    let start = match kind.as_str() {
        "single" | "done" => vec![fake("Laptop + DELL U4021QW", false, true, 0, 0, 3840, 2400)],
        _ => vec![fake("Laptop + DELL U2723QE + LG 27UL850", false, true, 0, 0, 3840, 2400)],
    };
    show(start, Some(kind.clone()), move |state, ctx| {
        let step = |phase: Phase, screens: Vec<Screen>, ms: u64| {
            set_phase_with(&state, &ctx, phase, Some(screens));
            sleep(Duration::from_millis(ms));
        };
        sleep(Duration::from_millis(1400));
        let arranged = preview_screens(&kind, false);
        step(Phase::Working("Arranging your screens".into()), arranged.clone(), 1000);
        step(Phase::Working(format!("Setting external displays to {}%", fix::EXTERNAL_SCALE)), arranged.clone(), 800);
        let last = match kind.as_str() {
            "error" => Phase::Failed("SetDisplayConfig (arrange) failed with error 87".into()),
            "done" => Phase::Done("DELL U4021QW is your main display.".into()),
            _ => Phase::Prompt,
        };
        set_phase_with(&state, &ctx, last, Some(arranged));
    })
    .map(|_| ())
}

fn fake(name: &str, internal: bool, primary: bool, x: i32, y: i32, width: i32, height: i32) -> Screen {
    Screen { id: name.into(), name: name.into(), internal, primary, x, y, width, height }
}

fn preview_screens(kind: &str, swapped: bool) -> Vec<Screen> {
    if matches!(kind, "single" | "done") {
        let laptop_x = if swapped { 5120 } else { -3840 };
        return vec![
            fake("Laptop", true, false, laptop_x, 1080, 3840, 2400),
            fake("DELL U4021QW", false, true, 0, 0, 5120, 2160),
        ];
    }
    let (left, right) = if swapped { ("LG 27UL850", "DELL U2723QE") } else { ("DELL U2723QE", "LG 27UL850") };
    vec![
        fake(left, false, false, -2560, 0, 2560, 1440),
        fake(right, false, true, 0, 0, 2560, 1440),
        fake("Laptop", true, false, -1920, 1440, 3840, 2400),
    ]
}

/// Shows the card and runs `work` alongside it. Returns false if the window never started.
fn show(
    screens: Vec<Screen>,
    preview: Option<String>,
    work: impl FnOnce(State, egui::Context) + Send + 'static,
) -> Result<bool, String> {
    let state: State = Arc::new(Mutex::new(Shared {
        phase: Phase::Working("Switching to extend".into()),
        since: Instant::now(),
        screens,
    }));
    let started = Arc::new(AtomicBool::new(false));
    let options = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_app_id("auto-display")
            .with_title("auto-display")
            .with_inner_size([WIDTH, HEIGHT])
            .with_decorations(false)
            .with_always_on_top()
            .with_taskbar(false)
            .with_resizable(false)
            .with_active(false)
            .with_visible(false),
        ..Default::default()
    };
    let (app_state, app_started) = (state.clone(), started.clone());
    let result = eframe::run_native(
        "auto-display",
        options,
        Box::new(move |cc| {
            install_fonts(&cc.egui_ctx);
            app_started.store(true, Ordering::SeqCst);
            let (worker_state, ctx) = (app_state.clone(), cc.egui_ctx.clone());
            spawn(move || work(worker_state, ctx));
            Ok(Box::new(Card::new(app_state, preview)))
        }),
    );
    if let Err(e) = result {
        log::line(&format!("Could not show the setup card: {e}"));
    }
    Ok(started.load(Ordering::SeqCst))
}

fn worker(state: State, ctx: egui::Context) {
    // Let the card fade in before the screens start flickering.
    sleep(Duration::from_millis(500));
    let result = (|| -> Result<bool, String> {
        log::line("Displays are duplicated; switching to extend.");
        display::extend()?;
        sleep(Duration::from_millis(1500));
        set_phase(&state, &ctx, Phase::Working("Arranging your screens".into()));
        sleep(Duration::from_millis(400));
        let arrangement = fix::arrange(false)?;
        set_phase(&state, &ctx, Phase::Working(format!("Setting external displays to {}%", fix::EXTERNAL_SCALE)));
        sleep(Duration::from_millis(400));
        fix::scale()?;
        Ok(arrangement.swappable)
    })();
    let phase = match result {
        Ok(true) => Phase::Prompt,
        Ok(false) => {
            let main = display::screens().ok().and_then(|s| s.into_iter().find(|s| s.primary)).map(|s| s.name);
            Phase::Done(match main {
                Some(name) => format!("{name} is your main display."),
                None => "Your displays are extended.".into(),
            })
        }
        Err(e) => {
            log::line(&format!("error: {e}"));
            Phase::Failed(e)
        }
    };
    set_phase(&state, &ctx, phase);
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
    Swap,
    Close,
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
    /// Work areas of the desktop monitors, main display first.
    monitors: Vec<RECT>,
    /// Opacity for this frame, shared by every window.
    alpha: u8,
    windows: HashMap<ViewportId, CardWindow>,
    /// The preview kind when playing with made-up screens.
    preview: Option<String>,
    swapped: bool,
}

impl Card {
    fn new(state: State, preview: Option<String>) -> Self {
        Card {
            state,
            shown: None,
            closing: None,
            monitors: Vec::new(),
            alpha: 0,
            windows: HashMap::new(),
            preview,
            swapped: false,
        }
    }

    /// Styles, fades and centres one card window on its monitor, and shows it once it is ready.
    fn manage(&mut self, ctx: &egui::Context, id: ViewportId, hwnd: Option<isize>, monitor: usize) {
        let alpha = self.alpha;
        let work = self.monitors.get(monitor).copied();
        let win = self.windows.entry(id).or_default();
        if win.hwnd.is_none() {
            win.hwnd = hwnd;
            if let Some(h) = hwnd {
                style_window(h);
                set_alpha(h, 0);
                win.alpha = Some(0);
            }
        }
        let Some(h) = win.hwnd else { return };
        if let Some(work) = work {
            centre_in(h, work);
        }
        if !win.shown {
            ctx.send_viewport_cmd_to(id, ViewportCommand::Visible(true));
            win.shown = true;
        }
        if win.alpha != Some(alpha) {
            set_alpha(h, alpha);
            win.alpha = Some(alpha);
        }
    }

    fn close(&mut self) {
        self.closing.get_or_insert_with(Instant::now);
    }

    fn act(&mut self, action: Action, ctx: &egui::Context) {
        match action {
            Action::Keep => {
                if self.preview.is_none() {
                    log::line("Kept the layout.");
                }
                self.close();
            }
            Action::Close => self.close(),
            Action::Swap if self.preview.is_some() => {
                self.swapped = !self.swapped;
                let current = self.state.lock().unwrap().screens.clone();
                set_phase_with(&self.state, ctx, Phase::Working("Swapping sides".into()), Some(current));
                let (state, ctx, swapped) = (self.state.clone(), ctx.clone(), self.swapped);
                let kind = self.preview.clone().unwrap_or_default();
                spawn(move || {
                    sleep(Duration::from_millis(700));
                    set_phase_with(&state, &ctx, Phase::Prompt, Some(preview_screens(&kind, swapped)));
                });
            }
            Action::Swap => {
                set_phase(&self.state, ctx, Phase::Working("Swapping sides".into()));
                let (state, ctx) = (self.state.clone(), ctx.clone());
                spawn(move || {
                    let phase = match fix::arrange(true) {
                        Ok(_) => Phase::Prompt,
                        Err(e) => {
                            log::line(&format!("error: {e}"));
                            Phase::Failed(e)
                        }
                    };
                    set_phase(&state, &ctx, phase);
                });
            }
        }
    }
}

impl eframe::App for Card {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        BG.to_normalized_gamma_f32()
    }

    /// Window management lives here because `ui` does not run while the window is hidden.
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        let now = Instant::now();
        // The screens move under us while we work, so look them up every frame.
        self.monitors = monitors();
        let fade_in = self.shown.map_or(0.0, |t| (now - t).as_secs_f32() / FADE_IN).clamp(0.0, 1.0);
        let fade_out = self.closing.map_or(1.0, |t| 1.0 - (now - t).as_secs_f32() / FADE_OUT).clamp(0.0, 1.0);
        self.alpha = (ease_out(fade_in) * fade_out * 255.0).round() as u8;

        self.manage(ctx, ViewportId::ROOT, hwnd(frame), 0);
        if self.windows.get(&ViewportId::ROOT).is_none_or(|w| w.hwnd.is_none()) {
            ctx.request_repaint();
            return;
        }
        self.shown.get_or_insert(now);
        if self.closing.is_some_and(|t| now - t > Duration::from_secs_f32(FADE_OUT + 0.02)) {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }

        let (phase, since) = {
            let s = self.state.lock().unwrap();
            (s.phase.clone(), s.since)
        };
        match phase {
            Phase::Done(_) if since.elapsed() > DONE_FOR => self.close(),
            Phase::Failed(_) if since.elapsed() > FAILED_FOR => self.close(),
            Phase::Prompt if since.elapsed() >= KEEP_AFTER && self.closing.is_none() => {
                self.act(Action::Keep, ctx)
            }
            _ => {}
        }
        ctx.request_repaint_after(Duration::from_millis(16));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let (phase, since, screens) = {
            let s = self.state.lock().unwrap();
            (s.phase.clone(), s.since, s.screens.clone())
        };
        let ctx = ui.ctx().clone();
        let mut action = draw(ui, &phase, since, &screens);

        // The same card on every other monitor; any of them can be clicked.
        for monitor in 1..self.monitors.len() {
            let id = ViewportId::from_hash_of(("card", monitor));
            let title = format!("auto-display ({})", monitor + 1);
            let builder = ViewportBuilder::default()
                .with_title(title.clone())
                .with_inner_size([WIDTH, HEIGHT])
                .with_decorations(false)
                .with_always_on_top()
                .with_taskbar(false)
                .with_resizable(false)
                .with_active(false)
                .with_visible(false);
            let clicked = ctx.show_viewport_immediate(id, builder, |ui, _| draw(ui, &phase, since, &screens));
            action = action.or(clicked);
            self.manage(&ctx, id, find_window(&title), monitor);
        }

        if let Some(action) = action {
            self.act(action, &ctx);
        }
    }
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

fn draw(ui: &mut egui::Ui, phase: &Phase, since: Instant, screens: &[Screen]) -> Option<Action> {
    let rect = ui.max_rect();
    let painter = ui.painter().clone();
    let t = ui.input(|i| i.time) as f32;

    // Accent bar along the top edge.
    let bar = Rect::from_min_size(rect.min, vec2(rect.width(), 3.0));
    match phase {
        Phase::Working(_) => {
            gradient(&painter, bar, &[(0.0, ACCENT.gamma_multiply(0.35)), (1.0, ACCENT_2.gamma_multiply(0.35))]);
            let p = (t * 0.55).fract() * 1.6 - 0.3;
            let clipped = painter.with_clip_rect(bar);
            gradient(
                &clipped,
                bar,
                &[(p - 0.25, Color32::TRANSPARENT), (p, ACCENT_2), (p + 0.25, Color32::TRANSPARENT)],
            );
        }
        Phase::Prompt => gradient(&painter, bar, &[(0.0, ACCENT), (1.0, ACCENT_2)]),
        Phase::Done(_) => gradient(&painter, bar, &[(0.0, GOOD), (1.0, ACCENT_2)]),
        Phase::Failed(_) => {
            painter.rect_filled(bar, CornerRadius::ZERO, BAD);
        }
    }

    // Icon.
    let c = rect.min + vec2(PAD + 24.0, 58.0);
    let r = 20.0;
    match phase {
        Phase::Working(_) => {
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
        Phase::Prompt => {
            painter.circle_filled(c, r + 2.0, ACCENT.gamma_multiply(0.18));
            let s = Stroke::new(2.4, ACCENT_HOVER);
            painter.line_segment([c + vec2(-9.0, -4.5), c + vec2(9.0, -4.5)], s);
            painter.line_segment([c + vec2(9.0, -4.5), c + vec2(4.5, -9.0)], s);
            painter.line_segment([c + vec2(9.0, 4.5), c + vec2(-9.0, 4.5)], s);
            painter.line_segment([c + vec2(-9.0, 4.5), c + vec2(-4.5, 9.0)], s);
        }
        Phase::Done(_) => {
            painter.circle_filled(c, r + 2.0, GOOD.gamma_multiply(0.18));
            let pop = ease_out((since.elapsed().as_secs_f32() / 0.35).min(1.0));
            let s = Stroke::new(3.0, GOOD);
            let a = c + vec2(-8.0, 0.5);
            let b = c + vec2(-2.5, 6.0);
            let e = c + vec2(9.0, -6.0);
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

    // Title and subtitle.
    let text_x = rect.left() + PAD + 64.0;
    let text_w = rect.right() - PAD - text_x;
    let (title, subtitle) = match phase {
        Phase::Working(step) => ("Setting up your displays".to_string(), format!("{step}{}", dots(t))),
        Phase::Prompt => prompt_text(screens),
        Phase::Done(summary) => ("Displays ready".to_string(), summary.clone()),
        Phase::Failed(e) => ("Couldn't set up your displays".to_string(), e.clone()),
    };
    let title_font = semibold(19.0);
    let sub_font = regular(14.0);
    painter.text(
        pos2(text_x, rect.top() + 46.0),
        Align2::LEFT_CENTER,
        fit(&painter, &title, &title_font, text_w),
        title_font,
        TEXT,
    );
    painter.text(
        pos2(text_x, rect.top() + 71.0),
        Align2::LEFT_CENTER,
        fit(&painter, &subtitle, &sub_font, text_w),
        sub_font,
        MUTED,
    );

    // Diagram of the screens.
    let area = Rect::from_min_max(pos2(rect.left() + PAD, rect.top() + 100.0), pos2(rect.right() - PAD, rect.bottom() - 66.0));
    painter.rect_filled(area, CornerRadius::same(12), SURFACE);
    draw_screens(ui, &painter, area.shrink2(vec2(18.0, 14.0)), screens);

    // Footer.
    let row = Rect::from_min_max(pos2(rect.left() + PAD, rect.bottom() - 52.0), pos2(rect.right() - PAD, rect.bottom() - 18.0));
    let footer_font = regular(13.0);
    let mut action = None;
    match phase {
        Phase::Prompt => {
            let left = KEEP_AFTER.saturating_sub(since.elapsed()).as_secs_f32();
            painter.text(
                pos2(row.left(), row.center().y),
                Align2::LEFT_CENTER,
                format!("Keeping this layout in {}s", left.ceil() as u32),
                footer_font,
                MUTED,
            );
            let keep = Rect::from_min_size(pos2(row.right() - 128.0, row.top()), vec2(128.0, row.height()));
            let swap = Rect::from_min_size(pos2(keep.left() - 10.0 - 118.0, row.top()), vec2(118.0, row.height()));
            if button(ui, &painter, swap, "swap", "Swap sides", false) {
                action = Some(Action::Swap);
            }
            // The countdown drains along the bottom edge of the card.
            let remaining = left / KEEP_AFTER.as_secs_f32();
            let track = Rect::from_min_max(pos2(rect.left(), rect.bottom() - 3.0), rect.max);
            painter.rect_filled(track, CornerRadius::ZERO, BUTTON);
            let fill = Rect::from_min_size(track.min, vec2(track.width() * remaining, track.height()));
            gradient(&painter.with_clip_rect(fill), track, &[(0.0, ACCENT), (1.0, ACCENT_2)]);
            if button(ui, &painter, keep, "keep", "Keep", true) {
                action = Some(Action::Keep);
            }
            ui.input(|i| {
                if i.key_pressed(Key::Enter) || i.key_pressed(Key::Escape) {
                    action = Some(Action::Keep);
                } else if i.key_pressed(Key::S) {
                    action = Some(Action::Swap);
                }
            });
        }
        Phase::Failed(_) => {
            let close = Rect::from_min_size(pos2(row.right() - 104.0, row.top()), vec2(104.0, row.height()));
            let note = fit(&painter, "Details in auto-display.log", &footer_font, close.left() - row.left() - 16.0);
            painter.text(pos2(row.left(), row.center().y), Align2::LEFT_CENTER, note, footer_font, MUTED);
            if button(ui, &painter, close, "close", "Close", false) {
                action = Some(Action::Close);
            }
        }
        _ => {
            painter.text(pos2(row.left(), row.center().y), Align2::LEFT_CENTER, "auto-display", footer_font, MUTED);
        }
    }
    action
}

fn dots(t: f32) -> &'static str {
    ["", ".", "..", "..."][((t * 2.5) as usize) % 4]
}

/// Title and subtitle for the swap prompt: about the two monitors, or about the laptop's side.
fn prompt_text(screens: &[Screen]) -> (String, String) {
    let mut externals: Vec<_> = screens.iter().filter(|s| !s.internal).collect();
    externals.sort_by_key(|s| s.x);
    if let [left, right] = externals.as_slice() {
        let sub = if left.name != right.name {
            format!("{} should be on your left, {} on your right.", left.name, right.name)
        } else {
            "The main display should be the one on your right.".into()
        };
        return ("Are your monitors the right way round?".into(), sub);
    }
    let main = externals.iter().find(|s| s.primary).or(externals.first());
    let laptop = screens.iter().find(|s| s.internal);
    let sub = match (main, laptop) {
        (Some(m), Some(l)) => {
            let side = if l.x >= m.x + m.width { "right" } else { "left" };
            format!("Your laptop should be to the {side} of {}.", m.name)
        }
        _ => "Swap if the screens are the wrong way round.".into(),
    };
    ("Is your laptop on the correct side?".into(), sub)
}

fn draw_screens(ui: &egui::Ui, painter: &Painter, area: Rect, screens: &[Screen]) {
    if screens.is_empty() {
        return;
    }
    let min_x = screens.iter().map(|s| s.x).min().unwrap() as f32;
    let min_y = screens.iter().map(|s| s.y).min().unwrap() as f32;
    let max_x = screens.iter().map(|s| s.x + s.width).max().unwrap() as f32;
    let max_y = screens.iter().map(|s| s.y + s.height).max().unwrap() as f32;
    let (bw, bh) = ((max_x - min_x).max(1.0), (max_y - min_y).max(1.0));
    let scale = (area.width() / bw).min(area.height() / bh);
    let offset = area.center() - vec2(bw, bh) * scale / 2.0;

    let ctx = ui.ctx();
    for s in screens {
        let target = Rect::from_min_size(
            offset + vec2(s.x as f32 - min_x, s.y as f32 - min_y) * scale,
            vec2(s.width as f32, s.height as f32) * scale,
        );
        let id = Id::new(("screen", &s.id));
        let anim = |k: &str, v: f32| ctx.animate_value_with_time(id.with(k), v, 0.35);
        let r = Rect::from_min_size(
            pos2(anim("x", target.left()), anim("y", target.top())),
            vec2(anim("w", target.width()), anim("h", target.height())),
        )
        .shrink(2.0);

        painter.rect_filled(r, CornerRadius::same(6), SCREEN);
        let edge = if s.primary { Stroke::new(1.5, ACCENT) } else { Stroke::new(1.0, SCREEN_EDGE) };
        painter.rect_stroke(r, CornerRadius::same(6), edge, StrokeKind::Inside);
        if s.internal {
            // A hint of a keyboard deck under the laptop screen.
            let deck = Rect::from_min_max(pos2(r.left() - 5.0, r.bottom() + 1.5), pos2(r.right() + 5.0, r.bottom() + 4.5));
            painter.rect_filled(deck, CornerRadius::same(2), SCREEN_EDGE);
        }

        let name_font = regular(12.5);
        let name = label(painter, &s.name, &name_font, r.width() - 12.0);
        if s.primary && r.height() > 38.0 {
            painter.text(r.center() - vec2(0.0, 8.0), Align2::CENTER_CENTER, name, name_font, TEXT);
            painter.text(r.center() + vec2(0.0, 9.0), Align2::CENTER_CENTER, "Main display", semibold(11.0), ACCENT_HOVER);
        } else {
            painter.text(r.center(), Align2::CENTER_CENTER, name, name_font, if s.primary { TEXT } else { MUTED });
        }
    }
}

fn button(
    ui: &egui::Ui,
    painter: &Painter,
    rect: Rect,
    id: &str,
    label: &str,
    primary: bool,
) -> bool {
    let response = ui.interact(rect, ui.id().with(id), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
    let hovered = response.hovered();
    let radius = CornerRadius::same(9);
    let fill = match (primary, hovered) {
        (true, false) => ACCENT,
        (true, true) => ACCENT_HOVER,
        (false, false) => BUTTON,
        (false, true) => BUTTON_HOVER,
    };
    painter.rect_filled(rect, radius, fill);
    if !primary {
        painter.rect_stroke(rect, radius, Stroke::new(1.0, SCREEN_EDGE), StrokeKind::Inside);
    }
    painter.text(rect.center(), Align2::CENTER_CENTER, label, semibold(14.0), TEXT);
    response.clicked()
}

/// A horizontal gradient through `stops` (position 0..1 across `rect`, colour).
fn gradient(painter: &Painter, rect: Rect, stops: &[(f32, Color32)]) {
    let mut mesh = Mesh::default();
    for (i, &(at, color)) in stops.iter().enumerate() {
        let x = rect.left() + rect.width() * at;
        mesh.colored_vertex(pos2(x, rect.top()), color);
        mesh.colored_vertex(pos2(x, rect.bottom()), color);
        if i > 0 {
            let b = i as u32 * 2;
            mesh.add_triangle(b - 2, b - 1, b);
            mesh.add_triangle(b - 1, b, b + 1);
        }
    }
    painter.add(Shape::mesh(mesh));
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

fn hwnd(frame: &eframe::Frame) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match frame.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(win) => Some(win.hwnd.get()),
        _ => None,
    }
}

/// Windows 11 rounded corners, a subtle border and a caption strip the colour of the card.
fn style_window(h: isize) {
    #[link(name = "dwmapi")]
    unsafe extern "system" {
        fn DwmSetWindowAttribute(hwnd: isize, attr: u32, value: *const core::ffi::c_void, size: u32) -> i32;
    }
    const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
    const DWMWA_BORDER_COLOR: u32 = 34;
    const DWMWA_CAPTION_COLOR: u32 = 35;
    const DWMWCP_ROUND: u32 = 2;
    let colorref = |c: Color32| u32::from_le_bytes([c.r(), c.g(), c.b(), 0]);
    let border = colorref(SCREEN_EDGE);
    let caption = colorref(BG);
    unsafe {
        DwmSetWindowAttribute(h, DWMWA_WINDOW_CORNER_PREFERENCE, (&DWMWCP_ROUND as *const u32).cast(), 4);
        DwmSetWindowAttribute(h, DWMWA_BORDER_COLOR, (&border as *const u32).cast(), 4);
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

/// Work areas of the monitors on the desktop, the main display first, then left to right.
fn monitors() -> Vec<RECT> {
    unsafe extern "system" fn collect(monitor: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let list = unsafe { &mut *(data as *mut Vec<(bool, RECT)>) };
        let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if unsafe { GetMonitorInfoW(monitor, &mut info) } != 0 {
            list.push((info.dwFlags & MONITORINFOF_PRIMARY != 0, info.rcWork));
        }
        1
    }
    let mut list: Vec<(bool, RECT)> = Vec::new();
    unsafe { EnumDisplayMonitors(null_mut(), null(), Some(collect), &mut list as *mut _ as LPARAM) };
    list.sort_by_key(|&(primary, r)| (!primary, r.left, r.top));
    list.into_iter().map(|(_, r)| r).collect()
}

fn find_window(title: &str) -> Option<isize> {
    let title = watch::wide(title);
    let h = unsafe { FindWindowW(null(), title.as_ptr()) };
    (!h.is_null()).then_some(h as isize)
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
            SetWindowPos(hwnd, HWND_TOPMOST, x, y, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }
}
