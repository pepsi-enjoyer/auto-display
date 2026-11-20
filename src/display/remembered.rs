use std::ptr::null;

use super::{
    Area, Config, MODE_INFO_TYPE_SOURCE, PATH_ACTIVE, PathInfo, QDC_DATABASE_CURRENT, Result,
    Rotation, SDC_APPLY, Screen, SetDisplayConfig, area, check, from_wide, is_duplicate, query,
    target_id, target_name,
};

const SDC_USE_DATABASE_CURRENT: u32 = 0xF;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupLayout {
    Current,
    Remembered,
    Defaults,
}

/// Inspect the persisted Windows layout for the currently connected displays.
pub fn remembered_layout(active: &[Screen]) -> StartupLayout {
    let saved = match query(QDC_DATABASE_CURRENT) {
        Ok(saved) => saved,
        // A failed read provides no reason to discard an existing layout.
        Err(_) if !active.is_empty() => return StartupLayout::Current,
        Err(_) => return StartupLayout::Defaults,
    };
    let Some(saved) = saved_panels(&saved) else { return StartupLayout::Defaults };
    choose_layout(active, &saved)
}

pub fn restore_remembered_layout() -> Result<()> {
    check(
        unsafe { SetDisplayConfig(0, null(), 0, null(), SDC_APPLY | SDC_USE_DATABASE_CURRENT) },
        "SetDisplayConfig (restore remembered layout)",
    )
}

#[derive(Debug)]
struct SavedPanel {
    id: String,
    area: Area,
    rotation: Rotation,
}

fn saved_panels(saved: &Config) -> Option<Vec<SavedPanel>> {
    if is_duplicate(&saved.paths) {
        return None;
    }
    let panels: Vec<_> = saved.paths.iter().filter(|path| path.flags & PATH_ACTIVE != 0)
        .map(|path| {
            let mode = saved.modes.get(path.source.mode_info_idx as usize)
                .filter(|mode| mode.info_type == MODE_INFO_TYPE_SOURCE)?;
            let mut geometry = area(mode);
            if geometry.w <= 0 || geometry.h <= 0 {
                return None;
            }
            let rotation = path.rotation();
            // The saved source position is in desktop coordinates, but its size can describe the
            // panel's raw surface before rotation.
            if rotation.swaps_axes() && geometry.w > geometry.h {
                (geometry.w, geometry.h) = (geometry.h, geometry.w);
            }
            Some(SavedPanel { id: panel_id(path), area: geometry, rotation })
        })
        .collect::<Option<_>>()?;
    (panels.iter().filter(|panel| (panel.area.x, panel.area.y) == (0, 0)).count() == 1)
        .then_some(panels)
}

fn panel_id(path: &PathInfo) -> String {
    let device_path = target_name(path).map(|target| from_wide(&target.device_path)).unwrap_or_default();
    target_id(path, &device_path)
}

fn choose_layout(active: &[Screen], saved: &[SavedPanel]) -> StartupLayout {
    if active.len() != saved.len()
        || active.iter().any(|screen| saved.iter().filter(|panel| panel.id == screen.id).count() != 1)
    {
        return StartupLayout::Defaults;
    }
    if saved.iter().all(|panel| active.iter().any(|screen| {
        screen.id == panel.id && screen.area() == panel.area && screen.rotation == panel.rotation
    })) {
        StartupLayout::Current
    } else {
        StartupLayout::Remembered
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(id: &str, x: i32) -> Screen {
        Screen {
            id: id.into(),
            name: id.into(),
            internal: false,
            primary: x == 0,
            x,
            y: 0,
            width: 1920,
            height: 1080,
            rotation: Rotation::Landscape,
            scale: None,
            mm: None,
        }
    }

    #[test]
    fn matches_saved_screens_by_identity_and_geometry() {
        let active = [screen("left", -1920), screen("right", 0)];
        let mut saved: Vec<_> = active.iter().rev().map(|screen| SavedPanel {
            id: screen.id.clone(), area: screen.area(), rotation: screen.rotation,
        }).collect();
        assert_eq!(choose_layout(&active, &saved), StartupLayout::Current);

        saved[1].area.y = 200;
        assert_eq!(choose_layout(&active, &saved), StartupLayout::Remembered);

        saved[1].id = "different monitor".into();
        assert_eq!(choose_layout(&active, &saved), StartupLayout::Defaults);
    }
}
