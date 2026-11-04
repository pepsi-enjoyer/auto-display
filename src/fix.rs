use std::thread::sleep;
use std::time::Duration;

use crate::{display, log};

pub const EXTERNAL_SCALE: u32 = 150;

/// Headless fix, used when the on-screen card cannot start: if the displays are duplicated, extend,
/// arrange and scale them.
pub fn if_duplicate() -> Result<bool, String> {
    if !display::is_duplicate(&display::active()?.paths) {
        return Ok(false);
    }
    log::line("Displays are duplicated; switching to extend.");
    display::extend()?;
    sleep(Duration::from_millis(1500));
    apply_layout()?;
    Ok(true)
}

/// Arranges the screens and sets external scaling. Returns the arrangement so the caller knows
/// whether the two-monitor layout was used.
pub fn apply_layout() -> Result<display::Arrangement, String> {
    let arrangement = arrange(false)?;
    scale()?;
    Ok(arrangement)
}

pub fn arrange(swap: bool) -> Result<display::Arrangement, String> {
    let arrangement = display::arrange(swap)?;
    if arrangement.changed {
        log::line(if swap {
            "Swapped the two external monitors."
        } else {
            "Made an external display primary and arranged the screens."
        });
    }
    Ok(arrangement)
}

pub fn scale() -> Result<(), String> {
    let n = display::scale_externals(EXTERNAL_SCALE)?;
    if n > 0 {
        log::line(&format!("Set {n} external display(s) to {EXTERNAL_SCALE}%."));
    }
    Ok(())
}
