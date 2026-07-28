use serde::Serialize;
use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};
use tauri::AppHandle;
use tauri_plugin_dialog::DialogExt;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveResult {
    pub saved: bool,
    pub path: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FinalVideoInfo {
    pub path: String,
    pub duration_ms: u64,
}

pub fn save_final_video_to(
    source_path: PathBuf,
    destination: PathBuf,
) -> Result<SaveResult, String> {
    validate_temporary_final_path(&source_path)?;
    let destination = ensure_mp4_extension(destination);
    move_temporary_final(&source_path, &destination)?;
    Ok(SaveResult {
        saved: true,
        path: Some(destination.display().to_string()),
    })
}

pub async fn choose_editor_destination(app: AppHandle) -> Result<Option<PathBuf>, String> {
    let default_name = default_export_file_name();
    let dialog = app
        .dialog()
        .file()
        .set_title("Save PikScreen recording")
        .set_file_name(default_name)
        .add_filter("MP4 video", &["mp4"]);
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    dialog.save_file(move |selection| {
        let _ = sender.send(selection);
    });
    let selected = tauri::async_runtime::spawn_blocking(move || receiver.recv())
        .await
        .map_err(|error| format!("Save As picker task failed: {error}"))?
        .map_err(|error| format!("Save As picker closed unexpectedly: {error}"))?;
    let Some(destination) = selected else {
        return Ok(None);
    };
    destination
        .into_path()
        .map(Some)
        .map_err(|error| format!("Save As returned an unsupported destination: {error}"))
}

pub fn save_editor_video_to(
    source_path: &Path,
    destination: PathBuf,
) -> Result<SaveResult, String> {
    if !source_path.is_file() {
        return Err("Editor MP4 is unavailable after rendering.".to_owned());
    }
    let destination = ensure_mp4_extension(destination);
    copy_video(source_path, &destination)?;
    Ok(SaveResult {
        saved: true,
        path: Some(destination.display().to_string()),
    })
}

pub fn canceled_save_result() -> SaveResult {
    SaveResult {
        saved: false,
        path: None,
    }
}

pub fn inspect_final_video(source_path: PathBuf) -> Result<FinalVideoInfo, String> {
    validate_temporary_final_path(&source_path)?;
    final_video_info(source_path)
}

pub fn trim_final_video(
    source_path: PathBuf,
    start_ms: u64,
    end_ms: u64,
) -> Result<FinalVideoInfo, String> {
    validate_temporary_final_path(&source_path)?;
    let destination = temporary_final_path("trim");
    let info = trim_video(&source_path, &destination, start_ms, end_ms)?;
    fs::remove_file(&source_path).map_err(|error| {
        format!("Trim completed but could not remove old temporary MP4: {error}")
    })?;
    Ok(info)
}

pub fn trim_video(
    source_path: &Path,
    destination: &Path,
    start_ms: u64,
    end_ms: u64,
) -> Result<FinalVideoInfo, String> {
    let source_duration_ms = video_duration_ms(&source_path)?;
    validate_trim_range(start_ms, end_ms, source_duration_ms)?;
    let has_audio = has_audio_stream(&source_path)?;
    let start_seconds = start_ms as f64 / 1_000.0;
    let end_seconds = end_ms as f64 / 1_000.0;
    let filter = if has_audio {
        format!(
            "[0:v]trim=start={start_seconds:.6}:end={end_seconds:.6},setpts=PTS-STARTPTS[video];[0:a]atrim=start={start_seconds:.6}:end={end_seconds:.6},asetpts=PTS-STARTPTS[audio]"
        )
    } else {
        format!(
            "[0:v]trim=start={start_seconds:.6}:end={end_seconds:.6},setpts=PTS-STARTPTS[video]"
        )
    };
    let mut command = Command::new("ffmpeg");
    command.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-i",
        source_path.to_string_lossy().as_ref(),
        "-filter_complex",
        &filter,
        "-map",
        "[video]",
    ]);
    if has_audio {
        command.args(["-map", "[audio]", "-c:a", "aac", "-b:a", "192k"]);
    }
    let output = command
        .args([
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-crf",
            "18",
            "-pix_fmt",
            "yuv420p",
            "-movflags",
            "+faststart",
            destination.to_string_lossy().as_ref(),
        ])
        .output()
        .map_err(|error| format!("Could not start FFmpeg trim: {error}"))?;
    if !output.status.success() {
        let _ = fs::remove_file(destination);
        return Err(format!(
            "Could not trim final MP4: {} [{}]",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    final_video_info(destination.to_path_buf())
}

pub fn discard_final_video(source_path: PathBuf) -> Result<(), String> {
    validate_temporary_final_path(&source_path)?;
    fs::remove_file(source_path)
        .map_err(|error| format!("Could not discard temporary final MP4: {error}"))
}

fn validate_temporary_final_path(path: &Path) -> Result<(), String> {
    if !is_temporary_final_path(path) {
        return Err("PikScreen can export only its current temporary final MP4.".to_owned());
    }
    let metadata = fs::metadata(path)
        .map_err(|error| format!("Could not access temporary final MP4: {error}"))?;
    if !metadata.is_file() {
        return Err("PikScreen temporary final MP4 is not a regular file.".to_owned());
    }
    Ok(())
}

fn final_video_info(path: PathBuf) -> Result<FinalVideoInfo, String> {
    Ok(FinalVideoInfo {
        duration_ms: video_duration_ms(&path)?,
        path: path.display().to_string(),
    })
}

fn video_duration_ms(path: &Path) -> Result<u64, String> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
            path.to_string_lossy().as_ref(),
        ])
        .output()
        .map_err(|error| format!("Could not inspect final MP4 duration: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "Could not inspect final MP4 duration: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let seconds = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<f64>()
        .map_err(|error| format!("FFprobe returned an invalid final duration: {error}"))?;
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err("Final MP4 has no usable duration.".to_owned());
    }
    Ok((seconds * 1_000.0).round() as u64)
}

fn has_audio_stream(path: &Path) -> Result<bool, String> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=index",
            "-of",
            "csv=p=0",
            path.to_string_lossy().as_ref(),
        ])
        .output()
        .map_err(|error| format!("Could not inspect final MP4 audio: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "Could not inspect final MP4 audio: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(!String::from_utf8_lossy(&output.stdout).trim().is_empty())
}

fn validate_trim_range(start_ms: u64, end_ms: u64, duration_ms: u64) -> Result<(), String> {
    if end_ms <= start_ms.saturating_add(100) {
        return Err("Trim end must be at least 100 ms after trim start.".to_owned());
    }
    if end_ms > duration_ms {
        return Err("Trim end is beyond the final MP4 duration.".to_owned());
    }
    Ok(())
}

fn is_temporary_final_path(path: &Path) -> bool {
    path.parent() == Some(std::env::temp_dir().as_path())
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("pikscreen-final-") && name.ends_with(".mp4"))
}

fn ensure_mp4_extension(mut path: PathBuf) -> PathBuf {
    if path
        .extension()
        .is_none_or(|extension| !extension.eq_ignore_ascii_case("mp4"))
    {
        path.set_extension("mp4");
    }
    path
}

fn move_temporary_final(source: &Path, destination: &Path) -> Result<(), String> {
    if destination.exists() {
        return Err(format!(
            "Refusing to overwrite existing file: {}",
            destination.display()
        ));
    }
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| "Save As destination has no parent directory.".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create export directory: {error}"))?;
    let mut input = File::open(source)
        .map_err(|error| format!("Could not read temporary final MP4: {error}"))?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|error| format!("Could not create export file: {error}"))?;
    let copy_result = io::copy(&mut input, &mut output)
        .and_then(|_| output.sync_all())
        .map_err(|error| format!("Could not write export file: {error}"));
    drop(output);
    if let Err(error) = copy_result {
        let _ = fs::remove_file(destination);
        return Err(error);
    }
    fs::remove_file(source)
        .map_err(|error| format!("Export completed but could not remove temporary MP4: {error}"))
}

fn copy_video(source: &Path, destination: &Path) -> Result<(), String> {
    if destination.exists() {
        return Err(format!(
            "The destination already exists: {}",
            destination.display()
        ));
    }
    let parent = destination
        .parent()
        .ok_or_else(|| "Save destination has no parent directory.".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create export directory: {error}"))?;
    let mut input =
        File::open(source).map_err(|error| format!("Could not read editor MP4: {error}"))?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|error| format!("Could not create export MP4: {error}"))?;
    io::copy(&mut input, &mut output)
        .map_err(|error| format!("Could not save editor MP4: {error}"))?;
    output
        .sync_all()
        .map_err(|error| format!("Could not finalize export MP4: {error}"))
}

fn default_export_file_name() -> String {
    let milliseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    format!("PikScreen-{milliseconds}.mp4")
}

fn temporary_final_path(kind: &str) -> PathBuf {
    let milliseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    std::env::temp_dir().join(format!("pikscreen-final-{milliseconds}-{kind}.mp4"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_pikscreen_final_mp4s_in_the_system_temp_directory() {
        let valid = std::env::temp_dir().join("pikscreen-final-unit-test.mp4");
        assert!(is_temporary_final_path(&valid));
        assert!(!is_temporary_final_path(
            &std::env::temp_dir().join("pikscreen-source.mp4")
        ));
        assert!(!is_temporary_final_path(Path::new(
            "/tmp/pikscreen-final-unit-test.mkv"
        )));
        assert!(!is_temporary_final_path(Path::new(
            "/home/example/pikscreen-final-unit-test.mp4"
        )));
    }

    #[test]
    fn adds_the_mp4_extension_when_save_as_omits_it() {
        assert_eq!(
            ensure_mp4_extension(PathBuf::from("/tmp/PikScreen")),
            PathBuf::from("/tmp/PikScreen.mp4")
        );
        assert_eq!(
            ensure_mp4_extension(PathBuf::from("/tmp/PikScreen.MP4")),
            PathBuf::from("/tmp/PikScreen.MP4")
        );
    }

    #[test]
    fn trim_ranges_require_a_meaningful_interval_within_the_video() {
        assert!(validate_trim_range(0, 100, 1_000).is_err());
        assert!(validate_trim_range(900, 1_001, 1_000).is_err());
        assert!(validate_trim_range(100, 900, 1_000).is_ok());
    }

    #[test]
    fn generated_fixture_trims_to_a_shorter_h264_mp4() {
        let source = temporary_final_path("fixture-source");
        let fixture = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=320x180:r=30:d=2",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                source.to_string_lossy().as_ref(),
            ])
            .output()
            .expect("ffmpeg fixture should start");
        assert!(
            fixture.status.success(),
            "{}",
            String::from_utf8_lossy(&fixture.stderr)
        );
        let before = video_duration_ms(&source).expect("fixture duration should be readable");
        let trimmed =
            trim_final_video(source.clone(), 500, 1_500).expect("fixture should trim successfully");
        assert!(trimmed.duration_ms < before);
        assert!(trimmed.duration_ms >= 900 && trimmed.duration_ms <= 1_100);
        assert!(Path::new(&trimmed.path).is_file());
        assert!(!source.exists());
        let _ = fs::remove_file(trimmed.path);
    }
}
