//! Thin wrapper over the Windows display configuration API (QueryDisplayConfig and friends).
//!
//! The structs are declared here rather than taken from windows-sys so their layout is explicit and
//! the undocumented DPI-scale packets (types -3 and -4) can sit next to the documented ones.

use std::mem::size_of;
use std::ptr::{null, null_mut};

pub type Result<T> = std::result::Result<T, String>;

/// Scaling steps Windows offers, in order. The DPI packets express scale as an offset into this list
/// from the monitor's recommended step.
pub const SCALES: [u32; 12] = [100, 125, 150, 175, 200, 225, 250, 300, 350, 400, 450, 500];

const QDC_ALL_PATHS: u32 = 0x1;
const QDC_ONLY_ACTIVE_PATHS: u32 = 0x2;
const SDC_TOPOLOGY_EXTEND: u32 = 0x4;
const SDC_USE_SUPPLIED_DISPLAY_CONFIG: u32 = 0x20;
const SDC_APPLY: u32 = 0x80;
const SDC_SAVE_TO_DATABASE: u32 = 0x200;
const SDC_ALLOW_CHANGES: u32 = 0x400;
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

fn query(flags: u32) -> Result<Config> {
    loop {
        let (mut np, mut nm) = (0u32, 0u32);
        check(unsafe { GetDisplayConfigBufferSizes(flags, &mut np, &mut nm) }, "GetDisplayConfigBufferSizes")?;
        let mut paths = vec![PathInfo::default(); np as usize];
        let mut modes = vec![ModeInfo::default(); nm as usize];
        let rc = unsafe {
            QueryDisplayConfig(flags, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), null_mut())
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

fn is_ultrawide(m: &SourceMode) -> bool {
    m.height > 0 && m.width as f64 / m.height as f64 >= ULTRAWIDE_RATIO
}

#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    /// (mode index, x, y) in physical pixels.
    pub moves: Vec<(usize, i32, i32)>,
    /// True when the layout has two sides that could be the wrong way round, so the user should be
    /// asked whether to swap.
    pub swappable: bool,
}

/// Where each source should sit.
///
/// - Two external monitors, neither ultrawide: side by side with tops aligned, keeping their current
///   left-to-right order (or reversing it when `swap` is set). The left one is primary. The laptop
///   screen sits below both, centred on the line between them.
/// - Otherwise: the first external display is primary at (0, 0), other externals follow it to the
///   right with tops aligned, and the laptop screen sits to the left of them with its top edge halfway
///   down the primary. With `swap`, the laptop moves to the other side from where it is now.
pub fn plan_layout(paths: &[PathInfo], modes: &[ModeInfo], swap: bool) -> Plan {
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
    let Some(&primary) = externals.first() else { return Plan::default() };
    let primary_mode = modes[primary].source_mode();

    if externals.len() == 2 && externals.iter().all(|&i| !is_ultrawide(&modes[i].source_mode())) {
        let mut pair = externals.clone();
        pair.sort_by_key(|&i| modes[i].source_mode().x);
        if swap {
            pair.reverse();
        }
        let (left, right) = (modes[pair[0]].source_mode(), modes[pair[1]].source_mode());
        let seam = left.width as i32;
        let bottom = left.height.max(right.height) as i32;
        let mut moves = vec![(pair[0], 0, 0), (pair[1], seam, 0)];
        let mut x = seam - internals.iter().map(|&i| modes[i].source_mode().width as i32).sum::<i32>() / 2;
        for &idx in &internals {
            moves.push((idx, x, bottom));
            x += modes[idx].source_mode().width as i32;
        }
        return Plan { moves, swappable: true };
    }

    let mut moves = Vec::new();
    let mut x = 0i32;
    for &idx in &externals {
        moves.push((idx, x, 0));
        x += modes[idx].source_mode().width as i32;
    }
    // Left by default; a swap puts the laptop on the opposite side to where it is now.
    let laptop_now_right = internals.first().is_some_and(|&i| modes[i].source_mode().x > primary_mode.x);
    let on_right = swap && !laptop_now_right;
    let top = primary_mode.height as i32 / 2;
    let mut left = 0i32;
    for &idx in &internals {
        let width = modes[idx].source_mode().width as i32;
        if on_right {
            moves.push((idx, x, top));
            x += width;
        } else {
            left -= width;
            moves.push((idx, left, top));
        }
    }
    Plan { moves, swappable: !internals.is_empty() }
}

#[derive(Debug, Default)]
pub struct Arrangement {
    pub changed: bool,
    pub swappable: bool,
}

/// Makes an external display primary and arranges the rest around it (see `plan_layout`).
pub fn arrange(swap: bool) -> Result<Arrangement> {
    let mut cfg = active()?;
    let plan = plan_layout(&cfg.paths, &cfg.modes, swap);
    let swappable = plan.swappable;
    let unchanged = plan.moves.iter().all(|&(i, x, y)| {
        let sm = cfg.modes[i].source_mode();
        sm.x == x && sm.y == y
    });
    if unchanged {
        return Ok(Arrangement { changed: false, swappable });
    }
    for (i, x, y) in plan.moves {
        cfg.modes[i].set_position(x, y);
    }
    check(
        unsafe {
            SetDisplayConfig(
                cfg.paths.len() as u32,
                cfg.paths.as_ptr(),
                cfg.modes.len() as u32,
                cfg.modes.as_ptr(),
                SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_SAVE_TO_DATABASE | SDC_ALLOW_CHANGES,
            )
        },
        "SetDisplayConfig (arrange)",
    )?;
    Ok(Arrangement { changed: true, swappable })
}

/// One desktop area as the user sees it. Duplicated monitors share one entry.
#[derive(Clone, Debug, PartialEq)]
pub struct Screen {
    pub name: String,
    pub internal: bool,
    pub primary: bool,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

pub fn screens() -> Result<Vec<Screen>> {
    let cfg = active()?;
    let mut out: Vec<(usize, Screen)> = Vec::new();
    for p in cfg.paths.iter().filter(|p| p.flags & PATH_ACTIVE != 0) {
        let idx = p.source.mode_info_idx as usize;
        let Some(m) = cfg.modes.get(idx).filter(|m| m.info_type == MODE_INFO_TYPE_SOURCE) else { continue };
        let sm = m.source_mode();
        let name = if p.is_internal() { String::from("Laptop") } else { monitor_name(p) };
        if let Some((_, s)) = out.iter_mut().find(|(i, _)| *i == idx) {
            s.name = format!("{} + {}", s.name, name);
            s.internal &= p.is_internal();
            continue;
        }
        out.push((
            idx,
            Screen {
                name,
                internal: p.is_internal(),
                primary: sm.x == 0 && sm.y == 0,
                x: sm.x,
                y: sm.y,
                width: sm.width as i32,
                height: sm.height as i32,
            },
        ));
    }
    Ok(out.into_iter().map(|(_, s)| s).collect())
}

pub struct Scale {
    pub current: u32,
    pub recommended: u32,
    min_rel: i32,
    max_rel: i32,
}

impl Scale {
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
}

pub fn scale(p: &PathInfo) -> Result<Scale> {
    let mut d = DpiGet { header: header::<DpiGet>(GET_DPI_SCALE, p.source.adapter_id, p.source.id), ..Default::default() };
    check(unsafe { DisplayConfigGetDeviceInfo(&mut d.header) }, "get DPI scale")?;
    Ok(Scale::from_packet(&d))
}

/// Sets every external display to `percent`. Returns how many were changed.
pub fn scale_externals(percent: u32) -> Result<usize> {
    let cfg = active()?;
    let mut changed = 0;
    let mut done = Vec::new();
    for p in cfg.paths.iter().filter(|p| !p.is_internal()) {
        if done.contains(&p.source_key()) {
            continue;
        }
        done.push(p.source_key());
        let s = scale(p)?;
        if s.current == percent {
            continue;
        }
        let packet = DpiSet { header: header::<DpiSet>(SET_DPI_SCALE, p.source.adapter_id, p.source.id), rel: s.relative_for(percent)? };
        check(unsafe { DisplayConfigSetDeviceInfo(&packet.header) }, "set DPI scale")?;
        changed += 1;
    }
    Ok(changed)
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
    let Some(t) = target_name(p) else { return String::from("(unknown)") };
    let name = from_wide(&t.friendly_name);
    if name.is_empty() && p.is_internal() { String::from("Built-in screen") } else { name }
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

    #[test]
    fn laptop_left_with_top_at_half_height() {
        let paths = [path(0, 1, true), path(1, 2, false)];
        let modes = [mode(3840, 2400, 0, 0), mode(5120, 2160, 3840, 0)];
        let mut plan = plan_layout(&paths, &modes, false).moves;
        plan.sort();
        assert_eq!(plan, vec![(0, -3840, 1080), (1, 0, 0)]);
    }

    #[test]
    fn two_normal_externals_with_laptop_centred_below() {
        // Windows currently has the 1440p monitor on the right; keep that order.
        let paths = [path(0, 1, true), path(1, 2, false), path(2, 3, false)];
        let modes = [mode(3840, 2400, 0, 0), mode(2560, 1440, 4000, 0), mode(1920, 1080, -1920, 0)];
        let mut plan = plan_layout(&paths, &modes, false).moves;
        plan.sort();
        assert_eq!(plan, vec![(0, 1920 - 1920, 1440), (1, 1920, 0), (2, 0, 0)]);
        assert!(plan_layout(&paths, &modes, false).swappable);
    }

    #[test]
    fn swap_reverses_the_pair_and_recentres_the_laptop() {
        let paths = [path(0, 1, true), path(1, 2, false), path(2, 3, false)];
        let modes = [mode(3840, 2400, 0, 0), mode(2560, 1440, 4000, 0), mode(1920, 1080, -1920, 0)];
        let mut plan = plan_layout(&paths, &modes, true).moves;
        plan.sort();
        assert_eq!(plan, vec![(0, 2560 - 1920, 1440), (1, 0, 0), (2, 2560, 0)]);
    }

    #[test]
    fn swap_moves_the_laptop_to_the_other_side_of_the_ultrawide() {
        let paths = [path(0, 1, true), path(1, 2, false)];
        // Laptop on the left (the default): swapping puts it on the right.
        let left = [mode(3840, 2400, -3840, 1080), mode(5120, 2160, 0, 0)];
        let plan = plan_layout(&paths, &left, true);
        assert!(plan.swappable);
        let mut moves = plan.moves;
        moves.sort();
        assert_eq!(moves, vec![(0, 5120, 1080), (1, 0, 0)]);
        // Laptop on the right: swapping puts it back on the left.
        let right = [mode(3840, 2400, 5120, 1080), mode(5120, 2160, 0, 0)];
        let mut moves = plan_layout(&paths, &right, true).moves;
        moves.sort();
        assert_eq!(moves, vec![(0, -3840, 1080), (1, 0, 0)]);
        // Without a swap the laptop goes on the left whatever the starting point.
        let mut moves = plan_layout(&paths, &right, false).moves;
        moves.sort();
        assert_eq!(moves, vec![(0, -3840, 1080), (1, 0, 0)]);
    }

    #[test]
    fn nothing_to_swap_with_lid_closed_and_one_monitor() {
        let paths = [path(1, 2, false)];
        let modes = [ModeInfo::default(), mode(5120, 2160, 0, 0)];
        assert!(!plan_layout(&paths, &modes, false).swappable);
    }

    #[test]
    fn ultrawide_pair_uses_default_layout() {
        let paths = [path(0, 1, true), path(1, 2, false), path(2, 3, false)];
        let modes = [mode(3840, 2400, 0, 0), mode(5120, 2160, 0, 0), mode(2560, 1440, 5120, 0)];
        let mut plan = plan_layout(&paths, &modes, false).moves;
        plan.sort();
        assert_eq!(plan, vec![(0, -3840, 1080), (1, 0, 0), (2, 5120, 0)]);
    }

    #[test]
    fn two_externals_with_lid_closed() {
        let paths = [path(1, 2, false), path(2, 3, false)];
        let modes = [ModeInfo::default(), mode(2560, 1440, 0, 0), mode(2560, 1440, 2560, 0)];
        let mut plan = plan_layout(&paths, &modes, false).moves;
        plan.sort();
        assert_eq!(plan, vec![(1, 0, 0), (2, 2560, 0)]);
    }

    #[test]
    fn no_plan_without_external() {
        let paths = [path(0, 1, true)];
        let modes = [mode(3840, 2400, 0, 0)];
        assert_eq!(plan_layout(&paths, &modes, false), Plan::default());
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
}
