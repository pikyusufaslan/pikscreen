use std::{
    env,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::Duration,
};

const EXTENSION_UUID: &str = "pikscreen-guide@pikyusufaslan.dev";
const BUS_NAME: &str = "dev.pikyusufaslan.PikScreen.Guides";
const OBJECT_PATH: &str = "/dev/pikyusufaslan/PikScreen/Guides";
const INTERFACE: &str = "dev.pikyusufaslan.PikScreen.Guides";
const EXTENSION_METADATA: &str =
    include_str!("../../tools/pikscreen-gnome-guide-extension/metadata.json");
const EXTENSION_JS: &str = include_str!("../../tools/pikscreen-gnome-guide-extension/extension.js");
const EXTENSION_STYLESHEET: &str =
    include_str!("../../tools/pikscreen-gnome-guide-extension/stylesheet.css");

pub fn session_from_environment(
    current_desktop: Option<&OsStr>,
    session_desktop: Option<&OsStr>,
) -> bool {
    [current_desktop, session_desktop]
        .into_iter()
        .flatten()
        .filter_map(OsStr::to_str)
        .flat_map(|desktop| desktop.split([':', ';']))
        .any(|desktop| desktop.eq_ignore_ascii_case("gnome"))
}

pub fn session_available() -> bool {
    session_from_environment(
        env::var_os("XDG_CURRENT_DESKTOP").as_deref(),
        env::var_os("XDG_SESSION_DESKTOP").as_deref(),
    )
}

fn extension_dir_from_environment(
    data_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Option<PathBuf> {
    let data_home = data_home
        .map(PathBuf::from)
        .or_else(|| home.map(|home| PathBuf::from(home).join(".local/share")))?;
    Some(
        data_home
            .join("gnome-shell")
            .join("extensions")
            .join(EXTENSION_UUID),
    )
}

fn extension_dir() -> Result<PathBuf, String> {
    extension_dir_from_environment(
        env::var_os("XDG_DATA_HOME").as_deref(),
        env::var_os("HOME").as_deref(),
    )
    .ok_or_else(|| "Could not determine the GNOME extension directory.".to_owned())
}

fn write_if_changed(path: &Path, contents: &str) -> Result<(), String> {
    if fs::read_to_string(path).ok().as_deref() == Some(contents) {
        return Ok(());
    }
    fs::write(path, contents).map_err(|error| {
        format!(
            "Could not install GNOME guide asset {}: {error}",
            path.display()
        )
    })
}

fn install_extension() -> Result<(), String> {
    let directory = extension_dir()?;
    fs::create_dir_all(&directory).map_err(|error| {
        format!(
            "Could not create the GNOME guide extension directory {}: {error}",
            directory.display()
        )
    })?;
    write_if_changed(&directory.join("metadata.json"), EXTENSION_METADATA)?;
    write_if_changed(&directory.join("extension.js"), EXTENSION_JS)?;
    write_if_changed(&directory.join("stylesheet.css"), EXTENSION_STYLESHEET)?;
    Ok(())
}

fn connection() -> Result<zbus::blocking::Connection, String> {
    zbus::blocking::Connection::session()
        .map_err(|error| format!("Could not connect to GNOME Shell's session bus: {error}"))
}

pub fn ensure() -> Result<(), String> {
    if !session_available() {
        return Ok(());
    }
    install_extension()?;
    let status = Command::new("/usr/bin/gnome-extensions")
        .args(["enable", EXTENSION_UUID])
        .status()
        .map_err(|error| format!("Could not enable PikScreen's GNOME guide extension: {error}"))?;
    if !status.success() {
        return Err(format!(
            "GNOME rejected PikScreen's guide extension: {status}"
        ));
    }
    Ok(())
}

pub fn pointer() -> Result<(i32, i32), String> {
    let connection = connection()?;
    let proxy = zbus::blocking::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE)
        .map_err(|error| format!("Could not access PikScreen's GNOME guide service: {error}"))?;
    proxy
        .call::<_, _, (i32, i32)>("GetPointer", &())
        .map_err(|error| format!("Could not read GNOME Shell's pointer position: {error}"))
}

pub fn show_countdown(x: i32, y: i32, duration_ms: u32) -> Result<(), String> {
    let connection = connection()?;
    let proxy = zbus::blocking::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE)
        .map_err(|error| format!("Could not access PikScreen's GNOME guide service: {error}"))?;
    proxy
        .call::<_, _, ()>("ShowCountdown", &(x, y, duration_ms))
        .map_err(|error| format!("Could not show GNOME's recording countdown: {error}"))?;
    thread::sleep(Duration::from_millis(u64::from(duration_ms)));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gnome_desktop_names_enable_the_shell_guide_backend() {
        assert!(session_from_environment(
            Some(OsStr::new("GNOME")),
            Some(OsStr::new("gnome")),
        ));
        assert!(session_from_environment(
            Some(OsStr::new("ubuntu:GNOME")),
            None,
        ));
        assert!(!session_from_environment(
            Some(OsStr::new("KDE")),
            Some(OsStr::new("plasma")),
        ));
    }

    #[test]
    fn extension_directory_uses_xdg_data_home_before_home_fallback() {
        assert_eq!(
            extension_dir_from_environment(
                Some(OsStr::new("/tmp/data")),
                Some(OsStr::new("/tmp/home"))
            ),
            Some(PathBuf::from(
                "/tmp/data/gnome-shell/extensions/pikscreen-guide@pikyusufaslan.dev"
            )),
        );
        assert_eq!(
            extension_dir_from_environment(None, Some(OsStr::new("/tmp/home"))),
            Some(PathBuf::from(
                "/tmp/home/.local/share/gnome-shell/extensions/pikscreen-guide@pikyusufaslan.dev"
            )),
        );
    }
}
