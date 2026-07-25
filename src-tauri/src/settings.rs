use crate::{
    portal::{
        default_click_effects_enabled, default_cursor_size, default_cursor_smoothing,
        default_cursor_visible, default_zoom_scale, AudioSettings, BackgroundStyle, VideoQuality,
    },
    webcam::WebcamSettings,
};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedSettings {
    pub fps: u32,
    pub quality: VideoQuality,
    pub background: BackgroundStyle,
    #[serde(default)]
    pub custom_background_path: Option<String>,
    #[serde(default = "default_cursor_visible")]
    pub cursor_visible: bool,
    #[serde(default)]
    pub custom_cursor_path: Option<String>,
    #[serde(default = "default_cursor_smoothing")]
    pub cursor_smoothing: u8,
    #[serde(default = "default_cursor_size")]
    pub cursor_size: u8,
    #[serde(default = "default_zoom_scale")]
    pub zoom_scale: f64,
    #[serde(default = "default_click_effects_enabled")]
    pub click_effects_enabled: bool,
    pub audio: AudioSettings,
    #[serde(default)]
    pub webcam: WebcamSettings,
    #[serde(default = "default_hide_bottom_bar")]
    pub hide_bottom_bar: bool,
    #[serde(default)]
    pub bar_height_correction: i32,
}

impl Default for SavedSettings {
    fn default() -> Self {
        Self {
            fps: 120,
            quality: VideoQuality::High,
            background: BackgroundStyle::TahoeDark,
            custom_background_path: None,
            cursor_visible: default_cursor_visible(),
            custom_cursor_path: None,
            cursor_smoothing: default_cursor_smoothing(),
            cursor_size: default_cursor_size(),
            zoom_scale: default_zoom_scale(),
            click_effects_enabled: default_click_effects_enabled(),
            audio: AudioSettings {
                system_enabled: true,
                microphone_enabled: true,
                system_volume: 1.0,
                microphone_volume: 1.0,
                system_device: None,
                microphone_device: None,
            },
            webcam: WebcamSettings::default(),
            hide_bottom_bar: true,
            bar_height_correction: 0,
        }
    }
}

pub fn load() -> SavedSettings {
    let Ok(contents) = fs::read_to_string(config_path()) else {
        return SavedSettings::default();
    };
    serde_json::from_str::<SavedSettings>(&contents)
        .ok()
        .filter(|settings| validate(settings).is_ok())
        .unwrap_or_default()
}

pub fn save(settings: SavedSettings) -> Result<(), String> {
    validate(&settings)?;
    let path = config_path();
    let parent = path
        .parent()
        .ok_or_else(|| "PikScreen settings path has no parent directory.".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create PikScreen settings directory: {error}"))?;
    let contents = serde_json::to_vec_pretty(&settings)
        .map_err(|error| format!("Could not serialize PikScreen settings: {error}"))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let temporary = parent.join(format!("settings-{nonce}.tmp"));
    fs::write(&temporary, contents)
        .map_err(|error| format!("Could not write PikScreen settings: {error}"))?;
    fs::rename(&temporary, &path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        format!("Could not finalize PikScreen settings: {error}")
    })
}

fn config_path() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("pikscreen")
        .join("settings.json")
}

fn default_hide_bottom_bar() -> bool {
    true
}

fn validate(settings: &SavedSettings) -> Result<(), String> {
    if !matches!(settings.fps, 60 | 120 | 180) {
        return Err("Saved recording FPS must be 60, 120, or 180.".to_owned());
    }
    if settings.cursor_smoothing > 100 {
        return Err("Saved cursor smoothing must be between 0 and 100.".to_owned());
    }
    if !(60..=150).contains(&settings.cursor_size) {
        return Err("Saved cursor size must be between 60 and 150 percent.".to_owned());
    }
    if !settings.zoom_scale.is_finite() || !(1.2..=2.4).contains(&settings.zoom_scale) {
        return Err("Saved zoom amount must be between 1.2× and 2.4×.".to_owned());
    }
    if let Some(path) = settings.custom_cursor_path.as_deref() {
        crate::appearance::validate_cursor(PathBuf::from(path).as_path())?;
    }
    if let Some(path) = settings.custom_background_path.as_deref() {
        crate::appearance::validate_background(PathBuf::from(path).as_path())?;
    }
    if !(-120..=160).contains(&settings.bar_height_correction) {
        return Err("Saved dock height correction must be between -120 and 160 pixels.".to_owned());
    }
    settings
        .audio
        .validate()
        .and_then(|_| settings.webcam.validate())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_recording_ui() {
        let settings = SavedSettings::default();
        assert_eq!(settings.fps, 120);
        assert_eq!(settings.cursor_smoothing, 35);
        assert_eq!(settings.cursor_size, 100);
        assert_eq!(settings.zoom_scale, 1.8);
        assert!(settings.cursor_visible);
        assert!(settings.click_effects_enabled);
        assert!(settings.hide_bottom_bar);
        assert!(validate(&settings).is_ok());
    }

    #[test]
    fn invalid_saved_settings_are_rejected_before_writing() {
        let mut settings = SavedSettings::default();
        settings.cursor_smoothing = 101;
        assert_eq!(
            validate(&settings),
            Err("Saved cursor smoothing must be between 0 and 100.".to_owned())
        );
    }

    #[test]
    fn settings_round_trip_through_json() {
        let settings = SavedSettings::default();
        let encoded = serde_json::to_string(&settings).expect("settings should serialize");
        let decoded: SavedSettings =
            serde_json::from_str(&encoded).expect("settings should deserialize");
        assert_eq!(decoded.cursor_smoothing, settings.cursor_smoothing);
        assert_eq!(
            decoded.bar_height_correction,
            settings.bar_height_correction
        );
    }
}
