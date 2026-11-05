use std::thread::sleep;
use std::time::Duration;

use crate::display::{self, Side};
use crate::{config, log};

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

/// Arranges the screens and applies the configured scaling.
pub fn apply_layout() -> Result<display::Arrangement, String> {
    let arrangement = arrange(None, false)?;
    scale()?;
    Ok(arrangement)
}

/// Arranges the screens, putting the laptop on `laptop` if given and reversing the two monitors if
/// `swap_monitors` is set.
pub fn arrange(laptop: Option<Side>, swap_monitors: bool) -> Result<display::Arrangement, String> {
    let arrangement = display::arrange(laptop, swap_monitors)?;
    if arrangement.changed {
        log::line(&match (laptop, swap_monitors) {
            (_, true) => "Swapped the two monitors.".to_string(),
            (Some(side), _) => format!("Moved the laptop screen: {}.", side_name(side)),
            _ => "Made an external display primary and arranged the screens.".to_string(),
        });
    }
    Ok(arrangement)
}

pub fn side_name(side: Side) -> &'static str {
    match side {
        Side::Left => "left",
        Side::Right => "right",
        Side::Below => "below",
    }
}

/// Applies the scaling from config.toml.
pub fn scale() -> Result<(), String> {
    let config = config::load();
    for (internal, percent, what) in [
        (false, config.external_scale, "external display(s)"),
        (true, config.laptop_scale, "laptop screen(s)"),
    ] {
        let Some(percent) = percent else { continue };
        let n = display::scale_screens(internal, percent)?;
        if n > 0 {
            log::line(&format!("Set {n} {what} to {percent}%."));
        }
    }
    Ok(())
}
