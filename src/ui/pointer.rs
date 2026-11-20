use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetPhysicalCursorPos, SetPhysicalCursorPos};

use crate::display::{Area, Screen};

/// The pointer's position relative to one physical display, independent of the desktop origin.
pub(super) struct PointerAnchor {
    screen_id: String,
    local: (i32, i32),
}

impl PointerAnchor {
    pub(super) fn capture(screens: &[Screen]) -> Option<Self> {
        Self::at(screens, physical_position()?)
    }

    pub(super) fn restore(self, screens: &[Screen]) {
        let _ = self.restore_now(screens);
    }

    /// Restore immediately when the native transaction finishes. Display changes can generate
    /// cursor moves themselves, including within the original monitor.
    pub(super) fn restore_now(self, screens: &[Screen]) -> Option<PointerSettlement> {
        let target = self.position_in(screens)?;
        set_physical_position(target);
        Some(PointerSettlement { target, restored_at: Instant::now(), input_time: last_input_time()? })
    }

    fn at(screens: &[Screen], position: (i32, i32)) -> Option<Self> {
        let screen = screens.iter().find(|screen| contains(screen.area(), position))?;
        Some(Self {
            screen_id: screen.id.clone(),
            local: (position.0 - screen.x, position.1 - screen.y),
        })
    }

    fn position_in(&self, screens: &[Screen]) -> Option<(i32, i32)> {
        let area = screens.iter()
            .find(|screen| screen.id == self.screen_id && screen.width > 0 && screen.height > 0)?
            .area();
        Some((
            area.x.saturating_add(self.local.0.min(area.w - 1)),
            area.y.saturating_add(self.local.1.min(area.h - 1)),
        ))
    }
}

/// One optional retry after the preview windows have caught up with the native layout.
pub(super) struct PointerSettlement {
    target: (i32, i32),
    restored_at: Instant,
    input_time: u32,
}

impl PointerSettlement {
    pub(super) fn finish(self) {
        if self.restored_at.elapsed() <= Duration::from_millis(125)
            && last_input_time() == Some(self.input_time)
        {
            set_physical_position(self.target);
        }
    }
}

fn set_physical_position(target: (i32, i32)) {
    if physical_position() != Some(target) {
        // Use physical pixels at both ends, even when the displays have different scaling.
        unsafe { SetPhysicalCursorPos(target.0, target.1) };
    }
}

fn physical_position() -> Option<(i32, i32)> {
    let mut point = POINT::default();
    (unsafe { GetPhysicalCursorPos(&mut point) } != 0).then_some((point.x, point.y))
}

fn last_input_time() -> Option<u32> {
    let mut info = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    (unsafe { GetLastInputInfo(&mut info) } != 0).then_some(info.dwTime)
}

fn contains(area: Area, (x, y): (i32, i32)) -> bool {
    (0..i64::from(area.w)).contains(&(i64::from(x) - i64::from(area.x)))
        && (0..i64::from(area.h)).contains(&(i64::from(y) - i64::from(area.y)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::Rotation;

    fn screen(id: &str, x: i32, width: i32) -> Screen {
        Screen {
            id: id.into(),
            name: "DELL".into(),
            internal: false,
            primary: x == 0,
            x,
            y: 0,
            width,
            height: 1080,
            rotation: Rotation::Landscape,
            scale: None,
            mm: None,
        }
    }

    #[test]
    fn keeps_local_position_when_the_main_display_changes() {
        let anchor = PointerAnchor::at(&[screen("left", -1920, 1920)], (-1720, 140)).unwrap();
        let after = [screen("right", 1920, 1920), screen("left", 0, 1920)];
        assert_eq!(anchor.position_in(&after), Some((200, 140)));
    }

    #[test]
    fn follows_the_same_panel_when_it_moves_below_the_primary() {
        let before = [screen("main", 0, 1920), screen("external", 1920, 1920)];
        let anchor = PointerAnchor::at(&before, (2300, 240)).unwrap();
        let mut external = screen("external", 0, 1920);
        external.y = 1080;
        let after = [screen("main", 0, 1920), external];
        assert_eq!(anchor.position_in(&after), Some((380, 1320)));
    }

    #[test]
    fn keeps_the_exact_pixel_when_the_panel_overlaps_its_old_position() {
        let anchor = PointerAnchor::at(&[screen("left", 0, 1920)], (500, 140)).unwrap();
        assert_eq!(anchor.position_in(&[screen("left", -100, 1920)]), Some((400, 140)));
        assert_eq!(anchor.position_in(&[screen("left", 0, 1920)]), Some((500, 140)));
    }

    #[test]
    fn clamps_to_visible_pixels_and_skips_a_removed_display() {
        let anchor = PointerAnchor::at(&[screen("left", 0, 1920)], (1800, 140)).unwrap();
        assert_eq!(anchor.position_in(&[screen("left", 0, 1280)]), Some((1279, 140)));
        assert_eq!(anchor.position_in(&[screen("right", 0, 1920)]), None);
    }
}
