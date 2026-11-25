//! Settings in %LOCALAPPDATA%\auto-display\config.toml. Read each time the displays are set up, so
//! edits apply next time without restarting anything.

use std::fs;
use std::path::PathBuf;

use serde::Deserialize;

use crate::display::SCALES;
use crate::log;

const DEFAULT: &str = "\
# auto-display settings. Changes apply the next time it sets up your displays.

# Scaling for a 4K external monitor, in percent.
# Other external resolutions use the closest supported scale in proportion to their shorter side.
# For example, 150% on 4K gives 100% on both 1440p and 1080p.
external_scale = 150

# Scaling for the laptop screen. Leave commented out to keep what Windows chose.
# laptop_scale = 250

# Apply the scaling above the first time the displays are set up after you sign in, even when
# Windows already has a layout for them. Set to false to keep the scaling Windows remembers.
apply_scaling_at_sign_in = true

# Check GitHub for updates automatically.
auto_update = true

# Restart Explorer after changing the scaling, so the taskbar is redrawn at the new size. This
# closes open File Explorer windows.
restart_explorer = true
";

#[derive(Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub external_scale: Option<u32>,
    pub laptop_scale: Option<u32>,
    pub apply_scaling_at_sign_in: bool,
    pub auto_update: bool,
    pub restart_explorer: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            external_scale: Some(150),
            laptop_scale: None,
            apply_scaling_at_sign_in: true,
            auto_update: true,
            restart_explorer: true,
        }
    }
}

pub fn path() -> PathBuf {
    log::data_dir().join("config.toml")
}

/// Writes the commented default config if there is none yet.
pub fn ensure() -> Result<PathBuf, String> {
    let path = path();
    if !path.exists() {
        fs::create_dir_all(log::data_dir()).map_err(|e| e.to_string())?;
        fs::write(&path, DEFAULT).map_err(|e| format!("could not write {}: {e}", path.display()))?;
    }
    Ok(path)
}

/// The config, or the defaults if the file is missing. Problems are logged and the bad setting
/// ignored, so a typo never stops the displays being fixed.
pub fn load() -> Config {
    let Ok(text) = fs::read_to_string(path()) else { return Config::default() };
    let mut config = match parse(&text) {
        Ok(c) => c,
        Err(e) => {
            log::line(&format!("config.toml: {e}; using the defaults."));
            return Config::default();
        }
    };
    for (name, value) in [("external_scale", &mut config.external_scale), ("laptop_scale", &mut config.laptop_scale)] {
        if let Some(v) = *value
            && !SCALES.contains(&v)
        {
            log::line(&format!("config.toml: {name} = {v} is not a Windows scaling step; ignoring it."));
            *value = None;
        }
    }
    config
}

fn parse(text: &str) -> Result<Config, String> {
    toml::from_str(text).map_err(|e| e.message().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_file_parses_to_the_defaults() {
        assert_eq!(parse(DEFAULT).unwrap(), Config::default());
    }

    #[test]
    fn reads_both_scales() {
        let c = parse("external_scale = 125\nlaptop_scale = 200\n").unwrap();
        assert_eq!(c, Config { external_scale: Some(125), laptop_scale: Some(200), ..Config::default() });
    }

    #[test]
    fn existing_settings_enable_updates_and_can_opt_out() {
        assert!(parse("external_scale = 150\n").unwrap().auto_update);
        assert!(!parse("auto_update = false\n").unwrap().auto_update);
        assert!(parse("external_scale = 150\n").unwrap().restart_explorer);
        assert!(!parse("restart_explorer = false\n").unwrap().restart_explorer);
        assert!(parse("external_scale = 150\n").unwrap().apply_scaling_at_sign_in);
        assert!(!parse("apply_scaling_at_sign_in = false\n").unwrap().apply_scaling_at_sign_in);
    }

    #[test]
    fn rejects_unknown_settings() {
        assert!(parse("externl_scale = 125\n").is_err());
    }
}
