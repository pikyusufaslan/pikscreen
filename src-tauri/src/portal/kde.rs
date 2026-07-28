use super::*;

pub(super) const KWIN_GUIDE_EXCLUSION_PLUGIN: &str =
    "dev.pikyusufaslan.pikscreen.guide-exclusion.v1";

pub(super) const KWIN_GUIDE_EXCLUSION_SCRIPT: &str =
    include_str!("../../../tools/pikscreen-kwin-hide-guides.js");

pub(super) fn kde_session_from_environment(
    current_desktop: Option<&OsStr>,
    session_desktop: Option<&OsStr>,
) -> bool {
    [current_desktop, session_desktop]
        .into_iter()
        .flatten()
        .filter_map(OsStr::to_str)
        .flat_map(|desktop| desktop.split([':', ';']))
        .any(|desktop| {
            desktop.eq_ignore_ascii_case("kde") || desktop.eq_ignore_ascii_case("plasma")
        })
}

pub(super) fn ensure_kde_guide_capture_exclusion() -> Result<(), String> {
    if !kde_session_from_environment(
        env::var_os("XDG_CURRENT_DESKTOP").as_deref(),
        env::var_os("XDG_SESSION_DESKTOP").as_deref(),
    ) {
        return Ok(());
    }

    let script_path = env::temp_dir().join("pikscreen-kwin-guide-exclusion-v1.js");
    fs::write(&script_path, KWIN_GUIDE_EXCLUSION_SCRIPT)
        .map_err(|error| format!("Could not prepare KWin guide exclusion: {error}"))?;

    let connection = zbus::blocking::Connection::session()
        .map_err(|error| format!("Could not connect to the KDE session bus: {error}"))?;
    let scripting = zbus::blocking::Proxy::new(
        &connection,
        "org.kde.KWin",
        "/Scripting",
        "org.kde.kwin.Scripting",
    )
    .map_err(|error| format!("Could not access KWin scripting: {error}"))?;
    let loaded: bool = scripting
        .call("isScriptLoaded", &(KWIN_GUIDE_EXCLUSION_PLUGIN,))
        .map_err(|error| format!("Could not inspect KWin guide exclusion: {error}"))?;
    if !loaded {
        let script_id: i32 = scripting
            .call(
                "loadScript",
                &(
                    script_path.to_string_lossy().as_ref(),
                    KWIN_GUIDE_EXCLUSION_PLUGIN,
                ),
            )
            .map_err(|error| format!("Could not load KWin guide exclusion: {error}"))?;
        if script_id < 0 {
            return Err("KWin rejected PikScreen's guide exclusion script.".to_owned());
        }
    }
    scripting
        .call::<_, _, ()>("start", &())
        .map_err(|error| format!("Could not start KWin guide exclusion: {error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kde_desktop_names_enable_guide_capture_exclusion() {
        assert!(kde_session_from_environment(
            Some(OsStr::new("KDE")),
            Some(OsStr::new("plasma")),
        ));
        assert!(kde_session_from_environment(
            Some(OsStr::new("ubuntu:KDE")),
            None,
        ));
        assert!(!kde_session_from_environment(
            Some(OsStr::new("niri")),
            Some(OsStr::new("niri")),
        ));
    }

    #[test]
    fn kwin_script_only_excludes_the_guide_helper() {
        assert!(
            KWIN_GUIDE_EXCLUSION_SCRIPT.contains("window.resourceClass === \"pikscreen-guide\"")
        );
        assert!(KWIN_GUIDE_EXCLUSION_SCRIPT.contains("window.excludeFromCapture = true"));
        assert!(KWIN_GUIDE_EXCLUSION_SCRIPT.contains("workspace.windowAdded.connect"));
        assert!(!KWIN_GUIDE_EXCLUSION_SCRIPT.contains("pikscreen_lib"));
    }
}
