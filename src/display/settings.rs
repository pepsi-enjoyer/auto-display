use super::{
    Config, PATH_ACTIVE, PathInfo, QDC_ALL_PATHS, Result, Scale, Screen, active, apply_positions,
    from_wide, is_duplicate, query, scale, screens_with_modes, set_relative_scale, target_id,
    target_name,
};

/// The Windows display configuration and per-source scaling before setup or a pending edit.
pub struct Snapshot {
    config: Config,
    screens: Vec<Screen>,
    targets: Vec<String>,
    connected: Vec<String>,
}

impl Snapshot {
    pub fn capture() -> Result<Self> {
        let config = active()?;
        let screens = screens_with_modes(&config)
            .into_iter()
            .map(|(_, screen)| screen)
            .collect();
        let targets: Vec<_> = config.paths.iter().map(path_identity).collect();
        let connected = connected_ids(&available_paths()?);
        if !targets.iter().all(|id| connected.contains(id)) {
            return Err("The connected displays changed while reading their settings.".into());
        }
        Ok(Self {
            config,
            screens,
            targets,
            connected,
        })
    }

    pub fn screens(&self) -> Vec<Screen> {
        self.screens.clone()
    }

    pub fn duplicated(&self) -> bool {
        is_duplicate(&self.config.paths)
    }

    pub fn restore(&self) -> Result<bool> {
        let before = Self::capture()?;
        self.restore_layout(&before)?;
        self.restore_scaling()
            .map_err(|error| roll_back(&before, error))
    }

    fn restore_layout(&self, current: &Self) -> Result<()> {
        if current.connected != self.connected {
            return Err(
                "The connected displays changed, so the previous layout cannot be restored.".into(),
            );
        }
        if !self.duplicated() && !current.duplicated() && self.screens.len() == current.screens.len()
            && self.screens.iter().all(|old| current.screens.iter().any(|screen| {
                old.id == screen.id && old.width == screen.width && old.height == screen.height
                    && old.rotation == screen.rotation
            }))
        {
            return apply_positions(&current.screens, &self.screens);
        }
        let available = available_paths()?;
        if connected_ids(&available) != self.connected {
            return Err("The connected displays changed, so the previous layout cannot be restored.".into());
        }
        restore_config(&self.config, &self.targets, &available)?
            .apply(0, "SetDisplayConfig (restore layout)")
    }

    fn restore_scaling(&self) -> Result<bool> {
        let choices: Vec<_> = self
            .screens
            .iter()
            .filter_map(|s| Some((s.id.as_str(), s.scale?.current)))
            .collect();
        set_scales(&choices)
    }
}

fn path_identity(path: &PathInfo) -> String {
    let device = target_name(path)
        .map(|t| from_wide(&t.device_path))
        .unwrap_or_default();
    target_id(path, &device)
}

fn available_paths() -> Result<Vec<(PathInfo, String)>> {
    let config = query(QDC_ALL_PATHS)?;
    Ok(config
        .paths
        .into_iter()
        .filter(|p| p.target.target_available != 0 || p.flags & PATH_ACTIVE != 0)
        .map(|p| (p, path_identity(&p)))
        .collect())
}

fn connected_ids(paths: &[(PathInfo, String)]) -> Vec<String> {
    let mut ids: Vec<_> = paths.iter().map(|(_, id)| id.clone()).collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// A target can get a new ID when it leaves a cloned source. Use the saved source and the
/// physical display identity to find the same route in Windows' current list of paths.
fn restore_config(
    saved: &Config,
    targets: &[String],
    available: &[(PathInfo, String)],
) -> Result<Config> {
    let mut config = Config {
        paths: saved.paths.clone(),
        modes: saved.modes.clone(),
    };
    for (path, identity) in config.paths.iter_mut().zip(targets) {
        let target = |p: &PathInfo| (p.target.adapter_id, p.target.id);
        let (live, _) = available
            .iter()
            .filter(|(p, id)| id == identity && p.source_key() == path.source_key())
            .min_by_key(|(p, _)| target(p) != target(path))
            .ok_or("Windows no longer offers a path used in the previous layout.")?;
        if target(live) != target(path) {
            path.target.adapter_id = live.target.adapter_id;
            path.target.id = live.target.id;
            if let Some(mode) = config.modes.get_mut(path.target.mode_info_idx as usize) {
                mode.adapter_id = live.target.adapter_id;
                mode.id = live.target.id;
            }
        }
    }
    Ok(config)
}

/// Applies a single live scaling edit to the selected physical display.
pub fn apply_scale(screen: &Screen, percent: u32) -> Result<bool> {
    let expected = screen.scale.ok_or("Scaling is unavailable for this display.")?;
    let cfg = active()?;
    let changed = "The active display configuration changed while the preview was open.";
    if is_duplicate(&cfg.paths) {
        return Err(changed.into());
    }
    let mut matching = cfg.paths.iter().filter(|path| {
        path.flags & PATH_ACTIVE != 0 && path_identity(path) == screen.id
    });
    let path = matching.next().ok_or(changed)?;
    if matching.next().is_some() {
        return Err(changed.into());
    }
    let Some(relative) = scale_change(expected.current, scale(path)?, percent)? else {
        return Ok(false);
    };
    set_relative_scale(path, relative)?;
    Ok(true)
}

fn scale_change(expected: u32, current: Scale, percent: u32) -> Result<Option<i32>> {
    if current.current != expected {
        return Err("Display scaling changed while the preview was open.".into());
    }
    if current.current == percent {
        return Ok(None);
    }
    current.relative_for(percent).map(Some)
}

fn set_scales(choices: &[(&str, u32)]) -> Result<bool> {
    if choices.is_empty() {
        return Ok(false);
    }
    let live = active()?;
    let paths: Vec<_> = live
        .paths
        .iter()
        .filter(|p| p.flags & PATH_ACTIVE != 0)
        .map(|p| (p, path_identity(p)))
        .collect();
    let mut changes: Vec<(&PathInfo, u32)> = Vec::new();
    for &(id, percent) in choices {
        let (path, _) = paths
            .iter()
            .find(|(_, target)| target == id)
            .ok_or("A display changed while applying scaling.")?;
        let path = *path;
        if let Some((_, previous)) = changes
            .iter()
            .find(|(p, _)| p.source_key() == path.source_key())
        {
            if *previous != percent {
                return Err("Duplicated displays must use the same scaling.".into());
            }
        } else {
            scale(path)?.relative_for(percent)?;
            changes.push((path, percent));
        }
    }
    let mut changed = false;
    for (path, percent) in changes {
        let setting = scale(path)?;
        if setting.current != percent {
            set_relative_scale(path, setting.relative_for(percent)?)?;
            changed = true;
        }
    }
    Ok(changed)
}

fn roll_back(before: &Snapshot, error: String) -> String {
    match Snapshot::capture()
        .and_then(|current| before.restore_layout(&current))
        .and_then(|()| before.restore_scaling())
    {
        Ok(_) => error,
        Err(restore_error) => {
            format!("{error}. Could not restore the previous settings: {restore_error}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::{ModeInfo, PathSource, PathTarget};

    fn path(source: u32, target: u32, mode: u32) -> PathInfo {
        PathInfo {
            source: PathSource {
                id: source,
                ..Default::default()
            },
            target: PathTarget {
                id: target,
                mode_info_idx: mode,
                ..Default::default()
            },
            flags: PATH_ACTIVE,
        }
    }

    #[test]
    fn restore_preserves_the_cloned_source_if_a_target_id_changes() {
        let saved = Config {
            paths: vec![path(0, 5, 0), path(0, 6, 1)],
            modes: vec![
                ModeInfo {
                    id: 5,
                    ..Default::default()
                },
                ModeInfo {
                    id: 6,
                    ..Default::default()
                },
            ],
        };
        let targets = ["laptop".into(), "external".into()];
        let available = [
            (path(0, 5, 0), "laptop".into()),
            (path(0, 6, 1), "different-display".into()),
            (path(2, 8, 1), "external".into()),
            (path(0, 8, 1), "external".into()),
        ];
        assert!(restore_config(&saved, &targets, &available[..3]).is_err());
        let restored = restore_config(&saved, &targets, &available).unwrap();
        assert!(is_duplicate(&restored.paths));
        assert_eq!(restored.paths[1].target.id, 8);
        assert_eq!(restored.modes[1].id, 8);
        assert_eq!(saved.paths[1].target.id, 6);
    }

    #[test]
    fn scaling_edits_check_current_value_and_range() {
        let current = Scale::sample(100);
        assert_eq!(scale_change(100, current, 125).unwrap(), Some(1));
        assert_eq!(scale_change(100, current, 100).unwrap(), None);
        assert!(scale_change(125, current, 150).is_err());
        assert!(scale_change(100, current, 123).is_err());
    }
}
