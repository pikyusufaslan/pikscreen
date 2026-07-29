use ashpd::desktop::{
    global_shortcuts::GlobalShortcuts,
    screencast::{CursorMode, Screencast, SourceType},
};
use ashpd::enumflags2::BitFlags;
use evdev::{Device, EventType, KeyCode};
use serde::Serialize;
use std::{
    collections::HashMap,
    env,
    ffi::OsStr,
    fs::{self, File},
    io::{BufRead, BufReader, BufWriter, Write},
    os::unix::net::UnixStream,
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Emitter};

use crate::{
    appearance,
    cursor::{CursorPoint, CursorSample, CursorSmoother, CursorTimeline, TahoeCursor},
    editor, gnome_guides,
    hyprland_cursor::{self, HyprlandCursorClient},
    hyprland_input::{
        self, BridgeReceiveError, HyprlandInputBridge, HyprlandInputPacket, BTN_LEFT, BTN_RIGHT,
        EVENT_KEYBOARD_KEY, EVENT_MOUSE_BUTTON, KEY_LEFTALT, KEY_RIGHTALT,
    },
    portal_cursor::{
        self, PortalCursorClassifier, PortalCursorCollector, PortalCursorEvent,
        PortalCursorObservation,
    },
    webcam::{self, WebcamRecorder, WebcamSettings},
    window_pipewire::{self, ChildLog, WindowPipelineProfile, WindowRecorder},
    zoom::{
        build_zoom_events, camera_transform_at, CameraTransform, Marker, RecordlyCameraSpring,
        ZoomTiming,
    },
};

const TAP_GUIDE_DURATION: Duration = Duration::from_secs(3);
const HELD_ALT_THRESHOLD: Duration = Duration::from_millis(220);
const ALT_REPEAT_GAP: Duration = Duration::from_millis(45);
// A guide must always self-expire even if a keyboard event is lost.
const GUIDE_MAX_DURATION_MS: &str = "3000";
const RECORDING_COUNTDOWN_MS: &str = "3000";
const PORTAL_INTERMEDIATE_MAX_BITRATE_KBPS: u32 = 100_000;
mod audio;
mod cursor_samples;
mod kde;
mod niri;
mod render;
pub use audio::*;
use cursor_samples::*;
use kde::*;
use niri::*;
use render::*;

static TEMPORARY_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn trace_portal_selection(stage: &str) {
    let _ = fs::write("/tmp/pikscreen-portal-selection.trace", stage);
}

#[derive(Debug, Serialize)]
pub struct PortalCapabilities {
    pub screencast: bool,
    pub cursor_metadata: bool,
    pub global_shortcuts: bool,
    pub detail: String,
}

pub async fn probe() -> PortalCapabilities {
    let screencast = Screencast::new().await;
    let (screencast_available, cursor_metadata, screencast_error) = match screencast {
        Ok(proxy) => match screen_cast_cursor_modes(&proxy).await {
            Ok(modes) => (true, modes.contains(CursorMode::Metadata), None),
            Err(error) => (true, false, Some(error.to_string())),
        },
        Err(error) => (false, false, Some(error.to_string())),
    };
    let global_shortcuts = GlobalShortcuts::new().await.is_ok();
    let detail = match screencast_error {
        Some(error) => error,
        None if !global_shortcuts => "Global Shortcuts portal is unavailable.".to_owned(),
        None if !cursor_metadata => {
            "The ScreenCast portal does not expose cursor metadata.".to_owned()
        }
        None => "PikScreen can request its required portals on this session.".to_owned(),
    };

    PortalCapabilities {
        screencast: screencast_available,
        cursor_metadata,
        global_shortcuts,
        detail,
    }
}

#[derive(Clone)]
pub struct CaptureState {
    active: Arc<Mutex<Option<ActiveCapture>>>,
    preview: Arc<Mutex<Option<PreviewSelection>>>,
    next_capture_id: Arc<AtomicU64>,
}

impl CaptureState {
    pub fn new() -> Self {
        Self {
            active: Arc::new(Mutex::new(None)),
            preview: Arc::new(Mutex::new(None)),
            next_capture_id: Arc::new(AtomicU64::new(1)),
        }
    }
}

pub struct ActiveCapture {
    id: u64,
    _portal: Screencast,
    _session: ashpd::desktop::Session<Screencast>,
    recorder: VideoRecorder,
    audio_recorder: Option<Child>,
    webcam_recorder: Option<WebcamRecorder>,
    webcam_start_warning: Option<String>,
    source_path: PathBuf,
    audio_path: Option<PathBuf>,
    width: u32,
    height: u32,
    backend: CaptureBackend,
    cursor_strategy: CursorStrategy,
    source_kind: CaptureSourceKind,
    window_id: Option<u64>,
    settings: RecordingSettings,
    started_at: Instant,
    markers: Vec<Marker>,
    cursor_samples: Vec<CursorSample>,
    cursor_sampling_error: Option<String>,
    click_samples: Vec<ClickSample>,
    click_sampling_error: Option<String>,
    border_dismiss_path: Option<GuideDismiss>,
    portal_cursor_collector: Option<PortalCursorCollector>,
}

#[derive(Clone)]
enum GuideDismiss {
    File(PathBuf),
}

#[derive(Debug, Serialize)]
pub struct CaptureSelection {
    pub node_id: u32,
    pub detail: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletedRecording {
    pub source_path: String,
    pub session_id: String,
}

struct PreviewSelection {
    id: u64,
    node_id: u32,
    backend: CaptureBackend,
    cursor_strategy: CursorStrategy,
    source_kind: CaptureSourceKind,
    window_id: Option<u64>,
    portal_profile: Option<WindowPipelineProfile>,
    _portal: Screencast,
    _session: ashpd::desktop::Session<Screencast>,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    source_width: i32,
    source_height: i32,
    crop: Arc<Mutex<CaptureCrop>>,
}

enum VideoRecorder {
    WfRecorder(Child, ChildLog),
    Portal(WindowRecorder),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureBackend {
    Niri,
    Portal,
}

impl CaptureBackend {
    fn cursor_strategy(
        self,
        available_modes: BitFlags<CursorMode>,
        hyprland_session: bool,
        gnome_session: bool,
    ) -> Result<CursorStrategy, String> {
        match self {
            Self::Niri if available_modes.contains(CursorMode::Hidden) => {
                Ok(CursorStrategy::SyntheticTahoe)
            }
            Self::Portal if gnome_session && available_modes.contains(CursorMode::Hidden) => {
                Ok(CursorStrategy::GnomeShell)
            }
            Self::Portal if available_modes.contains(CursorMode::Metadata) => {
                Ok(CursorStrategy::PortalMetadata)
            }
            Self::Portal
                if hyprland_session && available_modes.contains(CursorMode::Hidden) =>
            {
                Ok(CursorStrategy::HyprlandIpc)
            }
            Self::Portal if available_modes.contains(CursorMode::Embedded) => {
                Ok(CursorStrategy::Embedded)
            }
            _ => Err(format!(
                "The active ScreenCast portal has no compatible cursor mode for the {self:?} backend."
            )),
        }
    }

    fn uses_portal_stream(self, source_kind: CaptureSourceKind) -> bool {
        self == Self::Portal || source_kind == CaptureSourceKind::Window
    }

    fn allows_bottom_dock_crop(self, source_kind: CaptureSourceKind) -> bool {
        self == Self::Niri && source_kind == CaptureSourceKind::Monitor
    }

    fn pipeline_profiles(self) -> [WindowPipelineProfile; 2] {
        match self {
            Self::Niri => WindowPipelineProfile::ALL,
            Self::Portal => [
                WindowPipelineProfile::MemFdX264,
                WindowPipelineProfile::DmaBufGlVaapi,
            ],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CursorStrategy {
    SyntheticTahoe,
    GnomeShell,
    PortalMetadata,
    HyprlandIpc,
    Embedded,
}

impl CursorStrategy {
    fn portal_mode(self) -> CursorMode {
        match self {
            Self::SyntheticTahoe => CursorMode::Hidden,
            Self::GnomeShell => CursorMode::Hidden,
            Self::PortalMetadata => CursorMode::Metadata,
            Self::HyprlandIpc => CursorMode::Hidden,
            Self::Embedded => CursorMode::Embedded,
        }
    }

    fn uses_synthetic_render(self) -> bool {
        matches!(
            self,
            Self::SyntheticTahoe | Self::GnomeShell | Self::PortalMetadata | Self::HyprlandIpc
        )
    }
}

fn legacy_screen_cast_cursor_modes(interface_version: u32) -> Option<BitFlags<CursorMode>> {
    (interface_version < 2).then(|| CursorMode::Hidden.into())
}

async fn screen_cast_cursor_modes(
    portal: &Screencast,
) -> Result<BitFlags<CursorMode>, ashpd::Error> {
    if let Some(modes) = legacy_screen_cast_cursor_modes(portal.version()) {
        return Ok(modes);
    }
    portal.available_cursor_modes().await
}

fn requested_screen_cast_cursor_mode(
    interface_version: u32,
    strategy: CursorStrategy,
) -> Option<CursorMode> {
    (interface_version >= 2).then(|| strategy.portal_mode())
}

fn capture_backend_from_environment(
    niri_socket: Option<&OsStr>,
    session_type: Option<&OsStr>,
    wayland_display: Option<&OsStr>,
) -> Result<CaptureBackend, String> {
    if niri_socket.is_some() {
        return Ok(CaptureBackend::Niri);
    }
    if session_type.is_some_and(|value| value.eq_ignore_ascii_case("wayland"))
        || wayland_display.is_some()
    {
        return Ok(CaptureBackend::Portal);
    }
    Err("PikScreen's portable capture backend requires a Wayland session.".to_owned())
}

fn capture_backend() -> Result<CaptureBackend, String> {
    capture_backend_from_environment(
        env::var_os("NIRI_SOCKET").as_deref(),
        env::var_os("XDG_SESSION_TYPE").as_deref(),
        env::var_os("WAYLAND_DISPLAY").as_deref(),
    )
}

fn valid_portal_stream_size(size: Option<(i32, i32)>) -> Option<(i32, i32)> {
    size.filter(|(width, height)| *width >= 2 && *height >= 2)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PortalStreamSizeAuthority {
    Metadata,
    NegotiatedPipeWire,
}

fn portal_stream_size_authority(
    backend: CaptureBackend,
    source_kind: CaptureSourceKind,
) -> PortalStreamSizeAuthority {
    if backend.uses_portal_stream(source_kind) {
        PortalStreamSizeAuthority::NegotiatedPipeWire
    } else {
        PortalStreamSizeAuthority::Metadata
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CaptureSourceKind {
    Monitor,
    Window,
}

impl CaptureSourceKind {
    fn label(self) -> &'static str {
        match self {
            Self::Monitor => "monitor",
            Self::Window => "window",
        }
    }

    fn uses_bottom_dock_crop(self) -> bool {
        matches!(self, Self::Monitor)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureCrop {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    excluded_bottom: i32,
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewSettings {
    hide_bottom_bar: bool,
    bar_height_correction: i32,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewInfo {
    pub detail: String,
    pub crop: CaptureCrop,
    pub source_kind: CaptureSourceKind,
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartClick {}

#[derive(Clone, Debug, serde::Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingSettings {
    pub(crate) fps: u32,
    pub(crate) quality: VideoQuality,
    pub(crate) background: BackgroundStyle,
    /// With the scene off the recording fills the frame: no wallpaper, no
    /// padding, no rounded card. Older payloads predate the toggle and expect
    /// the scene, so absence means on.
    #[serde(default = "default_background_enabled")]
    pub(crate) background_enabled: bool,
    #[serde(default)]
    pub(crate) custom_background_path: Option<String>,
    #[serde(default = "default_cursor_visible")]
    pub(crate) cursor_visible: bool,
    #[serde(default)]
    pub(crate) custom_cursor_path: Option<String>,
    #[serde(default = "default_cursor_smoothing")]
    pub(crate) cursor_smoothing: u8,
    #[serde(default = "default_cursor_size")]
    pub(crate) cursor_size: u8,
    #[serde(default = "default_zoom_scale")]
    pub(crate) zoom_scale: f64,
    #[serde(default = "default_click_effects_enabled")]
    pub(crate) click_effects_enabled: bool,
    pub(crate) audio: AudioSettings,
    #[serde(default)]
    pub(crate) webcam: WebcamSettings,
}

pub(crate) fn default_background_enabled() -> bool {
    true
}

pub(crate) fn default_cursor_smoothing() -> u8 {
    35
}

pub(crate) fn default_cursor_size() -> u8 {
    100
}

pub(crate) fn default_zoom_scale() -> f64 {
    1.8
}

pub(crate) fn default_cursor_visible() -> bool {
    true
}

pub(crate) fn default_click_effects_enabled() -> bool {
    true
}

#[derive(Clone, Debug, serde::Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AudioSettings {
    pub(crate) system_enabled: bool,
    pub(crate) microphone_enabled: bool,
    pub(crate) system_volume: f64,
    pub(crate) microphone_volume: f64,
    pub(crate) system_device: Option<String>,
    pub(crate) microphone_device: Option<String>,
}

#[derive(Clone, Copy, Debug, serde::Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum VideoQuality {
    Balanced,
    High,
    Ultra,
}

#[derive(Clone, Copy, Debug, serde::Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum BackgroundStyle {
    PureBlack,
    PureWhite,
    TahoeLight,
    TahoeDark,
    Midnight8,
    Ipad17Dark,
    Ipad17Light,
    SequoiaBlue,
    SequoiaBlueOrange,
    Ventura,
    SonomaClouds,
    SonomaLight,
    SonomaDark,
    Glassmorphism3,
    Glassmorphism4,
    Energy19,
    Wallpaper3,
    Wallpaper4,
    Cityscape,
    Levels,
    Wallpaper10,
    VenturaDark,
    SonomaEvening,
    SonomaHorizon,
    Iridescent9,
    Energy17,
}

impl RecordingSettings {
    pub(crate) fn validate(self) -> Result<Self, String> {
        if !matches!(self.fps, 60 | 120 | 180) {
            return Err(format!(
                "Unsupported recording FPS: {}. Choose 60, 120, or 180.",
                self.fps
            ));
        }
        if self.cursor_smoothing > 100 {
            return Err("Cursor smoothing must be between 0 and 100.".to_owned());
        }
        if !(60..=150).contains(&self.cursor_size) {
            return Err("Cursor size must be between 60 and 150 percent.".to_owned());
        }
        if !self.zoom_scale.is_finite() || !(1.2..=2.4).contains(&self.zoom_scale) {
            return Err("Zoom amount must be between 1.2× and 2.4×.".to_owned());
        }
        if let Some(path) = self.custom_cursor_path.as_deref() {
            appearance::validate_cursor(Path::new(path))?;
        }
        if let Some(path) = self.custom_background_path.as_deref() {
            appearance::validate_background(Path::new(path))?;
        }
        self.audio.validate()?;
        self.webcam.validate()?;
        Ok(self)
    }

    fn fps_argument(&self) -> String {
        self.fps.to_string()
    }

    fn zoom_timing(&self) -> ZoomTiming {
        ZoomTiming {
            target_scale: self.zoom_scale,
            ..ZoomTiming::default()
        }
    }
}

impl AudioSettings {
    pub(crate) fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("System audio level", self.system_volume),
            ("Microphone level", self.microphone_volume),
        ] {
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                return Err(format!("{name} must be between 0 and 100%."));
            }
        }
        Ok(())
    }

    fn captures_audio(&self) -> bool {
        self.system_enabled || self.microphone_enabled
    }

    fn label(&self) -> &'static str {
        match (self.system_enabled, self.microphone_enabled) {
            (true, true) => "system audio + microphone",
            (true, false) => "system audio",
            (false, true) => "microphone",
            (false, false) => "no audio",
        }
    }
}

impl VideoQuality {
    fn encoder_profile(self) -> (&'static str, &'static str) {
        match self {
            Self::Balanced => ("20", "superfast"),
            Self::High => ("18", "medium"),
            Self::Ultra => ("16", "slow"),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Balanced => "Balanced",
            Self::High => "High",
            Self::Ultra => "Ultra",
        }
    }

    fn final_encoder_profile(self) -> (&'static str, &'static str) {
        let (crf, _) = self.encoder_profile();
        // On this RX 580, the VAAPI H.264 path is much slower than x264 for
        // final exports. Keep the chosen CRF while preferring a fast CPU
        // encode so post-processing does not turn a short recording into a
        // long wait.
        (crf, "ultrafast")
    }

    fn realtime_source_encoder_profile(self) -> (&'static str, &'static str) {
        let (crf, _) = self.encoder_profile();
        (crf, "ultrafast")
    }

    fn portal_intermediate_profile(self) -> (u32, &'static str, u32) {
        let quantizer = match self {
            Self::Balanced => 14,
            Self::High => 12,
            Self::Ultra => 10,
        };
        (quantizer, "veryfast", PORTAL_INTERMEDIATE_MAX_BITRATE_KBPS)
    }
}

impl BackgroundStyle {
    fn asset_file(self) -> Option<&'static str> {
        match self {
            Self::PureBlack | Self::PureWhite => None,
            Self::TahoeLight => Some("tahoe-light.jpg"),
            Self::TahoeDark => Some("tahoe-dark.jpg"),
            Self::Midnight8 => Some("midnight-8.jpg"),
            Self::Ipad17Dark => Some("ipad-17-dark.jpg"),
            Self::Ipad17Light => Some("ipad-17-light.jpg"),
            Self::SequoiaBlue => Some("sequoia-blue.jpg"),
            Self::SequoiaBlueOrange => Some("sequoia-blue-orange.jpg"),
            Self::Ventura => Some("ventura.jpg"),
            Self::SonomaClouds => Some("sonoma-clouds.jpg"),
            Self::SonomaLight => Some("sonoma-light.jpg"),
            Self::SonomaDark => Some("sonoma-dark.jpg"),
            Self::Glassmorphism3 => Some("glassmorphism-3.jpg"),
            Self::Glassmorphism4 => Some("glassmorphism-4.jpg"),
            Self::Energy19 => Some("energy-19.jpg"),
            Self::Wallpaper3 => Some("wallpaper3.jpg"),
            Self::Wallpaper4 => Some("wallpaper4.jpg"),
            Self::Cityscape => Some("cityscape.jpg"),
            Self::Levels => Some("levels.jpg"),
            Self::Wallpaper10 => Some("wallpaper10.jpg"),
            Self::VenturaDark => Some("ventura-dark.jpg"),
            Self::SonomaEvening => Some("sonoma-evening.jpg"),
            Self::SonomaHorizon => Some("sonoma-horizon.jpg"),
            Self::Iridescent9 => Some("iridescent-9.jpg"),
            Self::Energy17 => Some("energy-17.jpg"),
        }
    }

    fn solid_color(self) -> Option<&'static str> {
        match self {
            Self::PureBlack => Some("black"),
            Self::PureWhite => Some("white"),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct MarkerFeedback {
    count: usize,
    time_ms: u64,
    x: f64,
    y: f64,
}

pub async fn select_monitor(
    _app: AppHandle,
    state: &CaptureState,
    _click: StartClick,
    preview_settings: PreviewSettings,
) -> Result<PreviewInfo, String> {
    trace_portal_selection("select_monitor entered");
    if state
        .active
        .lock()
        .map_err(|_| "Capture state lock failed.".to_owned())?
        .is_some()
    {
        return Err("Stop the active recording before choosing another monitor.".to_owned());
    }
    if state
        .preview
        .lock()
        .map_err(|_| "Preview state lock failed.".to_owned())?
        .is_some()
    {
        return Err(
            "A source preview is already active. Start recording or choose another source."
                .to_owned(),
        );
    }
    let id = state.next_capture_id.fetch_add(1, Ordering::Relaxed);
    let backend = capture_backend()?;
    trace_portal_selection("opening ScreenCast portal");
    let portal = Screencast::new().await.map_err(|error| {
        trace_portal_selection(&format!("opening ScreenCast portal failed: {error}"));
        format!("Could not open the ScreenCast portal: {error}")
    })?;
    let interface_version = portal.version();
    let available_cursor_modes = screen_cast_cursor_modes(&portal)
        .await
        .map_err(|error| format!("Could not inspect ScreenCast cursor modes: {error}"))?;
    let gnome_session = gnome_guides::session_available();
    let cursor_strategy = backend.cursor_strategy(
        available_cursor_modes,
        hyprland_cursor::session_available(),
        gnome_session,
    )?;
    trace_portal_selection("creating ScreenCast session");
    let session = portal
        .create_session(Default::default())
        .await
        .map_err(|error| {
            trace_portal_selection(&format!("ScreenCast CreateSession failed: {error}"));
            format!("ScreenCast CreateSession failed: {error}")
        })?;
    let mut source_options = ashpd::desktop::screencast::SelectSourcesOptions::default()
        .set_sources(Some(SourceType::Monitor | SourceType::Window))
        .set_multiple(false);
    if let Some(cursor_mode) = requested_screen_cast_cursor_mode(interface_version, cursor_strategy)
    {
        source_options = source_options.set_cursor_mode(cursor_mode);
    }
    trace_portal_selection("requesting ScreenCast sources");
    portal
        .select_sources(&session, source_options)
        .await
        .map_err(|error| {
            trace_portal_selection(&format!("ScreenCast SelectSources failed: {error}"));
            format!("ScreenCast SelectSources failed: {error}")
        })?;
    trace_portal_selection("starting ScreenCast picker");
    let streams = portal
        .start(&session, None, Default::default())
        .await
        .map_err(|error| {
            trace_portal_selection(&format!("ScreenCast Start failed: {error}"));
            format!("ScreenCast Start failed: {error}")
        })?
        .response()
        .map_err(|error| {
            trace_portal_selection(&format!("ScreenCast Start response failed: {error}"));
            format!("ScreenCast Start response failed: {error}")
        })?;
    trace_portal_selection("ScreenCast stream selected");
    let stream = streams
        .streams()
        .iter()
        .next()
        .ok_or_else(|| "The portal returned no screen or window stream.".to_owned())?;
    let source_kind = match stream.source_type() {
        Some(SourceType::Window) => CaptureSourceKind::Window,
        Some(SourceType::Monitor) | None => CaptureSourceKind::Monitor,
        Some(other) => {
            return Err(format!(
                "PikScreen does not support this portal source: {other:?}"
            ))
        }
    };
    let node_id = stream.pipe_wire_node_id();
    let (mut x, mut y) = stream.position().unwrap_or((0, 0));
    let metadata_size = valid_portal_stream_size(stream.size());
    let (mut width, mut height) = match portal_stream_size_authority(backend, source_kind) {
        PortalStreamSizeAuthority::Metadata => valid_portal_stream_size(stream.size())
            .ok_or_else(|| "The portal did not report a valid selected source size.".to_owned())?,
        PortalStreamSizeAuthority::NegotiatedPipeWire => {
            // Niri's window portal stream can advertise a 1x1 placeholder until its
            // focused-window geometry settles. The geometry below is the authoritative
            // size for this path, so do not reject the selection on a redundant
            // PipeWire size-only connection first.
            if backend == CaptureBackend::Niri && source_kind == CaptureSourceKind::Window {
                metadata_size.unwrap_or((1, 1))
            } else {
                let pipewire_fd = portal
                    .open_pipe_wire_remote(&session, Default::default())
                    .await
                    .map_err(|error| format!("Could not open the portal size remote: {error}"))?;
                let (width, height) = portal_cursor::probe_stream_size(pipewire_fd, node_id)?;
                (
                    i32::try_from(width).map_err(|_| {
                        format!("PipeWire reported an invalid source width: {width}")
                    })?,
                    i32::try_from(height).map_err(|_| {
                        format!("PipeWire reported an invalid source height: {height}")
                    })?,
                )
            }
        }
    };
    let window_id = if backend == CaptureBackend::Niri && source_kind == CaptureSourceKind::Window {
        let window_id = selected_niri_window_id(node_id)?;
        focus_niri_window(window_id)?;
        let geometry = settled_niri_window_geometry(window_id).ok_or_else(|| {
            format!(
                "Niri focused window {window_id}, but its visible geometry never became available."
            )
        })?;
        x = geometry.x;
        y = geometry.y;
        width = geometry.width;
        height = geometry.height;
        Some(window_id)
    } else {
        None
    };
    let (source_width, source_height) = (width, height);
    let crop =
        capture_crop_for_backend(backend, source_kind, x, y, width, height, preview_settings)?;
    let crop = if backend == CaptureBackend::Niri && source_kind == CaptureSourceKind::Window {
        clamp_window_crop_to_niri_output(crop)
    } else {
        crop
    };
    let portal_profile = if backend == CaptureBackend::Niri
        && source_kind == CaptureSourceKind::Window
    {
        // Niri's window stream may only produce frames after the picker has
        // returned and focus has settled. A short preflight capture rejects
        // otherwise valid selections here, so defer the real pipeline check
        // to recording startup and use the CPU-readable profile.
        Some(WindowPipelineProfile::MemFdX264)
    } else if backend.uses_portal_stream(source_kind) {
        let probe_path = temporary_file("portal-pipewire-probe", "mp4");
        let mut failures = Vec::new();
        let mut selected = None;
        for profile in backend.pipeline_profiles() {
            let pipewire_fd = portal
                .open_pipe_wire_remote(&session, Default::default())
                .await
                .map_err(|error| format!("Could not open the portal PipeWire remote: {error}"))?;
            match window_pipewire::probe_profile(
                pipewire_fd,
                node_id,
                profile,
                source_width as u32,
                source_height as u32,
                &probe_path,
            ) {
                Ok(()) => {
                    eprintln!(
                        "PikScreen selected {} for the {} {} portal stream.",
                        profile.label(),
                        match backend {
                            CaptureBackend::Niri => "Niri",
                            CaptureBackend::Portal => "portable",
                        },
                        source_kind.label(),
                    );
                    selected = Some(profile);
                    break;
                }
                Err(error) => failures.push(error),
            }
        }
        let _ = fs::remove_file(&probe_path);
        let profile = selected.ok_or_else(|| {
            format!(
                "Could not consume the selected {} PipeWire stream. {}",
                source_kind.label(),
                failures.join(" | ")
            )
        })?;
        Some(profile)
    } else {
        None
    };
    let selected_crop = crop;
    let crop = Arc::new(Mutex::new(crop));
    *state
        .preview
        .lock()
        .map_err(|_| "Preview state lock failed.".to_owned())? = Some(PreviewSelection {
        id,
        node_id,
        backend,
        cursor_strategy,
        source_kind,
        window_id,
        portal_profile,
        _portal: portal,
        _session: session,
        x: selected_crop.x,
        y: selected_crop.y,
        width: selected_crop.width,
        height: selected_crop.height,
        source_width,
        source_height,
        crop: crop.clone(),
    });
    let crop = *crop
        .lock()
        .map_err(|_| "Preview crop lock failed.".to_owned())?;
    Ok(PreviewInfo {
        detail: format!(
            "Selected {}x{} {} source.",
            if source_kind == CaptureSourceKind::Window {
                source_width
            } else {
                crop.width
            },
            if source_kind == CaptureSourceKind::Window {
                source_height
            } else {
                crop.height
            },
            source_kind.label(),
        ),
        crop,
        source_kind,
    })
}

pub fn update_preview_crop(
    state: &CaptureState,
    preview_settings: PreviewSettings,
) -> Result<PreviewInfo, String> {
    let preview = state
        .preview
        .lock()
        .map_err(|_| "Preview state lock failed.".to_owned())?;
    let selection = preview
        .as_ref()
        .ok_or_else(|| "Choose a screen or window before changing the preview crop.".to_owned())?;
    let crop = capture_crop_for_backend(
        selection.backend,
        selection.source_kind,
        selection.x,
        selection.y,
        selection.width,
        selection.height,
        preview_settings,
    )?;
    *selection
        .crop
        .lock()
        .map_err(|_| "Preview crop lock failed.".to_owned())? = crop;
    Ok(PreviewInfo {
        detail: if !selection
            .backend
            .allows_bottom_dock_crop(selection.source_kind)
        {
            "This capture backend always uses the full portal source; bottom-dock cropping is unavailable."
                .to_owned()
        } else if crop.excluded_bottom > 0 {
            format!(
                "Preview hides the bottom {} px of the selected monitor.",
                crop.excluded_bottom
            )
        } else {
            "Preview uses the full selected monitor.".to_owned()
        },
        crop,
        source_kind: selection.source_kind,
    })
}

fn capture_crop_for_backend(
    backend: CaptureBackend,
    source_kind: CaptureSourceKind,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    mut settings: PreviewSettings,
) -> Result<CaptureCrop, String> {
    if !backend.allows_bottom_dock_crop(source_kind) {
        settings.hide_bottom_bar = false;
        settings.bar_height_correction = 0;
    }
    capture_crop(source_kind, x, y, width, height, settings)
}

pub fn cancel_preview(state: &CaptureState) -> Result<(), String> {
    state
        .preview
        .lock()
        .map_err(|_| "Preview state lock failed.".to_owned())?
        .take();
    Ok(())
}

pub fn start_recording(
    app: AppHandle,
    state: &CaptureState,
    settings: RecordingSettings,
) -> Result<CaptureSelection, String> {
    let settings = settings.validate()?;
    if state
        .active
        .lock()
        .map_err(|_| "Capture state lock failed.".to_owned())?
        .is_some()
    {
        return Err("A recording is already active.".to_owned());
    }
    let mut preview_guard = state
        .preview
        .lock()
        .map_err(|_| "Preview state lock failed.".to_owned())?;
    let preview = preview_guard
        .as_ref()
        .ok_or_else(|| "Choose a screen or window before recording.".to_owned())?;
    let crop = *preview
        .crop
        .lock()
        .map_err(|_| "Preview crop lock failed.".to_owned())?;
    ensure_kde_guide_capture_exclusion()?;
    run_recording_countdown(
        preview.x + preview.width / 2,
        preview.y + preview.height / 2,
    )?;
    let source_path = if preview.backend.uses_portal_stream(preview.source_kind) {
        temporary_file("source", "mkv")
    } else {
        temporary_path("source")
    };
    let (cursor_client, hyprland_input_bridge, cursor_samples, click_clock) = match preview
        .cursor_strategy
    {
        CursorStrategy::SyntheticTahoe => {
            let mut cursor_client = NiriCursorClient::connect()?;
            let initial_cursor = cursor_client.state()?;
            let initial_geometry = match preview.window_id {
                Some(window_id) => cursor_client.window_geometry(window_id)?,
                None => Some(crop),
            };
            let initial_cursor_sample =
                cursor_sample_for_geometry(initial_cursor, initial_geometry, (0.5, 0.5), 0);
            let click_clock = sample_niri_click_clock(&mut cursor_client).ok();
            (
                Some(cursor_client),
                None,
                vec![initial_cursor_sample],
                click_clock,
            )
        }
        CursorStrategy::HyprlandIpc => {
            let bridge = HyprlandInputBridge::connect()?;
            let cursor_client = HyprlandCursorClient::connect()?;
            let position = cursor_client.position()?;
            let initial_cursor_sample = if point_is_inside(position, crop) {
                cursor_sample(
                    position,
                    TahoeCursor::Arrow,
                    crop.x,
                    crop.y,
                    crop.width,
                    crop.height,
                    0,
                )
            } else {
                hidden_cursor_sample((0.5, 0.5), 0)
            };
            (None, Some(bridge), vec![initial_cursor_sample], None)
        }
        CursorStrategy::GnomeShell => {
            let position = gnome_guides::pointer()?;
            let initial_cursor_sample = gnome_cursor_sample(position, crop, (0.5, 0.5), 0);
            (None, None, vec![initial_cursor_sample], None)
        }
        CursorStrategy::PortalMetadata | CursorStrategy::Embedded => (None, None, Vec::new(), None),
    };
    let recording_started_at = Instant::now();
    let recording_epoch_ns = hyprland_input_bridge
        .as_ref()
        .map(|_| hyprland_input::monotonic_now_ns())
        .transpose()?;
    let click_epoch_ms = click_clock
        .map(|(now_ms, sampled_at)| translate_niri_epoch(now_ms, sampled_at, recording_started_at));
    let portal_cursor_fd = if preview.cursor_strategy == CursorStrategy::PortalMetadata {
        Some(
            pollster::block_on(
                preview
                    ._portal
                    .open_pipe_wire_remote(&preview._session, Default::default()),
            )
            .map_err(|error| format!("Could not open the portal cursor remote: {error}"))?,
        )
    } else {
        None
    };
    let mut recorder = if preview.backend.uses_portal_stream(preview.source_kind) {
        let preferred_profile = preview.portal_profile.ok_or_else(|| {
            "The selected source has no verified portal PipeWire profile.".to_owned()
        })?;
        let (quantizer, preset, max_bitrate_kbps) = match preview.backend {
            CaptureBackend::Portal => settings.quality.portal_intermediate_profile(),
            CaptureBackend::Niri => {
                let (quantizer, preset) = settings.quality.encoder_profile();
                (quantizer.parse().unwrap_or(18), preset, 2_048)
            }
        };
        let mut profiles = vec![preferred_profile];
        for profile in preview.backend.pipeline_profiles() {
            if !profiles.contains(&profile) {
                profiles.push(profile);
            }
        }
        let mut failures = Vec::new();
        let mut started_recorder = None;
        for profile in profiles {
            eprintln!(
                "PikScreen starting {} for {} PipeWire node {} at {}x{} / {} FPS.",
                profile.label(),
                preview.source_kind.label(),
                preview.node_id,
                preview.source_width,
                preview.source_height,
                settings.fps,
            );
            let pipewire_fd = match pollster::block_on(
                preview
                    ._portal
                    .open_pipe_wire_remote(&preview._session, Default::default()),
            ) {
                Ok(pipewire_fd) => pipewire_fd,
                Err(error) => {
                    failures.push(format!(
                        "{} could not open the recording remote: {error}",
                        profile.label()
                    ));
                    continue;
                }
            };
            let mut candidate = match window_pipewire::spawn_recorder(
                pipewire_fd,
                preview.node_id,
                profile,
                preview.source_width as u32,
                preview.source_height as u32,
                settings.fps,
                quantizer,
                preset,
                max_bitrate_kbps,
                &source_path,
            ) {
                Ok(recorder) => recorder,
                Err(error) => {
                    failures.push(format!("{} could not start: {error}", profile.label()));
                    continue;
                }
            };
            match window_pipewire::ensure_recorder_started(&mut candidate) {
                Ok(()) => {
                    eprintln!(
                        "PikScreen {} received the first {} frame.",
                        profile.label(),
                        preview.source_kind.label(),
                    );
                    started_recorder = Some(candidate);
                    break;
                }
                Err(error) => {
                    eprintln!("PikScreen {} startup failed: {error}", profile.label());
                    failures.push(error);
                }
            }
        }
        let recorder = started_recorder.ok_or_else(|| {
            format!(
                "Could not start the selected {} PipeWire stream. {}",
                preview.source_kind.label(),
                failures.join(" | ")
            )
        })?;
        VideoRecorder::Portal(recorder)
    } else {
        let (child, log) = spawn_cursorless_recorder(
            crop.x,
            crop.y,
            crop.width,
            crop.height,
            &source_path,
            settings.clone(),
        )?;
        VideoRecorder::WfRecorder(child, log)
    };
    if !preview.backend.uses_portal_stream(preview.source_kind) {
        ensure_video_recorder_started(&mut recorder)?;
    }
    let (audio_recorder, audio_path) = if settings.audio.captures_audio() {
        let audio_path = temporary_file("audio", "m4a");
        match spawn_audio_recorder(&audio_path, settings.audio.clone()) {
            Ok(recorder) => (Some(recorder), Some(audio_path)),
            Err(error) => {
                let _ = stop_video_recorder(recorder);
                return Err(error);
            }
        }
    } else {
        (None, None)
    };
    let (webcam_recorder, webcam_start_warning) =
        match webcam::start(&settings.webcam, temporary_file("webcam", "mkv")) {
            Ok(recorder) => (recorder, None),
            Err(error) => (None, Some(error)),
        };
    let (recording_width, recording_height) = if preview
        .backend
        .uses_portal_stream(preview.source_kind)
    {
        window_pipewire::even_dimensions(preview.source_width as u32, preview.source_height as u32)
    } else {
        (crop.width as u32, crop.height as u32)
    };
    let (portal_cursor_collector, portal_cursor_events) =
        if let Some(pipewire_fd) = portal_cursor_fd {
            let (collector_width, collector_height) =
                window_pipewire::preview_dimensions(recording_width, recording_height);
            match portal_cursor::spawn_collector(
                pipewire_fd,
                preview.node_id,
                collector_width,
                collector_height,
            ) {
                Ok((collector, events)) => (Some(collector), Some(events)),
                Err(error) => {
                    let _ = stop_video_recorder(recorder);
                    if let (Some(audio_recorder), Some(audio_path)) =
                        (audio_recorder, audio_path.as_ref())
                    {
                        let _ = stop_audio_recorder(audio_recorder, audio_path);
                    }
                    if let Some(webcam_recorder) = webcam_recorder {
                        let _ = webcam::stop(webcam_recorder);
                    }
                    return Err(error);
                }
            }
        } else {
            (None, None)
        };
    let border_dismiss_path = preview
        .backend
        .allows_bottom_dock_crop(preview.source_kind)
        .then(|| spawn_recording_border(crop.x, crop.y))
        .flatten();
    let backend = preview.backend;
    let cursor_strategy = preview.cursor_strategy;
    let source_kind = preview.source_kind;
    let capture_id = preview.id;
    let window_id = preview.window_id;
    let node_id = preview.node_id;
    let webcam_detail = match (&webcam_recorder, &webcam_start_warning) {
        (Some(_), _) => " Recordly-style webcam bubble enabled.".to_owned(),
        (None, Some(error)) => format!(" Webcam unavailable; continuing screen-only: {error}"),
        (None, None) => String::new(),
    };

    let preview = preview_guard
        .take()
        .ok_or_else(|| "The selected source disappeared during recording startup.".to_owned())?;
    let mut active = state
        .active
        .lock()
        .map_err(|_| "Capture state lock failed.".to_owned())?;
    *active = Some(ActiveCapture {
        id: capture_id,
        _portal: preview._portal,
        _session: preview._session,
        recorder,
        audio_recorder,
        webcam_recorder,
        webcam_start_warning,
        source_path,
        audio_path,
        width: recording_width,
        height: recording_height,
        backend,
        cursor_strategy,
        source_kind,
        window_id,
        settings: settings.clone(),
        started_at: recording_started_at,
        markers: Vec::new(),
        cursor_samples,
        cursor_sampling_error: None,
        click_samples: Vec::new(),
        click_sampling_error: None,
        border_dismiss_path,
        portal_cursor_collector,
    });
    drop(active);
    drop(preview_guard);

    if cursor_strategy.uses_synthetic_render() && cursor_strategy != CursorStrategy::HyprlandIpc {
        listen_for_alt_markers(
            app.clone(),
            state.active.clone(),
            capture_id,
            crop.x,
            crop.y,
            crop.width,
            crop.height,
        );
    }
    if let Some(cursor_client) = cursor_client {
        listen_for_cursor_samples(
            state.active.clone(),
            capture_id,
            cursor_client,
            crop.x,
            crop.y,
            crop.width,
            crop.height,
            window_id,
            settings.fps,
            click_epoch_ms,
        );
    }
    if let (Some(bridge), Some(recording_epoch_ns)) = (hyprland_input_bridge, recording_epoch_ns) {
        listen_for_hyprland_input_bridge(
            app.clone(),
            state.active.clone(),
            capture_id,
            bridge,
            crop,
            recording_epoch_ns,
        );
    }
    if let Some(events) = portal_cursor_events {
        listen_for_portal_cursor_events(state.active.clone(), capture_id, events);
    }
    if cursor_strategy == CursorStrategy::GnomeShell {
        listen_for_gnome_cursor_samples(state.active.clone(), capture_id, crop);
    }
    if matches!(
        cursor_strategy,
        CursorStrategy::PortalMetadata | CursorStrategy::GnomeShell
    ) {
        listen_for_portal_clicks(state.active.clone(), capture_id);
    }

    Ok(CaptureSelection {
        node_id,
        detail: match cursor_strategy {
            CursorStrategy::SyntheticTahoe => format!(
                "Recording {}x{} {} at {} FPS / {} quality with {}. Press Alt to place a zoom marker.{webcam_detail}",
                recording_width,
                recording_height,
                source_kind.label(),
                settings.fps,
                settings.quality.label(),
                settings.audio.label(),
            ),
            CursorStrategy::GnomeShell => format!(
                "Recording {}x{} {} at {} FPS / {} quality with {}, GNOME Shell cursor sampling, click effects, and Alt zoom markers.{webcam_detail}",
                recording_width,
                recording_height,
                source_kind.label(),
                settings.fps,
                settings.quality.label(),
                settings.audio.label(),
            ),
            CursorStrategy::Embedded => format!(
                "Recording {}x{} {} at {} FPS / {} quality with {} and KDE's embedded cursor. Custom click and Alt zoom effects are disabled for this milestone.{webcam_detail}",
                recording_width,
                recording_height,
                source_kind.label(),
                settings.fps,
                settings.quality.label(),
                settings.audio.label(),
            ),
            CursorStrategy::PortalMetadata => format!(
                "Recording {}x{} {} at {} FPS / {} quality with {}, PipeWire cursor metadata, click effects, and Alt zoom markers.{webcam_detail}",
                recording_width,
                recording_height,
                source_kind.label(),
                settings.fps,
                settings.quality.label(),
                settings.audio.label(),
            ),
            CursorStrategy::HyprlandIpc => format!(
                "Recording {}x{} {} at {} FPS / {} quality with {}, Hyprland's hidden cursor, Tahoe smoothing, click effects, and Alt zoom markers.{webcam_detail}",
                recording_width,
                recording_height,
                source_kind.label(),
                settings.fps,
                settings.quality.label(),
                settings.audio.label(),
            ),
        },
    })
}

fn capture_crop(
    source_kind: CaptureSourceKind,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    settings: PreviewSettings,
) -> Result<CaptureCrop, String> {
    if source_kind == CaptureSourceKind::Window && (width < 2 || height < 2) {
        return Err(
            "Window capture requires a real source size, not the portal's 1x1 placeholder."
                .to_owned(),
        );
    }
    if !(-240..=480).contains(&settings.bar_height_correction) {
        return Err("Bottom-bar correction must stay between -240 px and 480 px.".to_owned());
    }
    let detected = detect_quickshell_bottom_bar_height();
    let requested_exclusion = if source_kind.uses_bottom_dock_crop() && settings.hide_bottom_bar {
        (detected + settings.bar_height_correction).max(0)
    } else {
        0
    };
    let max_exclusion = (height - 120).max(0);
    let mut excluded_bottom = requested_exclusion.min(max_exclusion);
    // wf-recorder's H.264 output rounds an odd crop height down to the next
    // even value. Keep PikScreen's tracked geometry identical to that source
    // from the start, otherwise final rendering asks FFmpeg for one row that
    // does not exist and it closes the cursor pipe.
    if (height - excluded_bottom) % 2 != 0 && excluded_bottom < max_exclusion {
        excluded_bottom += 1;
    }
    // Window capture uses Niri's selected fixed global rectangle. Never apply
    // the desktop dock heuristic, even if the browser still sends the old
    // checkbox value while the UI is updating.
    if !source_kind.uses_bottom_dock_crop() {
        return Ok(CaptureCrop {
            x,
            y,
            width: width - width.rem_euclid(2),
            height: height - height.rem_euclid(2),
            excluded_bottom: 0,
        });
    }
    Ok(CaptureCrop {
        x,
        y,
        width,
        height: height.saturating_sub(excluded_bottom.min(max_exclusion)),
        excluded_bottom: excluded_bottom.min(max_exclusion),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClickSample {
    pub(crate) time_ms: u64,
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) primary: bool,
}

fn spawn_cursorless_recorder(
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    source_path: &PathBuf,
    settings: RecordingSettings,
) -> Result<(Child, ChildLog), String> {
    let binary = cursorless_recorder_binary()?;
    let geometry = format!("{x},{y} {width}x{height}");
    let fps = settings.fps_argument();
    let (crf, preset) = settings.quality.realtime_source_encoder_profile();
    let preset_argument = format!("preset={preset}");
    let crf_argument = format!("crf={crf}");
    Command::new(binary)
        .args([
            "--no-dmabuf",
            "--no-damage",
            "-r",
            &fps,
            "-p",
            &preset_argument,
            "-p",
            &crf_argument,
            "-g",
            &geometry,
            "-f",
            source_path.to_string_lossy().as_ref(),
            "-y",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("Could not start the cursorless recorder: {error}"))
        .map(|mut child| {
            // Same reason the PipeWire pipeline drains its pipe: an unread
            // stderr stalls the writer once 64 KB accumulate.
            let log = match child.stderr.take() {
                Some(stderr) => ChildLog::drain(stderr),
                None => ChildLog::detached(),
            };
            (child, log)
        })
}

fn cursorless_recorder_binary() -> Result<PathBuf, String> {
    if let Some(binary) = env::var_os("PIKSCREEN_CURSORLESS_RECORDER_BIN") {
        let binary = PathBuf::from(binary);
        if binary.is_file() {
            return Ok(binary);
        }
    }
    let bundled = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../wf-recorder-no-cursor/build/wf-recorder");
    if bundled.is_file() {
        return Ok(bundled);
    }
    Err(
        "PikScreen needs its patched cursorless wf-recorder binary. Set \
         PIKSCREEN_CURSORLESS_RECORDER_BIN to the built executable."
            .to_owned(),
    )
}

struct PendingAltMarker {
    marker_index: usize,
    marker_time_ms: u64,
    pressed_at: Instant,
    guide_started_at: Instant,
    dismiss_path: Option<GuideDismiss>,
}

fn spawn_capture_guide(
    cursor_x: f64,
    cursor_y: f64,
    hide_cursor: bool,
    source_width: u32,
    source_height: u32,
    zoom_scale: f64,
) -> Option<GuideDismiss> {
    if gnome_guides::session_available() {
        return None;
    }

    let binary = guide_binary()?;

    let dismiss_path = temporary_file("guide-dismiss", "signal");
    let _ = fs::remove_file(&dismiss_path);

    let guide_width = ((source_width as f64 / zoom_scale).round() as u32).max(1);
    let guide_height = ((source_height as f64 / zoom_scale).round() as u32).max(1);
    Command::new(binary)
        .args([
            "--x",
            &cursor_x.round().to_string(),
            "--y",
            &cursor_y.round().to_string(),
            "--width",
            &guide_width.to_string(),
            "--height",
            &guide_height.to_string(),
            "--duration-ms",
            GUIDE_MAX_DURATION_MS,
            "--dismiss-file",
            dismiss_path.to_string_lossy().as_ref(),
            "--accent",
            if hide_cursor { "orange" } else { "blue" },
        ])
        .spawn()
        .ok()
        .map(|_| GuideDismiss::File(dismiss_path))
}

fn run_recording_countdown(target_x: i32, target_y: i32) -> Result<(), String> {
    if gnome_guides::session_available() {
        if let Err(error) = gnome_guides::ensure().and_then(|_| {
            gnome_guides::show_countdown(
                target_x,
                target_y,
                RECORDING_COUNTDOWN_MS
                    .parse()
                    .expect("valid recording countdown"),
            )
        }) {
            eprintln!("PikScreen GNOME countdown is unavailable: {error}");
        }
        return Ok(());
    }

    let binary = guide_binary()
        .ok_or_else(|| "PikScreen's recording countdown helper is unavailable.".to_owned())?;
    let status = Command::new(binary)
        .args(recording_countdown_args(target_x, target_y))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("Could not show the recording countdown: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "The recording countdown exited unsuccessfully: {status}"
        ))
    }
}

fn recording_countdown_args(target_x: i32, target_y: i32) -> Vec<String> {
    vec![
        "--countdown".to_owned(),
        "--x".to_owned(),
        target_x.to_string(),
        "--y".to_owned(),
        target_y.to_string(),
        "--duration-ms".to_owned(),
        RECORDING_COUNTDOWN_MS.to_owned(),
    ]
}

fn guide_binary() -> Option<PathBuf> {
    env::var_os("PIKSCREEN_GUIDE_BIN")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .or_else(|| {
            let local = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tools/pikscreen-guide");
            local.is_file().then_some(local)
        })
}

fn spawn_recording_border(output_x: i32, output_y: i32) -> Option<GuideDismiss> {
    let guide_binary = guide_binary()?;
    let dismiss_path = temporary_file("recording-border-dismiss", "signal");
    match Command::new(guide_binary)
        .args([
            "--border",
            "--x",
            &output_x.to_string(),
            "--y",
            &output_y.to_string(),
            "--duration-ms",
            "86400000",
            "--dismiss-file",
            dismiss_path.to_string_lossy().as_ref(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(_) => Some(GuideDismiss::File(dismiss_path)),
        Err(error) => {
            eprintln!("PikScreen recording border is unavailable: {error}");
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn listen_for_alt_markers(
    app: AppHandle,
    active: Arc<Mutex<Option<ActiveCapture>>>,
    id: u64,
    output_x: i32,
    output_y: i32,
    width: i32,
    height: i32,
) {
    std::thread::spawn(move || {
        let mut devices = keyboard_devices();
        let mut last_marker = Instant::now() - Duration::from_secs(1);
        let mut shift_held = false;
        let mut pending: Option<PendingAltMarker> = None;
        loop {
            if !capture_is_active(&active, id) {
                if let Some(marker) = pending.take() {
                    dismiss_guide(marker.dismiss_path, Duration::ZERO);
                }
                return;
            }
            if devices.is_empty() {
                devices = keyboard_devices();
            }
            for device in &mut devices {
                let Ok(events) = device.fetch_events() else {
                    continue;
                };
                for event in events {
                    if is_shift_press(&event) {
                        shift_held = true;
                        continue;
                    }
                    if is_shift_release(&event) {
                        shift_held = false;
                        continue;
                    }
                    if is_alt_release(&event) {
                        if let Some(marker) = pending.take() {
                            finish_alt_marker(&active, id, marker);
                        }
                        continue;
                    }
                    if pending.is_some()
                        || !is_alt_press(&event)
                        || last_marker.elapsed() < ALT_REPEAT_GAP
                    {
                        continue;
                    }
                    last_marker = Instant::now();
                    pending = add_marker(
                        &app, &active, id, output_x, output_y, width, height, shift_held,
                    );
                }
            }
            std::thread::sleep(Duration::from_millis(8));
        }
    });
}

fn keyboard_devices() -> Vec<Device> {
    let Ok(entries) = std::fs::read_dir("/dev/input") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("event"))
        .filter_map(|entry| Device::open(entry.path()).ok())
        .filter(|device| {
            device.supported_keys().is_some_and(|keys| {
                keys.contains(KeyCode::KEY_LEFTALT) || keys.contains(KeyCode::KEY_RIGHTALT)
            })
        })
        .filter_map(|device| device.set_nonblocking(true).ok().map(|_| device))
        .collect()
}

fn is_alt_press(event: &evdev::InputEvent) -> bool {
    event.event_type() == EventType::KEY
        && event.value() == 1
        && matches!(
            KeyCode::new(event.code()),
            KeyCode::KEY_LEFTALT | KeyCode::KEY_RIGHTALT
        )
}

fn is_alt_release(event: &evdev::InputEvent) -> bool {
    event.event_type() == EventType::KEY
        && event.value() == 0
        && matches!(
            KeyCode::new(event.code()),
            KeyCode::KEY_LEFTALT | KeyCode::KEY_RIGHTALT
        )
}

fn is_shift_press(event: &evdev::InputEvent) -> bool {
    event.event_type() == EventType::KEY
        && event.value() == 1
        && matches!(
            KeyCode::new(event.code()),
            KeyCode::KEY_LEFTSHIFT | KeyCode::KEY_RIGHTSHIFT
        )
}

fn is_shift_release(event: &evdev::InputEvent) -> bool {
    event.event_type() == EventType::KEY
        && event.value() == 0
        && matches!(
            KeyCode::new(event.code()),
            KeyCode::KEY_LEFTSHIFT | KeyCode::KEY_RIGHTSHIFT
        )
}

fn capture_is_active(active: &Arc<Mutex<Option<ActiveCapture>>>, id: u64) -> bool {
    active
        .lock()
        .ok()
        .and_then(|active| active.as_ref().map(|capture| capture.id == id))
        .unwrap_or(false)
}

fn add_hyprland_marker(
    app: &AppHandle,
    active: &Arc<Mutex<Option<ActiveCapture>>>,
    id: u64,
    packet: &HyprlandInputPacket,
    geometry: CaptureCrop,
    time_ms: u64,
) -> Option<PendingAltMarker> {
    if !point_is_inside(packet.position, geometry) {
        return None;
    }
    let marker = Marker {
        time_ms,
        x: (packet.position.0 - geometry.x as f64) / geometry.width as f64,
        y: (packet.position.1 - geometry.y as f64) / geometry.height as f64,
        target_scale: None,
        held_until_ms: None,
        hide_cursor: packet.shift_held(),
    };
    store_marker(app, active, id, marker, packet.position)
}

fn add_marker(
    app: &AppHandle,
    active: &Arc<Mutex<Option<ActiveCapture>>>,
    id: u64,
    output_x: i32,
    output_y: i32,
    width: i32,
    height: i32,
    hide_cursor: bool,
) -> Option<PendingAltMarker> {
    let (cursor_strategy, window_id, started_at, portal_sample) = {
        let Ok(active) = active.lock() else {
            return None;
        };
        let Some(capture) = active.as_ref().filter(|capture| capture.id == id) else {
            return None;
        };
        (
            capture.cursor_strategy,
            capture.window_id,
            capture.started_at,
            capture.cursor_samples.last().copied(),
        )
    };
    if matches!(
        cursor_strategy,
        CursorStrategy::GnomeShell | CursorStrategy::PortalMetadata | CursorStrategy::HyprlandIpc
    ) {
        let marker = portal_marker_from_sample(
            portal_sample,
            started_at.elapsed().as_millis() as u64,
            hide_cursor,
        )?;
        let guide_x = output_x as f64 + marker.x * width as f64;
        let guide_y = output_y as f64 + marker.y * height as f64;
        return store_marker(app, active, id, marker, (guide_x, guide_y));
    }
    if cursor_strategy != CursorStrategy::SyntheticTahoe {
        return None;
    }
    let mut cursor_client = match NiriCursorClient::connect() {
        Ok(client) => client,
        Err(error) => {
            let _ = app.emit("marker-error", error);
            return None;
        }
    };
    let cursor = match cursor_client.state() {
        Ok(cursor) => cursor,
        Err(error) => {
            let _ = app.emit("marker-error", error);
            return None;
        }
    };
    let geometry = match window_id {
        Some(window_id) => match cursor_client.window_geometry(window_id) {
            Ok(geometry) => geometry,
            Err(error) => {
                let _ = app.emit("marker-error", error);
                return None;
            }
        },
        None => Some(CaptureCrop {
            x: output_x,
            y: output_y,
            width,
            height,
            excluded_bottom: 0,
        }),
    };
    let geometry = geometry.filter(|geometry| point_is_inside(cursor.position, *geometry))?;
    let x = (cursor.position.0 - geometry.x as f64) / geometry.width as f64;
    let y = (cursor.position.1 - geometry.y as f64) / geometry.height as f64;
    let marker = Marker {
        time_ms: started_at.elapsed().as_millis() as u64,
        x,
        y,
        target_scale: None,
        held_until_ms: None,
        hide_cursor,
    };
    store_marker(app, active, id, marker, cursor.position)
}

fn portal_marker_from_sample(
    sample: Option<CursorSample>,
    time_ms: u64,
    hide_cursor: bool,
) -> Option<Marker> {
    let sample = sample.filter(|sample| sample.kind != TahoeCursor::Hidden)?;
    Some(Marker {
        time_ms,
        x: sample.x,
        y: sample.y,
        target_scale: None,
        held_until_ms: None,
        hide_cursor,
    })
}

fn store_marker(
    app: &AppHandle,
    active: &Arc<Mutex<Option<ActiveCapture>>>,
    id: u64,
    marker: Marker,
    guide_position: (f64, f64),
) -> Option<PendingAltMarker> {
    let Ok(mut active) = active.lock() else {
        return None;
    };
    let Some(capture) = active.as_mut().filter(|capture| capture.id == id) else {
        return None;
    };
    let marker_index = capture.markers.len();
    capture.markers.push(marker);
    let dismiss_path = spawn_capture_guide(
        guide_position.0,
        guide_position.1,
        marker.hide_cursor,
        capture.width,
        capture.height,
        capture.settings.zoom_scale,
    );
    let _ = app.emit(
        "marker-added",
        MarkerFeedback {
            count: capture.markers.len(),
            time_ms: marker.time_ms,
            x: marker.x,
            y: marker.y,
        },
    );
    Some(PendingAltMarker {
        marker_index,
        marker_time_ms: marker.time_ms,
        pressed_at: Instant::now(),
        guide_started_at: Instant::now(),
        dismiss_path,
    })
}

fn finish_alt_marker(
    active: &Arc<Mutex<Option<ActiveCapture>>>,
    id: u64,
    marker: PendingAltMarker,
) {
    let released_at_ms = active
        .lock()
        .ok()
        .and_then(|active| {
            active
                .as_ref()
                .filter(|capture| capture.id == id)
                .map(|capture| capture.started_at.elapsed().as_millis() as u64)
        })
        .unwrap_or_else(|| {
            marker
                .marker_time_ms
                .saturating_add(marker.pressed_at.elapsed().as_millis() as u64)
        });
    finish_alt_marker_at(active, id, marker, released_at_ms);
}

fn finish_alt_marker_at(
    active: &Arc<Mutex<Option<ActiveCapture>>>,
    id: u64,
    marker: PendingAltMarker,
    released_at_ms: u64,
) {
    if released_at_ms.saturating_sub(marker.marker_time_ms) >= HELD_ALT_THRESHOLD.as_millis() as u64
    {
        if let Ok(mut active) = active.lock() {
            if let Some(capture) = active.as_mut().filter(|capture| capture.id == id) {
                if let Some(stored_marker) = capture.markers.get_mut(marker.marker_index) {
                    stored_marker.held_until_ms = Some(released_at_ms);
                }
            }
        }
        dismiss_guide(marker.dismiss_path, Duration::ZERO);
    } else {
        dismiss_guide(
            marker.dismiss_path,
            TAP_GUIDE_DURATION.saturating_sub(marker.guide_started_at.elapsed()),
        );
    }
}

fn dismiss_guide(guide: Option<GuideDismiss>, after: Duration) {
    let Some(guide) = guide else {
        return;
    };
    std::thread::spawn(move || {
        if !after.is_zero() {
            std::thread::sleep(after);
        }
        match guide {
            GuideDismiss::File(path) => {
                let _ = fs::write(path, []);
            }
        }
    });
}

pub fn stop(app: &AppHandle, state: &CaptureState) -> Result<CompletedRecording, String> {
    let mut active = state
        .active
        .lock()
        .map_err(|_| "Capture state lock failed.".to_owned())?
        .take()
        .ok_or_else(|| "No active recording.".to_owned())?;
    let capture_duration_ms = active.started_at.elapsed().as_millis() as u64;
    dismiss_guide(active.border_dismiss_path.clone(), Duration::ZERO);
    if let Some(collector) = active.portal_cursor_collector.take() {
        collector.stop()?;
    }
    emit_processing_progress(app, "Finalizing recording", 0.0);
    let video_stop_result = stop_video_recorder(active.recorder);
    let audio_stop_result = match (active.audio_recorder, active.audio_path.as_ref()) {
        (Some(audio_recorder), Some(audio_path)) => stop_audio_recorder(audio_recorder, audio_path),
        _ => Ok(()),
    };
    let webcam_path = match active.webcam_recorder.take() {
        Some(webcam_recorder) => match webcam::stop(webcam_recorder) {
            Ok(path) => Some(path),
            Err(error) => {
                emit_processing_progress(app, &format!("Webcam unavailable: {error}"), 4.0);
                None
            }
        },
        None => {
            if let Some(error) = active.webcam_start_warning.take() {
                eprintln!("PikScreen webcam capture was unavailable: {error}");
            }
            None
        }
    };
    video_stop_result?;
    audio_stop_result?;
    emit_processing_progress(app, "Preparing video", 5.0);
    if active.cursor_strategy.uses_synthetic_render() {
        if let Some(error) = active.cursor_sampling_error {
            return Err(format!(
                "Cursor sampling stopped: {error}. Source video: {}",
                active.source_path.display()
            ));
        }
    }
    if let Some(error) = active.click_sampling_error {
        eprintln!("PikScreen click effects were unavailable for this recording: {error}");
    }

    let markers = active.markers.clone();
    let cursor_samples = active.cursor_samples.clone();
    let click_samples = active.click_samples.clone();
    let settings = active.settings.clone();
    if active.cursor_strategy.uses_synthetic_render() && cursor_samples.is_empty() {
        return Err(format!(
            "No cursor samples were captured. Source video: {}",
            active.source_path.display()
        ));
    }
    let (width, height) = if active.backend.uses_portal_stream(active.source_kind) {
        source_video_dimensions(&active.source_path)?
    } else {
        (active.width, active.height)
    };
    let session = editor::create_session(
        &active.source_path,
        active.audio_path.as_deref(),
        webcam_path.as_deref(),
        width,
        height,
        capture_duration_ms,
        settings,
        markers,
        cursor_samples,
        click_samples,
        active.cursor_strategy.uses_synthetic_render(),
    )?;
    emit_processing_progress(app, "Recording ready", 100.0);
    for temporary in [
        Some(active.source_path.as_path()),
        active.audio_path.as_deref(),
        webcam_path.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if let Err(error) = fs::remove_file(temporary) {
            eprintln!(
                "PikScreen could not remove temporary capture {}: {error}",
                temporary.display()
            );
        }
    }
    Ok(CompletedRecording {
        source_path: session.source_path.display().to_string(),
        session_id: session.id,
    })
}

pub(crate) fn render_editor_session(
    app: &AppHandle,
    session: &mut editor::SessionManifest,
    changes: editor::EditorChanges,
) -> Result<(), String> {
    changes.validate(session.duration_ms)?;
    let segments = changes.segments();
    let audio_segments = changes.audio_segments();
    session.markers = changes.markers;
    session.trim_start_ms = changes.trim_start_ms;
    session.trim_end_ms = changes.trim_end_ms;
    session.segments = segments.clone();
    session.audio_segments = changes.audio_segments.clone();
    session.settings = changes.settings.validate()?;

    let events = build_zoom_events(&session.markers, session.settings.zoom_timing());
    let timeline = CursorTimeline::new(session.cursor_samples.clone());
    let next_path = session.directory.join("render-next.mp4");
    let _ = fs::remove_file(&next_path);
    render_final_video(
        app,
        &session.source_path,
        session.audio_path.as_ref(),
        session.webcam_path.as_ref(),
        &next_path,
        timeline,
        &events,
        &session.click_samples,
        session.width,
        session.height,
        session.duration_ms,
        &segments,
        &audio_segments,
        if session.synthetic_cursor {
            CursorStrategy::SyntheticTahoe
        } else {
            CursorStrategy::Embedded
        },
        session.settings.clone(),
    )?;
    let _ = fs::remove_file(&session.working_path);
    fs::rename(&next_path, &session.working_path)
        .map_err(|error| format!("Could not finalize the editor MP4: {error}"))?;
    editor::save(session)
}

pub fn abort_all(state: &CaptureState) {
    let _ = cancel_preview(state);
    let active = state
        .active
        .lock()
        .ok()
        .and_then(|mut active| active.take());
    let Some(mut active) = active else {
        return;
    };
    dismiss_guide(active.border_dismiss_path, Duration::ZERO);
    if let Some(collector) = active.portal_cursor_collector.take() {
        let _ = collector.stop();
    }
    let _ = stop_video_recorder(active.recorder);
    if let Some(audio_recorder) = active.audio_recorder {
        let _ = stop_audio_recorder(audio_recorder, &active.audio_path.unwrap_or_default());
    }
    if let Some(webcam_recorder) = active.webcam_recorder {
        let _ = webcam::stop(webcam_recorder);
    }
}

fn stop_child(recorder: Child, log: &ChildLog) -> Result<(), String> {
    if unsafe { libc::kill(recorder.id() as i32, libc::SIGINT) } != 0
        && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    {
        return Err(format!(
            "Could not stop the cursorless recorder: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut recorder = recorder;
    let status = recorder
        .wait()
        .map_err(|error| format!("Could not finalize recording: {error}"))?;
    if status.success() || status.signal() == Some(libc::SIGINT) {
        Ok(())
    } else {
        Err(format!(
            "Cursorless wf-recorder exited unsuccessfully: {} [recorder: {}]",
            status,
            log.snapshot().trim()
        ))
    }
}

fn ensure_video_recorder_started(recorder: &mut VideoRecorder) -> Result<(), String> {
    match recorder {
        VideoRecorder::Portal(recorder) => window_pipewire::ensure_recorder_started(recorder),
        VideoRecorder::WfRecorder(recorder, log) => {
            std::thread::sleep(Duration::from_millis(200));
            let Some(status) = recorder
                .try_wait()
                .map_err(|error| format!("Could not inspect the cursorless recorder: {error}"))?
            else {
                return Ok(());
            };
            Err(format!(
                "Cursorless recorder failed during startup with {status}: {}",
                log.snapshot().trim()
            ))
        }
    }
}

fn stop_video_recorder(recorder: VideoRecorder) -> Result<(), String> {
    match recorder {
        VideoRecorder::WfRecorder(recorder, log) => stop_child(recorder, &log),
        VideoRecorder::Portal(recorder) => window_pipewire::stop_recorder(recorder),
    }
}

fn stop_audio_recorder(recorder: Child, audio_path: &PathBuf) -> Result<(), String> {
    if unsafe { libc::kill(recorder.id() as i32, libc::SIGINT) } != 0
        && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    {
        return Err(format!(
            "Could not stop audio capture: {}",
            std::io::Error::last_os_error()
        ));
    }
    let output = recorder
        .wait_with_output()
        .map_err(|error| format!("Could not finalize audio capture: {error}"))?;
    if audio_path.is_file()
        && std::fs::metadata(audio_path)
            .map(|metadata| metadata.len() > 0)
            .unwrap_or(false)
    {
        return Ok(());
    }
    Err(format!(
        "System + microphone audio capture did not produce an audio file: {} [{}]",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

fn temporary_path(kind: &str) -> PathBuf {
    temporary_file(kind, "mp4")
}

fn temporary_file(kind: &str, extension: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_nanos();
    let sequence = TEMPORARY_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("pikscreen-{kind}-{nanos}-{sequence}.{extension}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn niri_socket_selects_the_niri_backend() {
        assert_eq!(
            capture_backend_from_environment(
                Some(OsStr::new("/run/user/1000/niri.sock")),
                Some(OsStr::new("wayland")),
                Some(OsStr::new("wayland-1")),
            ),
            Ok(CaptureBackend::Niri)
        );
    }

    #[test]
    fn non_niri_wayland_selects_the_portal_backend() {
        assert_eq!(
            capture_backend_from_environment(
                None,
                Some(OsStr::new("wayland")),
                Some(OsStr::new("wayland-0")),
            ),
            Ok(CaptureBackend::Portal)
        );
        assert_eq!(
            capture_backend_from_environment(None, None, Some(OsStr::new("wayland-0"))),
            Ok(CaptureBackend::Portal)
        );
    }

    #[test]
    fn portal_stream_size_prefers_only_valid_metadata() {
        assert_eq!(
            valid_portal_stream_size(Some((1920, 1080))),
            Some((1920, 1080))
        );
        assert_eq!(valid_portal_stream_size(None), None);
        assert_eq!(valid_portal_stream_size(Some((1, 1))), None);
        assert_eq!(valid_portal_stream_size(Some((0, 1080))), None);
    }

    #[test]
    fn portal_sources_use_negotiated_pipewire_dimensions() {
        assert_eq!(
            portal_stream_size_authority(CaptureBackend::Niri, CaptureSourceKind::Monitor),
            PortalStreamSizeAuthority::Metadata
        );
        assert_eq!(
            portal_stream_size_authority(CaptureBackend::Niri, CaptureSourceKind::Window),
            PortalStreamSizeAuthority::NegotiatedPipeWire
        );
        assert_eq!(
            portal_stream_size_authority(CaptureBackend::Portal, CaptureSourceKind::Monitor),
            PortalStreamSizeAuthority::NegotiatedPipeWire
        );
        assert_eq!(
            portal_stream_size_authority(CaptureBackend::Portal, CaptureSourceKind::Window),
            PortalStreamSizeAuthority::NegotiatedPipeWire
        );
    }

    #[test]
    fn portable_backend_rejects_non_wayland_sessions() {
        assert!(
            capture_backend_from_environment(None, Some(OsStr::new("x11")), None)
                .expect_err("X11 must not enter the Wayland portal path")
                .contains("Wayland")
        );
    }

    #[test]
    fn screen_cast_v1_uses_implicit_hidden_cursor_mode() {
        assert_eq!(
            legacy_screen_cast_cursor_modes(1),
            Some(CursorMode::Hidden.into())
        );
        assert_eq!(
            requested_screen_cast_cursor_mode(1, CursorStrategy::HyprlandIpc),
            None
        );
    }

    #[test]
    fn screen_cast_v2_keeps_explicit_cursor_mode_selection() {
        assert_eq!(legacy_screen_cast_cursor_modes(2), None);
        assert_eq!(
            requested_screen_cast_cursor_mode(2, CursorStrategy::PortalMetadata),
            Some(CursorMode::Metadata)
        );
        assert_eq!(legacy_screen_cast_cursor_modes(6), None);
    }

    #[test]
    fn backend_selects_the_matching_cursor_and_recorder_strategy() {
        let all_modes = CursorMode::Hidden | CursorMode::Embedded | CursorMode::Metadata;
        assert_eq!(
            CaptureBackend::Niri
                .cursor_strategy(all_modes, false, false)
                .expect("Niri hidden cursor mode"),
            CursorStrategy::SyntheticTahoe
        );
        assert_eq!(
            CursorStrategy::SyntheticTahoe.portal_mode(),
            CursorMode::Hidden
        );
        assert!(!CaptureBackend::Niri.uses_portal_stream(CaptureSourceKind::Monitor));
        assert!(CaptureBackend::Niri.uses_portal_stream(CaptureSourceKind::Window));

        assert_eq!(
            CaptureBackend::Portal
                .cursor_strategy(all_modes, false, false)
                .expect("portable metadata cursor mode"),
            CursorStrategy::PortalMetadata
        );
        assert_eq!(
            CursorStrategy::PortalMetadata.portal_mode(),
            CursorMode::Metadata
        );
        assert_eq!(
            CaptureBackend::Portal
                .cursor_strategy(CursorMode::Embedded.into(), false, false)
                .expect("portable embedded cursor fallback"),
            CursorStrategy::Embedded
        );
        assert_eq!(CursorStrategy::Embedded.portal_mode(), CursorMode::Embedded);
        assert!(CursorStrategy::SyntheticTahoe.uses_synthetic_render());
        assert!(CursorStrategy::PortalMetadata.uses_synthetic_render());
        assert_eq!(
            CaptureBackend::Portal
                .cursor_strategy(CursorMode::Hidden | CursorMode::Embedded, true, false)
                .expect("Hyprland hidden cursor strategy"),
            CursorStrategy::HyprlandIpc
        );
        assert_eq!(
            CursorStrategy::HyprlandIpc.portal_mode(),
            CursorMode::Hidden
        );
        assert!(CursorStrategy::HyprlandIpc.uses_synthetic_render());
        assert_eq!(
            CaptureBackend::Portal
                .cursor_strategy(all_modes, false, true)
                .expect("GNOME Shell hidden cursor strategy"),
            CursorStrategy::GnomeShell
        );
        assert_eq!(CursorStrategy::GnomeShell.portal_mode(), CursorMode::Hidden);
        assert!(CursorStrategy::GnomeShell.uses_synthetic_render());
        assert!(!CursorStrategy::Embedded.uses_synthetic_render());
        assert!(CaptureBackend::Portal.uses_portal_stream(CaptureSourceKind::Monitor));
        assert!(CaptureBackend::Portal.uses_portal_stream(CaptureSourceKind::Window));
        assert_eq!(
            CaptureBackend::Portal.pipeline_profiles(),
            [
                WindowPipelineProfile::MemFdX264,
                WindowPipelineProfile::DmaBufGlVaapi,
            ]
        );
        assert_eq!(
            CaptureBackend::Niri.pipeline_profiles(),
            WindowPipelineProfile::ALL
        );
    }

    #[test]
    fn portal_backend_never_applies_the_niri_bottom_dock_crop() {
        let crop = capture_crop_for_backend(
            CaptureBackend::Portal,
            CaptureSourceKind::Monitor,
            0,
            0,
            1_920,
            1_080,
            PreviewSettings {
                hide_bottom_bar: true,
                bar_height_correction: 120,
            },
        )
        .expect("portable monitor crop should remain valid");
        assert_eq!(crop.height, 1_080);
        assert_eq!(crop.excluded_bottom, 0);
    }

    #[test]
    fn portal_marker_uses_the_latest_visible_normalized_cursor_sample() {
        assert_eq!(
            portal_marker_from_sample(
                Some(CursorSample {
                    time_ms: 90,
                    x: 0.25,
                    y: 0.75,
                    kind: TahoeCursor::Text,
                }),
                125,
                true,
            ),
            Some(Marker {
                time_ms: 125,
                x: 0.25,
                y: 0.75,
                target_scale: None,
                held_until_ms: None,
                hide_cursor: true,
            })
        );
    }

    #[test]
    fn portal_marker_rejects_missing_or_hidden_cursor_samples() {
        assert_eq!(portal_marker_from_sample(None, 125, false), None);
        assert_eq!(
            portal_marker_from_sample(
                Some(CursorSample {
                    time_ms: 100,
                    x: 0.4,
                    y: 0.6,
                    kind: TahoeCursor::Hidden,
                }),
                125,
                false,
            ),
            None
        );
    }

    #[test]
    fn recording_countdown_targets_the_source_monitor_for_three_seconds() {
        assert_eq!(
            recording_countdown_args(960, 540),
            [
                "--countdown",
                "--x",
                "960",
                "--y",
                "540",
                "--duration-ms",
                "3000",
            ]
        );
    }

    #[test]
    fn bottom_dock_crop_preserves_source_origin_and_removes_only_bottom_pixels() {
        let crop = capture_crop(
            CaptureSourceKind::Monitor,
            1_920,
            0,
            1_920,
            1_080,
            PreviewSettings {
                hide_bottom_bar: true,
                bar_height_correction: 25,
            },
        )
        .expect("crop should be valid");
        let detected = detect_quickshell_bottom_bar_height();
        assert_eq!(crop.x, 1_920);
        assert_eq!(crop.y, 0);
        assert_eq!(crop.width, 1_920);
        let requested = (detected + 25).max(0);
        let expected_excluded = if (1_080 - requested) % 2 == 0 {
            requested
        } else {
            requested + 1
        };
        assert_eq!(crop.excluded_bottom, expected_excluded);
        assert_eq!(crop.height, 1_080 - crop.excluded_bottom);
    }

    #[test]
    fn bottom_dock_crop_can_be_disabled_and_never_erases_the_source() {
        let full = capture_crop(
            CaptureSourceKind::Monitor,
            0,
            0,
            1_920,
            1_080,
            PreviewSettings {
                hide_bottom_bar: false,
                bar_height_correction: 160,
            },
        )
        .expect("full crop should be valid");
        assert_eq!(full.height, 1_080);
        assert_eq!(full.excluded_bottom, 0);

        let clamped = capture_crop(
            CaptureSourceKind::Monitor,
            0,
            0,
            1_920,
            200,
            PreviewSettings {
                hide_bottom_bar: true,
                bar_height_correction: 480,
            },
        )
        .expect("crop should clamp instead of creating a zero-sized source");
        assert_eq!(clamped.height, 120);
    }

    #[test]
    fn window_capture_ignores_the_bottom_dock_settings() {
        let crop = capture_crop(
            CaptureSourceKind::Window,
            42,
            84,
            1_921,
            1_081,
            PreviewSettings {
                hide_bottom_bar: true,
                bar_height_correction: 160,
            },
        )
        .expect("window crop should be valid");
        assert_eq!(crop.x, 42);
        assert_eq!(crop.y, 84);
        assert_eq!(crop.width, 1_920);
        assert_eq!(crop.height, 1_080);
        assert_eq!(crop.excluded_bottom, 0);
    }

    #[test]
    fn window_capture_rejects_the_portal_placeholder_size() {
        let error = capture_crop(
            CaptureSourceKind::Window,
            0,
            0,
            1,
            1,
            PreviewSettings {
                hide_bottom_bar: false,
                bar_height_correction: 0,
            },
        )
        .expect_err("a 1x1 portal placeholder must never reach GStreamer");
        assert!(error.contains("1x1 placeholder"));
    }

    #[test]
    fn recording_settings_accept_the_supported_high_frame_rates() {
        let settings = RecordingSettings {
            background_enabled: true,
            fps: 180,
            quality: VideoQuality::Ultra,
            background: BackgroundStyle::SonomaClouds,
            custom_background_path: None,
            cursor_visible: true,
            custom_cursor_path: None,
            cursor_smoothing: 100,
            cursor_size: 100,
            zoom_scale: 1.8,
            click_effects_enabled: true,
            audio: AudioSettings {
                system_enabled: true,
                microphone_enabled: true,
                system_volume: 1.0,
                microphone_volume: 1.0,
                system_device: None,
                microphone_device: None,
            },
            webcam: WebcamSettings::default(),
        }
        .validate()
        .expect("180 FPS should be a supported profile");
        assert_eq!(settings.quality.encoder_profile(), ("16", "slow"));
        assert_eq!(
            settings.quality.realtime_source_encoder_profile(),
            ("16", "ultrafast")
        );
        assert_eq!(
            settings.quality.final_encoder_profile(),
            ("16", "ultrafast")
        );
        assert_eq!(settings.cursor_smoothing, 100);
        assert_eq!(settings.cursor_size, 100);
        assert_eq!(settings.zoom_scale, 1.8);
    }

    #[test]
    fn recording_settings_default_cursor_smoothing_preserves_old_payloads() {
        let settings: RecordingSettings = serde_json::from_str(
            r#"{
                "fps": 120,
                "quality": "high",
                "background": "tahoeDark",
                "audio": {
                    "systemEnabled": true,
                    "microphoneEnabled": true,
                    "systemVolume": 1.0,
                    "microphoneVolume": 1.0,
                    "systemDevice": null,
                    "microphoneDevice": null
                }
            }"#,
        )
        .expect("old settings payload should deserialize");
        assert!(settings.cursor_visible);
        assert_eq!(settings.cursor_smoothing, 35);
        assert_eq!(settings.cursor_size, 100);
        assert!(settings.click_effects_enabled);
    }

    #[test]
    fn final_export_keeps_high_quality_crf_without_a_slow_preset() {
        assert_eq!(
            VideoQuality::High.final_encoder_profile(),
            ("18", "ultrafast")
        );
        assert_eq!(
            VideoQuality::Balanced.final_encoder_profile(),
            ("20", "ultrafast")
        );
    }

    #[test]
    fn portal_capture_uses_a_high_quality_realtime_intermediate() {
        assert_eq!(
            VideoQuality::Balanced.portal_intermediate_profile(),
            (14, "veryfast", 100_000)
        );
        assert_eq!(
            VideoQuality::High.portal_intermediate_profile(),
            (12, "veryfast", 100_000)
        );
        assert_eq!(
            VideoQuality::Ultra.portal_intermediate_profile(),
            (10, "veryfast", 100_000)
        );
    }

    #[test]
    fn recording_settings_reject_an_unbounded_fps_value() {
        assert!(RecordingSettings {
            background_enabled: true,
            fps: 144,
            quality: VideoQuality::High,
            background: BackgroundStyle::TahoeDark,
            custom_background_path: None,
            cursor_visible: true,
            custom_cursor_path: None,
            cursor_smoothing: 35,
            cursor_size: 100,
            zoom_scale: 1.8,
            click_effects_enabled: true,
            audio: AudioSettings {
                system_enabled: true,
                microphone_enabled: true,
                system_volume: 1.0,
                microphone_volume: 1.0,
                system_device: None,
                microphone_device: None,
            },
            webcam: WebcamSettings::default(),
        }
        .validate()
        .is_err());
    }

    #[test]
    fn recording_settings_reject_cursor_smoothing_above_one_hundred() {
        let error = RecordingSettings {
            background_enabled: true,
            fps: 120,
            quality: VideoQuality::High,
            background: BackgroundStyle::TahoeDark,
            custom_background_path: None,
            cursor_visible: true,
            custom_cursor_path: None,
            cursor_smoothing: 101,
            cursor_size: 100,
            zoom_scale: 1.8,
            click_effects_enabled: true,
            audio: AudioSettings {
                system_enabled: true,
                microphone_enabled: true,
                system_volume: 1.0,
                microphone_volume: 1.0,
                system_device: None,
                microphone_device: None,
            },
            webcam: WebcamSettings::default(),
        }
        .validate()
        .expect_err("out-of-range cursor smoothing should be rejected");
        assert_eq!(error, "Cursor smoothing must be between 0 and 100.");
    }

    #[test]
    fn recording_settings_reject_cursor_sizes_outside_the_supported_range() {
        let error = RecordingSettings {
            background_enabled: true,
            fps: 120,
            quality: VideoQuality::High,
            background: BackgroundStyle::TahoeDark,
            custom_background_path: None,
            cursor_visible: true,
            custom_cursor_path: None,
            cursor_smoothing: 35,
            cursor_size: 151,
            zoom_scale: 1.8,
            click_effects_enabled: true,
            audio: AudioSettings {
                system_enabled: true,
                microphone_enabled: true,
                system_volume: 1.0,
                microphone_volume: 1.0,
                system_device: None,
                microphone_device: None,
            },
            webcam: WebcamSettings::default(),
        }
        .validate()
        .expect_err("oversized cursors should be rejected before rendering");
        assert_eq!(error, "Cursor size must be between 60 and 150 percent.");
    }

    #[test]
    fn recording_settings_reject_zoom_amounts_outside_the_supported_range() {
        let error = RecordingSettings {
            background_enabled: true,
            fps: 120,
            quality: VideoQuality::High,
            background: BackgroundStyle::TahoeDark,
            custom_background_path: None,
            cursor_visible: true,
            custom_cursor_path: None,
            cursor_smoothing: 35,
            cursor_size: 100,
            zoom_scale: 2.5,
            click_effects_enabled: true,
            audio: AudioSettings {
                system_enabled: true,
                microphone_enabled: true,
                system_volume: 1.0,
                microphone_volume: 1.0,
                system_device: None,
                microphone_device: None,
            },
            webcam: WebcamSettings::default(),
        }
        .validate()
        .expect_err("out-of-range zoom amount should be rejected");
        assert_eq!(error, "Zoom amount must be between 1.2× and 2.4×.");
    }

    #[test]
    fn audio_settings_reject_an_out_of_range_level() {
        assert!(AudioSettings {
            system_enabled: true,
            microphone_enabled: true,
            system_volume: 1.01,
            microphone_volume: 1.0,
            system_device: None,
            microphone_device: None,
        }
        .validate()
        .is_err());
    }
}
