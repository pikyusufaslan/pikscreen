use serde::{Deserialize, Serialize};
use std::{
    fs,
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};

const WEBCAM_CAPTURE_FPS: u32 = 30;
const WEBCAM_CAPTURE_WIDTH: u32 = 1_280;
const WEBCAM_CAPTURE_HEIGHT: u32 = 720;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WebcamSettings {
    #[serde(default)]
    pub(crate) enabled: bool,
    #[serde(default)]
    pub(crate) device_id: Option<String>,
    #[serde(default = "default_mirror")]
    pub(crate) mirror: bool,
    #[serde(default = "default_size_percent")]
    pub(crate) size_percent: u8,
    #[serde(default = "default_margin")]
    pub(crate) margin: u16,
    #[serde(default = "default_roundness")]
    pub(crate) roundness: u16,
    #[serde(default = "default_shadow")]
    pub(crate) shadow: f64,
    #[serde(default = "default_react_to_zoom")]
    pub(crate) react_to_zoom: bool,
}

impl Default for WebcamSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            device_id: None,
            mirror: default_mirror(),
            size_percent: default_size_percent(),
            margin: default_margin(),
            roundness: default_roundness(),
            shadow: default_shadow(),
            react_to_zoom: default_react_to_zoom(),
        }
    }
}

impl WebcamSettings {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if !(10..=100).contains(&self.size_percent) {
            return Err("Webcam size must be between 10 and 100%.".to_owned());
        }
        if self.margin > 96 {
            return Err("Webcam margin must be between 0 and 96 pixels.".to_owned());
        }
        if self.roundness > 160 {
            return Err("Webcam roundness must be between 0 and 160 pixels.".to_owned());
        }
        if !self.shadow.is_finite() || !(0.0..=1.0).contains(&self.shadow) {
            return Err("Webcam shadow must be between 0 and 100%.".to_owned());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebcamDevice {
    pub id: String,
    pub label: String,
}

pub struct WebcamRecorder {
    child: Child,
    path: PathBuf,
}

pub fn devices() -> Result<Vec<WebcamDevice>, String> {
    let root = Path::new("/sys/class/video4linux");
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut devices = fs::read_dir(root)
        .map_err(|error| format!("Could not list V4L2 cameras: {error}"))?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let entry_name = entry.file_name();
            let entry_name = entry_name.to_str()?;
            let id = v4l2_device_path(entry_name)?;
            if !id.exists() {
                return None;
            }
            let label = fs::read_to_string(entry.path().join("name"))
                .ok()
                .map(|label| label.trim().to_owned())
                .filter(|label| !label.is_empty())
                .unwrap_or_else(|| entry_name.to_owned());
            Some(WebcamDevice {
                id: id.display().to_string(),
                label,
            })
        })
        .collect::<Vec<_>>();
    devices.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(devices)
}

pub fn start(settings: &WebcamSettings, path: PathBuf) -> Result<Option<WebcamRecorder>, String> {
    settings.validate()?;
    if !settings.enabled {
        return Ok(None);
    }
    let device = selected_device(settings)?;
    let child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-thread_queue_size",
            "512",
            "-f",
            "v4l2",
            "-framerate",
            &WEBCAM_CAPTURE_FPS.to_string(),
            "-video_size",
            &format!("{WEBCAM_CAPTURE_WIDTH}x{WEBCAM_CAPTURE_HEIGHT}"),
            "-i",
            &device,
            "-an",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-crf",
            "18",
            "-pix_fmt",
            "yuv420p",
            "-g",
            &WEBCAM_CAPTURE_FPS.to_string(),
            "-f",
            "matroska",
            path.to_string_lossy().as_ref(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("Could not start webcam capture on {device}: {error}"))?;
    Ok(Some(WebcamRecorder { child, path }))
}

pub fn stop(recorder: WebcamRecorder) -> Result<PathBuf, String> {
    let WebcamRecorder { child, path } = recorder;
    if unsafe { libc::kill(child.id() as i32, libc::SIGINT) } != 0
        && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    {
        return Err(format!(
            "Could not stop webcam capture: {}",
            std::io::Error::last_os_error()
        ));
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("Could not finalize webcam capture: {error}"))?;
    if !output.status.success() && output.status.signal() != Some(libc::SIGINT) {
        return Err(format!(
            "Webcam recorder exited unsuccessfully: {} [{}]",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    ensure_usable_sidecar(&path)?;
    Ok(path)
}

fn selected_device(settings: &WebcamSettings) -> Result<String, String> {
    if let Some(device) = settings.device_id.as_deref() {
        let path = Path::new(device);
        if is_v4l2_path(path) && path.exists() {
            return Ok(device.to_owned());
        }
    }
    devices()?
        .into_iter()
        .next()
        .map(|device| device.id)
        .ok_or_else(|| "No V4L2 webcam was found. Connect a camera and choose it again.".to_owned())
}

fn ensure_usable_sidecar(path: &Path) -> Result<(), String> {
    let metadata =
        fs::metadata(path).map_err(|error| format!("Could not inspect webcam sidecar: {error}"))?;
    if metadata.len() < 4_096 {
        return Err("Webcam capture did not produce usable video frames.".to_owned());
    }
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-read_intervals",
            "%+#1",
            "-show_entries",
            "frame=best_effort_timestamp_time",
            "-of",
            "csv=p=0",
            path.to_string_lossy().as_ref(),
        ])
        .output()
        .map_err(|error| format!("Could not inspect webcam frames: {error}"))?;
    if output.status.success() && !String::from_utf8_lossy(&output.stdout).trim().is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Webcam capture did not produce readable video frames: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn v4l2_device_path(entry_name: &str) -> Option<PathBuf> {
    entry_name
        .strip_prefix("video")
        .filter(|suffix| {
            !suffix.is_empty() && suffix.chars().all(|character| character.is_ascii_digit())
        })
        .map(|_| PathBuf::from("/dev").join(entry_name))
}

fn is_v4l2_path(path: &Path) -> bool {
    path.parent() == Some(Path::new("/dev"))
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(v4l2_device_path)
            .is_some()
}

fn default_mirror() -> bool {
    true
}

fn default_size_percent() -> u8 {
    40
}

fn default_margin() -> u16 {
    24
}

fn default_roundness() -> u16 {
    90
}

fn default_shadow() -> f64 {
    0.67
}

fn default_react_to_zoom() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_numeric_video_entries_become_v4l2_paths() {
        assert_eq!(
            v4l2_device_path("video0"),
            Some(PathBuf::from("/dev/video0"))
        );
        assert_eq!(
            v4l2_device_path("video12"),
            Some(PathBuf::from("/dev/video12"))
        );
        assert_eq!(v4l2_device_path("video"), None);
        assert_eq!(v4l2_device_path("videoUSB"), None);
        assert_eq!(v4l2_device_path("not-a-camera"), None);
    }

    #[test]
    fn recordly_defaults_are_valid() {
        let settings = WebcamSettings::default();
        assert!(settings.mirror);
        assert_eq!(settings.size_percent, 40);
        assert_eq!(settings.margin, 24);
        assert_eq!(settings.roundness, 90);
        assert_eq!(settings.shadow, 0.67);
        assert!(settings.react_to_zoom);
        assert!(settings.validate().is_ok());
    }
}
