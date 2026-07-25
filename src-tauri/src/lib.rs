mod appearance;
mod cursor;
mod editor;
mod export;
mod gnome_guides;
mod hyprland_cursor;
mod hyprland_input;
mod portal;
mod portal_cursor;
mod review_server;
mod settings;
mod webcam;
mod window_pipewire;
pub mod zoom;

use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    webview::PageLoadEvent,
    Emitter, LogicalSize, Manager, PhysicalPosition, WebviewUrl, WebviewWindowBuilder,
};

#[cfg(target_os = "linux")]
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};

#[derive(Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct HudInputRegion {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

#[cfg(target_os = "linux")]
static HUD_INPUT_REGION: Mutex<Option<HudInputRegion>> = Mutex::new(None);

#[cfg(target_os = "linux")]
static HUD_INPUT_HOOKS_INSTALLED: AtomicBool = AtomicBool::new(false);

#[cfg(target_os = "linux")]
fn current_hud_input_region() -> Option<HudInputRegion> {
    HUD_INPUT_REGION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .to_owned()
}

#[cfg(target_os = "linux")]
fn store_hud_input_region(region: Option<HudInputRegion>) {
    *HUD_INPUT_REGION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = region;
}

#[cfg(target_os = "linux")]
fn apply_hud_input_region_to_gtk_window(
    gtk_window: &gtk::ApplicationWindow,
    region: HudInputRegion,
) {
    use gtk::prelude::*;

    let Some(gdk_window) = gtk_window.window() else {
        return;
    };
    let rectangle = gtk::cairo::RectangleInt::new(
        region.x.max(0),
        region.y.max(0),
        region.width.max(1),
        region.height.max(1),
    );
    let native_region = gtk::cairo::Region::create_rectangle(&rectangle);
    gtk_window.input_shape_combine_region(Some(&native_region));
    gdk_window.input_shape_combine_region(&native_region, 0, 0);
}

#[cfg(target_os = "linux")]
fn reapply_stored_hud_input_region(gtk_window: &gtk::ApplicationWindow) {
    if let Some(region) = current_hud_input_region() {
        apply_hud_input_region_to_gtk_window(gtk_window, region);
    }
}

#[cfg(target_os = "linux")]
fn install_hud_input_region_hooks(gtk_window: &gtk::ApplicationWindow) {
    use gtk::prelude::*;

    if HUD_INPUT_HOOKS_INSTALLED.swap(true, Ordering::AcqRel) {
        return;
    }

    gtk_window.connect_map(reapply_stored_hud_input_region);
}

#[cfg(target_os = "linux")]
fn apply_native_hud_input_region(
    window: &tauri::WebviewWindow,
    region: HudInputRegion,
) -> Result<(), String> {
    store_hud_input_region(Some(region));
    let window_for_main = window.clone();
    window
        .run_on_main_thread(move || {
            let Ok(gtk_window) = window_for_main.gtk_window() else {
                eprintln!("PikScreen could not access the GTK HUD window.");
                return;
            };
            install_hud_input_region_hooks(&gtk_window);
            apply_hud_input_region_to_gtk_window(&gtk_window, region);
        })
        .map_err(|error| format!("Could not schedule the native HUD input region: {error}"))
}

#[cfg(target_os = "linux")]
fn clear_native_hud_input_region(window: &tauri::WebviewWindow) -> Result<(), String> {
    store_hud_input_region(None);
    let window_for_main = window.clone();
    window
        .run_on_main_thread(move || {
            use gtk::prelude::*;

            let Ok(gtk_window) = window_for_main.gtk_window() else {
                eprintln!("PikScreen could not access the GTK window.");
                return;
            };
            install_hud_input_region_hooks(&gtk_window);
            gtk_window.input_shape_combine_region(None);
        })
        .map_err(|error| format!("Could not clear the native HUD input region: {error}"))
}

#[cfg(target_os = "linux")]
fn configure_native_hud_gtk_window(gtk_window: &gtk::ApplicationWindow, width: i32, height: i32) {
    use gtk::prelude::*;

    if !gtk_window.is_realized() {
        gtk_window.set_titlebar(Option::<&gtk::Widget>::None);
    }
    gtk_window.set_resizable(true);
    gtk_window.set_default_size(width, height);
    gtk_window.resize(width, height);
}

#[cfg(target_os = "linux")]
fn configure_native_hud_window(
    window: &tauri::WebviewWindow,
    width: i32,
    height: i32,
) -> Result<(), String> {
    let window_for_main = window.clone();
    window
        .run_on_main_thread(move || {
            let Ok(gtk_window) = window_for_main.gtk_window() else {
                eprintln!("PikScreen could not access the native GTK HUD window.");
                return;
            };
            configure_native_hud_gtk_window(&gtk_window, width, height);
        })
        .map_err(|error| format!("Could not configure the native PikScreen HUD window: {error}"))
}

#[cfg(not(target_os = "linux"))]
fn configure_native_hud_window(
    _window: &tauri::WebviewWindow,
    _width: i32,
    _height: i32,
) -> Result<(), String> {
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn clear_native_hud_input_region(_window: &tauri::WebviewWindow) -> Result<(), String> {
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn apply_native_hud_input_region(
    _window: &tauri::WebviewWindow,
    _region: HudInputRegion,
) -> Result<(), String> {
    Ok(())
}

fn ensure_main_window_interactive(
    window: &tauri::WebviewWindow,
    region: Option<HudInputRegion>,
) -> Result<(), String> {
    window
        .set_enabled(true)
        .map_err(|error| format!("Could not enable the PikScreen window: {error}"))?;
    #[cfg(not(target_os = "linux"))]
    window
        .set_ignore_cursor_events(false)
        .map_err(|error| format!("Could not restore PikScreen window input: {error}"))?;
    if let Some(region) = region {
        apply_native_hud_input_region(window, region)?;
    }
    Ok(())
}

fn schedule_main_window_input_recovery(app: tauri::AppHandle, region: Option<HudInputRegion>) {
    std::thread::spawn(move || {
        for delay_ms in [50, 200, 500] {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            let Some(window) = app.get_webview_window("main") else {
                return;
            };
            if let Err(error) = ensure_main_window_interactive(&window, region) {
                eprintln!("PikScreen HUD input recovery failed: {error}");
            }
        }
    });
}

#[tauri::command]
fn set_window_surface(app: tauri::AppHandle, surface: String) -> Result<(), String> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "PikScreen window is unavailable.".to_owned())?;

    let (width, height, dock_to_bottom) = match surface.as_str() {
        "hud" => (704.0, 80.0, true),
        "review" => (980.0, 860.0, false),
        _ => return Err(format!("Unsupported PikScreen window surface: {surface}")),
    };

    if surface != "hud" {
        clear_native_hud_input_region(&window)?;
    }

    let expand_surface = surface == "review";
    if surface == "hud" {
        configure_native_hud_window(&window, width as i32, height as i32)?;
    }
    if expand_surface {
        window
            .set_min_size(Option::<LogicalSize<f64>>::None)
            .map_err(|error| format!("Could not clear PikScreen's minimum size: {error}"))?;
        window
            .set_max_size(Option::<LogicalSize<f64>>::None)
            .map_err(|error| format!("Could not clear PikScreen's maximum size: {error}"))?;
    } else {
        window
            .set_min_size(Some(LogicalSize::new(width, height)))
            .map_err(|error| format!("Could not set PikScreen's minimum size: {error}"))?;
        window
            .set_max_size(Some(LogicalSize::new(width, height)))
            .map_err(|error| format!("Could not set PikScreen's maximum size: {error}"))?;
    }
    window
        .set_size(LogicalSize::new(width, height))
        .map_err(|error| format!("Could not resize PikScreen: {error}"))?;
    let native_resizable = expand_surface || cfg!(target_os = "linux");
    window
        .set_resizable(native_resizable)
        .map_err(|error| format!("Could not configure the PikScreen window surface: {error}"))?;
    window
        .set_always_on_top(dock_to_bottom)
        .map_err(|error| format!("Could not update PikScreen window priority: {error}"))?;
    window
        .set_skip_taskbar(!expand_surface)
        .map_err(|error| format!("Could not update PikScreen taskbar visibility: {error}"))?;

    if dock_to_bottom {
        let monitor = window
            .current_monitor()
            .map_err(|error| format!("Could not locate PikScreen's monitor: {error}"))?
            .or_else(|| window.primary_monitor().ok().flatten())
            .ok_or_else(|| "Could not locate a monitor for the PikScreen HUD.".to_owned())?;
        let scale_factor = window
            .scale_factor()
            .map_err(|error| format!("Could not measure PikScreen's scale factor: {error}"))?;
        let target_width = (width * scale_factor).round().max(1.0) as u32;
        let target_height = (height * scale_factor).round().max(1.0) as u32;
        let work_area = monitor.work_area();
        let x =
            work_area.position.x + (work_area.size.width.saturating_sub(target_width) / 2) as i32;
        let y =
            work_area.position.y + work_area.size.height.saturating_sub(target_height + 24) as i32;
        window
            .set_position(PhysicalPosition::new(x, y))
            .map_err(|error| format!("Could not place PikScreen HUD: {error}"))?;
    } else {
        window
            .center()
            .map_err(|error| format!("Could not center PikScreen: {error}"))?;
    }

    ensure_main_window_interactive(&window, None)?;
    schedule_main_window_input_recovery(app, None);
    Ok(())
}

#[tauri::command]
fn hud_ready(app: tauri::AppHandle, region: HudInputRegion) -> Result<(), String> {
    let hud = app
        .get_webview_window("main")
        .ok_or_else(|| "PikScreen HUD is unavailable.".to_owned())?;
    ensure_main_window_interactive(&hud, Some(region))?;
    schedule_main_window_input_recovery(app, Some(region));
    Ok(())
}

fn restore_hud(app: &tauri::AppHandle) -> Result<(), String> {
    set_window_surface(app.clone(), "hud".to_owned())?;
    let hud = app
        .get_webview_window("main")
        .ok_or_else(|| "PikScreen HUD is unavailable.".to_owned())?;
    hud.show()
        .map_err(|error| format!("Could not restore the PikScreen HUD: {error}"))?;
    ensure_main_window_interactive(&hud, None)?;
    schedule_main_window_input_recovery(app.clone(), None);
    hud.set_focus()
        .map_err(|error| format!("Could not focus the PikScreen HUD: {error}"))
}

fn editor_route(session_id: &str) -> Result<String, String> {
    if session_id.is_empty()
        || !session_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err("The editor session id is invalid.".to_owned());
    }
    Ok(format!("index.html?editor={session_id}"))
}

#[tauri::command]
fn open_editor_window(app: tauri::AppHandle, session_id: String) -> Result<(), String> {
    let route = editor_route(&session_id)?;
    let hud = app
        .get_webview_window("main")
        .ok_or_else(|| "PikScreen HUD is unavailable.".to_owned())?;

    if let Some(editor) = app.get_webview_window("editor") {
        editor
            .hide()
            .map_err(|error| format!("Could not prepare the PikScreen Editor: {error}"))?;
        editor
            .eval(format!("window.location.replace('{route}')"))
            .map_err(|error| format!("Could not load the PikScreen Editor session: {error}"))?;
        editor
            .set_title("PikScreen Editor")
            .map_err(|error| format!("Could not title the PikScreen Editor: {error}"))?;
        editor
            .set_decorations(true)
            .map_err(|error| format!("Could not decorate the PikScreen Editor: {error}"))?;
        editor
            .set_resizable(true)
            .map_err(|error| format!("Could not make the PikScreen Editor resizable: {error}"))?;
        editor
            .set_always_on_top(false)
            .map_err(|error| format!("Could not restore normal editor stacking: {error}"))?;
        editor.set_skip_taskbar(false).map_err(|error| {
            format!("Could not show the PikScreen Editor in the taskbar: {error}")
        })?;
        editor
            .set_min_size(Some(LogicalSize::new(960.0, 640.0)))
            .map_err(|error| format!("Could not set the PikScreen Editor minimum size: {error}"))?;
        editor
            .set_size(LogicalSize::new(1280.0, 820.0))
            .map_err(|error| format!("Could not size the PikScreen Editor: {error}"))?;
        hud.hide()
            .map_err(|error| format!("Could not hide the PikScreen HUD: {error}"))?;
        editor
            .unminimize()
            .map_err(|error| format!("Could not restore the PikScreen Editor: {error}"))?;
        return Ok(());
    }

    let editor = WebviewWindowBuilder::new(&app, "editor", WebviewUrl::App(route.into()))
        .title("PikScreen Editor")
        .inner_size(1280.0, 820.0)
        .min_inner_size(960.0, 640.0)
        .resizable(true)
        .decorations(true)
        .transparent(false)
        .always_on_top(false)
        .skip_taskbar(false)
        .visible(false)
        .on_page_load(|window, payload| {
            if matches!(payload.event(), PageLoadEvent::Finished) {
                let _ = window.show();
                let _ = window.set_focus();
            }
        })
        .build()
        .map_err(|error| format!("Could not open the PikScreen Editor: {error}"))?;

    let close_app = app.clone();
    editor.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            if let Some(editor) = close_app.get_webview_window("editor") {
                let _ = editor.hide();
            }
            let _ = restore_hud(&close_app);
        }
    });

    hud.hide()
        .map_err(|error| format!("Could not hide the PikScreen HUD: {error}"))
}

#[tauri::command]
fn return_to_hud(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(editor) = app.get_webview_window("editor") {
        editor
            .hide()
            .map_err(|error| format!("Could not hide the PikScreen Editor: {error}"))?;
    }
    restore_hud(&app)
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ReviewVideoInfo {
    path: String,
    duration_ms: u64,
    review_url: String,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct EditorSessionInfo {
    id: String,
    width: u32,
    height: u32,
    duration_ms: u64,
    trim_start_ms: u64,
    trim_end_ms: u64,
    markers: Vec<zoom::Marker>,
    cursor_samples: Vec<cursor::CursorSample>,
    click_samples: Vec<portal::ClickSample>,
    synthetic_cursor: bool,
    settings: portal::RecordingSettings,
    has_webcam: bool,
    video_url: String,
    audio_url: Option<String>,
    webcam_url: Option<String>,
    custom_background_url: Option<String>,
    custom_cursor_url: Option<String>,
}

fn editor_session_info(session: editor::SessionManifest) -> Result<EditorSessionInfo, String> {
    let video_url = review_server::publish(&session.source_path)?;
    let audio_url = session
        .audio_path
        .as_deref()
        .map(review_server::publish)
        .transpose()?;
    let webcam_url = session
        .webcam_path
        .as_deref()
        .map(review_server::publish)
        .transpose()?;
    let custom_background_url = session
        .settings
        .custom_background_path
        .as_deref()
        .map(std::path::Path::new)
        .map(review_server::publish)
        .transpose()?;
    let custom_cursor_url = session
        .settings
        .custom_cursor_path
        .as_deref()
        .map(std::path::Path::new)
        .map(review_server::publish)
        .transpose()?;
    Ok(EditorSessionInfo {
        id: session.id,
        width: session.width,
        height: session.height,
        duration_ms: session.duration_ms,
        trim_start_ms: session.trim_start_ms,
        trim_end_ms: session.trim_end_ms,
        markers: session.markers,
        cursor_samples: session.cursor_samples,
        click_samples: session.click_samples,
        synthetic_cursor: session.synthetic_cursor,
        settings: session.settings,
        has_webcam: session.webcam_path.is_some(),
        video_url,
        audio_url,
        webcam_url,
        custom_background_url,
        custom_cursor_url,
    })
}

fn review_video_info(
    source_path: std::path::PathBuf,
    info: export::FinalVideoInfo,
) -> Result<ReviewVideoInfo, String> {
    Ok(ReviewVideoInfo {
        path: info.path,
        duration_ms: info.duration_ms,
        review_url: review_server::publish(&source_path)?,
    })
}

#[tauri::command]
async fn probe_portals() -> portal::PortalCapabilities {
    portal::probe().await
}

#[tauri::command]
fn audio_devices() -> Result<portal::AudioDevices, String> {
    portal::audio_devices()
}

#[tauri::command]
fn webcam_devices() -> Result<Vec<webcam::WebcamDevice>, String> {
    webcam::devices()
}

#[tauri::command]
fn choose_appearance_asset(
    app: tauri::AppHandle,
    kind: String,
) -> Result<Option<appearance::AppearanceAsset>, String> {
    appearance::choose(&app, &kind)
}

#[tauri::command]
fn load_settings() -> settings::SavedSettings {
    settings::load()
}

#[tauri::command]
fn save_settings(app: tauri::AppHandle, settings: settings::SavedSettings) -> Result<(), String> {
    settings::save(settings.clone())?;
    app.emit("settings-saved", settings).map_err(|error| {
        format!("Could not notify PikScreen windows about saved settings: {error}")
    })
}

#[tauri::command]
fn open_settings_window(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("settings") {
        window
            .hide()
            .map_err(|error| format!("Could not prepare PikScreen Settings: {error}"))?;
        window
            .eval("window.location.replace('index.html?surface=settings')")
            .map_err(|error| format!("Could not refresh PikScreen Settings: {error}"))?;
        window
            .set_size(LogicalSize::new(920.0, 720.0))
            .map_err(|error| format!("Could not resize PikScreen Settings: {error}"))?;
        window
            .set_always_on_top(true)
            .map_err(|error| format!("Could not raise PikScreen Settings: {error}"))?;
        return Ok(());
    }

    WebviewWindowBuilder::new(
        &app,
        "settings",
        WebviewUrl::App("index.html?surface=settings".into()),
    )
    .title("PikScreen Settings")
    .inner_size(920.0, 720.0)
    .min_inner_size(760.0, 560.0)
    .resizable(true)
    .decorations(true)
    .transparent(false)
    .always_on_top(true)
    .visible(false)
    .on_page_load(|window, payload| {
        if matches!(payload.event(), PageLoadEvent::Finished) {
            let _ = window.show();
            let _ = window.set_focus();
        }
    })
    .build()
    .map_err(|error| format!("Could not open PikScreen Settings: {error}"))?;

    Ok(())
}

#[tauri::command]
fn load_editor_session(session_id: String) -> Result<EditorSessionInfo, String> {
    editor_session_info(editor::load(&session_id)?)
}

#[tauri::command]
async fn save_editor_session(
    app: tauri::AppHandle,
    session_id: String,
    changes: editor::EditorChanges,
) -> Result<export::SaveResult, String> {
    let Some(destination) = export::choose_editor_destination(&app)? else {
        return Ok(export::canceled_save_result());
    };
    tauri::async_runtime::spawn_blocking(move || {
        let mut session = editor::load(&session_id)?;
        portal::render_editor_session(&app, &mut session, changes)
            .map_err(|error| format!("Could not render the final MP4: {error}"))?;
        export::save_editor_video_to(&session.working_path, destination)
            .map_err(|error| format!("The MP4 rendered but could not be saved: {error}"))
    })
    .await
    .map_err(|error| format!("Final export task failed: {error}"))?
}

#[tauri::command]
fn discard_editor_session(session_id: String) -> Result<(), String> {
    editor::discard(&session_id)
}

#[tauri::command]
fn publish_appearance_asset(kind: String, path: String) -> Result<String, String> {
    let path = std::path::PathBuf::from(path);
    match kind.as_str() {
        "background" => appearance::validate_background(&path)?,
        "cursor" => appearance::validate_cursor(&path)?,
        _ => return Err("Unknown appearance asset kind.".to_owned()),
    }
    review_server::publish(&path)
}

#[tauri::command]
async fn select_monitor(
    app: tauri::AppHandle,
    state: tauri::State<'_, portal::CaptureState>,
    click: portal::StartClick,
    preview_settings: portal::PreviewSettings,
) -> Result<portal::PreviewInfo, String> {
    portal::select_monitor(app, &state, click, preview_settings).await
}

#[tauri::command]
fn update_preview_crop(
    state: tauri::State<'_, portal::CaptureState>,
    preview_settings: portal::PreviewSettings,
) -> Result<portal::PreviewInfo, String> {
    portal::update_preview_crop(&state, preview_settings)
}

#[tauri::command]
fn cancel_preview(state: tauri::State<'_, portal::CaptureState>) -> Result<(), String> {
    portal::cancel_preview(&state)
}

#[tauri::command]
async fn start_recording(
    app: tauri::AppHandle,
    state: tauri::State<'_, portal::CaptureState>,
    settings: portal::RecordingSettings,
) -> Result<portal::CaptureSelection, String> {
    let hud = app
        .get_webview_window("main")
        .ok_or_else(|| "PikScreen HUD is unavailable.".to_owned())?;
    hud.show()
        .map_err(|error| format!("Could not keep the PikScreen HUD visible: {error}"))?;

    let state = state.inner().clone();
    let recording_app = app.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        portal::start_recording(recording_app, &state, settings)
    })
    .await
    .map_err(|error| format!("Recording startup task failed: {error}"))?;

    if result.is_err() {
        let _ = restore_hud(&app);
    }
    result
}

#[tauri::command]
async fn stop_recording(
    app: tauri::AppHandle,
    state: tauri::State<'_, portal::CaptureState>,
) -> Result<portal::CompletedRecording, String> {
    let state = state.inner().clone();
    let stop_app = app.clone();
    let result = tauri::async_runtime::spawn_blocking(move || portal::stop(&stop_app, &state))
        .await
        .map_err(|error| format!("Video processing task failed: {error}"))?;

    if result.is_err() {
        let _ = restore_hud(&app);
    }
    result
}

#[tauri::command]
async fn save_final_video(
    app: tauri::AppHandle,
    source_path: String,
) -> Result<export::SaveResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        export::save_final_video(&app, std::path::PathBuf::from(source_path))
    })
    .await
    .map_err(|error| format!("Save As task failed: {error}"))?
}

#[tauri::command]
fn inspect_final_video(source_path: String) -> Result<ReviewVideoInfo, String> {
    let source_path = std::path::PathBuf::from(source_path);
    let info = export::inspect_final_video(source_path.clone())?;
    review_video_info(source_path, info)
}

#[tauri::command]
async fn trim_final_video(
    source_path: String,
    start_ms: u64,
    end_ms: u64,
) -> Result<ReviewVideoInfo, String> {
    let info = tauri::async_runtime::spawn_blocking(move || {
        export::trim_final_video(std::path::PathBuf::from(source_path), start_ms, end_ms)
    })
    .await
    .map_err(|error| format!("Trim task failed: {error}"))??;
    review_video_info(std::path::PathBuf::from(&info.path), info)
}

#[tauri::command]
fn discard_final_video(source_path: String) -> Result<(), String> {
    export::discard_final_video(std::path::PathBuf::from(source_path))
}

fn install_tray(app: &tauri::App) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show-pikscreen", "Show PikScreen", true, None::<&str>)?;
    let stop = MenuItem::with_id(
        app,
        "stop-recording",
        "Stop recording",
        true,
        Some("Ctrl+Shift+R"),
    )?;
    let quit = MenuItem::with_id(app, "quit-pikscreen", "Quit PikScreen", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &stop, &quit])?;

    let mut tray = TrayIconBuilder::with_id("pikscreen")
        .menu(&menu)
        .tooltip("PikScreen")
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show-pikscreen" => {
                let _ = restore_hud(app);
            }
            "stop-recording" => {
                let _ = app.emit("stop-recording-requested", ());
            }
            "quit-pikscreen" => app.exit(0),
            _ => {}
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .manage(portal::CaptureState::new())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .on_page_load(|webview, payload| {
            if webview.label() != "main" || !matches!(payload.event(), PageLoadEvent::Finished) {
                return;
            }
            if let Some(hud) = webview.app_handle().get_webview_window("main") {
                if let Err(error) = ensure_main_window_interactive(&hud, None) {
                    eprintln!("PikScreen HUD page-load input recovery failed: {error}");
                }
                schedule_main_window_input_recovery(webview.app_handle().clone(), None);
            }
        })
        .setup(|app| {
            let hud = app
                .get_webview_window("main")
                .ok_or_else(|| std::io::Error::other("PikScreen HUD is unavailable."))?;
            #[cfg(target_os = "linux")]
            {
                let gtk_window = hud
                    .gtk_window()
                    .map_err(|error| std::io::Error::other(error.to_string()))?;
                configure_native_hud_gtk_window(&gtk_window, 704, 80);
            }
            ensure_main_window_interactive(&hud, None).map_err(std::io::Error::other)?;
            schedule_main_window_input_recovery(app.handle().clone(), None);
            install_tray(app)?;
            hud.show()?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            set_window_surface,
            hud_ready,
            open_editor_window,
            return_to_hud,
            probe_portals,
            audio_devices,
            webcam_devices,
            choose_appearance_asset,
            load_settings,
            save_settings,
            open_settings_window,
            load_editor_session,
            save_editor_session,
            discard_editor_session,
            publish_appearance_asset,
            select_monitor,
            update_preview_crop,
            cancel_preview,
            start_recording,
            stop_recording,
            save_final_video,
            inspect_final_video,
            trim_final_video,
            discard_final_video
        ])
        .build(tauri::generate_context!())
        .expect("error while running tauri application");
    let state = app.state::<portal::CaptureState>().inner().clone();
    app.run(move |_handle, event| {
        if matches!(
            event,
            tauri::RunEvent::ExitRequested { .. } | tauri::RunEvent::Exit
        ) {
            portal::abort_all(&state);
        }
    });
}
