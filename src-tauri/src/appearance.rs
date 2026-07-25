use serde::Serialize;
use std::{
    env, fs,
    path::{Path, PathBuf},
};
use tauri::AppHandle;
use tauri_plugin_dialog::DialogExt;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppearanceAsset {
    pub path: String,
    pub label: String,
}

pub fn choose(app: &AppHandle, kind: &str) -> Result<Option<AppearanceAsset>, String> {
    let (title, slot, filters) = match kind {
        "cursor" => (
            "Choose custom cursor",
            "custom-cursor",
            vec![("Cursor image", vec!["png", "jpg", "jpeg", "webp"])],
        ),
        "background" => (
            "Choose custom background",
            "custom-background",
            vec![(
                "Background image",
                vec!["png", "jpg", "jpeg", "webp", "avif"],
            )],
        ),
        _ => return Err("Unknown appearance asset kind.".to_owned()),
    };
    let mut dialog = app.dialog().file().set_title(title);
    for (name, extensions) in filters {
        dialog = dialog.add_filter(name, &extensions);
    }
    let Some(selection) = dialog.blocking_pick_file() else {
        return Ok(None);
    };
    let source = selection
        .into_path()
        .map_err(|error| format!("Selected appearance asset has no local path: {error}"))?;
    validate_kind(kind, &source)?;
    let extension = source
        .extension()
        .and_then(|extension| extension.to_str())
        .ok_or_else(|| "Selected appearance asset has no usable file extension.".to_owned())?
        .to_ascii_lowercase();
    let directory = config_assets_dir();
    fs::create_dir_all(&directory)
        .map_err(|error| format!("Could not create PikScreen appearance assets: {error}"))?;
    let destination = directory.join(format!("{slot}.{extension}"));
    if source != destination {
        fs::copy(&source, &destination)
            .map_err(|error| format!("Could not import selected appearance asset: {error}"))?;
    }
    Ok(Some(AppearanceAsset {
        path: destination.display().to_string(),
        label: source
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Custom asset")
            .to_owned(),
    }))
}

pub fn validate_cursor(path: &Path) -> Result<(), String> {
    validate_kind("cursor", path)
}

pub fn validate_background(path: &Path) -> Result<(), String> {
    validate_kind("background", path)
}

fn validate_kind(kind: &str, path: &Path) -> Result<(), String> {
    let allowed = match kind {
        "cursor" => ["png", "jpg", "jpeg", "webp"].as_slice(),
        "background" => ["png", "jpg", "jpeg", "webp", "avif"].as_slice(),
        _ => return Err("Unknown appearance asset kind.".to_owned()),
    };
    let metadata = fs::metadata(path)
        .map_err(|error| format!("Could not access selected appearance asset: {error}"))?;
    if !metadata.is_file() {
        return Err("Selected appearance asset is not a regular file.".to_owned());
    }
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| "Selected appearance asset has no supported file extension.".to_owned())?;
    if !allowed.iter().any(|allowed| *allowed == extension) {
        return Err(format!(
            "Unsupported custom {kind} format. Choose {}.",
            allowed.join(", ")
        ));
    }
    Ok(())
}

fn config_assets_dir() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("pikscreen")
        .join("assets")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_an_unsupported_cursor_extension() {
        let path = std::env::temp_dir().join("pikscreen-custom-cursor.txt");
        fs::write(&path, "not a cursor").expect("fixture should be writable");
        assert!(validate_cursor(&path).is_err());
        let _ = fs::remove_file(path);
    }
}
