use std::thread::sleep;
use std::time::Duration;

use crate::{display, log};

pub const EXTERNAL_SCALE: u32 = 150;

/// If the displays are duplicated: extend, make an external display primary, put the laptop screen
/// to its left, and set external displays to 150%.
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

pub fn apply_layout() -> Result<(), String> {
    if display::arrange()? {
        log::line("Made the external display primary and moved the laptop screen to its left.");
    }
    let n = display::scale_externals(EXTERNAL_SCALE)?;
    if n > 0 {
        log::line(&format!("Set {n} external display(s) to {EXTERNAL_SCALE}%."));
    }
    Ok(())
}
