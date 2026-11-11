//! The on-screen card shown while auto-display fixes duplicated displays.
//!
//! Runs in its own short-lived process (`auto-display setup`) so the watcher stays a plain Win32
//! message loop. A worker thread does the display changes and publishes progress; the card shows it
//! on every monitor, then lets the user move the laptop screen (by clicking a faded "ghost" laptop)
//! or swap two monitors, keeping the layout after 10 seconds.

use std::collections::HashMap;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{sleep, spawn};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, FontData, FontDefinitions, FontFamily, FontId, Id, Key, Mesh,
    Painter, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Vec2, ViewportBuilder, ViewportCommand, ViewportId, pos2,
    vec2,
};
use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HWND, LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GWL_EXSTYLE, GetWindowLongPtrW, GetWindowRect, HWND_TOPMOST, LWA_ALPHA, MONITORINFOF_PRIMARY,
    SWP_NOACTIVATE, SWP_NOSIZE, SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPos, WS_EX_LAYERED,
};
use windows_sys::core::BOOL;

use crate::display::{self, Area, SIDES, Screen, Side};
use crate::{fix, log, watch};

const WIDTH: f32 = 540.0;
const HEIGHT: f32 = 372.0;
const PAD: f32 = 28.0;
const KEEP_AFTER: Duration = Duration::from_secs(10);
const DONE_FOR: Duration = Duration::from_millis(2800);
const FAILED_FOR: Duration = Duration::from_secs(10);
const FADE_IN: f32 = 0.22;
const FADE_OUT: f32 = 0.25;
/// The card is slightly see-through (whole-window opacity, out of 255).
const OPACITY: f32 = 236.0;

const BG_TOP: Color32 = Color32::from_rgb(0x1b, 0x1d, 0x26);
const BG_BOTTOM: Color32 = Color32::from_rgb(0x11, 0x12, 0x17);
const SURFACE: Color32 = Color32::from_rgb(0x17, 0x19, 0x20);
const SURFACE_EDGE: Color32 = Color32::from_rgb(0x25, 0x28, 0x32);
const BEZEL: Color32 = Color32::from_rgb(0x0c, 0x0d, 0x11);
const PANEL_TOP: Color32 = Color32::from_rgb(0x34, 0x39, 0x4a);
const PANEL_BOTTOM: Color32 = Color32::from_rgb(0x22, 0x26, 0x31);
const SCREEN_EDGE: Color32 = Color32::from_rgb(0x3a, 0x40, 0x4e);
const BUTTON: Color32 = Color32::from_rgb(0x24, 0x28, 0x32);
const BUTTON_HOVER: Color32 = Color32::from_rgb(0x2f, 0x34, 0x41);
const TEXT: Color32 = Color32::from_rgb(0xf2, 0xf4, 0xf8);
const MUTED: Color32 = Color32::from_rgb(0x8b, 0x93, 0xa3);
const ACCENT: Color32 = Color32::from_rgb(0x7c, 0x6c, 0xff);
const ACCENT_HOVER: Color32 = Color32::from_rgb(0x96, 0x89, 0xff);
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

fn worker(state: State, ctx: egui::Context) {
    // Let the card fade in before the screens start flickering.
    sleep(Duration::from_millis(500));
    let result = (|| -> Result<bool, String> {
        log::line("Displays are duplicated; switching to extend.");
        display::extend()?;
        sleep(Duration::from_millis(1500));
        set_phase(&state, &ctx, Phase::Working("Arranging your screens".into()));
        sleep(Duration::from_millis(400));
        let arrangement = fix::arrange(None, false)?;
        set_phase(&state, &ctx, Phase::Working("Setting display scaling".into()));
        sleep(Duration::from_millis(400));
        fix::scale()?;
        Ok(arrangement.has_choices())
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

// ---------------------------------------------------------------------------------------------
// Preview: the same card with made-up screens; nothing on the real displays changes.

#[derive(Clone)]
struct Preview {
    kind: String,
    laptop: Side,
    swapped: bool,
}

impl Preview {
    fn dual(&self) -> bool {
        !matches!(self.kind.as_str(), "single" | "done")
    }

    fn screens(&self) -> Vec<Screen> {
        let mut screens = if self.dual() {
            let dell = ("DELL U2723QE", (597.0, 336.0));
            let lg = ("LG 27UL850", (597.0, 336.0));
            let (left, right) = if self.swapped { (lg, dell) } else { (dell, lg) };
            vec![
                fake(left.0, false, false, -3840, 0, 3840, 2160, left.1),
                fake(right.0, false, true, 0, 0, 3840, 2160, right.1),
            ]
        } else {
            vec![fake("DELL U4021QW", false, true, 0, 0, 5120, 2160, (977.0, 412.0))]
        };
        let areas: Vec<Area> = screens.iter().map(Screen::area).collect();
        let (x, y) = display::laptop_spot(&areas, (3840, 2400), self.laptop);
        screens.push(fake("Laptop", true, false, x, y, 3840, 2400, (345.0, 215.0)));
        screens
    }
}

fn fake(name: &str, internal: bool, primary: bool, x: i32, y: i32, width: i32, height: i32, mm: (f32, f32)) -> Screen {
    Screen { id: name.into(), name: name.into(), internal, primary, x, y, width, height, mm: Some(mm) }
}

/// Plays the card with made-up screens and changes nothing. `kind` is "dual" (two monitors),
/// "single" (one ultrawide), "done" (nothing to ask) or "error".
pub fn preview(kind: &str) -> Result<(), String> {
    let mut preview = Preview { kind: kind.to_owned(), laptop: Side::Left, swapped: false };
    if preview.dual() {
        preview.laptop = Side::Below;
    }
    let names = if preview.dual() { "Laptop + DELL U2723QE + LG 27UL850" } else { "Laptop + DELL U4021QW" };
    let start = vec![fake(names, false, true, 0, 0, 3840, 2400, (597.0, 336.0))];
    let worker_preview = preview.clone();
    show(start, Some(preview), move |state, ctx| {
        sleep(Duration::from_millis(1400));
        let arranged = worker_preview.screens();
        set_phase_with(&state, &ctx, Phase::Working("Arranging your screens".into()), Some(arranged.clone()));
        sleep(Duration::from_millis(1000));
        set_phase_with(&state, &ctx, Phase::Working("Setting display scaling".into()), Some(arranged.clone()));
        sleep(Duration::from_millis(800));
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
    work: impl FnOnce(State, egui::Context) + Send + 'static,
) -> Result<bool, String> {
    let state: State = Arc::new(Mutex::new(Shared {
        phase: Phase::Working("Switching to extend".into()),
        since: Instant::now(),
        screens,
    }));
    let started = Arc::new(AtomicBool::new(false));
    let options = eframe::NativeOptions { viewport: card_viewport("auto-display").with_app_id("auto-display"), ..Default::default() };
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
    Close,
    MoveLaptop(Side),
    SwapMonitors,
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
    preview: Option<Preview>,
}

impl Card {
    fn new(state: State, preview: Option<Preview>) -> Self {
        Card { state, shown: None, closing: None, monitors: Vec::new(), alpha: 0, windows: HashMap::new(), preview }
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
        let (working, laptop, swap) = match action {
            Action::Keep => {
                if self.preview.is_none() {
                    log::line("Kept the layout.");
                }
                return self.close();
            }
            Action::Close => return self.close(),
            Action::MoveLaptop(side) => ("Moving your laptop screen", Some(side), false),
            Action::SwapMonitors => ("Swapping your monitors", None, true),
        };
        let (state, ctx) = (self.state.clone(), ctx.clone());
        if let Some(preview) = &mut self.preview {
            if let Some(side) = laptop {
                preview.laptop = side;
            }
            preview.swapped ^= swap;
            let current = state.lock().unwrap().screens.clone();
            set_phase_with(&state, &ctx, Phase::Working(working.into()), Some(current));
            let screens = preview.screens();
            spawn(move || {
                sleep(Duration::from_millis(600));
                set_phase_with(&state, &ctx, Phase::Prompt, Some(screens));
            });
            return;
        }
        set_phase(&state, &ctx, Phase::Working(working.into()));
        spawn(move || {
            let phase = match fix::arrange(laptop, swap) {
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

impl eframe::App for Card {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        BG_BOTTOM.to_normalized_gamma_f32()
    }

    /// Window management lives here because `ui` does not run while the window is hidden.
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        let now = Instant::now();
        // The screens move under us while we work, so look them up every frame.
        self.monitors = monitors();
        let fade_in = self.shown.map_or(0.0, |t| (now - t).as_secs_f32() / FADE_IN).clamp(0.0, 1.0);
        let fade_out = self.closing.map_or(1.0, |t| 1.0 - (now - t).as_secs_f32() / FADE_OUT).clamp(0.0, 1.0);
        self.alpha = (ease_out(fade_in) * fade_out * OPACITY).round() as u8;

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
            Phase::Prompt if since.elapsed() >= KEEP_AFTER && self.closing.is_none() => self.act(Action::Keep, ctx),
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
            let clicked =
                ctx.show_viewport_immediate(id, card_viewport(&title), |ui, _| draw(ui, &phase, since, &screens));
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

// ---------------------------------------------------------------------------------------------
// Drawing.

/// What the prompt is about, worked out from the screens on the card.
struct Choices {
    dual: bool,
    laptop: Option<Screen>,
    /// Faded laptops wherever the laptop is not.
    ghosts: Vec<(Side, Screen)>,
}

fn choices(screens: &[Screen]) -> Choices {
    let externals: Vec<Area> = screens.iter().filter(|s| !s.internal).map(Screen::area).collect();
    let laptop = screens.iter().find(|s| s.internal).cloned();
    let mut ghosts = Vec::new();
    if let Some(l) = &laptop
        && !externals.is_empty()
    {
        let current = display::laptop_side(&externals, l.area());
        for side in SIDES.into_iter().filter(|&s| Some(s) != current) {
            let (x, y) = display::laptop_spot(&externals, (l.width, l.height), side);
            let id = format!("ghost-{}", fix::side_name(side));
            ghosts.push((side, Screen { id, x, y, primary: false, ..l.clone() }));
        }
    }
    Choices { dual: display::is_dual(&externals), laptop, ghosts }
}

fn draw(ui: &mut egui::Ui, phase: &Phase, since: Instant, screens: &[Screen]) -> Option<Action> {
    let rect = ui.max_rect();
    let painter = ui.painter().clone();
    let t = ui.input(|i| i.time) as f32;
    let choices = (*phase == Phase::Prompt).then(|| choices(screens));

    // Background: a soft vertical gradient with a faint glow behind the icon.
    vgradient(&painter, rect, BG_TOP, BG_BOTTOM);
    let glow_colour = match phase {
        Phase::Done(_) => GOOD,
        Phase::Failed(_) => BAD,
        _ => ACCENT,
    };
    for i in 0..10 {
        painter.circle_filled(rect.min + vec2(PAD + 24.0, 58.0), 30.0 + i as f32 * 14.0, glow_colour.gamma_multiply(0.012));
    }

    // Accent bar along the top edge.
    let bar = Rect::from_min_size(rect.min, vec2(rect.width(), 3.0));
    match phase {
        Phase::Working(_) => {
            gradient(&painter, bar, &[(0.0, ACCENT.gamma_multiply(0.35)), (1.0, ACCENT_2.gamma_multiply(0.35))]);
            let p = (t * 0.55).fract() * 1.6 - 0.3;
            let clipped = painter.with_clip_rect(bar);
            gradient(&clipped, bar, &[(p - 0.25, Color32::TRANSPARENT), (p, ACCENT_2), (p + 0.25, Color32::TRANSPARENT)]);
        }
        Phase::Prompt => gradient(&painter, bar, &[(0.0, ACCENT), (1.0, ACCENT_2)]),
        Phase::Done(_) => gradient(&painter, bar, &[(0.0, GOOD), (1.0, ACCENT_2)]),
        Phase::Failed(_) => {
            painter.rect_filled(bar, CornerRadius::ZERO, BAD);
        }
    }

    draw_icon(&painter, rect.min + vec2(PAD + 24.0, 58.0), phase, since, t);

    // Title and subtitle.
    let text_x = rect.left() + PAD + 64.0;
    let text_w = rect.right() - PAD - text_x;
    let (title, subtitle) = match phase {
        Phase::Working(step) => ("Setting up your displays".to_string(), format!("{step}{}", dots(t))),
        Phase::Prompt => prompt_text(screens, choices.as_ref().unwrap()),
        Phase::Done(summary) => ("Displays ready".to_string(), summary.clone()),
        Phase::Failed(e) => ("Couldn't set up your displays".to_string(), e.clone()),
    };
    let (title_font, sub_font) = (semibold(19.0), regular(14.0));
    let title = fit(&painter, &title, &title_font, text_w);
    painter.text(pos2(text_x, rect.top() + 46.0), Align2::LEFT_CENTER, title, title_font, TEXT);
    let subtitle = fit(&painter, &subtitle, &sub_font, text_w);
    painter.text(pos2(text_x, rect.top() + 71.0), Align2::LEFT_CENTER, subtitle, sub_font, MUTED);

    // Diagram of the screens, drawn to physical scale.
    let area = Rect::from_min_max(pos2(rect.left() + PAD, rect.top() + 100.0), pos2(rect.right() - PAD, rect.bottom() - 70.0));
    painter.rect_filled(area, CornerRadius::same(14), SURFACE.gamma_multiply(0.85));
    painter.rect_stroke(area, CornerRadius::same(14), Stroke::new(1.0, SURFACE_EDGE), StrokeKind::Inside);
    let ghosts = choices.as_ref().map_or(&[][..], |c| &c.ghosts[..]);
    let mut action = draw_screens(ui, &painter, area.shrink2(vec2(22.0, 18.0)), screens, ghosts).map(Action::MoveLaptop);

    // Footer.
    let row = Rect::from_min_max(pos2(rect.left() + PAD, rect.bottom() - 54.0), pos2(rect.right() - PAD, rect.bottom() - 20.0));
    let footer_font = regular(13.0);
    match phase {
        Phase::Prompt => {
            let dual = choices.as_ref().is_some_and(|c| c.dual);
            let left = KEEP_AFTER.saturating_sub(since.elapsed()).as_secs_f32();
            let keep = Rect::from_min_size(pos2(row.right() - 118.0, row.top()), vec2(118.0, row.height()));
            let mut text_right = keep.left() - 12.0;
            if dual {
                let swap = Rect::from_min_size(pos2(keep.left() - 10.0 - 148.0, row.top()), vec2(148.0, row.height()));
                text_right = swap.left() - 12.0;
                if button(ui, &painter, swap, "swap", "Swap monitors", false) {
                    action = Some(Action::SwapMonitors);
                }
            }
            let note = fit(&painter, &format!("Keeping this layout in {}s", left.ceil() as u32), &footer_font, text_right - row.left());
            painter.text(pos2(row.left(), row.center().y), Align2::LEFT_CENTER, note, footer_font, MUTED);
            if button(ui, &painter, keep, "keep", "Keep", true) {
                action = Some(Action::Keep);
            }
            // The countdown drains along the bottom edge of the card.
            let track = Rect::from_min_max(pos2(rect.left(), rect.bottom() - 3.0), rect.max);
            painter.rect_filled(track, CornerRadius::ZERO, BUTTON);
            let fill = Rect::from_min_size(track.min, vec2(track.width() * left / KEEP_AFTER.as_secs_f32(), track.height()));
            gradient(&painter.with_clip_rect(fill), track, &[(0.0, ACCENT), (1.0, ACCENT_2)]);
            ui.input(|i| {
                if i.key_pressed(Key::Enter) || i.key_pressed(Key::Escape) {
                    action = Some(Action::Keep);
                } else if dual && i.key_pressed(Key::S) {
                    action = Some(Action::SwapMonitors);
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

fn draw_icon(painter: &Painter, c: Pos2, phase: &Phase, since: Instant, t: f32) {
    let r = 20.0;
    match phase {
        Phase::Working(_) => {
            painter.circle_filled(c, r + 2.0, ACCENT.gamma_multiply(0.10));
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
            // A small screen with a laptop beside it.
            let s = Stroke::new(2.0, ACCENT_HOVER);
            let screen = Rect::from_center_size(c + vec2(3.0, -3.0), vec2(16.0, 10.0));
            painter.rect_stroke(screen, CornerRadius::same(2), s, StrokeKind::Middle);
            painter.line_segment([c + vec2(3.0, 2.0), c + vec2(3.0, 5.0)], s);
            let laptop = Rect::from_center_size(c + vec2(-8.0, 5.0), vec2(8.0, 5.0));
            painter.rect_stroke(laptop, CornerRadius::same(1), s, StrokeKind::Middle);
            painter.line_segment([c + vec2(-13.0, 9.0), c + vec2(-3.0, 9.0)], s);
        }
        Phase::Done(_) => {
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

fn dots(t: f32) -> &'static str {
    ["", ".", "..", "..."][((t * 2.5) as usize) % 4]
}

fn prompt_text(screens: &[Screen], choices: &Choices) -> (String, String) {
    match (choices.laptop.is_some(), choices.dual) {
        (true, true) => ("Is everything in the right place?".into(), "Click a faded laptop to move it, or swap the monitors.".into()),
        (true, false) => ("Is your laptop in the right place?".into(), "Click a faded laptop to move it there.".into()),
        _ => {
            let mut externals: Vec<_> = screens.iter().filter(|s| !s.internal).collect();
            externals.sort_by_key(|s| s.x);
            let sub = match externals.as_slice() {
                [left, right] if left.name != right.name => format!("{} should be on your left, {} on your right.", left.name, right.name),
                _ => "The main display should be the one on your right.".into(),
            };
            ("Are your monitors the right way round?".into(), sub)
        }
    }
}

/// Size of a screen's picture in millimetres, from its EDID, or a guess from its pixels.
fn size_mm(s: &Screen) -> Vec2 {
    match s.mm {
        // EDID describes the panel in landscape; follow the mode if the screen is rotated.
        Some((w, h)) if w > 0.0 && h > 0.0 => {
            if (s.width < s.height) != (w < h) { vec2(h, w) } else { vec2(w, h) }
        }
        _ => vec2(s.width as f32, s.height as f32) * 0.2,
    }
}

/// Places `s` (size `size` mm) against the first already-placed screen it touches on the desktop.
/// Side by side keeps the vertical offset in proportion; above or below keeps the centre in
/// proportion, so a laptop centred on a seam stays centred on it.
fn place(s: &Screen, size: Vec2, placed: &[(Area, Rect)]) -> Option<Rect> {
    let a = s.area();
    let near = |p: i32, q: i32| (p - q).abs() <= 1;
    for (n, r) in placed {
        let v_overlap = a.y < n.y + n.h && n.y < a.y + a.h;
        let h_overlap = a.x < n.x + n.w && n.x < a.x + a.w;
        if v_overlap && (near(a.x + a.w, n.x) || near(a.x, n.x + n.w)) {
            let x = if near(a.x + a.w, n.x) { r.left() - size.x } else { r.right() };
            let y = r.top() + (a.y - n.y) as f32 / n.h as f32 * r.height();
            return Some(Rect::from_min_size(pos2(x, y), size));
        }
        if h_overlap && (near(a.y, n.y + n.h) || near(a.y + a.h, n.y)) {
            let y = if near(a.y, n.y + n.h) { r.bottom() } else { r.top() - size.y };
            let cx = r.left() + ((a.x as f32 + a.w as f32 / 2.0) - n.x as f32) / n.w as f32 * r.width();
            return Some(Rect::from_min_size(pos2(cx - size.x / 2.0, y), size));
        }
    }
    None
}

/// Lays the screens (then the ghosts) out in millimetres, keeping how they touch on the desktop.
fn physical_layout(screens: &[Screen], ghosts: &[Screen]) -> (Vec<Rect>, Vec<Rect>) {
    let mut rects: Vec<Option<Rect>> = vec![None; screens.len()];
    let first = screens.iter().position(|s| s.primary).unwrap_or(0);
    rects[first] = Some(Rect::from_min_size(Pos2::ZERO, size_mm(&screens[first])));
    loop {
        let placed: Vec<(Area, Rect)> = screens.iter().zip(&rects).filter_map(|(s, r)| r.map(|r| (s.area(), r))).collect();
        let mut progress = false;
        for (i, s) in screens.iter().enumerate() {
            if rects[i].is_none()
                && let Some(r) = place(s, size_mm(s), &placed)
            {
                rects[i] = Some(r);
                progress = true;
            }
        }
        if !progress {
            break;
        }
    }
    // Anything not touching another screen keeps its pixel offset from the main display, scaled.
    let anchor = &screens[first];
    let ratio = size_mm(anchor).x / anchor.width.max(1) as f32;
    let fallback = |s: &Screen| {
        Rect::from_min_size(pos2((s.x - anchor.x) as f32 * ratio, (s.y - anchor.y) as f32 * ratio), size_mm(s))
    };
    let rects: Vec<Rect> = screens.iter().zip(rects).map(|(s, r)| r.unwrap_or_else(|| fallback(s))).collect();
    let placed: Vec<(Area, Rect)> = screens.iter().map(Screen::area).zip(rects.iter().copied()).collect();
    let ghost_rects = ghosts.iter().map(|g| place(g, size_mm(g), &placed).unwrap_or_else(|| fallback(g))).collect();
    (rects, ghost_rects)
}

/// Draws the screens to scale, plus clickable ghost laptops. Returns the side of a clicked ghost.
fn draw_screens(ui: &egui::Ui, painter: &Painter, area: Rect, screens: &[Screen], ghosts: &[(Side, Screen)]) -> Option<Side> {
    if screens.is_empty() {
        return None;
    }
    let ghost_screens: Vec<Screen> = ghosts.iter().map(|(_, g)| g.clone()).collect();
    let (rects, ghost_rects) = physical_layout(screens, &ghost_screens);
    let bounds = rects.iter().chain(&ghost_rects).fold(Rect::NOTHING, |b, r| b.union(*r));
    // Leave room under the laptops for their keyboard decks.
    let room = vec2(area.width(), area.height() - 6.0);
    let scale = (room.x / bounds.width().max(1.0)).min(room.y / bounds.height().max(1.0));
    let offset = area.min + (room - bounds.size() * scale) / 2.0;
    let to_card = |r: &Rect| Rect::from_min_size(offset + (r.min - bounds.min) * scale, r.size() * scale);

    let ctx = ui.ctx();
    let animate = |key: &str, target: Rect| {
        // Viewports share an animation manager, but DPI rounding can give each a different target.
        let id = Id::new(("screen", ctx.viewport_id(), key));
        let a = |k: &str, v: f32| ctx.animate_value_with_time(id.with(k), v, 0.35);
        Rect::from_min_size(pos2(a("x", target.left()), a("y", target.top())), vec2(a("w", target.width()), a("h", target.height())))
    };

    let mut clicked = None;
    for ((side, ghost), r) in ghosts.iter().zip(&ghost_rects) {
        let r = animate(&ghost.id, to_card(r)).shrink(2.0);
        let response = ui.interact(r, ui.id().with(&ghost.id), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
        draw_ghost(painter, r, response.hovered());
        if response.clicked() {
            clicked = Some(*side);
        }
    }
    for (s, r) in screens.iter().zip(&rects) {
        draw_screen(painter, s, animate(&s.id, to_card(r)).shrink(2.0));
    }
    clicked
}

fn draw_screen(painter: &Painter, s: &Screen, r: Rect) {
    let radius = CornerRadius::same(5);
    if s.primary {
        // A soft glow around the main display.
        for i in 1..=4 {
            let grow = i as f32 * 2.0;
            painter.rect_filled(r.expand(grow), CornerRadius::same(5 + i * 2), ACCENT.gamma_multiply(0.05));
        }
    }
    painter.rect_filled(r, radius, BEZEL);
    let panel = r.shrink(2.5);
    vgradient(&painter.with_clip_rect(panel), panel, PANEL_TOP, PANEL_BOTTOM);
    let edge = if s.primary { Stroke::new(1.5, ACCENT) } else { Stroke::new(1.0, SCREEN_EDGE) };
    painter.rect_stroke(r, radius, edge, StrokeKind::Inside);
    if s.internal {
        // The laptop's keyboard deck.
        let deck = Rect::from_min_max(pos2(r.left() - 6.0, r.bottom() + 1.0), pos2(r.right() + 6.0, r.bottom() + 5.0));
        painter.rect_filled(deck, CornerRadius::same(2), SCREEN_EDGE);
    }

    let name_font = regular(12.5);
    let name = label(painter, &s.name, &name_font, r.width() - 12.0);
    let inches = s.mm.map(|(w, h)| (w * w + h * h).sqrt() / 25.4).filter(|d| *d > 5.0 && !s.name.contains(" + "));
    let detail = match (s.primary, inches) {
        (true, Some(d)) => Some((format!("Main display · {d:.0}\""), ACCENT_HOVER)),
        (true, None) => Some(("Main display".to_string(), ACCENT_HOVER)),
        (false, Some(d)) => Some((format!("{d:.0}\""), MUTED)),
        (false, None) => None,
    };
    match detail {
        Some((text, colour)) if r.height() > 40.0 => {
            painter.text(r.center() - vec2(0.0, 8.0), Align2::CENTER_CENTER, name, name_font, if s.primary { TEXT } else { MUTED });
            let detail_font = semibold(11.0);
            let text = fit(painter, &text, &detail_font, r.width() - 12.0);
            painter.text(r.center() + vec2(0.0, 9.0), Align2::CENTER_CENTER, text, detail_font, colour);
        }
        _ => {
            painter.text(r.center(), Align2::CENTER_CENTER, name, name_font, if s.primary { TEXT } else { MUTED });
        }
    }
}

fn draw_ghost(painter: &Painter, r: Rect, hovered: bool) {
    let colour = if hovered { ACCENT_HOVER } else { MUTED.gamma_multiply(0.55) };
    painter.rect_filled(r, CornerRadius::same(5), ACCENT.gamma_multiply(if hovered { 0.16 } else { 0.04 }));
    let dashed = |rect: Rect| {
        let pts = [rect.left_top(), rect.right_top(), rect.right_bottom(), rect.left_bottom(), rect.left_top()];
        painter.extend(Shape::dashed_line(&pts, Stroke::new(1.2, colour), 5.0, 4.0));
    };
    dashed(r);
    let deck = Rect::from_min_max(pos2(r.left() - 6.0, r.bottom() + 1.0), pos2(r.right() + 6.0, r.bottom() + 5.0));
    dashed(deck);
    let font = regular(12.0);
    let options: &[&str] = if hovered { &["Move here", "Move"] } else { &["Laptop here?", "Here?"] };
    let width = |s: &str| painter.layout_no_wrap(s.to_owned(), font.clone(), TEXT).size().x;
    let text = options.iter().find(|s| width(s) <= r.width() - 10.0).copied().unwrap_or("");
    painter.text(r.center(), Align2::CENTER_CENTER, text, font, if hovered { TEXT } else { MUTED });
}

fn button(ui: &egui::Ui, painter: &Painter, rect: Rect, id: &str, label: &str, primary: bool) -> bool {
    let response = ui.interact(rect, ui.id().with(id), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
    let hovered = response.hovered();
    let radius = CornerRadius::same(10);
    if primary {
        painter.rect_filled(rect, radius, if hovered { ACCENT_HOVER } else { ACCENT });
        painter.rect_stroke(rect, radius, Stroke::new(1.0, ACCENT_HOVER.gamma_multiply(0.6)), StrokeKind::Inside);
    } else {
        painter.rect_filled(rect, radius, if hovered { BUTTON_HOVER } else { BUTTON });
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

/// A vertical gradient from `top` to `bottom` filling `rect`.
fn vgradient(painter: &Painter, rect: Rect, top: Color32, bottom: Color32) {
    let mut mesh = Mesh::default();
    mesh.colored_vertex(rect.left_top(), top);
    mesh.colored_vertex(rect.right_top(), top);
    mesh.colored_vertex(rect.left_bottom(), bottom);
    mesh.colored_vertex(rect.right_bottom(), bottom);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 2, 3);
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

// ---------------------------------------------------------------------------------------------
// Win32 window helpers.

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
    let caption = colorref(BG_TOP);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(id: &str, internal: bool, primary: bool, area: (i32, i32, i32, i32), mm: (f32, f32)) -> Screen {
        let (x, y, width, height) = area;
        Screen { id: id.into(), name: id.into(), internal, primary, x, y, width, height, mm: Some(mm) }
    }

    #[test]
    fn physical_layout_uses_millimetres_and_keeps_the_seam() {
        // Two 40" 4K monitors with a 16" laptop centred below the seam.
        let screens = [
            screen("left", false, false, (-3840, 0, 3840, 2160), (878.0, 485.0)),
            screen("right", false, true, (0, 0, 3840, 2160), (878.0, 485.0)),
            screen("laptop", true, false, (-1920, 2160, 3840, 2400), (345.0, 215.0)),
        ];
        let (rects, _) = physical_layout(&screens, &[]);
        assert_eq!(rects[1], Rect::from_min_size(Pos2::ZERO, vec2(878.0, 485.0)));
        assert_eq!(rects[0], Rect::from_min_size(pos2(-878.0, 0.0), vec2(878.0, 485.0)));
        // Much smaller than the monitors, and centred on the seam at x = 0.
        assert_eq!(rects[2].size(), vec2(345.0, 215.0));
        assert!((rects[2].center().x).abs() < 0.01);
        assert_eq!(rects[2].top(), 485.0);
    }

    #[test]
    fn ghosts_appear_where_the_laptop_is_not() {
        let screens = [
            screen("dell", false, true, (0, 0, 5120, 2160), (977.0, 412.0)),
            screen("laptop", true, false, (-3840, 1080, 3840, 2400), (345.0, 215.0)),
        ];
        let c = choices(&screens);
        let sides: Vec<Side> = c.ghosts.iter().map(|(s, _)| *s).collect();
        assert_eq!(sides, vec![Side::Right, Side::Below]);
        assert!(!c.dual);
        let ghost_screens: Vec<Screen> = c.ghosts.iter().map(|(_, g)| g.clone()).collect();
        let (_, ghosts) = physical_layout(&screens, &ghost_screens);
        // Right ghost: against the monitor's right edge, top halfway down it.
        assert_eq!(ghosts[0].min, pos2(977.0, 206.0));
        // Below ghost: centred under the monitor.
        assert!((ghosts[1].center().x - 977.0 / 2.0).abs() < 0.01);
        assert_eq!(ghosts[1].top(), 412.0);
    }

    fn paint_frame(
        ctx: &egui::Context,
        viewport: ViewportId,
        size: Vec2,
        time: f64,
        paint: impl FnMut(&mut egui::Ui),
    ) -> egui::FullOutput {
        let input = egui::RawInput {
            viewport_id: viewport,
            viewports: [(viewport, egui::ViewportInfo::default())].into_iter().collect(),
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
            time: Some(time),
            ..Default::default()
        };
        let mut paint = paint;
        let mut output = ctx.run_ui(input, |ui| paint(ui));
        output.textures_delta.clear();
        output
    }

    fn screen_rects(output: &egui::FullOutput) -> Vec<Rect> {
        output.shapes.iter().filter_map(|s| match &s.shape {
            Shape::Rect(r) if r.fill == BEZEL => Some(r.rect),
            _ => None,
        }).collect()
    }

    #[test]
    fn screen_animations_are_independent_on_each_monitor() {
        let ctx = egui::Context::default();
        install_fonts(&ctx);
        let screens = Preview { kind: "dual".into(), laptop: Side::Below, swapped: false }.screens();
        // Different DPI rounding/window sizes give each viewport different diagram coordinates.
        let viewports = [
            (ViewportId::ROOT, vec2(WIDTH, HEIGHT)),
            (ViewportId::from_hash_of("second monitor"), vec2(WIDTH - 1.0, HEIGHT - 1.0)),
        ];
        let mut expected = Vec::new();
        for (viewport, size) in viewports {
            let isolated = egui::Context::default();
            install_fonts(&isolated);
            let output = paint_frame(&isolated, viewport, size, 0.0, |ui| {
                draw(ui, &Phase::Prompt, Instant::now(), &screens);
            });
            let rects = screen_rects(&output);
            assert_eq!(rects.len(), screens.len());
            expected.push(rects);
        }
        for frame in 0..120 {
            for (i, (viewport, size)) in viewports.into_iter().enumerate() {
                let output = paint_frame(&ctx, viewport, size, frame as f64 / 60.0, |ui| {
                    draw(ui, &Phase::Prompt, Instant::now(), &screens);
                });
                assert_eq!(screen_rects(&output), expected[i], "frame {frame}, monitor {i}");
            }
        }
    }

    #[test]
    fn button_fill_stays_inside_rounded_corners() {
        let ctx = egui::Context::default();
        install_fonts(&ctx);
        ctx.tessellation_options_mut(|o| o.prerasterized_discs = false);
        let rect = Rect::from_min_size(pos2(20.0, 20.0), vec2(118.0, 34.0));
        for primary in [false, true] {
            let output = paint_frame(&ctx, ViewportId::ROOT, vec2(WIDTH, HEIGHT), 0.0, |ui| {
                button(ui, ui.painter(), rect, "test button", "Keep", primary);
            });
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
