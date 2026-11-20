use std::mem::size_of;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::POINTL;
use windows_sys::Win32::Graphics::Gdi::{
    CDS_NORESET, CDS_SET_PRIMARY, CDS_UPDATEREGISTRY, ChangeDisplaySettingsExW,
    DEVMODEW, DISP_CHANGE_SUCCESSFUL, DM_POSITION, ENUM_CURRENT_SETTINGS,
    ENUM_DISPLAY_SETTINGS_MODE, ENUM_REGISTRY_SETTINGS, EnumDisplaySettingsExW,
};

use super::{Area, Config, MODE_INFO_TYPE_SOURCE, ModeChange, PATH_ACTIVE, Result, gdi_name, is_duplicate, valid_layout};

struct Device {
    name: Vec<u16>,
    mode: DEVMODEW,
    old: (i32, i32),
    new: (i32, i32),
}

/// Returns None if these modes cannot use position-only updates. A rejected batch may fall back
/// only if its staged positions have been cleared and the original modes are still in place.
pub(super) fn try_apply(config: &Config, changes: &[ModeChange]) -> Option<Result<()>> {
    if is_duplicate(&config.paths) || changes.iter().any(|change| change.rotation.is_some()) {
        return None;
    }
    let mut devices: Vec<Device> = Vec::new();
    for path in config.paths.iter().filter(|path| path.flags & PATH_ACTIVE != 0) {
        let index = path.source.mode_info_idx as usize;
        let source = config.modes.get(index)?;
        if source.info_type != MODE_INFO_TYPE_SOURCE {
            return None;
        }
        let name = crate::watch::wide(&gdi_name(path));
        if devices.iter().any(|device| device.name == name) {
            return None;
        }
        let current = read_mode(&name, ENUM_CURRENT_SETTINGS)?;
        let registered = read_mode(&name, ENUM_REGISTRY_SETTINGS)?;
        // The global commit reads saved settings. Do not apply unrelated saved mode changes.
        if !same_mode(&current, &registered) || position(&current) != position(&registered) {
            return None;
        }
        let old = position(&current);
        let source = source.source_mode();
        if old != (source.x, source.y) {
            return Some(Err("The display positions changed while preparing the layout.".into()));
        }
        let new = changes.iter().find(|change| change.mode == index)
            .map_or(old, |change| (change.area.x, change.area.y));
        devices.push(Device { name, mode: current, old, new });
    }

    let proposed: Vec<_> = devices.iter().map(|device| Area {
        x: device.new.0, y: device.new.1,
        w: device.mode.dmPelsWidth as i32, h: device.mode.dmPelsHeight as i32,
    }).collect();
    if !valid_layout(&proposed) || !devices.iter().any(|device| device.new == (0, 0)) {
        return None;
    }

    let changed: Vec<_> = devices.iter().filter(|device| device.old != device.new).collect();
    let new_primary = changed.iter().any(|device| device.new == (0, 0));
    if changed.is_empty() {
        return Some(Ok(()));
    }

    // Setting the primary display can rebase registered positions. Set it first, then stage the
    // requested position for every other affected display.
    commit(&devices, changed, new_primary)
}

fn read_mode(name: &[u16], settings: ENUM_DISPLAY_SETTINGS_MODE) -> Option<DEVMODEW> {
    let mut mode = DEVMODEW { dmSize: size_of::<DEVMODEW>() as u16, ..Default::default() };
    (unsafe { EnumDisplaySettingsExW(name.as_ptr(), settings, &mut mode, 0) } != 0
        && mode.dmFields & DM_POSITION != 0).then_some(mode)
}

fn position(mode: &DEVMODEW) -> (i32, i32) {
    // SAFETY: read_mode queried a display device and populated the display arm of the union.
    let point = unsafe { mode.Anonymous1.Anonymous2.dmPosition };
    (point.x, point.y)
}

fn same_mode(a: &DEVMODEW, b: &DEVMODEW) -> bool {
    let orientation = |mode: &DEVMODEW| {
        // SAFETY: Both modes were populated by display queries.
        let display = unsafe { mode.Anonymous1.Anonymous2 };
        (display.dmDisplayOrientation, display.dmDisplayFixedOutput)
    };
    a.dmFields == b.dmFields
        && a.dmPelsWidth == b.dmPelsWidth
        && a.dmPelsHeight == b.dmPelsHeight
        && a.dmBitsPerPel == b.dmBitsPerPel
        && a.dmDisplayFrequency == b.dmDisplayFrequency
        && orientation(a) == orientation(b)
        // SAFETY: This is the display flags member for display devices.
        && unsafe { a.Anonymous2.dmDisplayFlags == b.Anonymous2.dmDisplayFlags }
}

fn primary_flag(change_primary: bool, position: (i32, i32)) -> u32 {
    if change_primary && position == (0, 0) { CDS_SET_PRIMARY } else { 0 }
}

fn set_position(device: &Device, position: (i32, i32), flags: u32) -> Result<()> {
    let mut mode = device.mode;
    mode.dmDriverExtra = 0;
    mode.dmFields = DM_POSITION;
    mode.Anonymous1.Anonymous2.dmPosition = POINTL { x: position.0, y: position.1 };
    check(unsafe { ChangeDisplaySettingsExW(device.name.as_ptr(), &mode, null_mut(), flags, null()) })
}

fn check(status: i32) -> Result<()> {
    if status == DISP_CHANGE_SUCCESSFUL {
        Ok(())
    } else {
        Err(format!("ChangeDisplaySettingsEx failed with status {status}"))
    }
}

fn activate() -> Result<()> {
    check(unsafe { ChangeDisplaySettingsExW(null(), null(), null_mut(), 0, null()) })
}

fn positions_match(devices: &[Device], settings: ENUM_DISPLAY_SETTINGS_MODE, original: bool) -> bool {
    devices.iter().all(|device| {
        let expected = if original { device.old } else { device.new };
        read_mode(&device.name, settings).is_some_and(|mode| {
            position(&mode) == expected && same_mode(&device.mode, &mode)
        })
    })
}

fn commit(devices: &[Device], mut changed: Vec<&Device>, change_primary: bool) -> Option<Result<()>> {
    changed.sort_by_key(|device| device.new != (0, 0));
    let stage_flags = CDS_UPDATEREGISTRY | CDS_NORESET;
    for device in changed {
        if let Err(error) = set_position(device, device.new, stage_flags | primary_flag(change_primary, device.new)) {
            return match rollback(devices, change_primary) {
                Ok(()) => None,
                Err(restore_error) => Some(Err(format!("{error}. Could not restore previous display positions: {restore_error}"))),
            };
        }
    }
    // Drivers can keep reporting the previous coordinates until the batch is activated.
    let result = activate().and_then(|()| {
        if positions_match(devices, ENUM_CURRENT_SETTINGS, false) {
            Ok(())
        } else {
            Err("Windows did not apply the requested display positions.".into())
        }
    });
    Some(result.map_err(|error| match rollback(devices, change_primary) {
        Ok(()) => error,
        Err(restore_error) => format!("{error}. Could not restore previous display positions: {restore_error}"),
    }))
}

fn rollback(devices: &[Device], change_primary: bool) -> Result<()> {
    let active_changed = !positions_match(devices, ENUM_CURRENT_SETTINGS, true);
    let mut original: Vec<_> = devices.iter().collect();
    original.sort_by_key(|device| device.old != (0, 0));
    let mut rollback_error = None;
    for device in original {
        let flags = CDS_UPDATEREGISTRY | CDS_NORESET | primary_flag(change_primary, device.old);
        if let Err(e) = set_position(device, device.old, flags) {
            rollback_error.get_or_insert(e);
        }
    }
    if rollback_error.is_none() && active_changed {
        rollback_error = activate().err();
    }
    if let Some(error) = rollback_error {
        return Err(error);
    }
    if !positions_match(devices, ENUM_CURRENT_SETTINGS, true)
        || !positions_match(devices, ENUM_REGISTRY_SETTINGS, true)
    {
        return Err("Windows returned a different layout after restoring the old positions.".into());
    }
    Ok(())
}
