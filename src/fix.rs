use crate::display::{self, Side};
use crate::{config, log, shell};

/// Restore the Windows display layout when newly connected monitors come up duplicated.
pub fn if_duplicate() -> Result<bool, String> {
    if !display::is_duplicate(&display::active()?.paths) {
        return Ok(false);
    }
    log::line("Displays are duplicated; switching to extend.");
    display::extend()?;
    setup_current_layout()?;
    Ok(true)
}

/// Prepare the desktop if the on-screen card cannot start.
pub fn setup() -> Result<(), String> {
    if !if_duplicate()? {
        setup_current_layout()?;
    }
    Ok(())
}

fn setup_current_layout() -> Result<(), String> {
    match display::remembered_layout(&display::screens()?) {
        display::StartupLayout::Current => Ok(()),
        display::StartupLayout::Remembered => {
            display::restore_remembered_layout()?;
            log::line("Restored the Windows layout for these displays.");
            Ok(())
        }
        display::StartupLayout::Defaults => apply_layout().map(|_| ()),
    }
}

/// Arranges the screens and applies the configured scaling.
pub fn apply_layout() -> Result<display::Arrangement, String> {
    let arrangement = arrange(None, false)?;
    if scale()? {
        refresh_taskbar();
    }
    Ok(arrangement)
}

/// Restarts Explorer so the taskbar is redrawn at the new scaling, unless config.toml turns that
/// off. A failure is logged and otherwise ignored: the displays themselves are already set up.
pub fn refresh_taskbar() {
    if !config::load().restart_explorer {
        return;
    }
    match shell::restart_explorer() {
        Ok(()) => log::line("Restarted Explorer so the taskbar matches the new scaling."),
        Err(e) => log::line(&format!("error: {e}")),
    }
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

/// Applies the scaling from config.toml. Returns true if any screen's scaling changed.
pub fn scale() -> Result<bool, String> {
    let config = config::load();
    let mut changed = false;
    for (internal, percent) in [(false, config.external_scale), (true, config.laptop_scale)] {
        let Some(percent) = percent else { continue };
        for (name, applied) in display::scale_screens(internal, percent)? {
            log::line(&format!("Set {name} to {applied}% scaling."));
            changed = true;
        }
    }
    Ok(changed)
}
