//! Thin wrapper over the Windows display configuration API (QueryDisplayConfig and friends).
//!
//! The structs are declared here rather than taken from windows-sys so their layout is explicit and
//! the undocumented DPI-scale packets (types -3 and -4) can sit next to the documented ones.

use std::collections::HashSet;
use std::mem::size_of;
use std::ptr::{null, null_mut};

mod placement;
mod remembered;
mod settings;
pub use remembered::{StartupLayout, remembered_layout, restore_remembered_layout};
pub use settings::{Snapshot, apply_scale};

pub type Result<T> = std::result::Result<T, String>;

/// Scaling steps Windows offers, in order. The DPI packets express scale as an offset into this list
/// from the monitor's recommended step.
pub const SCALES: [u32; 12] = [100, 125, 150, 175, 200, 225, 250, 300, 350, 400, 450, 500];

const QDC_ALL_PATHS: u32 = 0x1;
const QDC_ONLY_ACTIVE_PATHS: u32 = 0x2;
const QDC_DATABASE_CURRENT: u32 = 0x4;
const SDC_TOPOLOGY_EXTEND: u32 = 0x4;
const SDC_USE_SUPPLIED_DISPLAY_CONFIG: u32 = 0x20;
const SDC_VALIDATE: u32 = 0x40;
const SDC_APPLY: u32 = 0x80;
const SDC_SAVE_TO_DATABASE: u32 = 0x200;
const ERROR_INSUFFICIENT_BUFFER: i32 = 122;
const MODE_INFO_TYPE_SOURCE: u32 = 1;
const PATH_ACTIVE: u32 = 0x1;

const OUTPUT_INTERNAL: u32 = 0x8000_0000;
const OUTPUT_DISPLAYPORT_EMBEDDED: u32 = 11;
const OUTPUT_UDI_EMBEDDED: u32 = 13;

const GET_SOURCE_NAME: i32 = 1;
const GET_TARGET_NAME: i32 = 2;
const GET_DPI_SCALE: i32 = -3;
const SET_DPI_SCALE: i32 = -4;

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Luid {
    pub low: u32,
    pub high: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct PathSource {
    pub adapter_id: Luid,
    pub id: u32,
    pub mode_info_idx: u32,
    pub status_flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct PathTarget {
    pub adapter_id: Luid,
    pub id: u32,
    pub mode_info_idx: u32,
    pub output_technology: u32,
    pub rotation: u32,
    pub scaling: u32,
    pub refresh_num: u32,
    pub refresh_den: u32,
    pub scan_line_ordering: u32,
    pub target_available: i32,
    pub status_flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct PathInfo {
    pub source: PathSource,
    pub target: PathTarget,
    pub flags: u32,
}

impl PathInfo {
    pub fn is_internal(&self) -> bool {
        matches!(
            self.target.output_technology,
            OUTPUT_INTERNAL | OUTPUT_DISPLAYPORT_EMBEDDED | OUTPUT_UDI_EMBEDDED
        )
    }

    fn source_key(&self) -> (Luid, u32) {
        (self.source.adapter_id, self.source.id)
    }

    pub fn rotation(&self) -> Rotation {
        match self.target.rotation {
            2 => Rotation::Portrait,
            3 => Rotation::LandscapeFlipped,
            4 => Rotation::PortraitFlipped,
            _ => Rotation::Landscape,
        }
    }
}

/// Windows display target rotation, in clockwise quarter turns.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rotation {
    Landscape = 1,
    Portrait = 2,
    LandscapeFlipped = 3,
    PortraitFlipped = 4,
}

impl Rotation {
    pub fn label(self) -> &'static str {
        match self {
            Self::Landscape => "Landscape",
            Self::Portrait => "Portrait",
            Self::LandscapeFlipped => "Landscape (flipped)",
            Self::PortraitFlipped => "Portrait (flipped)",
        }
    }

    pub fn swaps_axes(self) -> bool {
        matches!(self, Self::Portrait | Self::PortraitFlipped)
    }
}

/// DISPLAYCONFIG_MODE_INFO: a 16-byte header followed by a 48-byte union.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct ModeInfo {
    pub info_type: u32,
    pub id: u32,
    pub adapter_id: Luid,
    data: [u64; 6],
}

/// The source-mode arm of the ModeInfo union.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SourceMode {
    pub width: u32,
    pub height: u32,
    pub pixel_format: u32,
    pub x: i32,
    pub y: i32,
}

impl ModeInfo {
    pub fn source_mode(&self) -> SourceMode {
        // SAFETY: SourceMode is 20 bytes of plain integers and fits inside the 48-byte union.
        unsafe { *(self.data.as_ptr() as *const SourceMode) }
    }

    fn set_position(&mut self, x: i32, y: i32) {
        // SAFETY: as above; only called on source modes.
        let sm = unsafe { &mut *(self.data.as_mut_ptr() as *mut SourceMode) };
        sm.x = x;
        sm.y = y;
    }

    fn set_area(&mut self, area: Area) {
        // SAFETY: This is called on source modes matched by screens_with_modes().
        let sm = unsafe { &mut *(self.data.as_mut_ptr() as *mut SourceMode) };
        (sm.x, sm.y) = (area.x, area.y);
        (sm.width, sm.height) = (area.w as u32, area.h as u32);
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Header {
    kind: i32,
    size: u32,
    adapter_id: Luid,
    id: u32,
}

#[repr(C)]
struct TargetName {
    header: Header,
    flags: u32,
    output_technology: u32,
    edid_manufacture_id: u16,
    edid_product_code_id: u16,
    connector_instance: u32,
    friendly_name: [u16; 64],
    device_path: [u16; 128],
}

#[repr(C)]
struct SourceName {
    header: Header,
    gdi_name: [u16; 32],
}

#[repr(C)]
#[derive(Default)]
struct DpiGet {
    header: Header,
    min_rel: i32,
    cur_rel: i32,
    max_rel: i32,
}

#[repr(C)]
struct DpiSet {
    header: Header,
    rel: i32,
}

#[link(name = "user32")]
unsafe extern "system" {
    fn GetDisplayConfigBufferSizes(flags: u32, num_paths: *mut u32, num_modes: *mut u32) -> i32;
    fn QueryDisplayConfig(
        flags: u32,
        num_paths: *mut u32,
        paths: *mut PathInfo,
        num_modes: *mut u32,
        modes: *mut ModeInfo,
        topology: *mut u32,
    ) -> i32;
    fn SetDisplayConfig(
        num_paths: u32,
        paths: *const PathInfo,
        num_modes: u32,
        modes: *const ModeInfo,
        flags: u32,
    ) -> i32;
    fn DisplayConfigGetDeviceInfo(packet: *mut Header) -> i32;
    fn DisplayConfigSetDeviceInfo(packet: *const Header) -> i32;
}

fn check(rc: i32, what: &str) -> Result<()> {
    if rc == 0 { Ok(()) } else { Err(format!("{what} failed with error {rc}")) }
}

fn header<T>(kind: i32, adapter_id: Luid, id: u32) -> Header {
    Header { kind, size: size_of::<T>() as u32, adapter_id, id }
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

pub struct Config {
    pub paths: Vec<PathInfo>,
    pub modes: Vec<ModeInfo>,
}

impl Config {
    fn apply(&self, extra_flags: u32, action: &str) -> Result<()> {
        self.set(SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_SAVE_TO_DATABASE | extra_flags, action)
    }

    fn set(&self, flags: u32, action: &str) -> Result<()> {
        check(
            unsafe {
                SetDisplayConfig(
                    self.paths.len() as u32,
                    self.paths.as_ptr(),
                    self.modes.len() as u32,
                    self.modes.as_ptr(),
                    flags,
                )
            },
            action,
        )
    }
}

fn query(flags: u32) -> Result<Config> {
    loop {
        let (mut np, mut nm) = (0u32, 0u32);
        check(unsafe { GetDisplayConfigBufferSizes(flags, &mut np, &mut nm) }, "GetDisplayConfigBufferSizes")?;
        let mut paths = vec![PathInfo::default(); np as usize];
        let mut modes = vec![ModeInfo::default(); nm as usize];
        let mut topology = 0;
        let topology_out = if flags == QDC_DATABASE_CURRENT { &mut topology } else { null_mut() };
        let rc = unsafe {
            QueryDisplayConfig(flags, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), topology_out)
        };
        // The configuration can change between the two calls; ask again for the new sizes.
        if rc == ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        check(rc, "QueryDisplayConfig")?;
        paths.truncate(np as usize);
        modes.truncate(nm as usize);
        return Ok(Config { paths, modes });
    }
}

pub fn active() -> Result<Config> {
    query(QDC_ONLY_ACTIVE_PATHS)
}

/// The monitors Windows can see, whether or not they are showing anything, by device path. Used to
/// notice when a display is plugged in or removed. Target ids are no good for this: Windows gives a
/// monitor a different target id while it is duplicated.
pub fn connected_monitors() -> Result<Vec<String>> {
    let cfg = query(QDC_ALL_PATHS)?;
    let mut ids: Vec<_> = cfg
        .paths
        .iter()
        .filter(|p| p.target.target_available != 0)
        .filter_map(|p| target_name(p).map(|t| from_wide(&t.device_path).to_lowercase()))
        .collect();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

/// Duplicate (clone) mode shows up as two or more active paths sharing one source.
pub fn is_duplicate(paths: &[PathInfo]) -> bool {
    let mut sources: Vec<_> = paths.iter().filter(|p| p.flags & PATH_ACTIVE != 0).map(PathInfo::source_key).collect();
    let total = sources.len();
    sources.sort();
    sources.dedup();
    sources.len() < total
}

pub fn extend() -> Result<()> {
    check(
        unsafe { SetDisplayConfig(0, null(), 0, null(), SDC_APPLY | SDC_TOPOLOGY_EXTEND) },
        "SetDisplayConfig (extend)",
    )
}

/// Width-to-height ratio at or above which a monitor counts as ultrawide (21:9 is about 2.33).
const ULTRAWIDE_RATIO: f64 = 2.0;

pub fn is_ultrawide(width: i32, height: i32) -> bool {
    height > 0 && width as f64 / height as f64 >= ULTRAWIDE_RATIO
}

/// Where the laptop screen sits relative to the external monitors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
    Below,
}

pub const SIDES: [Side; 3] = [Side::Left, Side::Right, Side::Below];

/// A rectangle on the desktop, in physical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Area {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// The desktop must form a connected set of screens with no overlapping interiors.
pub fn valid_layout(areas: &[Area]) -> bool {
    if areas.is_empty() || areas.iter().any(|area| area.w <= 0 || area.h <= 0) {
        return false;
    }
    let mut connected = vec![false; areas.len()];
    let mut pending = vec![0];
    connected[0] = true;
    while let Some(i) = pending.pop() {
        let a = areas[i];
        for (j, &b) in areas.iter().enumerate() {
            if i == j {
                continue;
            }
            let extent = |a: i32, a_len: i32, b: i32, b_len: i32| {
                (i64::from(a) + i64::from(a_len)).min(i64::from(b) + i64::from(b_len)) - i64::from(a.max(b))
            };
            let x = extent(a.x, a.w, b.x, b.w);
            let y = extent(a.y, a.h, b.y, b.h);
            if x > 0 && y > 0 {
                return false;
            }
            if !connected[j] && ((x == 0 && y > 0) || (x > 0 && y == 0)) {
                connected[j] = true;
                pending.push(j);
            }
        }
    }
    connected.into_iter().all(|adjacent| adjacent)
}

/// Two external monitors, neither ultrawide: they sit side by side and the right one is primary.
pub fn is_dual(externals: &[Area]) -> bool {
    externals.len() == 2 && externals.iter().all(|a| !is_ultrawide(a.w, a.h))
}

/// Top-left corner for a laptop screen of `size` (w, h) on `side` of the external monitors, which
/// are already arranged in a row.
///
/// - Left / Right: beside the outermost monitor, with the laptop's top edge halfway down it.
/// - Below: under the row, centred on the seam between two monitors, or on the middle of the row.
pub fn laptop_spot(externals: &[Area], size: (i32, i32), side: Side) -> (i32, i32) {
    let Some(left) = externals.iter().min_by_key(|a| a.x) else { return (0, 0) };
    let right = externals.iter().max_by_key(|a| a.x + a.w).unwrap();
    match side {
        Side::Left => (left.x - size.0, left.y + left.h / 2),
        Side::Right => (right.x + right.w, right.y + right.h / 2),
        Side::Below => {
            let bottom = externals.iter().map(|a| a.y + a.h).max().unwrap();
            let centre = if externals.len() == 2 { left.x + left.w } else { (left.x + right.x + right.w) / 2 };
            (centre - size.0 / 2, bottom)
        }
    }
}

/// Which side the laptop is on, if it is exactly where `laptop_spot` would put it.
pub fn laptop_side(externals: &[Area], laptop: Area) -> Option<Side> {
    SIDES.into_iter().find(|&s| laptop_spot(externals, (laptop.w, laptop.h), s) == (laptop.x, laptop.y))
}

fn area(m: &ModeInfo) -> Area {
    let sm = m.source_mode();
    Area { x: sm.x, y: sm.y, w: sm.width as i32, h: sm.height as i32 }
}

/// GDI supplies the desktop geometry in physical pixels. CCD source sizes can describe the raw
/// surface before rotation, even though its position is already in desktop coordinates.
pub fn desktop_area(p: &PathInfo, m: &ModeInfo) -> Area {
    use windows_sys::Win32::Graphics::Gdi::{
        DEVMODEW, DM_PELSHEIGHT, DM_PELSWIDTH, DM_POSITION, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsExW,
    };

    let device = crate::watch::wide(&gdi_name(p));
    let mut mode = DEVMODEW { dmSize: size_of::<DEVMODEW>() as u16, ..Default::default() };
    let fields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_POSITION;
    if unsafe { EnumDisplaySettingsExW(device.as_ptr(), ENUM_CURRENT_SETTINGS, &mut mode, 0) } != 0
        && mode.dmFields & fields == fields && mode.dmPelsWidth > 0 && mode.dmPelsHeight > 0
    {
        // SAFETY: A display call populated the mode's position field.
        let pos = unsafe { mode.Anonymous1.Anonymous2.dmPosition };
        return Area { x: pos.x, y: pos.y, w: mode.dmPelsWidth as i32, h: mode.dmPelsHeight as i32 };
    }

    let mut desktop = area(m);
    if p.rotation().swaps_axes() && desktop.w > desktop.h {
        (desktop.w, desktop.h) = (desktop.h, desktop.w);
    }
    desktop
}

#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    /// (mode index, x, y) in physical pixels.
    pub moves: Vec<(usize, i32, i32)>,
    /// The two-monitor layout, whose order may need swapping.
    pub dual: bool,
    /// Where the laptop screen ends up, used by planner tests.
    #[cfg(test)]
    pub laptop: Option<Side>,
}

/// Where each source should sit.
///
/// - Two external monitors, neither ultrawide: side by side with tops aligned, keeping their current
///   left-to-right order (reversed with `swap_monitors`). The right one is primary.
/// - Otherwise: the first external display is primary at (0, 0) and any others follow it to the right
///   with tops aligned.
///
/// The laptop goes on `laptop` if given, else stays on the side it is on now, else goes below two
/// monitors or to the left of one.
pub fn plan_layout(paths: &[PathInfo], modes: &[ModeInfo], laptop: Option<Side>, swap_monitors: bool) -> Plan {
    let mut seen = Vec::new();
    let mut externals = Vec::new();
    let mut internals = Vec::new();
    for p in paths.iter().filter(|p| p.flags & PATH_ACTIVE != 0) {
        let idx = p.source.mode_info_idx as usize;
        if idx >= modes.len() || modes[idx].info_type != MODE_INFO_TYPE_SOURCE || seen.contains(&idx) {
            continue;
        }
        seen.push(idx);
        if p.is_internal() { internals.push(idx) } else { externals.push(idx) }
    }
    if externals.is_empty() {
        return Plan::default();
    }

    let before: Vec<Area> = externals.iter().map(|&i| area(&modes[i])).collect();
    let current_side = internals.first().and_then(|&i| laptop_side(&before, area(&modes[i])));
    let dual = is_dual(&before);

    let mut moves = Vec::new();
    if dual {
        let mut pair = externals.clone();
        pair.sort_by_key(|&i| modes[i].source_mode().x);
        if swap_monitors {
            pair.reverse();
        }
        // The primary display must sit at (0, 0), so the seam between the two is at x = 0.
        moves.push((pair[0], -area(&modes[pair[0]]).w, 0));
        moves.push((pair[1], 0, 0));
    } else {
        let mut x = 0;
        for &idx in &externals {
            moves.push((idx, x, 0));
            x += area(&modes[idx]).w;
        }
    }
    let after: Vec<Area> = moves.iter().map(|&(i, x, y)| Area { x, y, ..area(&modes[i]) }).collect();

    let side = laptop.or(current_side).unwrap_or(if dual { Side::Below } else { Side::Left });
    let mut next: Option<(i32, i32)> = None;
    for &idx in &internals {
        let a = area(&modes[idx]);
        // A second internal panel (rare) sits to the right of the first.
        let (x, y) = next.unwrap_or_else(|| laptop_spot(&after, (a.w, a.h), side));
        moves.push((idx, x, y));
        next = Some((x + a.w, y));
    }
    Plan { moves, dual, #[cfg(test)] laptop: (!internals.is_empty()).then_some(side) }
}

#[derive(Debug, Default)]
pub struct Arrangement {
    pub changed: bool,
    pub dual: bool,
}

/// Makes an external display primary and arranges the rest around it (see `plan_layout`).
pub fn arrange(laptop: Option<Side>, swap_monitors: bool) -> Result<Arrangement> {
    let cfg = active()?;
    let mut desktop_modes = cfg.modes.clone();
    for path in cfg.paths.iter().filter(|p| p.flags & PATH_ACTIVE != 0) {
        let idx = path.source.mode_info_idx as usize;
        if let Some(m) = cfg.modes.get(idx).filter(|m| m.info_type == MODE_INFO_TYPE_SOURCE) {
            desktop_modes[idx].set_area(desktop_area(path, m));
        }
    }
    let plan = plan_layout(&cfg.paths, &desktop_modes, laptop, swap_monitors);
    let dual = plan.dual;
    let changes: Vec<_> = plan.moves.iter().filter_map(|&(mode, x, y)| {
        let before = area(&desktop_modes[mode]);
        ((before.x, before.y) != (x, y)).then_some(ModeChange {
            mode, area: Area { x, y, ..before }, rotation: None,
        })
    }).collect();
    if changes.is_empty() {
        return Ok(Arrangement { changed: false, dual });
    }
    apply_layout_changes(cfg, &changes)?;
    Ok(Arrangement { changed: true, dual })
}

/// Saves the preview's desktop positions and rotations, keeping the other mode settings intact.
pub fn apply_positions(original: &[Screen], proposed: &[Screen]) -> Result<()> {
    let cfg = active()?;
    let current = query_screens(&cfg, false);
    if current.len() != cfg.paths.iter().filter(|p| p.flags & PATH_ACTIVE != 0).count() {
        return Err("The active display configuration changed while the preview was open.".into());
    }
    let changes = position_changes(&current, original, proposed)?;
    if changes.is_empty() {
        return Ok(());
    }
    apply_layout_changes(cfg, &changes)
}

fn apply_layout_changes(mut cfg: Config, changes: &[ModeChange]) -> Result<()> {
    if let Some(result) = placement::try_apply(&cfg, changes) {
        return result;
    }

    // This keeps the current topology and exact modes for any device that cannot stage a
    // position-only GDI request.
    apply_mode_changes(&mut cfg, changes);
    cfg.set(SDC_VALIDATE | SDC_USE_SUPPLIED_DISPLAY_CONFIG, "SetDisplayConfig (validate layout)")?;
    cfg.apply(0, "SetDisplayConfig (save layout)")
}

#[derive(Debug, PartialEq, Eq)]
struct ModeChange {
    mode: usize,
    area: Area,
    rotation: Option<Rotation>,
}

fn apply_mode_changes(cfg: &mut Config, changes: &[ModeChange]) {
    for change in changes {
        // Keep the source surface and target timing intact. The desktop size changes with the
        // path's rotation without changing either mode's raw size.
        cfg.modes[change.mode].set_position(change.area.x, change.area.y);
        if let Some(rotation) = change.rotation {
            for path in cfg.paths.iter_mut().filter(|p| {
                p.flags & PATH_ACTIVE != 0 && p.source.mode_info_idx as usize == change.mode
            }) {
                path.target.rotation = rotation as u32;
            }
        }
    }
}

fn position_changes(current: &[(usize, Screen)], original: &[Screen], proposed: &[Screen]) -> Result<Vec<ModeChange>> {
    let changed = || "The active display configuration changed while the preview was open.".to_string();
    let invalid = || "The preview layout is invalid.".to_string();
    if current.len() != original.len() || current.len() != proposed.len() {
        return Err(changed());
    }
    if proposed.iter().filter(|s| s.primary).count() != 1
        || !valid_layout(&proposed.iter().map(Screen::area).collect::<Vec<_>>())
    {
        return Err(invalid());
    }
    let mut seen = HashSet::new();
    let mut moves = Vec::new();
    for (idx, active) in current {
        if !seen.insert(&active.id) {
            return Err(changed());
        }
        let before = original.iter().find(|s| s.id == active.id).ok_or_else(changed)?;
        if active.area() != before.area() || active.primary != before.primary || active.rotation != before.rotation {
            return Err(changed());
        }
        let after = proposed.iter().find(|s| s.id == active.id).ok_or_else(invalid)?;
        let size = if after.rotation.swaps_axes() == active.rotation.swaps_axes() {
            (active.width, active.height)
        } else {
            (active.height, active.width)
        };
        if (after.width, after.height) != size || after.primary != (after.x == 0 && after.y == 0)
        {
            return Err(invalid());
        }
        let rotation = (after.rotation != active.rotation).then_some(after.rotation);
        if after.area() != active.area() || rotation.is_some() {
            moves.push(ModeChange { mode: *idx, area: after.area(), rotation });
        }
    }
    Ok(moves)
}

/// One desktop area as the user sees it. Duplicated monitors share one entry.
#[derive(Clone, Debug, PartialEq)]
pub struct Screen {
    /// Target device path, falling back to the display configuration's target identifier.
    pub id: String,
    pub name: String,
    pub internal: bool,
    pub primary: bool,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub rotation: Rotation,
    pub scale: Option<Scale>,
    /// Physical size of the visible picture in millimetres (width, height), from the monitor's EDID.
    pub mm: Option<(f32, f32)>,
}

impl Screen {
    pub fn area(&self) -> Area {
        Area { x: self.x, y: self.y, w: self.width, h: self.height }
    }
}

pub fn screens() -> Result<Vec<Screen>> {
    let cfg = active()?;
    Ok(screens_with_modes(&cfg).into_iter().map(|(_, s)| s).collect())
}

fn screens_with_modes(cfg: &Config) -> Vec<(usize, Screen)> {
    query_screens(cfg, true)
}

fn query_screens(cfg: &Config, read_panel_settings: bool) -> Vec<(usize, Screen)> {
    let mut out: Vec<(usize, Screen)> = Vec::new();
    for p in cfg.paths.iter().filter(|p| p.flags & PATH_ACTIVE != 0) {
        let idx = p.source.mode_info_idx as usize;
        let Some(m) = cfg.modes.get(idx).filter(|m| m.info_type == MODE_INFO_TYPE_SOURCE) else { continue };
        let desktop = desktop_area(p, m);
        let target = target_name(p);
        let name = if p.is_internal() { String::from("Laptop") } else { friendly_name(target.as_ref()) };
        if let Some((_, s)) = out.iter_mut().find(|(i, _)| *i == idx) {
            s.name = format!("{} + {}", s.name, name);
            s.internal &= p.is_internal();
            continue;
        }
        let device_path = target.map(|t| from_wide(&t.device_path)).unwrap_or_default();
        out.push((
            idx,
            Screen {
                id: target_id(p, &device_path),
                name,
                internal: p.is_internal(),
                primary: desktop.x == 0 && desktop.y == 0,
                x: desktop.x,
                y: desktop.y,
                width: desktop.w,
                height: desktop.h,
                rotation: p.rotation(),
                scale: if read_panel_settings { scale(p).ok() } else { None },
                mm: if read_panel_settings { physical_size(&device_path) } else { None },
            },
        ));
    }
    out
}

fn target_id(p: &PathInfo, device_path: &str) -> String {
    if device_path.is_empty() {
        let adapter = p.target.adapter_id;
        format!("{}:{}:{}", adapter.high, adapter.low, p.target.id)
    } else {
        device_path.to_lowercase()
    }
}

/// The picture size in millimetres from the monitor's EDID, which Windows keeps in the registry
/// under the monitor's device instance. Uses the first detailed timing (millimetre precision),
/// falling back to the basic size (centimetres).
fn physical_size(device_path: &str) -> Option<(f32, f32)> {
    use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_BINARY, RegGetValueW};
    // \\?\DISPLAY#DEL4205#4&30cd1115&0&UID45127#{e6f07b5f-...}
    let mut parts = device_path.split('#');
    let (_, model, instance) = (parts.next()?, parts.next()?, parts.next()?);
    let key = format!(r"SYSTEM\CurrentControlSet\Enum\DISPLAY\{model}\{instance}\Device Parameters");
    let (key, value) = (crate::watch::wide(&key), crate::watch::wide("EDID"));
    // 128 bytes per block; monitors with extension blocks (HDMI, DisplayID) send 256 to 512 or more.
    let mut edid = [0u8; 2048];
    let mut len = edid.len() as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_BINARY,
            null_mut(),
            edid.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if rc != 0 || len < 128 {
        return None;
    }
    edid_size(&edid[..len as usize])
}

fn edid_size(edid: &[u8]) -> Option<(f32, f32)> {
    let d = &edid[54..72];
    let pixel_clock = u16::from_le_bytes([d[0], d[1]]);
    let w = d[12] as u32 | ((d[14] as u32 >> 4) << 8);
    let h = d[13] as u32 | ((d[14] as u32 & 0x0f) << 8);
    if pixel_clock != 0 && w > 0 && h > 0 {
        return Some((w as f32, h as f32));
    }
    let (w_cm, h_cm) = (edid[21], edid[22]);
    (w_cm > 0 && h_cm > 0).then_some((w_cm as f32 * 10.0, h_cm as f32 * 10.0))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scale {
    pub current: u32,
    pub recommended: u32,
    min_rel: i32,
    max_rel: i32,
}

impl Scale {
    pub fn sample(current: u32) -> Self {
        Self { current, recommended: SCALES[0], min_rel: 0, max_rel: SCALES.len() as i32 - 1 }
    }

    fn from_packet(d: &DpiGet) -> Scale {
        let rec = (-d.min_rel).max(0) as usize;
        let cur = (rec as i32 + d.cur_rel).clamp(0, SCALES.len() as i32 - 1) as usize;
        Scale {
            current: SCALES[cur],
            recommended: SCALES[rec.min(SCALES.len() - 1)],
            min_rel: d.min_rel,
            max_rel: d.max_rel,
        }
    }

    /// The offset from the recommended step that gives `percent`.
    pub fn relative_for(&self, percent: u32) -> Result<i32> {
        let want = SCALES.iter().position(|&s| s == percent).ok_or(format!("{percent}% is not a Windows scaling step"))?;
        let rel = want as i32 + self.min_rel;
        if rel < self.min_rel || rel > self.max_rel {
            return Err(format!("{percent}% is outside what this monitor allows"));
        }
        Ok(rel)
    }

    fn for_4k_baseline(&self, baseline: u32, desktop: Area) -> Result<u32> {
        const SHORT_SIDE_4K: u32 = 2160;
        let short_side = desktop.w.min(desktop.h);
        let short_side = if short_side > 0 { short_side as u32 } else { SHORT_SIDE_4K };
        // Compare exact distances before dividing by 2160. SCALES is in ascending order,
        // so a tie picks the lower step.
        let desired = u64::from(baseline) * u64::from(short_side);
        SCALES.into_iter()
            .filter(|&step| self.relative_for(step).is_ok())
            .min_by_key(|&step| (u64::from(step) * u64::from(SHORT_SIDE_4K)).abs_diff(desired))
            .ok_or_else(|| "Windows did not report a supported display scale.".into())
    }
}

pub fn scale(p: &PathInfo) -> Result<Scale> {
    let mut d = DpiGet { header: header::<DpiGet>(GET_DPI_SCALE, p.source.adapter_id, p.source.id), ..Default::default() };
    check(unsafe { DisplayConfigGetDeviceInfo(&mut d.header) }, "get DPI scale")?;
    Ok(Scale::from_packet(&d))
}

/// Applies a literal scale to laptop screens or a resolution-adjusted 4K baseline to external
/// monitors. Returns the name and applied scale of each changed display.
pub fn scale_screens(internal: bool, configured_percent: u32) -> Result<Vec<(String, u32)>> {
    let cfg = active()?;
    let mut changes = Vec::new();
    let mut done = Vec::new();
    for p in cfg.paths.iter().filter(|p| p.flags & PATH_ACTIVE != 0 && p.is_internal() == internal) {
        if done.contains(&p.source_key()) {
            continue;
        }
        done.push(p.source_key());
        let s = scale(p)?;
        let percent = if internal {
            configured_percent
        } else {
            let desktop = cfg.modes.get(p.source.mode_info_idx as usize)
                .filter(|m| m.info_type == MODE_INFO_TYPE_SOURCE)
                .map_or_else(Area::default, |m| desktop_area(p, m));
            s.for_4k_baseline(configured_percent, desktop)?
        };
        if s.current != percent {
            changes.push((p, percent, s.relative_for(percent)?));
        }
    }

    let mut changed = Vec::new();
    for (p, percent, relative) in changes {
        set_relative_scale(p, relative)?;
        let name = monitor_name(p);
        let name = if name.is_empty() || name == "(unknown)" { gdi_name(p) } else { name };
        changed.push((name, percent));
    }
    Ok(changed)
}

fn set_relative_scale(p: &PathInfo, rel: i32) -> Result<()> {
    let packet = DpiSet { header: header::<DpiSet>(SET_DPI_SCALE, p.source.adapter_id, p.source.id), rel };
    check(unsafe { DisplayConfigSetDeviceInfo(&packet.header) }, "set DPI scale")
}

fn target_name(p: &PathInfo) -> Option<TargetName> {
    let mut t = TargetName {
        header: header::<TargetName>(GET_TARGET_NAME, p.target.adapter_id, p.target.id),
        flags: 0,
        output_technology: 0,
        edid_manufacture_id: 0,
        edid_product_code_id: 0,
        connector_instance: 0,
        friendly_name: [0; 64],
        device_path: [0; 128],
    };
    (unsafe { DisplayConfigGetDeviceInfo(&mut t.header) } == 0).then_some(t)
}

pub fn monitor_name(p: &PathInfo) -> String {
    let name = friendly_name(target_name(p).as_ref());
    if name.is_empty() && p.is_internal() { String::from("Built-in screen") } else { name }
}

fn friendly_name(target: Option<&TargetName>) -> String {
    target.map_or_else(|| String::from("(unknown)"), |t| from_wide(&t.friendly_name))
}

pub fn gdi_name(p: &PathInfo) -> String {
    let mut s = SourceName { header: header::<SourceName>(GET_SOURCE_NAME, p.source.adapter_id, p.source.id), gdi_name: [0; 32] };
    if unsafe { DisplayConfigGetDeviceInfo(&mut s.header) } != 0 {
        return String::from("?");
    }
    from_wide(&s.gdi_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(source: u32, target: u32, internal: bool) -> PathInfo {
        let mut p = PathInfo::default();
        p.flags = PATH_ACTIVE;
        p.source.id = source;
        p.source.mode_info_idx = source;
        p.target.id = target;
        p.target.output_technology = if internal { OUTPUT_INTERNAL } else { 10 };
        p
    }

    fn mode(width: u32, height: u32, x: i32, y: i32) -> ModeInfo {
        let mut m = ModeInfo { info_type: MODE_INFO_TYPE_SOURCE, ..Default::default() };
        // SAFETY: test-only write of the source arm.
        let sm = unsafe { &mut *(m.data.as_mut_ptr() as *mut SourceMode) };
        sm.width = width;
        sm.height = height;
        sm.x = x;
        sm.y = y;
        m
    }

    #[test]
    fn struct_sizes_match_windows() {
        assert_eq!(size_of::<PathInfo>(), 72);
        assert_eq!(size_of::<ModeInfo>(), 64);
        assert_eq!(size_of::<TargetName>(), 420);
        assert_eq!(size_of::<SourceName>(), 84);
        assert_eq!(size_of::<DpiGet>(), 32);
        assert_eq!(size_of::<DpiSet>(), 24);
    }

    #[test]
    fn detects_duplicate() {
        assert!(is_duplicate(&[path(0, 1, true), path(0, 2, false)]));
        assert!(!is_duplicate(&[path(0, 1, true), path(1, 2, false)]));
        assert!(!is_duplicate(&[path(0, 1, true)]));
    }

    fn sorted(mut moves: Vec<(usize, i32, i32)>) -> Vec<(usize, i32, i32)> {
        moves.sort();
        moves
    }

    #[test]
    fn ultrawide_defaults_to_laptop_left_with_top_at_half_height() {
        let paths = [path(0, 1, true), path(1, 2, false)];
        let modes = [mode(3840, 2400, 0, 0), mode(5120, 2160, 3840, 0)];
        let plan = plan_layout(&paths, &modes, None, false);
        assert_eq!(plan.laptop, Some(Side::Left));
        assert!(!plan.dual);
        assert_eq!(sorted(plan.moves), vec![(0, -3840, 1080), (1, 0, 0)]);
    }

    #[test]
    fn ultrawide_laptop_right_or_below() {
        let paths = [path(0, 1, true), path(1, 2, false)];
        let modes = [mode(3840, 2400, -3840, 1080), mode(5120, 2160, 0, 0)];
        let right = plan_layout(&paths, &modes, Some(Side::Right), false);
        assert_eq!(sorted(right.moves), vec![(0, 5120, 1080), (1, 0, 0)]);
        // Below: centred under the monitor.
        let below = plan_layout(&paths, &modes, Some(Side::Below), false);
        assert_eq!(sorted(below.moves), vec![(0, 2560 - 1920, 2160), (1, 0, 0)]);
    }

    #[test]
    fn keeps_the_laptop_on_its_current_side() {
        let paths = [path(0, 1, true), path(1, 2, false)];
        let modes = [mode(3840, 2400, 5120, 1080), mode(5120, 2160, 0, 0)];
        let plan = plan_layout(&paths, &modes, None, false);
        assert_eq!(plan.laptop, Some(Side::Right));
        assert_eq!(sorted(plan.moves), vec![(0, 5120, 1080), (1, 0, 0)]);
    }

    #[test]
    fn dual_defaults_to_right_primary_and_laptop_below_the_seam() {
        // Windows currently has the 1440p monitor on the right; keep that order.
        let paths = [path(0, 1, true), path(1, 2, false), path(2, 3, false)];
        let modes = [mode(3840, 2400, 0, 0), mode(2560, 1440, 4000, 0), mode(1920, 1080, -1920, 0)];
        let plan = plan_layout(&paths, &modes, None, false);
        assert!(plan.dual);
        assert_eq!(plan.laptop, Some(Side::Below));
        assert_eq!(sorted(plan.moves), vec![(0, -1920, 1440), (1, 0, 0), (2, -1920, 0)]);
    }

    #[test]
    fn dual_laptop_left_or_right() {
        let paths = [path(0, 1, true), path(1, 2, false), path(2, 3, false)];
        let modes = [mode(3840, 2400, 0, 0), mode(2560, 1440, 4000, 0), mode(1920, 1080, -1920, 0)];
        // Left of the left (1080p) monitor, top halfway down it.
        let left = plan_layout(&paths, &modes, Some(Side::Left), false);
        assert_eq!(sorted(left.moves), vec![(0, -1920 - 3840, 540), (1, 0, 0), (2, -1920, 0)]);
        // Right of the right (1440p) monitor, top halfway down it.
        let right = plan_layout(&paths, &modes, Some(Side::Right), false);
        assert_eq!(sorted(right.moves), vec![(0, 2560, 720), (1, 0, 0), (2, -1920, 0)]);
    }

    #[test]
    fn swapping_monitors_keeps_the_laptop_side() {
        // Laptop already below the seam; swapping the pair keeps it below the new seam.
        let paths = [path(0, 1, true), path(1, 2, false), path(2, 3, false)];
        let modes = [mode(3840, 2400, -1920, 1440), mode(2560, 1440, 0, 0), mode(1920, 1080, -1920, 0)];
        let plan = plan_layout(&paths, &modes, None, true);
        assert_eq!(plan.laptop, Some(Side::Below));
        assert_eq!(sorted(plan.moves), vec![(0, -1920, 1440), (1, -2560, 0), (2, 0, 0)]);
    }

    #[test]
    fn nothing_to_choose_with_lid_closed_and_one_monitor() {
        let paths = [path(1, 2, false)];
        let modes = [ModeInfo::default(), mode(5120, 2160, 0, 0)];
        let plan = plan_layout(&paths, &modes, None, false);
        assert!(!plan.dual);
        assert_eq!(plan.laptop, None);
    }

    #[test]
    fn ultrawide_pair_is_a_row() {
        let paths = [path(0, 1, true), path(1, 2, false), path(2, 3, false)];
        let modes = [mode(3840, 2400, 0, 0), mode(5120, 2160, 0, 0), mode(2560, 1440, 5120, 0)];
        let plan = plan_layout(&paths, &modes, None, false);
        assert!(!plan.dual);
        assert_eq!(sorted(plan.moves), vec![(0, -3840, 1080), (1, 0, 0), (2, 5120, 0)]);
    }

    #[test]
    fn two_externals_with_lid_closed() {
        let paths = [path(1, 2, false), path(2, 3, false)];
        let modes = [ModeInfo::default(), mode(2560, 1440, 0, 0), mode(2560, 1440, 2560, 0)];
        let plan = plan_layout(&paths, &modes, None, false);
        assert_eq!(sorted(plan.moves), vec![(1, -2560, 0), (2, 0, 0)]);
    }

    #[test]
    fn no_plan_without_external() {
        let paths = [path(0, 1, true)];
        let modes = [mode(3840, 2400, 0, 0)];
        assert_eq!(plan_layout(&paths, &modes, None, false), Plan::default());
    }

    #[test]
    fn custom_layout_keeps_the_portrait_geometry_and_uses_current_mode_indices() {
        let main = Screen {
            id: "main".into(), name: "Display".into(), internal: false, primary: true,
            x: 0, y: 0, width: 2560, height: 1440, rotation: Rotation::Landscape, scale: None, mm: None,
        };
        let portrait = Screen {
            id: "portrait".into(), name: "Display".into(), internal: false, primary: false,
            x: -1440, y: -200, width: 1440, height: 2560, rotation: Rotation::Portrait, scale: None, mm: None,
        };
        let current = [(7, portrait.clone()), (3, main.clone())];
        let original = [main.clone(), portrait.clone()];
        let mut proposed = [main, Screen { x: 300, y: -2560, ..portrait }];
        assert_eq!(
            position_changes(&current, &original, &proposed).unwrap(),
            vec![ModeChange { mode: 7, area: proposed[1].area(), rotation: None }],
        );

        proposed[1].y -= 1;
        assert!(position_changes(&current, &original, &proposed).is_err());
        proposed[1].y += 2;
        assert!(position_changes(&current, &original, &proposed).is_err());
    }

    #[test]
    fn custom_layout_rejects_a_replaced_or_rotated_display() {
        let main = Screen {
            id: "main".into(), name: "Display".into(), internal: false, primary: true,
            x: 0, y: 0, width: 2560, height: 1440, rotation: Rotation::Landscape, scale: None, mm: None,
        };
        let secondary = Screen {
            id: "secondary".into(), name: "Display".into(), internal: false, primary: false,
            x: 2560, y: 0, width: 2560, height: 1440, rotation: Rotation::Landscape, scale: None, mm: None,
        };
        let original = [main.clone(), secondary.clone()];
        let proposed = [main.clone(), Screen { y: 200, ..secondary.clone() }];
        let replacement = Screen { id: "replacement".into(), ..secondary.clone() };
        let rotated = Screen { rotation: Rotation::LandscapeFlipped, ..secondary };
        for changed in [replacement, rotated] {
            let current = [(3, main.clone()), (7, changed)];
            assert!(position_changes(&current, &original, &proposed).is_err());
        }
    }

    #[test]
    fn custom_layout_can_rebase_to_a_different_primary_display() {
        let main = Screen {
            id: "main".into(), name: "Display".into(), internal: false, primary: true,
            x: 0, y: 0, width: 1920, height: 1080, rotation: Rotation::Landscape, scale: None, mm: None,
        };
        let other = Screen {
            id: "other".into(), name: "Display".into(), internal: false, primary: false,
            x: 1920, y: 250, width: 1440, height: 2560, rotation: Rotation::Portrait, scale: None, mm: None,
        };
        let original = [main.clone(), other.clone()];
        let current = [(1, main.clone()), (2, other.clone())];
        let mut proposed = [
            Screen { x: -1920, y: -250, primary: false, ..main },
            Screen { x: 0, y: 0, primary: true, ..other },
        ];
        let changes = position_changes(&current, &original, &proposed).unwrap();
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].area, proposed[0].area());
        assert_eq!(changes[1].area, proposed[1].area());
        assert!(changes.iter().all(|change| change.rotation.is_none()));

        proposed[0].primary = true;
        assert!(position_changes(&current, &original, &proposed).is_err());
        proposed[1].primary = false;
        assert!(position_changes(&current, &original, &proposed).is_err());
    }

    #[test]
    fn custom_rotation_preserves_the_raw_source_size() {
        let main = Screen {
            id: "main".into(), name: "Display".into(), internal: false, primary: true,
            x: 0, y: 0, width: 1920, height: 1080, rotation: Rotation::Landscape, scale: None, mm: None,
        };
        let external = Screen {
            id: "external".into(), name: "Display".into(), internal: false, primary: false,
            x: 1920, y: 0, width: 2560, height: 1440, rotation: Rotation::Landscape, scale: None, mm: None,
        };
        let current = [(0, main.clone()), (1, external.clone())];
        let original = [main.clone(), external.clone()];
        let mut proposed = [main, Screen { rotation: Rotation::PortraitFlipped, ..external }];
        assert!(position_changes(&current, &original, &proposed).is_err());

        (proposed[1].width, proposed[1].height) = (1440, 2560);
        proposed[1].y = 300;
        let changes = position_changes(&current, &original, &proposed).unwrap();
        let mut cfg = Config {
            paths: vec![path(0, 5, false), path(1, 6, false)],
            modes: vec![mode(1920, 1080, 0, 0), mode(2560, 1440, 1920, 0)],
        };
        apply_mode_changes(&mut cfg, &changes);
        assert_eq!(area(&cfg.modes[1]), Area { y: 300, ..original[1].area() });
        assert_eq!(cfg.paths[1].rotation(), Rotation::PortraitFlipped);
        assert_eq!(area(&cfg.modes[0]), original[0].area());
        assert_eq!(cfg.paths[0].rotation(), Rotation::Landscape);
    }

    #[test]
    fn reads_size_from_edid() {
        let mut edid = [0u8; 128];
        edid[21] = 93;
        edid[22] = 39;
        assert_eq!(edid_size(&edid), Some((930.0, 390.0)));
        // Detailed timing: 977 x 412 mm.
        edid[54] = 0x01;
        edid[66] = (977 & 0xff) as u8;
        edid[67] = (412 & 0xff) as u8;
        edid[68] = (((977 >> 8) << 4) | (412 >> 8)) as u8;
        assert_eq!(edid_size(&edid), Some((977.0, 412.0)));
    }
    #[test]
    fn relative_scale_from_recommended() {
        // Recommended 150% (index 2): min_rel is -2.
        let s = Scale::from_packet(&DpiGet { min_rel: -2, cur_rel: 0, max_rel: 3, ..Default::default() });
        assert_eq!(s.recommended, 150);
        assert_eq!(s.current, 150);
        assert_eq!(s.relative_for(150).unwrap(), 0);
        assert_eq!(s.relative_for(100).unwrap(), -2);
        assert!(s.relative_for(300).is_err());
    }

    #[test]
    fn external_scaling_uses_the_shorter_side_in_any_orientation() {
        let scale = Scale::sample(150);
        for (w, h, expected) in [
            (3840, 2160, 150),
            (2160, 3840, 150),
            (5120, 2160, 150),
            (2560, 1440, 100),
            (3440, 1440, 100),
            (1080, 1920, 100),
            (7680, 4320, 300),
        ] {
            let desktop = Area { w, h, ..Default::default() };
            assert_eq!(scale.for_4k_baseline(150, desktop).unwrap(), expected, "{w}×{h}");
        }
    }

    #[test]
    fn external_scaling_rounds_to_the_nearest_allowed_step() {
        let scale = Scale::from_packet(&DpiGet { min_rel: -1, max_rel: 1, ..Default::default() });
        let hd = Area { w: 1920, h: 1080, ..Default::default() };
        let qhd = Area { w: 2560, h: 1440, ..Default::default() };
        let uhd = Area { w: 3840, h: 2160, ..Default::default() };
        assert_eq!(scale.for_4k_baseline(225, hd).unwrap(), 100); // Tied between 100 and 125.
        assert_eq!(scale.for_4k_baseline(200, qhd).unwrap(), 125);
        assert_eq!(scale.for_4k_baseline(300, uhd).unwrap(), 150); // Largest supported step.
    }
}
