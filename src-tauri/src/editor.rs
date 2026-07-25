use crate::{
    cursor::CursorSample,
    portal::{ClickSample, RecordingSettings},
    zoom::Marker,
};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionManifest {
    pub id: String,
    pub directory: PathBuf,
    pub source_path: PathBuf,
    pub audio_path: Option<PathBuf>,
    pub webcam_path: Option<PathBuf>,
    pub working_path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub duration_ms: u64,
    pub trim_start_ms: u64,
    pub trim_end_ms: u64,
    pub settings: RecordingSettings,
    pub markers: Vec<Marker>,
    pub cursor_samples: Vec<CursorSample>,
    pub click_samples: Vec<ClickSample>,
    pub synthetic_cursor: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorChanges {
    pub markers: Vec<Marker>,
    pub trim_start_ms: u64,
    pub trim_end_ms: u64,
    pub settings: RecordingSettings,
}

impl EditorChanges {
    pub fn validate(&self, duration_ms: u64) -> Result<(), String> {
        if self.trim_start_ms >= self.trim_end_ms || self.trim_end_ms > duration_ms {
            return Err("Editor trim range must stay inside the recording.".to_owned());
        }
        for marker in &self.markers {
            if marker.time_ms > duration_ms
                || !(0.0..=1.0).contains(&marker.x)
                || !(0.0..=1.0).contains(&marker.y)
                || marker
                    .target_scale
                    .is_some_and(|scale| !scale.is_finite() || !(1.2..=2.4).contains(&scale))
            {
                return Err("Editor zoom marker is outside the recording frame.".to_owned());
            }
            if marker
                .held_until_ms
                .is_some_and(|end| end < marker.time_ms || end > duration_ms)
            {
                return Err("Editor zoom hold ends outside the recording.".to_owned());
            }
        }
        self.settings.clone().validate()?;
        Ok(())
    }
}

pub fn create_session(
    source_path: &Path,
    audio_path: Option<&Path>,
    webcam_path: Option<&Path>,
    width: u32,
    height: u32,
    duration_ms: u64,
    settings: RecordingSettings,
    markers: Vec<Marker>,
    cursor_samples: Vec<CursorSample>,
    click_samples: Vec<ClickSample>,
    synthetic_cursor: bool,
) -> Result<SessionManifest, String> {
    let id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("Could not create editor session id: {error}"))?
        .as_millis()
        .to_string();
    let directory = sessions_root().join(&id);
    create_session_in_directory(
        id,
        directory,
        source_path,
        audio_path,
        webcam_path,
        width,
        height,
        duration_ms,
        settings,
        markers,
        cursor_samples,
        click_samples,
        synthetic_cursor,
    )
}

#[allow(clippy::too_many_arguments)]
fn create_session_in_directory(
    id: String,
    directory: PathBuf,
    source_path: &Path,
    audio_path: Option<&Path>,
    webcam_path: Option<&Path>,
    width: u32,
    height: u32,
    duration_ms: u64,
    settings: RecordingSettings,
    markers: Vec<Marker>,
    cursor_samples: Vec<CursorSample>,
    click_samples: Vec<ClickSample>,
    synthetic_cursor: bool,
) -> Result<SessionManifest, String> {
    fs::create_dir_all(&directory)
        .map_err(|error| format!("Could not create editor session directory: {error}"))?;

    let source_path = copy_asset(source_path, &directory, "source")?;
    let audio_path = audio_path
        .map(|path| copy_asset(path, &directory, "audio"))
        .transpose()?;
    let webcam_path = webcam_path
        .map(|path| copy_asset(path, &directory, "webcam"))
        .transpose()?;
    let working_path = directory.join("render.mp4");

    let manifest = SessionManifest {
        id,
        directory,
        source_path,
        audio_path,
        webcam_path,
        working_path,
        width,
        height,
        duration_ms,
        trim_start_ms: 0,
        trim_end_ms: duration_ms,
        settings,
        markers,
        cursor_samples,
        click_samples,
        synthetic_cursor,
    };
    save(&manifest)?;
    Ok(manifest)
}

pub fn load(id: &str) -> Result<SessionManifest, String> {
    if !valid_id(id) {
        return Err("Invalid editor session id.".to_owned());
    }
    let path = sessions_root().join(id).join("manifest.json");
    let contents =
        fs::read(&path).map_err(|error| format!("Could not read editor session: {error}"))?;
    let manifest: SessionManifest = serde_json::from_slice(&contents)
        .map_err(|error| format!("Could not parse editor session: {error}"))?;
    if manifest.id != id || manifest.directory != sessions_root().join(id) {
        return Err("Editor session path validation failed.".to_owned());
    }
    Ok(manifest)
}

pub fn save(manifest: &SessionManifest) -> Result<(), String> {
    let contents = serde_json::to_vec_pretty(manifest)
        .map_err(|error| format!("Could not serialize editor session: {error}"))?;
    let temporary = manifest.directory.join("manifest.json.tmp");
    fs::write(&temporary, contents)
        .map_err(|error| format!("Could not write editor session: {error}"))?;
    fs::rename(&temporary, manifest.directory.join("manifest.json"))
        .map_err(|error| format!("Could not finalize editor session: {error}"))
}

pub fn discard(id: &str) -> Result<(), String> {
    let manifest = load(id)?;
    fs::remove_dir_all(manifest.directory)
        .map_err(|error| format!("Could not discard editor session: {error}"))
}

fn sessions_root() -> PathBuf {
    env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("pikscreen")
        .join("sessions")
}

fn copy_asset(source: &Path, directory: &Path, stem: &str) -> Result<PathBuf, String> {
    let extension = source
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("bin");
    let destination = directory.join(format!("{stem}.{extension}"));
    fs::copy(source, &destination)
        .map_err(|error| format!("Could not preserve {stem} for the editor: {error}"))?;
    Ok(destination)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 32 && id.bytes().all(|byte| byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_marker_outside_frame() {
        let changes = EditorChanges {
            markers: vec![Marker {
                time_ms: 10,
                x: 1.1,
                y: 0.5,
                target_scale: None,
                held_until_ms: None,
                hide_cursor: false,
            }],
            trim_start_ms: 0,
            trim_end_ms: 100,
            settings: test_settings(),
        };
        assert!(changes.validate(100).is_err());
    }

    #[test]
    fn editor_changes_accept_the_frontend_marker_contract() {
        let changes: EditorChanges = serde_json::from_value(serde_json::json!({
            "markers": [{
                "timeMs": 2288,
                "x": 0.26,
                "y": 0.25,
                "targetScale": 2.4,
                "heldUntilMs": 3983,
                "hideCursor": false
            }],
            "trimStartMs": 0,
            "trimEndMs": 5000,
            "settings": test_settings()
        }))
        .expect("frontend editor payload should deserialize");
        assert_eq!(changes.markers[0].time_ms, 2_288);
        assert_eq!(changes.markers[0].held_until_ms, Some(3_983));
        changes
            .validate(5_000)
            .expect("frontend editor payload should validate");
    }

    #[test]
    fn new_editor_session_preserves_sources_without_pre_rendering() {
        let root = env::temp_dir().join(format!(
            "pikscreen-editor-session-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock should be valid")
                .as_nanos()
        ));
        let source = root.join("capture.mkv");
        fs::create_dir_all(&root).expect("fixture directory should exist");
        fs::write(&source, b"source-video").expect("source fixture should be written");

        let manifest = create_session_in_directory(
            "123".to_owned(),
            root.join("session"),
            &source,
            None,
            None,
            1_920,
            1_080,
            5_000,
            test_settings(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            false,
        )
        .expect("source-only editor session should be created");

        assert_eq!(
            fs::read(&manifest.source_path).expect("preserved source should be readable"),
            b"source-video"
        );
        assert!(
            !manifest.working_path.exists(),
            "stopping a recording must not create a rendered MP4"
        );
        assert!(manifest.directory.join("manifest.json").is_file());
        let _ = fs::remove_dir_all(root);
    }

    fn test_settings() -> RecordingSettings {
        serde_json::from_value(serde_json::json!({
            "fps": 60,
            "quality": "high",
            "background": "tahoeDark",
            "cursorVisible": true,
            "cursorSmoothing": 35,
            "cursorSize": 100,
            "zoomScale": 1.8,
            "clickEffectsEnabled": true,
            "audio": {
                "systemEnabled": false,
                "microphoneEnabled": false,
                "systemVolume": 1.0,
                "microphoneVolume": 1.0,
                "systemDevice": null,
                "microphoneDevice": null
            },
            "webcam": {
                "enabled": false,
                "deviceId": null,
                "mirror": true,
                "sizePercent": 25,
                "margin": 32,
                "roundness": 32,
                "shadow": 0.45,
                "reactToZoom": false
            }
        }))
        .expect("test settings should deserialize")
    }
}
