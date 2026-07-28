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
    io::{BufRead, BufReader, BufWriter, Read, Write},
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
    window_pipewire::{self, WindowPipelineProfile, WindowRecorder},
    zoom::{
        build_zoom_events, camera_transform_at, CameraTransform, Marker, RecordlyCameraSpring,
        ZoomTiming,
    },
};

const RECORDLY_TAHOE_CURSOR_HEIGHT: f64 = 80.0;
const RECORDLY_TAHOE_CURSOR_HOTSPOT_X: f64 = 0.14;
const RECORDLY_TAHOE_CURSOR_HOTSPOT_Y: f64 = 0.06;
const TAP_GUIDE_DURATION: Duration = Duration::from_secs(3);
const HELD_ALT_THRESHOLD: Duration = Duration::from_millis(220);
const ALT_REPEAT_GAP: Duration = Duration::from_millis(45);
// A guide must always self-expire even if a keyboard event is lost.
const GUIDE_MAX_DURATION_MS: &str = "3000";
const RECORDING_COUNTDOWN_MS: &str = "3000";
// Some of Tahoe's stateful cursors (notably NotAllowed) are wider than the
// pointer asset once they are scaled for the recording.  Keep a generous,
// fixed transparent tile so every cursor state can use the same raw-video
// stream without clipping or changing the overlay coordinate system.
const CURSOR_TILE_WIDTH: u32 = 256;
const CURSOR_TILE_HEIGHT: u32 = 256;
const CURSOR_TILE_MARGIN: i32 = 64;
const RECORDLY_CURSOR_MOTION_BLUR: f64 = 0.4;
const RECORDLY_CURSOR_MOTION_BLUR_MULTIPLIER: f64 = 0.08;
const RECORDLY_CURSOR_MOTION_BLUR_THRESHOLD: f64 = 0.05;
const CLICK_PULSE_DURATION_MS: u64 = 360;
const CLICK_TILE_SIZE: u32 = 160;
const PORTAL_CLICK_DEDUP_MS: u64 = 16;
const PORTAL_INTERMEDIATE_MAX_BITRATE_KBPS: u32 = 100_000;
const KWIN_GUIDE_EXCLUSION_PLUGIN: &str = "dev.pikyusufaslan.pikscreen.guide-exclusion.v1";
const KWIN_GUIDE_EXCLUSION_SCRIPT: &str = include_str!("../../tools/pikscreen-kwin-hide-guides.js");
static TEMPORARY_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn trace_portal_selection(stage: &str) {
    let _ = fs::write("/tmp/pikscreen-portal-selection.trace", stage);
}

#[derive(Clone, Debug, Serialize)]
struct ProcessingProgress {
    stage: String,
    percent: u8,
}

#[derive(Clone, Copy, Debug)]
struct CursorTileFrame {
    time_ms: u64,
    origin_x: i32,
    origin_y: i32,
    fractional_x: f64,
    fractional_y: f64,
    visible: bool,
    kind: TahoeCursor,
}

impl CursorTileFrame {
    fn position(self) -> (f64, f64) {
        (
            self.origin_x as f64 + CURSOR_TILE_MARGIN as f64 + self.fractional_x,
            self.origin_y as f64 + CURSOR_TILE_MARGIN as f64 + self.fractional_y,
        )
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct CursorMotionBlur {
    velocity_x: f64,
    velocity_y: f64,
    kernel_size: usize,
    offset: f64,
}

#[derive(Clone, Copy, Debug)]
struct TahoeCursorMetrics {
    aspect_ratio: f64,
    hotspot_x: f64,
    hotspot_y: f64,
}

#[derive(Debug)]
struct CursorTrack {
    frames: Vec<CursorTileFrame>,
}

#[derive(Debug)]
struct ClickEffectTrack {
    path: PathBuf,
    commands_path: PathBuf,
    start_ms: u64,
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
    WfRecorder(Child),
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

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioDevice {
    id: String,
    label: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioDevices {
    system_outputs: Vec<AudioDevice>,
    microphones: Vec<AudioDevice>,
    default_system_output: Option<String>,
    default_microphone: Option<String>,
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
    let portal = Screencast::new()
        .await
        .map_err(|error| {
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
        VideoRecorder::WfRecorder(spawn_cursorless_recorder(
            crop.x,
            crop.y,
            crop.width,
            crop.height,
            &source_path,
            settings.clone(),
        )?)
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

/// Keep a portal-provided window rectangle inside one Niri output.
///
/// Niri window decorations can extend a few pixels beyond the output edge.
/// `grim` can screenshot that global rectangle, but wf-recorder intentionally
/// rejects regions which overlap multiple outputs. Picking the output with the
/// largest overlap keeps preview and recording on the same fixed visible area.
fn clamp_window_crop_to_niri_output(crop: CaptureCrop) -> CaptureCrop {
    let output = match Command::new("niri").args(["msg", "-j", "outputs"]).output() {
        Ok(output) if output.status.success() => output,
        _ => return crop,
    };
    let Ok(layout) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return crop;
    };
    clamp_window_crop_to_output_layout(crop, &layout)
}

fn clamp_window_crop_to_output_layout(
    crop: CaptureCrop,
    layout: &serde_json::Value,
) -> CaptureCrop {
    let Some(outputs) = layout.as_object() else {
        return crop;
    };
    let crop_left = crop.x as i64;
    let crop_top = crop.y as i64;
    let crop_right = crop_left + crop.width as i64;
    let crop_bottom = crop_top + crop.height as i64;
    let mut best: Option<(i64, CaptureCrop)> = None;

    for output in outputs.values() {
        let Some(logical) = output.get("logical") else {
            continue;
        };
        let Some(output_x) = logical.get("x").and_then(serde_json::Value::as_i64) else {
            continue;
        };
        let Some(output_y) = logical.get("y").and_then(serde_json::Value::as_i64) else {
            continue;
        };
        let Some(output_width) = logical.get("width").and_then(serde_json::Value::as_i64) else {
            continue;
        };
        let Some(output_height) = logical.get("height").and_then(serde_json::Value::as_i64) else {
            continue;
        };
        let left = crop_left.max(output_x);
        let top = crop_top.max(output_y);
        let right = crop_right.min(output_x + output_width);
        let bottom = crop_bottom.min(output_y + output_height);
        let width = right - left;
        let height = bottom - top;
        if width < 2 || height < 2 {
            continue;
        }
        let area = width * height;
        if best
            .as_ref()
            .is_some_and(|(best_area, _)| *best_area >= area)
        {
            continue;
        }
        let width = width - width.rem_euclid(2);
        let height = height - height.rem_euclid(2);
        best = Some((
            area,
            CaptureCrop {
                x: left as i32,
                y: top as i32,
                width: width as i32,
                height: height as i32,
                excluded_bottom: 0,
            },
        ));
    }

    best.map(|(_, crop)| crop).unwrap_or(crop)
}

fn pikscreen_niri_binary() -> PathBuf {
    if let Some(binary) = env::var_os("PIKSCREEN_NIRI_BIN") {
        let binary = PathBuf::from(binary);
        if binary.is_file() {
            return binary;
        }
    }
    let bundled =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../niri-cursor-ipc/target/debug/niri");
    if bundled.is_file() {
        return bundled;
    }
    PathBuf::from("niri")
}

fn selected_niri_window_id(node_id: u32) -> Result<u64, String> {
    let mut last_error = None;
    for _ in 0..40 {
        let output = Command::new(pikscreen_niri_binary())
            .args(["msg", "-j", "casts"])
            .output();
        match output {
            Ok(output) if output.status.success() => {
                match serde_json::from_slice::<serde_json::Value>(&output.stdout) {
                    Ok(casts) => {
                        if let Some(window_id) = niri_window_id_for_node(&casts, node_id) {
                            return Ok(window_id);
                        }
                    }
                    Err(error) => last_error = Some(format!("invalid casts JSON: {error}")),
                }
            }
            Ok(output) => {
                last_error = Some(String::from_utf8_lossy(&output.stderr).trim().to_owned())
            }
            Err(error) => last_error = Some(error.to_string()),
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(format!(
        "Niri never exposed a window cast matching PipeWire node {node_id}.{}",
        last_error
            .filter(|error| !error.is_empty())
            .map(|error| format!(" Last error: {error}"))
            .unwrap_or_default(),
    ))
}

fn niri_window_id_for_node(casts: &serde_json::Value, node_id: u32) -> Option<u64> {
    casts.as_array()?.iter().find_map(|cast| {
        (cast.get("pw_node_id").and_then(serde_json::Value::as_u64) == Some(node_id as u64))
            .then(|| cast.get("target")?.get("Window")?.get("id")?.as_u64())
            .flatten()
    })
}

fn focus_niri_window(window_id: u64) -> Result<(), String> {
    let output = Command::new(pikscreen_niri_binary())
        .args([
            "msg",
            "action",
            "focus-window",
            "--id",
            &window_id.to_string(),
        ])
        .output()
        .map_err(|error| format!("Could not ask Niri to focus window {window_id}: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr);
    let detail = detail.trim();
    Err(format!(
        "Niri could not focus selected window {window_id}: {}",
        if detail.is_empty() {
            output.status.to_string()
        } else {
            detail.to_owned()
        }
    ))
}

fn current_niri_window_geometry(window_id: u64) -> Option<CaptureCrop> {
    let output = Command::new(pikscreen_niri_binary())
        .args(["msg", "-j", "window-geometry", &window_id.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let geometry = serde_json::from_slice::<serde_json::Value>(&output.stdout).ok()?;
    Some(CaptureCrop {
        x: geometry.get("x")?.as_i64()? as i32,
        y: geometry.get("y")?.as_i64()? as i32,
        width: geometry.get("width")?.as_i64()? as i32,
        height: geometry.get("height")?.as_i64()? as i32,
        excluded_bottom: 0,
    })
}

fn settled_niri_window_geometry(window_id: u64) -> Option<CaptureCrop> {
    let mut previous = None;
    let mut stable_samples = 0;
    for _ in 0..20 {
        let geometry = current_niri_window_geometry(window_id);
        if geometry.is_some() && geometry == previous {
            stable_samples += 1;
            if stable_samples >= 2 {
                return geometry;
            }
        } else {
            stable_samples = 0;
            previous = geometry;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    previous
}

fn detect_quickshell_bottom_bar_height() -> i32 {
    let config_root = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    let Some(config_root) = config_root else {
        return 0;
    };
    let config_path = config_root.join("inir/config.json");
    let Ok(contents) = fs::read_to_string(config_path) else {
        return 0;
    };
    let Ok(config) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return 0;
    };
    let Some(dock) = config.get("dock") else {
        return 0;
    };
    let enabled = dock.get("enable").and_then(serde_json::Value::as_bool) == Some(true);
    let bottom = dock.get("position").and_then(serde_json::Value::as_str) == Some("bottom");
    if !enabled || !bottom {
        return 0;
    }
    let height = dock
        .get("height")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(60)
        .clamp(24, 240) as i32;
    // Inir's Dock.qml adds the elevation and outer gap to the configured dock
    // height, so crop the whole visible bottom panel rather than only its icons.
    height + 15
}

struct NiriCursorClient {
    stream: BufReader<UnixStream>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct NiriCursorState {
    position: (f64, f64),
    kind: TahoeCursor,
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClickSample {
    pub(crate) time_ms: u64,
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) primary: bool,
}

#[derive(Clone, Debug)]
struct NiriClickEvents {
    now_ms: u64,
    events: Vec<NiriClickEvent>,
}

#[derive(Clone, Copy, Debug)]
struct NiriClickEvent {
    time_ms: u64,
    position: (f64, f64),
    primary: bool,
}

impl NiriCursorClient {
    fn connect() -> Result<Self, String> {
        let socket = env::var_os("NIRI_SOCKET").ok_or_else(|| {
            "NIRI_SOCKET is unavailable; launch PikScreen from the active Niri session.".to_owned()
        })?;
        let stream = UnixStream::connect(socket)
            .map_err(|error| format!("Could not connect to Niri cursor IPC: {error}"))?;
        stream
            .set_read_timeout(Some(Duration::from_millis(250)))
            .map_err(|error| format!("Could not configure Niri cursor IPC: {error}"))?;
        Ok(Self {
            stream: BufReader::new(stream),
        })
    }

    fn request(&mut self, request: &[u8]) -> Result<String, String> {
        self.stream
            .get_mut()
            .write_all(request)
            .map_err(|error| format!("Could not request Niri cursor state: {error}"))?;
        self.stream
            .get_mut()
            .flush()
            .map_err(|error| format!("Could not flush the Niri cursor request: {error}"))?;
        let mut line = String::new();
        self.stream
            .read_line(&mut line)
            .map_err(|error| format!("Could not read the Niri cursor state: {error}"))?;
        Ok(line)
    }

    fn position(&mut self) -> Result<(f64, f64), String> {
        parse_niri_cursor_reply(&self.request(b"\"CursorPosition\"\n")?)
    }

    fn state(&mut self) -> Result<NiriCursorState, String> {
        let reply = self.request(b"\"CursorState\"\n")?;
        match parse_niri_cursor_state_reply(&reply) {
            Ok(state) => Ok(state),
            // Keep pre-CursorState Niri sessions recordable while the user
            // builds and activates the updated compositor binary.
            Err(_) => Ok(NiriCursorState {
                position: self.position()?,
                kind: TahoeCursor::Arrow,
            }),
        }
    }

    fn click_events(&mut self, since_ms: u64) -> Result<NiriClickEvents, String> {
        let request = format!("{{\"ClickEvents\":{{\"since_ms\":{since_ms}}}}}\n");
        parse_niri_click_events_reply(&self.request(request.as_bytes())?)
    }

    fn window_geometry(&mut self, id: u64) -> Result<Option<CaptureCrop>, String> {
        let request = format!("{{\"WindowGeometry\":{{\"id\":{id}}}}}\n");
        parse_niri_window_geometry_reply(&self.request(request.as_bytes())?)
    }
}

fn sample_niri_click_clock(cursor_client: &mut NiriCursorClient) -> Result<(u64, Instant), String> {
    let request_started_at = Instant::now();
    let now_ms = cursor_client.click_events(0)?.now_ms;
    let round_trip = request_started_at.elapsed();
    Ok((now_ms, request_started_at + round_trip / 2))
}

fn translate_niri_epoch(
    niri_now_ms: u64,
    sampled_at: Instant,
    recording_started_at: Instant,
) -> u64 {
    niri_now_ms.saturating_add(
        recording_started_at
            .saturating_duration_since(sampled_at)
            .as_millis() as u64,
    )
}

fn parse_niri_cursor_reply(reply: &str) -> Result<(f64, f64), String> {
    let value: serde_json::Value = serde_json::from_str(reply)
        .map_err(|error| format!("Niri returned invalid cursor IPC JSON: {error}"))?;
    let position = value
        .pointer("/Ok/CursorPosition")
        .and_then(serde_json::Value::as_array)
        .filter(|position| position.len() == 2)
        .ok_or_else(|| format!("Niri rejected the cursor position request: {value}"))?;
    let x = position[0]
        .as_f64()
        .ok_or_else(|| "Niri returned a non-numeric cursor X coordinate.".to_owned())?;
    let y = position[1]
        .as_f64()
        .ok_or_else(|| "Niri returned a non-numeric cursor Y coordinate.".to_owned())?;
    Ok((x, y))
}

fn parse_niri_cursor_state_reply(reply: &str) -> Result<NiriCursorState, String> {
    let value: serde_json::Value = serde_json::from_str(reply)
        .map_err(|error| format!("Niri returned invalid cursor-state IPC JSON: {error}"))?;
    let state = value
        .pointer("/Ok/CursorState")
        .ok_or_else(|| format!("Niri rejected the cursor state request: {value}"))?;
    let position = state
        .get("position")
        .and_then(serde_json::Value::as_array)
        .filter(|position| position.len() == 2)
        .ok_or_else(|| "Niri cursor state did not include a two-axis position.".to_owned())?;
    let x = position[0]
        .as_f64()
        .ok_or_else(|| "Niri cursor state X is not numeric.".to_owned())?;
    let y = position[1]
        .as_f64()
        .ok_or_else(|| "Niri cursor state Y is not numeric.".to_owned())?;
    let kind = match state.get("image") {
        Some(serde_json::Value::Object(image)) => image
            .get("Named")
            .and_then(serde_json::Value::as_str)
            .map(tahoe_cursor_for_niri_name)
            .unwrap_or(TahoeCursor::Arrow),
        Some(serde_json::Value::String(image)) if image == "Hidden" => TahoeCursor::Hidden,
        _ => TahoeCursor::Arrow,
    };
    Ok(NiriCursorState {
        position: (x, y),
        kind,
    })
}

fn parse_niri_window_geometry_reply(reply: &str) -> Result<Option<CaptureCrop>, String> {
    let value: serde_json::Value = serde_json::from_str(reply)
        .map_err(|error| format!("Niri returned invalid window-geometry IPC JSON: {error}"))?;
    let geometry = value
        .pointer("/Ok/WindowGeometry")
        .ok_or_else(|| format!("Niri rejected the window geometry request: {value}"))?;
    if geometry.is_null() {
        return Ok(None);
    }
    let number = |name: &str| {
        geometry
            .get(name)
            .and_then(serde_json::Value::as_i64)
            .and_then(|value| i32::try_from(value).ok())
            .ok_or_else(|| format!("Niri window geometry has an invalid {name}: {geometry}"))
    };
    let crop = CaptureCrop {
        x: number("x")?,
        y: number("y")?,
        width: number("width")?,
        height: number("height")?,
        excluded_bottom: 0,
    };
    if crop.width < 1 || crop.height < 1 {
        return Err(format!(
            "Niri returned an empty window geometry: {geometry}"
        ));
    }
    Ok(Some(crop))
}

fn parse_niri_click_events_reply(reply: &str) -> Result<NiriClickEvents, String> {
    let value: serde_json::Value = serde_json::from_str(reply)
        .map_err(|error| format!("Niri returned invalid click-event IPC JSON: {error}"))?;
    let response = value
        .pointer("/Ok/ClickEvents")
        .ok_or_else(|| format!("Niri rejected the click-event request: {value}"))?;
    let now_ms = response
        .get("now_ms")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| "Niri click events did not include now_ms.".to_owned())?;
    let events = response
        .get("events")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "Niri click events did not include an event array.".to_owned())?
        .iter()
        .filter_map(|event| {
            let time_ms = event.get("time_ms")?.as_u64()?;
            let position = event.get("position")?.as_array()?;
            let x = position.first()?.as_f64()?;
            let y = position.get(1)?.as_f64()?;
            let primary = match event.get("button")?.as_str()? {
                "Primary" => true,
                "Secondary" => false,
                _ => return None,
            };
            Some(NiriClickEvent {
                time_ms,
                position: (x, y),
                primary,
            })
        })
        .collect();
    Ok(NiriClickEvents { now_ms, events })
}

fn listen_for_cursor_samples(
    active: Arc<Mutex<Option<ActiveCapture>>>,
    id: u64,
    mut cursor_client: NiriCursorClient,
    output_x: i32,
    output_y: i32,
    width: i32,
    height: i32,
    window_id: Option<u64>,
    fps: u32,
    click_epoch_ms: Option<u64>,
) {
    std::thread::spawn(move || {
        let mut last_click_timestamp = click_epoch_ms;
        let mut next_click_poll = Instant::now();
        let monitor_geometry = CaptureCrop {
            x: output_x,
            y: output_y,
            width,
            height,
            excluded_bottom: 0,
        };
        let mut window_geometry = window_id.is_none().then_some(monitor_geometry);
        let mut next_geometry_poll = Instant::now();
        let mut last_visible_position = (0.5, 0.5);
        loop {
            if !capture_is_active(&active, id) {
                return;
            }
            if let Some(window_id) = window_id {
                if Instant::now() >= next_geometry_poll {
                    match cursor_client.window_geometry(window_id) {
                        Ok(geometry) => window_geometry = geometry,
                        Err(error) => {
                            if let Ok(mut active) = active.lock() {
                                if let Some(capture) =
                                    active.as_mut().filter(|capture| capture.id == id)
                                {
                                    capture.cursor_sampling_error = Some(error);
                                }
                            }
                            return;
                        }
                    }
                    next_geometry_poll = Instant::now() + Duration::from_millis(32);
                }
            }
            match cursor_client.state() {
                Ok(cursor) => {
                    let Ok(mut active) = active.lock() else {
                        return;
                    };
                    let Some(capture) = active.as_mut().filter(|capture| capture.id == id) else {
                        return;
                    };
                    let time_ms = capture.started_at.elapsed().as_millis() as u64;
                    let sample = cursor_sample_for_geometry(
                        cursor,
                        window_geometry,
                        last_visible_position,
                        time_ms,
                    );
                    if sample.kind != TahoeCursor::Hidden {
                        last_visible_position = (sample.x, sample.y);
                    }
                    capture.cursor_samples.push(sample);
                }
                Err(error) => {
                    if let Ok(mut active) = active.lock() {
                        if let Some(capture) = active.as_mut().filter(|capture| capture.id == id) {
                            capture.cursor_sampling_error = Some(error);
                        }
                    }
                    return;
                }
            }
            if last_click_timestamp.is_some() && Instant::now() >= next_click_poll {
                let since_ms = last_click_timestamp.unwrap_or_default();
                match cursor_client.click_events(since_ms) {
                    Ok(clicks) => {
                        last_click_timestamp = Some(clicks.now_ms);
                        if let Ok(mut active) = active.lock() {
                            if let Some(capture) =
                                active.as_mut().filter(|capture| capture.id == id)
                            {
                                let epoch_ms = click_epoch_ms.unwrap_or(clicks.now_ms);
                                for click in clicks.events {
                                    if let Some(sample) = window_geometry.and_then(|geometry| {
                                        click_sample(
                                            click,
                                            epoch_ms,
                                            geometry.x,
                                            geometry.y,
                                            geometry.width,
                                            geometry.height,
                                        )
                                    }) {
                                        capture.click_samples.push(sample);
                                    }
                                }
                            }
                        }
                    }
                    Err(error) => {
                        if let Ok(mut active) = active.lock() {
                            if let Some(capture) =
                                active.as_mut().filter(|capture| capture.id == id)
                            {
                                capture.click_sampling_error = Some(error);
                            }
                        }
                        last_click_timestamp = None;
                    }
                }
                next_click_poll = Instant::now() + Duration::from_millis(40);
            }
            std::thread::sleep(Duration::from_millis(1_000 / fps as u64));
        }
    });
}

fn listen_for_hyprland_input_bridge(
    app: AppHandle,
    active: Arc<Mutex<Option<ActiveCapture>>>,
    id: u64,
    bridge: HyprlandInputBridge,
    geometry: CaptureCrop,
    recording_epoch_ns: u64,
) {
    std::thread::spawn(move || {
        let mut last_visible_position = (0.5, 0.5);
        let mut pending_marker: Option<PendingAltMarker> = None;
        let mut last_marker_time_ms: Option<u64> = None;
        loop {
            if !capture_is_active(&active, id) {
                if let Some(marker) = pending_marker.take() {
                    dismiss_guide(marker.dismiss_path, Duration::ZERO);
                }
                return;
            }
            match bridge.recv() {
                Ok(packet) => {
                    let time_ms = packet.recording_time_ms(recording_epoch_ns);
                    let sample =
                        hyprland_input_sample(&packet, geometry, last_visible_position, time_ms);
                    if sample.kind != TahoeCursor::Hidden {
                        last_visible_position = (sample.x, sample.y);
                    }
                    let click = (packet.kind == EVENT_MOUSE_BUTTON && packet.pressed)
                        .then(|| hyprland_click_sample(&packet, geometry, time_ms))
                        .flatten();
                    {
                        let Ok(mut active) = active.lock() else {
                            return;
                        };
                        let Some(capture) = active.as_mut().filter(|capture| capture.id == id)
                        else {
                            return;
                        };
                        push_cursor_sample(&mut capture.cursor_samples, sample);
                        if let Some(click) = click {
                            capture.click_samples.push(click);
                        }
                    }

                    if packet.kind != EVENT_KEYBOARD_KEY
                        || !matches!(packet.code, KEY_LEFTALT | KEY_RIGHTALT)
                    {
                        continue;
                    }
                    if packet.pressed {
                        if pending_marker.is_some()
                            || last_marker_time_ms.is_some_and(|previous| {
                                time_ms.saturating_sub(previous) < ALT_REPEAT_GAP.as_millis() as u64
                            })
                        {
                            continue;
                        }
                        last_marker_time_ms = Some(time_ms);
                        pending_marker =
                            add_hyprland_marker(&app, &active, id, &packet, geometry, time_ms);
                    } else if let Some(marker) = pending_marker.take() {
                        finish_alt_marker_at(&active, id, marker, time_ms);
                    }
                }
                Err(BridgeReceiveError::Timeout) => continue,
                Err(BridgeReceiveError::Invalid(error)) => {
                    if let Ok(mut active) = active.lock() {
                        if let Some(capture) = active.as_mut().filter(|capture| capture.id == id) {
                            capture.cursor_sampling_error = Some(error);
                        }
                    }
                    return;
                }
                Err(BridgeReceiveError::Io(error)) => {
                    if let Ok(mut active) = active.lock() {
                        if let Some(capture) = active.as_mut().filter(|capture| capture.id == id) {
                            capture.cursor_sampling_error =
                                Some(format!("Hyprland input bridge disconnected: {error}"));
                        }
                    }
                    return;
                }
            }
        }
    });
}

fn push_cursor_sample(samples: &mut Vec<CursorSample>, sample: CursorSample) {
    if let Some(previous) = samples
        .last_mut()
        .filter(|previous| previous.time_ms == sample.time_ms)
    {
        *previous = sample;
    } else {
        samples.push(sample);
    }
}

fn hyprland_click_sample(
    packet: &HyprlandInputPacket,
    geometry: CaptureCrop,
    time_ms: u64,
) -> Option<ClickSample> {
    let primary = match packet.code {
        BTN_LEFT => true,
        BTN_RIGHT => false,
        _ => return None,
    };
    let x = (packet.position.0 - geometry.x as f64) / geometry.width as f64;
    let y = (packet.position.1 - geometry.y as f64) / geometry.height as f64;
    ((0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y)).then_some(ClickSample {
        time_ms,
        x,
        y,
        primary,
    })
}

fn listen_for_portal_cursor_events(
    active: Arc<Mutex<Option<ActiveCapture>>>,
    id: u64,
    events: std::sync::mpsc::Receiver<PortalCursorEvent>,
) {
    std::thread::spawn(move || {
        let classifier = PortalCursorClassifier::from_environment();
        eprintln!(
            "PikScreen loaded {} cursor images from XCursor theme '{}'.",
            classifier.candidate_count(),
            classifier.theme_name(),
        );
        let mut cursor_kinds = HashMap::new();
        let mut last_visible_position = (0.5, 0.5);
        let mut last_visible_kind = TahoeCursor::Arrow;
        let mut reported_stream = false;
        loop {
            if !capture_is_active(&active, id) {
                return;
            }
            match events.recv_timeout(Duration::from_millis(100)) {
                Ok(PortalCursorEvent::Observation(observation)) => {
                    if let Some(bitmap) = observation.bitmap.as_ref() {
                        match classifier.classify(
                            bitmap,
                            observation.hotspot_x,
                            observation.hotspot_y,
                        ) {
                            Some(matched) => {
                                cursor_kinds.insert(observation.cursor_id, matched.kind);
                                last_visible_kind = matched.kind;
                                eprintln!(
                                    "PikScreen matched PipeWire cursor id {} to {} / {:?} (score {:.4}).",
                                    observation.cursor_id,
                                    matched.name,
                                    matched.kind,
                                    matched.score,
                                );
                            }
                            None => {
                                cursor_kinds.insert(observation.cursor_id, TahoeCursor::Arrow);
                                last_visible_kind = TahoeCursor::Arrow;
                                eprintln!(
                                    "PikScreen could not classify PipeWire cursor id {} ({}x{}, format {}); using Tahoe Arrow.",
                                    observation.cursor_id,
                                    bitmap.width,
                                    bitmap.height,
                                    bitmap.format,
                                );
                            }
                        }
                    }
                    let kind = cursor_kinds
                        .get(&observation.cursor_id)
                        .copied()
                        .unwrap_or(last_visible_kind);
                    let Ok(mut active) = active.lock() else {
                        return;
                    };
                    let Some(capture) = active.as_mut().filter(|capture| capture.id == id) else {
                        return;
                    };
                    let time_ms = capture.started_at.elapsed().as_millis() as u64;
                    let sample =
                        portal_cursor_sample(&observation, last_visible_position, kind, time_ms);
                    if sample.kind != TahoeCursor::Hidden {
                        last_visible_position = (sample.x, sample.y);
                        last_visible_kind = sample.kind;
                    }
                    capture.cursor_samples.push(sample);
                    if !reported_stream {
                        eprintln!(
                            "PikScreen PipeWire cursor metadata active: stream {}x{}, cursor id {}, bitmap {:?}x{:?}, hotspot ({}, {}).",
                            observation.stream_width,
                            observation.stream_height,
                            observation.cursor_id,
                            observation.bitmap_width,
                            observation.bitmap_height,
                            observation.hotspot_x,
                            observation.hotspot_y,
                        );
                        reported_stream = true;
                    }
                }
                Ok(PortalCursorEvent::Error(error)) => {
                    if let Ok(mut active) = active.lock() {
                        if let Some(capture) = active.as_mut().filter(|capture| capture.id == id) {
                            capture.cursor_sampling_error = Some(error);
                        }
                    }
                    return;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    if let Ok(mut active) = active.lock() {
                        if let Some(capture) = active.as_mut().filter(|capture| capture.id == id) {
                            capture.cursor_sampling_error = Some(
                                "PipeWire cursor metadata stream disconnected unexpectedly."
                                    .to_owned(),
                            );
                        }
                    }
                    return;
                }
            }
        }
    });
}

fn portal_cursor_sample(
    observation: &PortalCursorObservation,
    last_visible_position: (f64, f64),
    kind: TahoeCursor,
    time_ms: u64,
) -> CursorSample {
    if !observation.is_inside_stream()
        || observation.stream_width == 0
        || observation.stream_height == 0
    {
        return hidden_cursor_sample(last_visible_position, time_ms);
    }
    CursorSample {
        time_ms,
        x: (observation.x as f64 / observation.stream_width as f64).clamp(0.0, 1.0),
        y: (observation.y as f64 / observation.stream_height as f64).clamp(0.0, 1.0),
        kind,
    }
}

fn gnome_cursor_sample(
    (x, y): (i32, i32),
    geometry: CaptureCrop,
    last_visible_position: (f64, f64),
    time_ms: u64,
) -> CursorSample {
    let position = (f64::from(x), f64::from(y));
    if !point_is_inside(position, geometry) {
        return hidden_cursor_sample(last_visible_position, time_ms);
    }
    cursor_sample(
        position,
        TahoeCursor::Arrow,
        geometry.x,
        geometry.y,
        geometry.width,
        geometry.height,
        time_ms,
    )
}

fn listen_for_gnome_cursor_samples(
    active: Arc<Mutex<Option<ActiveCapture>>>,
    id: u64,
    geometry: CaptureCrop,
) {
    std::thread::spawn(move || {
        let mut last_visible_position = (0.5, 0.5);
        loop {
            let time_ms = {
                let Ok(active) = active.lock() else {
                    return;
                };
                let Some(capture) = active.as_ref().filter(|capture| capture.id == id) else {
                    return;
                };
                capture.started_at.elapsed().as_millis() as u64
            };
            let sample = match gnome_guides::pointer() {
                Ok(position) => {
                    gnome_cursor_sample(position, geometry, last_visible_position, time_ms)
                }
                Err(error) => {
                    if let Ok(mut active) = active.lock() {
                        if let Some(capture) = active.as_mut().filter(|capture| capture.id == id) {
                            capture.cursor_sampling_error = Some(error);
                        }
                    }
                    return;
                }
            };
            if sample.kind != TahoeCursor::Hidden {
                last_visible_position = (sample.x, sample.y);
            }
            {
                let Ok(mut active) = active.lock() else {
                    return;
                };
                let Some(capture) = active.as_mut().filter(|capture| capture.id == id) else {
                    return;
                };
                push_cursor_sample(&mut capture.cursor_samples, sample);
            }
            std::thread::sleep(Duration::from_millis(16));
        }
    });
}

fn listen_for_portal_clicks(active: Arc<Mutex<Option<ActiveCapture>>>, id: u64) {
    std::thread::spawn(move || {
        let mut devices = pointer_button_devices();
        let mut next_rescan = Instant::now() + Duration::from_millis(500);
        let mut last_click: Option<(u64, bool)> = None;
        loop {
            if !capture_is_active(&active, id) {
                return;
            }
            if devices.is_empty() && Instant::now() >= next_rescan {
                devices = pointer_button_devices();
                next_rescan = Instant::now() + Duration::from_millis(500);
            }
            for device in &mut devices {
                let Ok(events) = device.fetch_events() else {
                    continue;
                };
                for event in events {
                    let Some(primary) = portal_click_button(&event) else {
                        continue;
                    };
                    let Ok(mut active) = active.lock() else {
                        return;
                    };
                    let Some(capture) = active.as_mut().filter(|capture| capture.id == id) else {
                        return;
                    };
                    let time_ms = capture.started_at.elapsed().as_millis() as u64;
                    if portal_click_is_duplicate(last_click, time_ms, primary) {
                        continue;
                    }
                    last_click = Some((time_ms, primary));
                    capture.click_samples.push(ClickSample {
                        time_ms,
                        x: 0.5,
                        y: 0.5,
                        primary,
                    });
                }
            }
            std::thread::sleep(Duration::from_millis(8));
        }
    });
}

fn pointer_button_devices() -> Vec<Device> {
    let Ok(entries) = std::fs::read_dir("/dev/input") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("event"))
        .filter_map(|entry| Device::open(entry.path()).ok())
        .filter(|device| {
            device.supported_keys().is_some_and(|keys| {
                keys.contains(KeyCode::BTN_LEFT) || keys.contains(KeyCode::BTN_RIGHT)
            })
        })
        .filter_map(|device| device.set_nonblocking(true).ok().map(|_| device))
        .collect()
}

fn portal_click_button(event: &evdev::InputEvent) -> Option<bool> {
    if event.event_type() != EventType::KEY || event.value() != 1 {
        return None;
    }
    match KeyCode::new(event.code()) {
        KeyCode::BTN_LEFT => Some(true),
        KeyCode::BTN_RIGHT => Some(false),
        _ => None,
    }
}

fn portal_click_is_duplicate(previous: Option<(u64, bool)>, time_ms: u64, primary: bool) -> bool {
    previous.is_some_and(|(previous_time_ms, previous_primary)| {
        previous_primary == primary
            && time_ms.saturating_sub(previous_time_ms) <= PORTAL_CLICK_DEDUP_MS
    })
}

fn cursor_sample_for_geometry(
    cursor: NiriCursorState,
    geometry: Option<CaptureCrop>,
    last_visible_position: (f64, f64),
    time_ms: u64,
) -> CursorSample {
    let Some(geometry) = geometry else {
        return hidden_cursor_sample(last_visible_position, time_ms);
    };
    if !point_is_inside(cursor.position, geometry) {
        return hidden_cursor_sample(last_visible_position, time_ms);
    }
    cursor_sample(
        cursor.position,
        cursor.kind,
        geometry.x,
        geometry.y,
        geometry.width,
        geometry.height,
        time_ms,
    )
}

fn hyprland_input_sample(
    packet: &HyprlandInputPacket,
    geometry: CaptureCrop,
    last_visible_position: (f64, f64),
    time_ms: u64,
) -> CursorSample {
    if !point_is_inside(packet.position, geometry) {
        return hidden_cursor_sample(last_visible_position, time_ms);
    }
    cursor_sample(
        packet.position,
        TahoeCursor::from_standard_name(&packet.shape),
        geometry.x,
        geometry.y,
        geometry.width,
        geometry.height,
        time_ms,
    )
}

fn hidden_cursor_sample(position: (f64, f64), time_ms: u64) -> CursorSample {
    CursorSample {
        time_ms,
        x: position.0,
        y: position.1,
        kind: TahoeCursor::Hidden,
    }
}

fn point_is_inside((x, y): (f64, f64), geometry: CaptureCrop) -> bool {
    x >= geometry.x as f64
        && y >= geometry.y as f64
        && x < (geometry.x + geometry.width) as f64
        && y < (geometry.y + geometry.height) as f64
}

fn cursor_sample(
    (cursor_x, cursor_y): (f64, f64),
    kind: TahoeCursor,
    output_x: i32,
    output_y: i32,
    width: i32,
    height: i32,
    time_ms: u64,
) -> CursorSample {
    CursorSample {
        time_ms,
        x: ((cursor_x - output_x as f64) / width as f64).clamp(0.0, 1.0),
        y: ((cursor_y - output_y as f64) / height as f64).clamp(0.0, 1.0),
        kind,
    }
}

fn click_sample(
    click: NiriClickEvent,
    epoch_ms: u64,
    output_x: i32,
    output_y: i32,
    width: i32,
    height: i32,
) -> Option<ClickSample> {
    let x = (click.position.0 - output_x as f64) / width as f64;
    let y = (click.position.1 - output_y as f64) / height as f64;
    ((0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y)).then_some(ClickSample {
        time_ms: click.time_ms.saturating_sub(epoch_ms),
        x,
        y,
        primary: click.primary,
    })
}

fn spawn_cursorless_recorder(
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    source_path: &PathBuf,
    settings: RecordingSettings,
) -> Result<Child, String> {
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
}

fn spawn_audio_recorder(audio_path: &PathBuf, audio: AudioSettings) -> Result<Child, String> {
    let mut command = Command::new("ffmpeg");
    command.args(["-hide_banner", "-loglevel", "error", "-y"]);
    if audio.system_enabled {
        let system_source = audio
            .system_device
            .as_deref()
            .map(monitor_source_from_sink)
            .unwrap_or(default_system_audio_source()?);
        command.args([
            "-thread_queue_size",
            "1024",
            "-f",
            "pulse",
            "-i",
            &system_source,
        ]);
    }
    if audio.microphone_enabled {
        let microphone_source = audio
            .microphone_device
            .clone()
            .unwrap_or(pactl_default("source")?);
        command.args([
            "-thread_queue_size",
            "1024",
            "-f",
            "pulse",
            "-i",
            &microphone_source,
        ]);
    }
    command
        .args([
            "-filter_complex",
            &audio_mix_filter(&audio),
            "-map",
            "[mixed_audio]",
            "-ar",
            "48000",
            "-c:a",
            "aac",
            "-b:a",
            "192k",
            audio_path.to_string_lossy().as_ref(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("Could not start {} capture: {error}", audio.label()))
}

pub fn audio_devices() -> Result<AudioDevices, String> {
    let system_outputs = pactl_devices("sinks")?;
    let microphones = pactl_devices("sources")?
        .into_iter()
        .filter(|device| !device.id.ends_with(".monitor"))
        .collect();
    Ok(AudioDevices {
        system_outputs,
        microphones,
        default_system_output: pactl_default("sink").ok(),
        default_microphone: pactl_default("source").ok(),
    })
}

fn pactl_devices(kind: &str) -> Result<Vec<AudioDevice>, String> {
    let output = Command::new("pactl")
        .args(["list", "short", kind])
        .output()
        .map_err(|error| format!("Could not list audio {kind}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "Could not list audio {kind}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut columns = line.split('\t');
            let _index = columns.next()?;
            let id = columns.next()?.to_owned();
            Some(AudioDevice {
                label: id.clone(),
                id,
            })
        })
        .collect())
}

fn default_system_audio_source() -> Result<String, String> {
    Ok(monitor_source_from_sink(&pactl_default("sink")?))
}

fn monitor_source_from_sink(sink: &str) -> String {
    format!("{sink}.monitor")
}

fn pactl_default(kind: &str) -> Result<String, String> {
    let output = Command::new("pactl")
        .arg(format!("get-default-{kind}"))
        .output()
        .map_err(|error| format!("Could not run pactl to find the default {kind}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "Could not find the default {kind}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if value.is_empty() {
        return Err(format!("PulseAudio reported no default {kind}."));
    }
    Ok(value)
}

fn audio_mix_filter(audio: &AudioSettings) -> String {
    let mut filters = Vec::new();
    let mut labels = Vec::new();
    let mut input_index = 0;
    if audio.system_enabled {
        filters.push(format!(
            "[{input_index}:a]volume={:.3}[system]",
            audio.system_volume
        ));
        labels.push("[system]");
        input_index += 1;
    }
    if audio.microphone_enabled {
        filters.push(format!(
            "[{input_index}:a]volume={:.3}[microphone]",
            audio.microphone_volume
        ));
        labels.push("[microphone]");
    }
    format!(
        "{}{}{}amix=inputs={}:normalize=0[mixed_audio]",
        filters.join(";"),
        if filters.is_empty() { "" } else { ";" },
        labels.join(""),
        labels.len(),
    )
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

fn kde_session_from_environment(
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

fn ensure_kde_guide_capture_exclusion() -> Result<(), String> {
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
    session.markers = changes.markers;
    session.trim_start_ms = changes.trim_start_ms;
    session.trim_end_ms = changes.trim_end_ms;
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
        session.trim_start_ms,
        session.trim_end_ms,
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

fn source_video_dimensions(source_path: &PathBuf) -> Result<(u32, u32), String> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "csv=p=0:s=x",
            source_path.to_string_lossy().as_ref(),
        ])
        .output()
        .map_err(|error| format!("Could not inspect the recorded window stream: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "Could not inspect the recorded window stream: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let dimensions = String::from_utf8_lossy(&output.stdout);
    let (width, height) = dimensions
        .trim()
        .split_once('x')
        .ok_or_else(|| format!("ffprobe did not return window dimensions: {dimensions}"))?;
    let width = width
        .parse::<u32>()
        .map_err(|_| format!("ffprobe returned an invalid window width: {width}"))?;
    let height = height
        .parse::<u32>()
        .map_err(|_| format!("ffprobe returned an invalid window height: {height}"))?;
    if width < 2 || height < 2 {
        return Err(format!(
            "Recorded window stream has invalid dimensions: {width}x{height}"
        ));
    }
    Ok((width, height))
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

fn render_final_video(
    app: &AppHandle,
    source_path: &PathBuf,
    audio_path: Option<&PathBuf>,
    webcam_path: Option<&PathBuf>,
    final_path: &PathBuf,
    timeline: CursorTimeline,
    events: &[crate::zoom::ZoomEvent],
    click_samples: &[ClickSample],
    width: u32,
    height: u32,
    duration_ms: u64,
    trim_start_ms: u64,
    trim_end_ms: u64,
    cursor_strategy: CursorStrategy,
    settings: RecordingSettings,
) -> Result<(), String> {
    let (_, content_height) = recordly_padded_size(width, height);
    let frame_assets = recordly_frame_assets(
        settings.background,
        settings.custom_background_path.as_deref(),
        width,
        height,
    )?;
    let webcam_assets = webcam_path
        .map(|_| recordly_webcam_frame_assets(width, height, &settings.webcam))
        .transpose()?;
    let cursor_render = if settings.cursor_visible {
        match cursor_strategy {
            CursorStrategy::SyntheticTahoe
            | CursorStrategy::GnomeShell
            | CursorStrategy::PortalMetadata
            | CursorStrategy::HyprlandIpc => {
                let commands_path = temporary_file("cursor-commands", "txt");
                let cursor_track = build_cursor_track(
                    &timeline,
                    events,
                    width,
                    height,
                    duration_ms,
                    settings.fps,
                    settings.cursor_smoothing,
                    settings.cursor_size,
                )?;
                let cursor_paths = scaled_cursor_asset_paths(
                    &cursor_track,
                    content_height as f64 / height as f64 * settings.cursor_size as f64 / 100.0,
                    settings.custom_cursor_path.as_deref(),
                )?;
                write_cursor_commands(&commands_path, &cursor_track)?;
                Some((cursor_track, cursor_paths, commands_path))
            }
            CursorStrategy::Embedded => None,
        }
    } else {
        None
    };
    let click_tracks = if settings.click_effects_enabled {
        build_click_effect_tracks(
            click_samples,
            &timeline,
            events,
            width,
            height,
            duration_ms,
            settings.fps,
        )?
    } else {
        Vec::new()
    };
    let camera_commands_path = temporary_file("camera-commands", "txt");
    if !events.is_empty() {
        write_camera_commands(
            &camera_commands_path,
            events,
            width,
            height,
            duration_ms,
            settings.fps,
        )?;
    }
    let cursor_input_index = cursor_render.as_ref().map(|_| 1);
    let click_input_start = 1 + usize::from(cursor_input_index.is_some());
    let audio_input_index = click_input_start + click_tracks.len();
    let wallpaper_input_index = audio_input_index + usize::from(audio_path.is_some());
    let mask_input_index = wallpaper_input_index + 1;
    let shadow_input_index = wallpaper_input_index + 2;
    let webcam_input_index = wallpaper_input_index + 3;
    let webcam_mask_input_index = webcam_input_index + 1;
    let mut filter = recordly_card_filter(
        width,
        height,
        duration_ms,
        settings.fps,
        mask_input_index,
        shadow_input_index,
    );
    filter.push(';');
    filter.push_str(&wallpaper_background_filter(
        wallpaper_input_index,
        "card",
        "scene",
    ));
    match (&cursor_render, cursor_input_index) {
        (Some((_, _, commands_path)), Some(input_index)) => filter.push_str(&format!(
            ";[{input_index}:v]setpts=PTS-STARTPTS,sendcmd=f={}[cursor];[scene][cursor]overlay@cursor=x=0:y=0:eval=init:format=auto:shortest=1[scene0]",
            commands_path.display()
        )),
        (None, None) => filter.push_str(";[scene]null[scene0]"),
        _ => unreachable!("cursor render input and index must agree"),
    }
    let mut scene_label = "scene0".to_owned();
    for (index, track) in click_tracks.iter().enumerate() {
        let next_scene_label = format!("scene{}", index + 1);
        let input_index = click_input_start + index;
        filter.push_str(&format!(
            ";[{input_index}:v]setpts=PTS-STARTPTS+{:.6}/TB,sendcmd=f={}[click{index}];[{scene_label}][click{index}]overlay@click{index}=x=-1000:y=-1000:eval=init:format=auto:eof_action=pass:shortest=0[{next_scene_label}]",
            track.start_ms as f64 / 1_000.0,
            track.commands_path.display(),
        ));
        scene_label = next_scene_label;
    }
    if events.is_empty() {
        filter.push_str(&format!(";[{scene_label}]null[screen]"));
    } else {
        filter.push(';');
        filter.push_str(&camera_filter_chain(
            &scene_label,
            "screen",
            &camera_commands_path,
            width,
            height,
        ));
    }
    let mut stage_label = "screen".to_owned();
    if let Some(webcam_assets) = webcam_assets.as_ref() {
        filter.push_str(&webcam_overlay_filter(
            &stage_label,
            webcam_input_index,
            webcam_mask_input_index,
            webcam_assets.layout,
            settings.webcam.mirror,
            settings.webcam.shadow,
            duration_ms,
            settings.fps,
        ));
        stage_label = "stage_webcam".to_owned();
    }
    append_final_trim_filter(
        &mut filter,
        &stage_label,
        audio_path.is_some().then_some(audio_input_index),
        trim_start_ms,
        trim_end_ms,
    );
    let output_duration_ms = trim_end_ms.saturating_sub(trim_start_ms);
    let fps = settings.fps_argument();
    let cursor_video_size = format!("{CURSOR_TILE_WIDTH}x{CURSOR_TILE_HEIGHT}");
    let (crf, preset) = settings.quality.final_encoder_profile();
    let mut command = Command::new("ffmpeg");
    command.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-progress",
        "pipe:1",
        "-nostats",
        "-y",
        "-i",
        source_path.to_string_lossy().as_ref(),
    ]);
    if cursor_render.is_some() {
        command.args([
            "-f",
            "rawvideo",
            "-pixel_format",
            "rgba",
            "-video_size",
            &cursor_video_size,
            "-framerate",
            &fps,
            "-i",
            "pipe:0",
        ]);
    }
    let click_video_size = format!("{CLICK_TILE_SIZE}x{CLICK_TILE_SIZE}");
    for track in &click_tracks {
        command.args([
            "-f",
            "rawvideo",
            "-pixel_format",
            "rgba",
            "-video_size",
            &click_video_size,
            "-framerate",
            &fps,
            "-i",
            track.path.to_string_lossy().as_ref(),
        ]);
    }
    if let Some(audio_path) = audio_path {
        command.args(["-i", audio_path.to_string_lossy().as_ref()]);
    }
    command.args([
        "-loop",
        "1",
        "-framerate",
        &fps,
        "-i",
        frame_assets.wallpaper_path.to_string_lossy().as_ref(),
        "-loop",
        "1",
        "-framerate",
        &fps,
        "-i",
        frame_assets.mask_path.to_string_lossy().as_ref(),
        "-loop",
        "1",
        "-framerate",
        &fps,
        "-i",
        frame_assets.shadow_path.to_string_lossy().as_ref(),
    ]);
    if let (Some(webcam_path), Some(webcam_assets)) = (webcam_path, webcam_assets.as_ref()) {
        command.args(["-i", webcam_path.to_string_lossy().as_ref()]);
        command.args([
            "-loop",
            "1",
            "-framerate",
            &fps,
            "-i",
            webcam_assets.mask_path.to_string_lossy().as_ref(),
        ]);
    }
    command.args(["-filter_threads", &ffmpeg_filter_threads()]);
    command.args(["-filter_complex_threads", &ffmpeg_filter_threads()]);
    command.args(["-filter_complex", &filter, "-map", "[out]"]);
    if audio_path.is_some() {
        command.args(["-map", "[out_audio]", "-c:a", "aac", "-b:a", "192k"]);
    }
    command.args([
        "-movflags",
        "+faststart",
        "-shortest",
        "-t",
        &format!("{:.3}", output_duration_ms as f64 / 1_000.0),
    ]);
    command.args([
        "-c:v", "libx264", "-preset", preset, "-crf", crf, "-pix_fmt", "yuv420p",
    ]);
    command.arg(final_path);
    let render_started = Instant::now();
    let cursor_stream = cursor_render.map(|(track, paths, _)| (track, paths));
    let result = run_ffmpeg_with_progress(
        command,
        app,
        "Rendering final MP4",
        output_duration_ms,
        5.0,
        100.0,
        "FFmpeg final render",
        cursor_stream,
    );
    for track in &click_tracks {
        let _ = fs::remove_file(&track.path);
        let _ = fs::remove_file(&track.commands_path);
    }
    if let Some(webcam_assets) = webcam_assets {
        let _ = fs::remove_file(webcam_assets.mask_path);
    }
    if result.is_ok() {
        eprintln!(
            "PikScreen final render completed in {:.2}s for {:.2}s of {} FPS video.",
            render_started.elapsed().as_secs_f64(),
            output_duration_ms as f64 / 1_000.0,
            settings.fps,
        );
    }
    result
}

fn append_final_trim_filter(
    filter: &mut String,
    stage_label: &str,
    audio_input_index: Option<usize>,
    trim_start_ms: u64,
    trim_end_ms: u64,
) {
    let trim_start_seconds = trim_start_ms as f64 / 1_000.0;
    let trim_end_seconds = trim_end_ms as f64 / 1_000.0;
    filter.push_str(&format!(
        ";[{stage_label}]trim=start={trim_start_seconds:.6}:end={trim_end_seconds:.6},setpts=PTS-STARTPTS,format=yuv420p[out]"
    ));
    if let Some(audio_input_index) = audio_input_index {
        filter.push_str(&format!(
            ";[{audio_input_index}:a:0]atrim=start={trim_start_seconds:.6}:end={trim_end_seconds:.6},asetpts=PTS-STARTPTS[out_audio]"
        ));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WebcamLayout {
    size: u32,
    x: u32,
    y: u32,
    roundness: u32,
}

struct WebcamFrameAssets {
    layout: WebcamLayout,
    mask_path: PathBuf,
}

fn recordly_webcam_layout(width: u32, height: u32, webcam: &WebcamSettings) -> WebcamLayout {
    let min_dimension = width.min(height);
    let margin = webcam.margin as u32;
    let maximum_size = min_dimension
        .saturating_sub(margin.saturating_mul(2))
        .max(56);
    let requested_size = (min_dimension as f64 * webcam.size_percent as f64 / 100.0).round() as u32;
    let mut size = requested_size.clamp(56, maximum_size).max(2);
    if size % 2 != 0 {
        size = size.saturating_sub(1).max(2);
    }
    WebcamLayout {
        size,
        x: width.saturating_sub(size.saturating_add(margin)),
        y: height.saturating_sub(size.saturating_add(margin)),
        roundness: (webcam.roundness as u32).min(size / 2),
    }
}

fn recordly_webcam_frame_assets(
    width: u32,
    height: u32,
    webcam: &WebcamSettings,
) -> Result<WebcamFrameAssets, String> {
    let layout = recordly_webcam_layout(width, height, webcam);
    let mask_svg_path = temporary_file("recordly-webcam-mask", "svg");
    let mask_path = temporary_file("recordly-webcam-mask", "png");
    let path = recordly_squircle_path(
        layout.size as f64,
        layout.size as f64,
        layout.roundness as f64,
    );
    fs::write(
        &mask_svg_path,
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" viewBox=\"0 0 {} {}\"><path d=\"{}\" fill=\"white\"/></svg>",
            layout.size, layout.size, layout.size, layout.size, path
        ),
    )
    .map_err(|error| format!("Could not create Recordly webcam mask: {error}"))?;
    let result = rasterize_static_svg(&mask_svg_path, &mask_path, "Recordly webcam mask");
    let _ = fs::remove_file(&mask_svg_path);
    result?;
    Ok(WebcamFrameAssets { layout, mask_path })
}

fn webcam_overlay_filter(
    stage_label: &str,
    webcam_input_index: usize,
    webcam_mask_input_index: usize,
    layout: WebcamLayout,
    mirror: bool,
    shadow: f64,
    duration_ms: u64,
    fps: u32,
) -> String {
    let mirror = if mirror { "hflip," } else { "" };
    let extension = duration_ms as f64 / 1_000.0 + 1.0 / fps.max(1) as f64;
    let shadow_offset = (layout.size as f64 * 0.06).round() as u32;
    let blur = (layout.size as f64 * 0.08).round().clamp(2.0, 40.0) as u32;
    format!(
        ";[{webcam_input_index}:v]setpts=PTS-STARTPTS,fps={fps},tpad=stop_mode=clone:stop_duration={extension:.6},scale={size}:{size}:force_original_aspect_ratio=increase,crop={size}:{size},{mirror}format=yuv420p[webcam_video];[{webcam_mask_input_index}:v]format=gray,scale={size}:{size}[webcam_mask];[webcam_video][webcam_mask]alphamerge[webcam_alpha];[webcam_alpha]split=2[webcam_foreground][webcam_shadow_source];[webcam_shadow_source]colorchannelmixer=rr=0:gg=0:bb=0:aa={shadow:.3},boxblur={blur}:1[webcam_shadow];[{stage_label}][webcam_shadow]overlay=x={shadow_x}:y={shadow_y}:format=auto:eof_action=pass:shortest=0[stage_webcam_shadow];[stage_webcam_shadow][webcam_foreground]overlay=x={x}:y={y}:format=yuv420:eof_action=pass:shortest=0[stage_webcam]",
        size = layout.size,
        shadow_x = layout.x.saturating_add(shadow_offset),
        shadow_y = layout.y.saturating_add(shadow_offset),
        x = layout.x,
        y = layout.y,
    )
}

fn camera_filter_chain(
    input_label: &str,
    output_label: &str,
    commands_path: &PathBuf,
    width: u32,
    height: u32,
) -> String {
    format!(
        "[{input_label}]sendcmd=f={},crop@camera=w={width}:h={height}:x=0:y=0:exact=1,scale={width}:{height}:flags=lanczos,format=rgba[{output_label}]",
        commands_path.display()
    )
}

fn emit_processing_progress(app: &AppHandle, stage: &str, percent: f64) {
    let _ = app.emit(
        "processing-progress",
        ProcessingProgress {
            stage: stage.to_owned(),
            percent: percent.round().clamp(0.0, 100.0) as u8,
        },
    );
}

fn run_ffmpeg_with_progress(
    mut command: Command,
    app: &AppHandle,
    stage: &str,
    duration_ms: u64,
    progress_start: f64,
    progress_end: f64,
    operation: &str,
    cursor_stream: Option<(CursorTrack, HashMap<TahoeCursor, PathBuf>)>,
) -> Result<(), String> {
    emit_processing_progress(app, stage, progress_start);
    if cursor_stream.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("Could not run {operation}: {error}"))?;
    let cursor_writer = match cursor_stream {
        Some((cursor_track, cursor_paths)) => {
            let stdin = child
                .stdin
                .take()
                .ok_or_else(|| format!("Could not stream cursor frames to {operation}."))?;
            Some(std::thread::spawn(move || {
                cursor_track.write_raw_frames(&cursor_paths, stdin)
            }))
        }
        None => None,
    };
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("Could not read {operation} progress."))?;
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        let Some(value) = line
            .strip_prefix("out_time_us=")
            .or_else(|| line.strip_prefix("out_time_ms="))
        else {
            continue;
        };
        let Ok(output_time_us) = value.parse::<u64>() else {
            continue;
        };
        let ratio = output_time_us as f64 / (duration_ms.max(1) as f64 * 1_000.0);
        emit_processing_progress(
            app,
            stage,
            progress_start + (progress_end - progress_start) * ratio.clamp(0.0, 1.0),
        );
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("Could not finalize {operation}: {error}"))?;
    let cursor_writer_result = cursor_writer
        .map(|writer| {
            writer
                .join()
                .map_err(|_| "Cursor frame streaming thread panicked.".to_owned())
        })
        .transpose()?;
    if !output.status.success() {
        return Err(format!(
            "{operation} failed: {} [{}]",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    if let Some(result) = cursor_writer_result {
        result?;
    }
    emit_processing_progress(app, stage, progress_end);
    Ok(())
}

fn build_cursor_track(
    timeline: &CursorTimeline,
    events: &[crate::zoom::ZoomEvent],
    width: u32,
    height: u32,
    duration_ms: u64,
    fps: u32,
    cursor_smoothing: u8,
    cursor_size: u8,
) -> Result<CursorTrack, String> {
    let frame_count = duration_ms.saturating_mul(fps as u64) / 1_000 + 2;
    let mut frames = Vec::with_capacity(frame_count as usize);
    let mut cursor_metrics = HashMap::new();
    let mut smoother =
        (cursor_smoothing > 0).then(|| CursorSmoother::with_amount(cursor_smoothing));
    let mut previous_time_ms = 0;
    for frame_index in 0..frame_count {
        let time_ms = frame_index.saturating_mul(1_000) / fps as u64;
        let target = timeline
            .at(time_ms)
            .ok_or_else(|| "Cursor timeline unexpectedly became empty.".to_owned())?;
        let rendered_target = match smoother.as_mut() {
            Some(smoother) => smoother.step(target, time_ms.saturating_sub(previous_time_ms)),
            None => target,
        };
        previous_time_ms = time_ms;
        let kind = timeline.kind_at(time_ms).unwrap_or(TahoeCursor::Arrow);
        if cursor_is_hidden(events, time_ms) || kind == TahoeCursor::Hidden {
            frames.push(CursorTileFrame {
                time_ms,
                origin_x: -1_000,
                origin_y: -1_000,
                fractional_x: 0.0,
                fractional_y: 0.0,
                visible: false,
                kind,
            });
        } else {
            let metrics = match cursor_metrics.get(&kind) {
                Some(metrics) => *metrics,
                None => {
                    let metrics = tahoe_cursor_metrics(kind)?;
                    cursor_metrics.insert(kind, metrics);
                    metrics
                }
            };
            let (x, y) = recordly_stage_cursor_position(
                rendered_target,
                metrics,
                width,
                height,
                cursor_size as f64 / 100.0,
            );
            let base_x = x.floor();
            let base_y = y.floor();
            frames.push(CursorTileFrame {
                time_ms,
                origin_x: base_x as i32 - CURSOR_TILE_MARGIN,
                origin_y: base_y as i32 - CURSOR_TILE_MARGIN,
                fractional_x: x - base_x,
                fractional_y: y - base_y,
                visible: true,
                kind,
            });
        }
    }
    Ok(CursorTrack { frames })
}

impl CursorTrack {
    fn write_raw_frames(
        &self,
        cursor_paths: &HashMap<TahoeCursor, PathBuf>,
        mut sink: impl Write,
    ) -> Result<(), String> {
        let mut cursors = HashMap::new();
        for kind in self
            .frames
            .iter()
            .filter(|frame| frame.visible)
            .map(|frame| frame.kind)
        {
            if cursors.contains_key(&kind) {
                continue;
            }
            let path = cursor_paths
                .get(&kind)
                .ok_or_else(|| format!("Cursor asset path is missing for {kind:?}."))?;
            let cursor = image::open(path)
                .map_err(|error| format!("Could not load cursor sprite for {kind:?}: {error}"))?
                .to_rgba8();
            let required_width = cursor
                .width()
                .saturating_add((CURSOR_TILE_MARGIN * 2 + 1) as u32);
            let required_height = cursor
                .height()
                .saturating_add((CURSOR_TILE_MARGIN * 2 + 1) as u32);
            if required_width > CURSOR_TILE_WIDTH || required_height > CURSOR_TILE_HEIGHT {
                return Err(format!(
                    "Cursor sprite {kind:?} ({}x{}) does not fit inside the {}x{} subpixel tile.",
                    cursor.width(),
                    cursor.height(),
                    CURSOR_TILE_WIDTH,
                    CURSOR_TILE_HEIGHT,
                ));
            }
            cursors.insert(kind, cursor);
        }
        let mut previous_frame = None;
        for frame in &self.frames {
            let motion_blur = previous_frame
                .map(|previous| recordly_cursor_motion_blur(previous, *frame))
                .unwrap_or_default();
            let tile = if frame.visible {
                let cursor = cursors
                    .get(&frame.kind)
                    .expect("visible cursor track frame must have a loaded asset");
                rasterize_cursor_tile(cursor, *frame, motion_blur)
            } else {
                rasterize_cursor_tile(&image::RgbaImage::new(0, 0), *frame, motion_blur)
            };
            sink.write_all(&tile)
                .map_err(|error| format!("Could not stream subpixel cursor frame: {error}"))?;
            previous_frame = Some(*frame);
        }
        sink.flush()
            .map_err(|error| format!("Could not finalize subpixel cursor stream: {error}"))
    }
}

fn build_click_effect_tracks(
    samples: &[ClickSample],
    timeline: &CursorTimeline,
    events: &[crate::zoom::ZoomEvent],
    width: u32,
    height: u32,
    duration_ms: u64,
    fps: u32,
) -> Result<Vec<ClickEffectTrack>, String> {
    let mut tracks = Vec::new();
    for sample in samples
        .iter()
        .copied()
        .filter(|sample| sample.time_ms < duration_ms)
        .filter_map(|sample| cursor_locked_click_sample(sample, timeline, events))
    {
        let path = temporary_file("click-pulse", "rgba");
        let commands_path = temporary_file("click-commands", "txt");
        write_click_effect_frames(&path, sample.primary, fps)?;
        write_click_effect_commands(&commands_path, tracks.len(), sample, width, height, fps)?;
        tracks.push(ClickEffectTrack {
            path,
            commands_path,
            start_ms: sample.time_ms,
        });
    }
    Ok(tracks)
}

fn cursor_locked_click_sample(
    mut sample: ClickSample,
    timeline: &CursorTimeline,
    events: &[crate::zoom::ZoomEvent],
) -> Option<ClickSample> {
    if cursor_is_hidden(events, sample.time_ms)
        || timeline.kind_at(sample.time_ms)? == TahoeCursor::Hidden
    {
        return None;
    }
    let point = timeline.at(sample.time_ms)?;
    sample.x = point.x;
    sample.y = point.y;
    Some(sample)
}

fn write_click_effect_frames(path: &PathBuf, primary: bool, fps: u32) -> Result<(), String> {
    let file = File::create(path)
        .map_err(|error| format!("Could not create click-effect frame stream: {error}"))?;
    let mut file = BufWriter::new(file);
    let frame_count = CLICK_PULSE_DURATION_MS.saturating_mul(fps as u64) / 1_000 + 2;
    for frame_index in 0..frame_count {
        let progress = (frame_index.saturating_mul(1_000) as f64
            / (fps as u64 * CLICK_PULSE_DURATION_MS) as f64)
            .clamp(0.0, 1.0);
        file.write_all(&rasterize_click_pulse_tile(primary, progress))
            .map_err(|error| format!("Could not write click-effect frame: {error}"))?;
    }
    file.flush()
        .map_err(|error| format!("Could not finalize click-effect frame stream: {error}"))
}

fn rasterize_click_pulse_tile(primary: bool, progress: f64) -> Vec<u8> {
    let size = CLICK_TILE_SIZE as usize;
    let mut tile = vec![0_u8; size * size * 4];
    let center = (CLICK_TILE_SIZE as f64 - 1.0) / 2.0;
    let eased = 1.0 - (1.0 - progress).powi(3);
    let fade = (1.0 - progress).powf(1.35);
    let radius = 14.0 + 42.0 * eased;
    let (red, green, blue) = if primary {
        (255_u8, 205_u8, 110_u8)
    } else {
        (192_u8, 133_u8, 255_u8)
    };
    for y in 0..CLICK_TILE_SIZE {
        for x in 0..CLICK_TILE_SIZE {
            let distance = (x as f64 - center).hypot(y as f64 - center);
            let glow = (1.0 - distance / (radius + 17.0)).clamp(0.0, 1.0).powi(3) * fade * 0.22;
            let ring = (1.0 - (distance - radius).abs() / 3.2).clamp(0.0, 1.0) * fade * 0.9;
            let core = (1.0 - distance / (11.0 + 6.0 * (1.0 - eased)))
                .clamp(0.0, 1.0)
                .powi(2)
                * fade
                * 0.35;
            let alpha = glow.max(ring).max(core);
            if alpha <= 0.0 {
                continue;
            }
            let index = ((y as usize * size + x as usize) * 4) as usize;
            tile[index] = red;
            tile[index + 1] = green;
            tile[index + 2] = blue;
            tile[index + 3] = (alpha * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
    tile
}

fn write_click_effect_commands(
    path: &PathBuf,
    index: usize,
    sample: ClickSample,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<(), String> {
    let file = File::create(path)
        .map_err(|error| format!("Could not create click-effect command file: {error}"))?;
    let mut file = BufWriter::new(file);
    writeln!(
        file,
        "0.000000 overlay@click{index} x -1000, overlay@click{index} y -1000;"
    )
    .map_err(|error| format!("Could not initialize click-effect command file: {error}"))?;
    let frame_count = CLICK_PULSE_DURATION_MS.saturating_mul(fps as u64) / 1_000 + 2;
    for frame_index in 0..frame_count {
        let local_ms = frame_index.saturating_mul(1_000) / fps as u64;
        let time_ms = sample.time_ms.saturating_add(local_ms);
        let point = CursorPoint {
            x: sample.x,
            y: sample.y,
        };
        let (x, y) = recordly_stage_click_position(point, width, height);
        writeln!(
            file,
            "{:.6} overlay@click{index} x {:.3}, overlay@click{index} y {:.3};",
            time_ms as f64 / 1_000.0,
            x,
            y,
        )
        .map_err(|error| format!("Could not write click-effect command file: {error}"))?;
    }
    file.flush()
        .map_err(|error| format!("Could not finalize click-effect command file: {error}"))
}

fn recordly_cursor_motion_blur(
    previous: CursorTileFrame,
    current: CursorTileFrame,
) -> CursorMotionBlur {
    if !previous.visible || !current.visible {
        return CursorMotionBlur::default();
    }
    let delta_ms = current
        .time_ms
        .saturating_sub(previous.time_ms)
        .clamp(1, 80) as f64;
    let (previous_x, previous_y) = previous.position();
    let (current_x, current_y) = current.position();
    let velocity_scale =
        1_000.0 / delta_ms * RECORDLY_CURSOR_MOTION_BLUR * RECORDLY_CURSOR_MOTION_BLUR_MULTIPLIER;
    let mut velocity_x = (current_x - previous_x) * velocity_scale;
    let mut velocity_y = (current_y - previous_y) * velocity_scale;
    let magnitude = velocity_x.hypot(velocity_y);
    if magnitude <= RECORDLY_CURSOR_MOTION_BLUR_THRESHOLD {
        return CursorMotionBlur::default();
    }

    // Pixi grows the filter padding with velocity. The raw-video input needs a
    // fixed tile size, so preserve direction while keeping every sample inside
    // the margin reserved around the Tahoe sprite.
    let max_component = velocity_x.abs().max(velocity_y.abs());
    let max_velocity = (CURSOR_TILE_MARGIN - 2) as f64;
    if max_component > max_velocity {
        let clamp_scale = max_velocity / max_component;
        velocity_x *= clamp_scale;
        velocity_y *= clamp_scale;
    }

    CursorMotionBlur {
        velocity_x,
        velocity_y,
        kernel_size: if magnitude > 3.0 {
            9
        } else if magnitude > 1.0 {
            7
        } else {
            5
        },
        offset: if magnitude > 0.5 { -0.25 } else { 0.0 },
    }
}

fn rasterize_cursor_tile(
    cursor: &image::RgbaImage,
    frame: CursorTileFrame,
    motion_blur: CursorMotionBlur,
) -> Vec<u8> {
    let pixel_count = (CURSOR_TILE_WIDTH * CURSOR_TILE_HEIGHT) as usize;
    if !frame.visible {
        return vec![0; pixel_count * 4];
    }

    // Store premultiplied colour while distributing every source pixel across
    // its four neighbours. This preserves the cursor's alpha edge while
    // allowing fractional placement inside a tile that FFmpeg can overlay at
    // an integer coordinate.
    let mut accumulation = vec![0.0_f32; pixel_count * 4];
    let base_x = CURSOR_TILE_MARGIN as u32;
    let base_y = CURSOR_TILE_MARGIN as u32;
    let weights = [
        (
            0_u32,
            0_u32,
            (1.0 - frame.fractional_x) * (1.0 - frame.fractional_y),
        ),
        (
            1_u32,
            0_u32,
            frame.fractional_x * (1.0 - frame.fractional_y),
        ),
        (
            0_u32,
            1_u32,
            (1.0 - frame.fractional_x) * frame.fractional_y,
        ),
        (1_u32, 1_u32, frame.fractional_x * frame.fractional_y),
    ];
    for (source_x, source_y, pixel) in cursor.enumerate_pixels() {
        let alpha = pixel[3] as f32 / 255.0;
        if alpha <= 0.0 {
            continue;
        }
        for (offset_x, offset_y, weight) in weights {
            if weight <= 0.0 {
                continue;
            }
            let target_x = base_x + source_x + offset_x;
            let target_y = base_y + source_y + offset_y;
            let target = ((target_y * CURSOR_TILE_WIDTH + target_x) * 4) as usize;
            let weighted_alpha = alpha * weight as f32;
            accumulation[target] += pixel[0] as f32 / 255.0 * weighted_alpha;
            accumulation[target + 1] += pixel[1] as f32 / 255.0 * weighted_alpha;
            accumulation[target + 2] += pixel[2] as f32 / 255.0 * weighted_alpha;
            accumulation[target + 3] += weighted_alpha;
        }
    }

    let accumulation = apply_recordly_cursor_motion_blur(accumulation, motion_blur);
    premultiplied_rgba_to_bytes(&accumulation)
}

fn apply_recordly_cursor_motion_blur(source: Vec<f32>, blur: CursorMotionBlur) -> Vec<f32> {
    if blur.kernel_size == 0 || (blur.velocity_x == 0.0 && blur.velocity_y == 0.0) {
        return source;
    }

    // This mirrors pixi-filters' MotionBlurFilter shader: retain the centre
    // sample, then take kernel_size - 1 bilinear samples along the velocity.
    let velocity_length = blur.velocity_x.hypot(blur.velocity_y);
    let shader_offset = -blur.offset / velocity_length - 0.5;
    let sample_count = blur.kernel_size - 1;
    let padding = blur.velocity_x.abs().max(blur.velocity_y.abs()).ceil() as i32 + 2;
    let Some((min_x, min_y, max_x, max_y)) = premultiplied_alpha_bounds(&source) else {
        return source;
    };
    let start_x = (min_x - padding).max(0);
    let start_y = (min_y - padding).max(0);
    let end_x = (max_x + padding).min(CURSOR_TILE_WIDTH as i32 - 1);
    let end_y = (max_y + padding).min(CURSOR_TILE_HEIGHT as i32 - 1);
    let mut destination = vec![0.0_f32; source.len()];
    for y in start_y..=end_y {
        for x in start_x..=end_x {
            let mut color = sample_premultiplied_rgba(&source, x as f64, y as f64);
            for sample_index in 0..sample_count {
                let progress = sample_index as f64 / sample_count as f64 + shader_offset;
                let sample = sample_premultiplied_rgba(
                    &source,
                    x as f64 + blur.velocity_x * progress,
                    y as f64 + blur.velocity_y * progress,
                );
                for channel in 0..4 {
                    color[channel] += sample[channel];
                }
            }
            let target = ((y as u32 * CURSOR_TILE_WIDTH + x as u32) * 4) as usize;
            for channel in 0..4 {
                destination[target + channel] = color[channel] / blur.kernel_size as f32;
            }
        }
    }
    destination
}

fn premultiplied_alpha_bounds(pixels: &[f32]) -> Option<(i32, i32, i32, i32)> {
    let mut min_x = CURSOR_TILE_WIDTH as i32;
    let mut min_y = CURSOR_TILE_HEIGHT as i32;
    let mut max_x = -1;
    let mut max_y = -1;
    for y in 0..CURSOR_TILE_HEIGHT {
        for x in 0..CURSOR_TILE_WIDTH {
            let alpha = pixels[((y * CURSOR_TILE_WIDTH + x) * 4 + 3) as usize];
            if alpha <= 0.0 {
                continue;
            }
            min_x = min_x.min(x as i32);
            min_y = min_y.min(y as i32);
            max_x = max_x.max(x as i32);
            max_y = max_y.max(y as i32);
        }
    }
    (max_x >= 0).then_some((min_x, min_y, max_x, max_y))
}

fn sample_premultiplied_rgba(pixels: &[f32], x: f64, y: f64) -> [f32; 4] {
    let x0 = x.floor() as i32;
    let y0 = y.floor() as i32;
    let fraction_x = (x - x.floor()) as f32;
    let fraction_y = (y - y.floor()) as f32;
    let mut result = [0.0_f32; 4];
    for (sample_x, sample_y, weight) in [
        (x0, y0, (1.0 - fraction_x) * (1.0 - fraction_y)),
        (x0 + 1, y0, fraction_x * (1.0 - fraction_y)),
        (x0, y0 + 1, (1.0 - fraction_x) * fraction_y),
        (x0 + 1, y0 + 1, fraction_x * fraction_y),
    ] {
        if weight <= 0.0
            || sample_x < 0
            || sample_y < 0
            || sample_x >= CURSOR_TILE_WIDTH as i32
            || sample_y >= CURSOR_TILE_HEIGHT as i32
        {
            continue;
        }
        let source = ((sample_y as u32 * CURSOR_TILE_WIDTH + sample_x as u32) * 4) as usize;
        for channel in 0..4 {
            result[channel] += pixels[source + channel] * weight;
        }
    }
    result
}

fn premultiplied_rgba_to_bytes(pixels: &[f32]) -> Vec<u8> {
    let mut tile = vec![0_u8; pixels.len()];
    for (destination, source) in tile.chunks_exact_mut(4).zip(pixels.chunks_exact(4)) {
        let alpha = source[3].clamp(0.0, 1.0);
        if alpha <= 0.0 {
            continue;
        }
        destination[0] = (source[0] / alpha * 255.0).round().clamp(0.0, 255.0) as u8;
        destination[1] = (source[1] / alpha * 255.0).round().clamp(0.0, 255.0) as u8;
        destination[2] = (source[2] / alpha * 255.0).round().clamp(0.0, 255.0) as u8;
        destination[3] = (alpha * 255.0).round() as u8;
    }
    tile
}

fn write_cursor_commands(path: &PathBuf, track: &CursorTrack) -> Result<(), String> {
    let file = File::create(path)
        .map_err(|error| format!("Could not create cursor command file: {error}"))?;
    let mut file = BufWriter::new(file);
    let mut previous_command = None;
    for frame in &track.frames {
        let command = (frame.origin_x, frame.origin_y);
        if previous_command == Some(command) {
            continue;
        }
        previous_command = Some(command);
        writeln!(
            file,
            "{:.6} overlay@cursor x {:.3}, overlay@cursor y {:.3};",
            frame.time_ms as f64 / 1_000.0,
            frame.origin_x,
            frame.origin_y
        )
        .map_err(|error| format!("Could not write cursor command file: {error}"))?;
    }
    file.flush()
        .map_err(|error| format!("Could not finalize cursor command file: {error}"))
}

fn write_camera_commands(
    path: &PathBuf,
    events: &[crate::zoom::ZoomEvent],
    width: u32,
    height: u32,
    duration_ms: u64,
    fps: u32,
) -> Result<(), String> {
    let file = File::create(path)
        .map_err(|error| format!("Could not create camera command file: {error}"))?;
    let mut file = BufWriter::new(file);
    for interval in camera_command_intervals(events, width, height, duration_ms, fps) {
        writeln!(
            file,
            "{:.6}-{:.6} [enter] crop@camera w {}, [enter] crop@camera h {}, [enter] crop@camera x {}, [enter] crop@camera y {};",
            interval.start_ms as f64 / 1_000.0,
            interval.end_ms as f64 / 1_000.0,
            interval.rect.width,
            interval.rect.height,
            interval.rect.x,
            interval.rect.y,
        )
        .map_err(|error| format!("Could not write camera command file: {error}"))?;
    }
    file.flush()
        .map_err(|error| format!("Could not finalize camera command file: {error}"))
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct CameraCommandInterval {
    start_ms: u64,
    end_ms: u64,
    rect: CameraCropRect,
}

fn camera_command_intervals(
    events: &[crate::zoom::ZoomEvent],
    width: u32,
    height: u32,
    duration_ms: u64,
    fps: u32,
) -> Vec<CameraCommandInterval> {
    let timing = ZoomTiming::default();
    let mut states = Vec::new();
    let mut previous_rect = None;
    let mut spring = RecordlyCameraSpring::default();
    let mut previous_time_ms = None;
    let frame_count = duration_ms.saturating_mul(fps as u64) / 1_000 + 2;
    for frame_index in 0..frame_count {
        let time_ms = frame_index.saturating_mul(1_000) / fps as u64;
        let target = camera_transform_at(events, time_ms, timing);
        let delta_ms = previous_time_ms
            .map(|previous| time_ms.saturating_sub(previous) as f64)
            .unwrap_or(1_000.0 / fps.max(1) as f64);
        previous_time_ms = Some(time_ms);
        let transform = spring.step(target, delta_ms);
        let rect = camera_crop_rect(transform, width, height);
        if previous_rect == Some(rect) {
            continue;
        }
        previous_rect = Some(rect);
        states.push((time_ms, rect));
    }
    let final_end_ms = duration_ms
        .saturating_add(1_000_u64.saturating_add(fps as u64 - 1) / fps as u64)
        .max(1);
    states
        .iter()
        .enumerate()
        .map(|(index, (start_ms, rect))| CameraCommandInterval {
            start_ms: *start_ms,
            end_ms: states
                .get(index + 1)
                .map(|(next_start_ms, _)| *next_start_ms)
                .unwrap_or(final_end_ms)
                .max(start_ms.saturating_add(1)),
            rect: *rect,
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct CameraCropRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

fn camera_crop_rect(transform: CameraTransform, width: u32, height: u32) -> CameraCropRect {
    let crop_width = ((width as f64 / transform.scale).round() as u32).clamp(1, width);
    let crop_height = ((height as f64 / transform.scale).round() as u32).clamp(1, height);
    let (content_width, content_height) = recordly_padded_size(width, height);
    let inset_x = width.saturating_sub(content_width) as f64 / 2.0;
    let inset_y = height.saturating_sub(content_height) as f64 / 2.0;
    let focus_x = transform.crop_x + 0.5 / transform.scale;
    let focus_y = transform.crop_y + 0.5 / transform.scale;
    let stage_focus_x = inset_x + focus_x * content_width as f64;
    let stage_focus_y = inset_y + focus_y * content_height as f64;
    let max_x = width.saturating_sub(crop_width);
    let max_y = height.saturating_sub(crop_height);
    CameraCropRect {
        x: (stage_focus_x - crop_width as f64 / 2.0)
            .clamp(0.0, max_x as f64)
            .round() as u32,
        y: (stage_focus_y - crop_height as f64 / 2.0)
            .clamp(0.0, max_y as f64)
            .round() as u32,
        width: crop_width,
        height: crop_height,
    }
}

fn tahoe_cursor_for_niri_name(name: &str) -> TahoeCursor {
    TahoeCursor::from_standard_name(name)
}

fn tahoe_cursor_asset_file(kind: TahoeCursor) -> &'static str {
    match kind {
        TahoeCursor::Arrow | TahoeCursor::Hidden => "pikscreen-tahoe-pointer.svg",
        TahoeCursor::ContextMenu => "contextualmenu-1__12-5.svg",
        TahoeCursor::Help => "help-1__50-50.svg",
        TahoeCursor::PointingHand => "pointinghand-1__40-10.svg",
        TahoeCursor::Busy => "beachball-1__50-46.svg",
        TahoeCursor::Cell => "cell-1__50-50.svg",
        TahoeCursor::Crosshair => "crosshair-1__50-50.svg",
        TahoeCursor::Text => "ibeam-1__50-44.svg",
        TahoeCursor::VerticalText => "ibeamvertical-1__50-50.svg",
        TahoeCursor::Alias => "makealias-1__63-29.svg",
        TahoeCursor::Copy => "copy-1__23-0.svg",
        TahoeCursor::Move => "move-1__50-46.svg",
        TahoeCursor::NotAllowed => "notallowed-1__23-0.svg",
        TahoeCursor::OpenHand => "openhand-1__55-57.svg",
        TahoeCursor::ClosedHand => "closedhand-1__50-46.svg",
        TahoeCursor::ResizeEast => "resizeeast-1__50-50.svg",
        TahoeCursor::ResizeNorth => "resizenorth-1__50-49.svg",
        TahoeCursor::ResizeNorthEast => "resizenortheast-1__50-49.svg",
        TahoeCursor::ResizeNorthWest => "resizenorthwest-1__50-49.svg",
        TahoeCursor::ResizeSouth => "resizesouth-1__50-49.svg",
        TahoeCursor::ResizeSouthEast => "resizesoutheast-1__50-49.svg",
        TahoeCursor::ResizeSouthWest => "resizesouthwest-1__50-49.svg",
        TahoeCursor::ResizeWest => "resizewest-1__50-50.svg",
        TahoeCursor::ResizeEastWest => "resizeeastwest-1__50-50.svg",
        TahoeCursor::ResizeNorthSouth => "resizenorthsouth-1__50-49.svg",
        TahoeCursor::ResizeNorthEastSouthWest => "resizenortheastsouthwest-1__50-46.svg",
        TahoeCursor::ResizeNorthWestSouthEast => "resizenorthwestsoutheast-1__51-46.svg",
        TahoeCursor::ResizeLeftRight => "resizeleftright-1__50-43.svg",
        TahoeCursor::ResizeUpDown => "resizeupdown-1__51-48.svg",
        TahoeCursor::ZoomIn => "zoomin-1__42-39.svg",
        TahoeCursor::ZoomOut => "zoomout-1__43-39.svg",
    }
}

fn tahoe_cursor_asset_path(kind: TahoeCursor) -> Result<PathBuf, String> {
    let file = tahoe_cursor_asset_file(kind);
    let assets = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets");
    let path = if kind == TahoeCursor::Arrow || kind == TahoeCursor::Hidden {
        assets.join(file)
    } else {
        assets.join("tahoe").join(file)
    };
    path.exists()
        .then_some(path)
        .ok_or_else(|| format!("PikScreen Tahoe cursor asset is missing: {file}"))
}

fn tahoe_cursor_metrics(kind: TahoeCursor) -> Result<TahoeCursorMetrics, String> {
    if kind == TahoeCursor::Arrow || kind == TahoeCursor::Hidden {
        return Ok(TahoeCursorMetrics {
            aspect_ratio: 618.0 / 958.0,
            hotspot_x: RECORDLY_TAHOE_CURSOR_HOTSPOT_X,
            hotspot_y: RECORDLY_TAHOE_CURSOR_HOTSPOT_Y,
        });
    }
    let source_path = tahoe_cursor_asset_path(kind)?;
    let svg = fs::read_to_string(&source_path).map_err(|error| {
        format!(
            "Could not read Tahoe cursor asset {}: {error}",
            source_path.display()
        )
    })?;
    let svg_tag = svg
        .split_once("<svg")
        .and_then(|(_, rest)| rest.split_once('>'))
        .map(|(tag, _)| tag)
        .ok_or_else(|| {
            format!(
                "Tahoe cursor asset has no SVG tag: {}",
                source_path.display()
            )
        })?;
    let dimension = |name: &str| -> Result<f64, String> {
        let marker = format!("{name}=\"");
        svg_tag
            .split_once(&marker)
            .and_then(|(_, rest)| rest.split_once('\"'))
            .and_then(|(value, _)| value.parse::<f64>().ok())
            .ok_or_else(|| {
                format!(
                    "Tahoe cursor asset has no numeric {name}: {}",
                    source_path.display()
                )
            })
    };
    let width = dimension("width")?;
    let height = dimension("height")?;
    let asset_file = tahoe_cursor_asset_file(kind);
    let hotspot_suffix = asset_file
        .strip_suffix(".svg")
        .and_then(|file| file.rsplit_once("__"))
        .ok_or_else(|| format!("Tahoe cursor asset has no hotspot suffix: {asset_file}"))?
        .1;
    let (hotspot_x, hotspot_y) = hotspot_suffix
        .split_once('-')
        .and_then(|(x, y)| {
            Some((
                x.parse::<f64>().ok()? / 100.0,
                y.parse::<f64>().ok()? / 100.0,
            ))
        })
        .ok_or_else(|| {
            format!("Tahoe cursor asset has invalid hotspot suffix: {hotspot_suffix}")
        })?;
    Ok(TahoeCursorMetrics {
        aspect_ratio: width / height,
        hotspot_x,
        hotspot_y,
    })
}

fn scaled_cursor_asset_paths(
    track: &CursorTrack,
    stage_scale: f64,
    custom_cursor_path: Option<&str>,
) -> Result<HashMap<TahoeCursor, PathBuf>, String> {
    let mut paths = HashMap::new();
    for kind in track
        .frames
        .iter()
        .filter(|frame| frame.visible)
        .map(|frame| frame.kind)
    {
        if paths.contains_key(&kind) {
            continue;
        }
        let source_path = custom_cursor_path
            .map(PathBuf::from)
            .unwrap_or(tahoe_cursor_asset_path(kind)?);
        let output_path = temporary_file("pikscreen-tahoe-cursor", "png");
        let output = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-i",
                source_path.to_string_lossy().as_ref(),
                "-vf",
                &format!(
                    "scale=-1:{:.0}",
                    (RECORDLY_TAHOE_CURSOR_HEIGHT * stage_scale).max(1.0)
                ),
                "-frames:v",
                "1",
                output_path.to_string_lossy().as_ref(),
            ])
            .output()
            .map_err(|error| format!("Could not scale the Recordly Tahoe cursor: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "Could not scale Tahoe cursor {kind:?}: {} [{}]",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        paths.insert(kind, output_path);
    }
    Ok(paths)
}

fn recordly_wallpaper_asset_path(
    background: BackgroundStyle,
    custom_background_path: Option<&str>,
) -> Result<PathBuf, String> {
    if let Some(path) = custom_background_path {
        let path = PathBuf::from(path);
        appearance::validate_background(&path)?;
        return Ok(path);
    }
    let file = background
        .asset_file()
        .ok_or_else(|| "Solid backgrounds do not use wallpaper assets.".to_owned())?;
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("assets")
        .join("wallpapers")
        .join(file);
    path.exists()
        .then_some(path)
        .ok_or_else(|| format!("Recordly wallpaper asset is missing: {file}"))
}

fn recordly_card_filter(
    width: u32,
    height: u32,
    duration_ms: u64,
    fps: u32,
    mask_input_index: usize,
    shadow_input_index: usize,
) -> String {
    let (content_width, content_height) = recordly_padded_size(width, height);
    let padded_duration = duration_ms as f64 / 1_000.0 + 1.0 / fps.max(1) as f64;
    format!(
        "[0:v]setpts=PTS-STARTPTS,fps={fps},tpad=stop_mode=clone:stop_duration={padded_duration:.6},scale={content_width}:{content_height}:flags=lanczos,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2:color=black,format=rgba[content];[{mask_input_index}:v]format=gray,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2:color=black[frame_mask];[content][frame_mask]alphamerge[content_with_mask];[{shadow_input_index}:v]format=rgba[frame_shadow];[frame_shadow][content_with_mask]overlay=0:0:format=auto:shortest=1[card]"
    )
}

fn wallpaper_background_filter(
    wallpaper_input_index: usize,
    card_label: &str,
    output_label: &str,
) -> String {
    format!(
        "[{wallpaper_input_index}:v]format=rgba[wallpaper];[wallpaper][{card_label}]overlay=0:0:format=auto:shortest=1,format=rgba[{output_label}]"
    )
}

struct RecordlyFrameAssets {
    mask_path: PathBuf,
    wallpaper_path: PathBuf,
    shadow_path: PathBuf,
}

fn recordly_frame_assets(
    background: BackgroundStyle,
    custom_background_path: Option<&str>,
    width: u32,
    height: u32,
) -> Result<RecordlyFrameAssets, String> {
    let (content_width, content_height) = recordly_padded_size(width, height);
    let mask_svg_path = temporary_file("recordly-squircle-mask", "svg");
    let shadow_svg_path = temporary_file("recordly-squircle-shadow", "svg");
    let mask_path = temporary_file("recordly-squircle-mask", "png");
    let shadow_path = temporary_file("recordly-squircle-shadow", "png");
    let wallpaper_path = temporary_file("recordly-static-wallpaper", "png");
    let path = recordly_squircle_path(content_width as f64, content_height as f64, 12.5);
    std::fs::write(
        &mask_svg_path,
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{content_width}\" height=\"{content_height}\" viewBox=\"0 0 {content_width} {content_height}\"><path d=\"{path}\" fill=\"white\"/></svg>"
        ),
    )
    .map_err(|error| format!("Could not create Recordly squircle mask: {error}"))?;
    std::fs::write(
        &shadow_svg_path,
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\"><defs><filter id=\"shadow\" x=\"-20%\" y=\"-20%\" width=\"140%\" height=\"150%\"><feGaussianBlur in=\"SourceAlpha\" stdDeviation=\"32.16\" result=\"blur1\"/><feOffset in=\"blur1\" dx=\"8.04\" dy=\"8.04\" result=\"offset1\"/><feFlood flood-color=\"black\" flood-opacity=\"0.469\" result=\"color1\"/><feComposite in=\"color1\" in2=\"offset1\" operator=\"in\" result=\"shadow1\"/><feGaussianBlur in=\"SourceAlpha\" stdDeviation=\"10.72\" result=\"blur2\"/><feOffset in=\"blur2\" dx=\"2.68\" dy=\"2.68\" result=\"offset2\"/><feFlood flood-color=\"black\" flood-opacity=\"0.335\" result=\"color2\"/><feComposite in=\"color2\" in2=\"offset2\" operator=\"in\" result=\"shadow2\"/><feGaussianBlur in=\"SourceAlpha\" stdDeviation=\"5.36\" result=\"blur3\"/><feOffset in=\"blur3\" dx=\"1.34\" dy=\"1.34\" result=\"offset3\"/><feFlood flood-color=\"black\" flood-opacity=\"0.201\" result=\"color3\"/><feComposite in=\"color3\" in2=\"offset3\" operator=\"in\" result=\"shadow3\"/><feMerge><feMergeNode in=\"shadow1\"/><feMergeNode in=\"shadow2\"/><feMergeNode in=\"shadow3\"/></feMerge></filter></defs><path d=\"{path}\" transform=\"translate({:.3} {:.3})\" fill=\"black\" filter=\"url(#shadow)\"/></svg>",
            (width.saturating_sub(content_width)) as f64 / 2.0,
            (height.saturating_sub(content_height)) as f64 / 2.0,
        ),
    )
    .map_err(|error| format!("Could not create Recordly shadow layer: {error}"))?;
    rasterize_static_svg(&mask_svg_path, &mask_path, "Recordly squircle mask")?;
    rasterize_static_svg(&shadow_svg_path, &shadow_path, "Recordly shadow layer")?;
    if custom_background_path.is_none() {
        if let Some(color) = background.solid_color() {
            rasterize_solid_wallpaper(color, &wallpaper_path, width, height)?;
        } else {
            rasterize_wallpaper(
                &recordly_wallpaper_asset_path(background, None)?,
                &wallpaper_path,
                width,
                height,
            )?;
        }
    } else {
        rasterize_wallpaper(
            &recordly_wallpaper_asset_path(background, custom_background_path)?,
            &wallpaper_path,
            width,
            height,
        )?;
    }
    let _ = std::fs::remove_file(mask_svg_path);
    let _ = std::fs::remove_file(shadow_svg_path);
    Ok(RecordlyFrameAssets {
        mask_path,
        wallpaper_path,
        shadow_path,
    })
}

fn rasterize_wallpaper(
    wallpaper_path: &PathBuf,
    output_path: &PathBuf,
    width: u32,
    height: u32,
) -> Result<(), String> {
    let filter = format!(
        "scale={width}:{height}:force_original_aspect_ratio=increase,crop={width}:{height}"
    );
    let output = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-loop",
            "1",
            "-i",
            wallpaper_path.to_string_lossy().as_ref(),
            "-vf",
            &filter,
            "-frames:v",
            "1",
            output_path.to_string_lossy().as_ref(),
        ])
        .output()
        .map_err(|error| format!("Could not rasterize Recordly wallpaper: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "Could not rasterize Recordly wallpaper: {} [{}]",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn rasterize_solid_wallpaper(
    color: &str,
    output_path: &PathBuf,
    width: u32,
    height: u32,
) -> Result<(), String> {
    let source = format!("color=c={color}:s={width}x{height}");
    let output = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &source,
            "-vf",
            "format=rgb24",
            "-frames:v",
            "1",
            output_path.to_string_lossy().as_ref(),
        ])
        .output()
        .map_err(|error| format!("Could not create a solid background: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "Could not create a solid background: {} [{}]",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn rasterize_static_svg(
    source: &PathBuf,
    destination: &PathBuf,
    label: &str,
) -> Result<(), String> {
    let output = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-i",
            source.to_string_lossy().as_ref(),
            "-frames:v",
            "1",
            destination.to_string_lossy().as_ref(),
        ])
        .output()
        .map_err(|error| format!("Could not rasterize {label}: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "Could not rasterize {label}: {} [{}]",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn recordly_padded_size(width: u32, height: u32) -> (u32, u32) {
    // Port of Recordly's DEFAULT_PADDING (20) and PADDING_SCALE_FACTOR (0.2):
    // each linked side consumes 4% of the canvas dimension.
    (
        (width as f64 * 0.92).round() as u32,
        (height as f64 * 0.92).round() as u32,
    )
}

fn ffmpeg_filter_threads() -> String {
    std::thread::available_parallelism()
        .map(|threads| threads.get().clamp(1, 12))
        .unwrap_or(4)
        .to_string()
}

fn recordly_squircle_path(width: f64, height: f64, radius: f64) -> String {
    let radius = radius.clamp(0.0, width.min(height) / 2.0);
    if radius <= 0.5 {
        return format!("M 0 0 L {width} 0 L {width} {height} L 0 {height} Z");
    }
    let mut points = vec![(radius, 0.0)];
    let corners = [
        (width - radius, radius, -std::f64::consts::FRAC_PI_2, 0.0),
        (
            width - radius,
            height - radius,
            0.0,
            std::f64::consts::FRAC_PI_2,
        ),
        (
            radius,
            height - radius,
            std::f64::consts::FRAC_PI_2,
            std::f64::consts::PI,
        ),
        (
            radius,
            radius,
            std::f64::consts::PI,
            std::f64::consts::PI * 1.5,
        ),
    ];
    for (center_x, center_y, start, end) in corners {
        for index in 1..=10 {
            let angle = start + (end - start) * index as f64 / 10.0;
            let exponent = 2.0 / 4.5;
            let x = center_x + angle.cos().signum() * radius * angle.cos().abs().powf(exponent);
            let y = center_y + angle.sin().signum() * radius * angle.sin().abs().powf(exponent);
            points.push((x, y));
        }
    }
    let mut path = format!("M {:.3} {:.3}", points[0].0, points[0].1);
    for (x, y) in points.iter().skip(1) {
        path.push_str(&format!(" L {x:.3} {y:.3}"));
    }
    path.push_str(" Z");
    path
}

fn tahoe_cursor_hotspot_offset(metrics: TahoeCursorMetrics, stage_scale: f64) -> (f64, f64) {
    let cursor_height = RECORDLY_TAHOE_CURSOR_HEIGHT * stage_scale;
    let width = cursor_height * metrics.aspect_ratio;
    (width * metrics.hotspot_x, cursor_height * metrics.hotspot_y)
}

fn recordly_stage_cursor_position(
    point: CursorPoint,
    metrics: TahoeCursorMetrics,
    width: u32,
    height: u32,
    cursor_scale: f64,
) -> (f64, f64) {
    let (content_width, content_height) = recordly_padded_size(width, height);
    let inset_x = (width.saturating_sub(content_width)) as f64 / 2.0;
    let inset_y = (height.saturating_sub(content_height)) as f64 / 2.0;
    let stage_scale = content_height as f64 / height as f64;
    let (hotspot_x, hotspot_y) = tahoe_cursor_hotspot_offset(metrics, stage_scale * cursor_scale);
    (
        inset_x + point.x * content_width as f64 - hotspot_x,
        inset_y + point.y * content_height as f64 - hotspot_y,
    )
}

fn recordly_stage_click_position(point: CursorPoint, width: u32, height: u32) -> (f64, f64) {
    let (content_width, content_height) = recordly_padded_size(width, height);
    let inset_x = (width.saturating_sub(content_width)) as f64 / 2.0;
    let inset_y = (height.saturating_sub(content_height)) as f64 / 2.0;
    (
        inset_x + point.x * content_width as f64 - CLICK_TILE_SIZE as f64 / 2.0,
        inset_y + point.y * content_height as f64 - CLICK_TILE_SIZE as f64 / 2.0,
    )
}

fn cursor_is_hidden(events: &[crate::zoom::ZoomEvent], time_ms: u64) -> bool {
    events.iter().any(|event| {
        event.hide_cursor
            && time_ms >= event.start_ms
            && time_ms < event.end_ms(ZoomTiming::default())
    })
}

fn stop_child(recorder: Child) -> Result<(), String> {
    if unsafe { libc::kill(recorder.id() as i32, libc::SIGINT) } != 0
        && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    {
        return Err(format!(
            "Could not stop the cursorless recorder: {}",
            std::io::Error::last_os_error()
        ));
    }
    let output = recorder
        .wait_with_output()
        .map_err(|error| format!("Could not finalize recording: {error}"))?;
    if output.status.success() || output.status.signal() == Some(libc::SIGINT) {
        Ok(())
    } else {
        Err(format!(
            "Cursorless wf-recorder exited unsuccessfully: {} [recorder: {}]",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn ensure_video_recorder_started(recorder: &mut VideoRecorder) -> Result<(), String> {
    match recorder {
        VideoRecorder::Portal(recorder) => window_pipewire::ensure_recorder_started(recorder),
        VideoRecorder::WfRecorder(recorder) => {
            std::thread::sleep(Duration::from_millis(200));
            let Some(status) = recorder
                .try_wait()
                .map_err(|error| format!("Could not inspect the cursorless recorder: {error}"))?
            else {
                return Ok(());
            };
            let mut stderr = String::new();
            if let Some(mut pipe) = recorder.stderr.take() {
                let _ = pipe.read_to_string(&mut stderr);
            }
            Err(format!(
                "Cursorless recorder failed during startup with {status}: {}",
                stderr.trim()
            ))
        }
    }
}

fn stop_video_recorder(recorder: VideoRecorder) -> Result<(), String> {
    match recorder {
        VideoRecorder::WfRecorder(recorder) => stop_child(recorder),
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
    fn gnome_shell_pointer_uses_global_coordinates_inside_the_selected_crop() {
        let geometry = CaptureCrop {
            x: 1_920,
            y: 0,
            width: 1_920,
            height: 1_080,
            excluded_bottom: 0,
        };
        let sample = gnome_cursor_sample((2_880, 540), geometry, (0.5, 0.5), 42);
        assert_eq!(sample.time_ms, 42);
        assert_eq!(sample.kind, TahoeCursor::Arrow);
        assert_eq!((sample.x, sample.y), (0.5, 0.5));

        let hidden = gnome_cursor_sample((640, 540), geometry, (0.5, 0.5), 43);
        assert_eq!(hidden.kind, TahoeCursor::Hidden);
        assert_eq!((hidden.x, hidden.y), (0.5, 0.5));
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
    fn camera_crop_uses_the_recordly_camera_transform() {
        let transform = CameraTransform::at(
            build_zoom_events(
                &[Marker {
                    time_ms: 1_000,
                    x: 0.25,
                    y: 0.75,
                    target_scale: None,
                    held_until_ms: None,
                    hide_cursor: false,
                }],
                ZoomTiming::default(),
            )[0],
            1_000 + ZoomTiming::default().zoom_in_ms,
            ZoomTiming::default(),
        );
        let rect = camera_crop_rect(transform, 1_920, 1_080);
        assert_eq!((rect.width, rect.height), (1_067, 600));
        assert!(rect.x < 853);
        assert!(rect.y < 480);
    }

    #[test]
    fn camera_crop_keeps_single_pixel_precision_for_smooth_motion() {
        let rect = camera_crop_rect(
            CameraTransform {
                scale: 1.8,
                crop_x: 0.422_222_222_222_222_2,
                crop_y: 0.0,
            },
            1_920,
            1_080,
        );
        assert_eq!(rect.width, 1_067);
        assert_eq!(rect.x, 780);
        assert_eq!(rect.y, 19);
    }

    #[test]
    fn camera_commands_use_bounded_non_overlapping_intervals() {
        let events = build_zoom_events(
            &[Marker {
                time_ms: 1_000,
                x: 0.25,
                y: 0.75,
                target_scale: None,
                held_until_ms: None,
                hide_cursor: false,
            }],
            ZoomTiming::default(),
        );
        let intervals = camera_command_intervals(&events, 1_920, 1_080, 5_000, 120);
        assert_eq!(intervals.first().map(|interval| interval.start_ms), Some(0));
        assert!(intervals
            .windows(2)
            .all(|pair| pair[0].end_ms == pair[1].start_ms));
        assert!(intervals
            .iter()
            .all(|interval| interval.end_ms > interval.start_ms));
        assert!(intervals
            .iter()
            .any(|interval| (interval.rect.width, interval.rect.height) == (1_067, 600)));
    }

    #[test]
    fn camera_command_file_marks_every_state_as_an_enter_interval() {
        let path = temporary_file("camera-command-test", "txt");
        let events = build_zoom_events(
            &[Marker {
                time_ms: 500,
                x: 0.5,
                y: 0.5,
                target_scale: None,
                held_until_ms: None,
                hide_cursor: false,
            }],
            ZoomTiming::default(),
        );
        write_camera_commands(&path, &events, 1_920, 1_080, 4_500, 120)
            .expect("camera commands should be written");
        let commands = fs::read_to_string(&path).expect("camera commands should be readable");
        let _ = fs::remove_file(path);
        assert!(commands.lines().all(|line| {
            line.split_once(' ')
                .is_some_and(|(interval, _)| interval.contains('-'))
                && line.matches("[enter]").count() == 4
        }));
    }

    #[test]
    fn camera_filter_transforms_the_composited_scene() {
        let filter = camera_filter_chain(
            "scene4",
            "screen",
            &PathBuf::from("/tmp/camera-commands.txt"),
            1_920,
            1_080,
        );
        let scene = filter.find("[scene4]sendcmd=").expect("composited scene");
        let sendcmd = filter.find("sendcmd=").expect("camera commands");
        let crop = filter.find("crop@camera").expect("camera crop");
        let scale = filter.find("scale=1920:1080").expect("camera scale");
        assert!(scene <= sendcmd && sendcmd < crop && crop < scale);
        assert!(!filter.contains("pad="));
        assert!(filter.ends_with("format=rgba[screen]"));
    }

    #[test]
    fn parses_the_existing_niri_cursor_ipc_reply() {
        assert_eq!(
            parse_niri_cursor_reply("{\"Ok\":{\"CursorPosition\":[123.5,456.25]}}\n"),
            Ok((123.5, 456.25))
        );
    }

    #[test]
    fn parses_atomic_niri_cursor_state_and_maps_standard_shapes() {
        assert_eq!(
            parse_niri_cursor_state_reply(
                "{\"Ok\":{\"CursorState\":{\"position\":[123.5,456.25],\"image\":{\"Named\":\"text\"}}}}\n"
            ),
            Ok(NiriCursorState {
                position: (123.5, 456.25),
                kind: TahoeCursor::Text,
            })
        );
        assert_eq!(
            parse_niri_cursor_state_reply(
                "{\"Ok\":{\"CursorState\":{\"position\":[1.0,2.0],\"image\":\"Hidden\"}}}\n"
            ),
            Ok(NiriCursorState {
                position: (1.0, 2.0),
                kind: TahoeCursor::Hidden,
            })
        );
    }

    #[test]
    fn parses_visible_and_hidden_window_geometry() {
        assert_eq!(
            parse_niri_window_geometry_reply(
                "{\"Ok\":{\"WindowGeometry\":{\"x\":24,\"y\":25,\"width\":1874,\"height\":982}}}\n"
            ),
            Ok(Some(CaptureCrop {
                x: 24,
                y: 25,
                width: 1_874,
                height: 982,
                excluded_bottom: 0,
            }))
        );
        assert_eq!(
            parse_niri_window_geometry_reply("{\"Ok\":{\"WindowGeometry\":null}}\n"),
            Ok(None)
        );
    }

    #[test]
    fn window_cursor_is_hidden_when_geometry_is_unavailable_or_pointer_is_outside() {
        let cursor = NiriCursorState {
            position: (900.0, 500.0),
            kind: TahoeCursor::Text,
        };
        let hidden = cursor_sample_for_geometry(cursor, None, (0.25, 0.75), 42);
        assert_eq!(hidden.kind, TahoeCursor::Hidden);
        assert_eq!((hidden.x, hidden.y), (0.25, 0.75));

        let outside = cursor_sample_for_geometry(
            cursor,
            Some(CaptureCrop {
                x: 0,
                y: 0,
                width: 800,
                height: 600,
                excluded_bottom: 0,
            }),
            (0.4, 0.6),
            43,
        );
        assert_eq!(outside.kind, TahoeCursor::Hidden);
        assert_eq!((outside.x, outside.y), (0.4, 0.6));
    }

    #[test]
    fn window_cursor_maps_inside_visible_geometry() {
        let sample = cursor_sample_for_geometry(
            NiriCursorState {
                position: (510.0, 270.0),
                kind: TahoeCursor::PointingHand,
            },
            Some(CaptureCrop {
                x: 10,
                y: 20,
                width: 1_000,
                height: 500,
                excluded_bottom: 0,
            }),
            (0.0, 0.0),
            44,
        );
        assert_eq!(sample.kind, TahoeCursor::PointingHand);
        assert_eq!((sample.x, sample.y), (0.5, 0.5));
    }

    #[test]
    fn portal_cursor_metadata_maps_stream_local_hotspot_coordinates() {
        let observation = PortalCursorObservation {
            cursor_id: 9,
            x: 480,
            y: 270,
            hotspot_x: 6,
            hotspot_y: 4,
            stream_width: 960,
            stream_height: 540,
            bitmap_width: Some(48),
            bitmap_height: Some(48),
            bitmap: None,
        };
        assert_eq!(
            portal_cursor_sample(&observation, (0.1, 0.2), TahoeCursor::Text, 125),
            CursorSample {
                time_ms: 125,
                x: 0.5,
                y: 0.5,
                kind: TahoeCursor::Text,
            }
        );

        let hidden = portal_cursor_sample(
            &PortalCursorObservation {
                x: -1,
                ..observation.clone()
            },
            (0.3, 0.7),
            TahoeCursor::PointingHand,
            126,
        );
        assert_eq!(hidden.kind, TahoeCursor::Hidden);
        assert_eq!((hidden.x, hidden.y), (0.3, 0.7));
    }

    #[test]
    fn hyprland_input_is_normalized_and_keeps_the_cursor_shape() {
        let geometry = CaptureCrop {
            x: 1920,
            y: 0,
            width: 1920,
            height: 1080,
            excluded_bottom: 0,
        };
        let mut packet = HyprlandInputPacket {
            monotonic_ns: 0,
            kind: hyprland_input::EVENT_SNAPSHOT,
            code: 0,
            pressed: false,
            modifiers: 0,
            position: (2880.0, 540.0),
            shape: "text".to_owned(),
        };
        assert_eq!(
            hyprland_input_sample(&packet, geometry, (0.1, 0.2), 125),
            CursorSample {
                time_ms: 125,
                x: 0.5,
                y: 0.5,
                kind: TahoeCursor::Text,
            }
        );
        packet.position = (100.0, 100.0);
        assert_eq!(
            hyprland_input_sample(&packet, geometry, (0.1, 0.2), 150),
            CursorSample {
                time_ms: 150,
                x: 0.1,
                y: 0.2,
                kind: TahoeCursor::Hidden,
            }
        );
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
    fn parses_primary_and_secondary_niri_click_events() {
        let clicks = parse_niri_click_events_reply(
            "{\"Ok\":{\"ClickEvents\":{\"now_ms\":120,\"events\":[{\"time_ms\":100,\"position\":[123.5,456.25],\"button\":\"Primary\"},{\"time_ms\":110,\"position\":[11.0,22.0],\"button\":\"Secondary\"}]}}}\n",
        )
        .expect("the click-events reply should parse");
        assert_eq!(clicks.now_ms, 120);
        assert_eq!(clicks.events.len(), 2);
        assert_eq!(clicks.events[0].position, (123.5, 456.25));
        assert!(clicks.events[0].primary);
        assert!(!clicks.events[1].primary);
    }

    #[test]
    fn shared_epoch_absorbs_recorder_startup_delay() {
        let sampled_at = Instant::now();
        let recording_started_at = sampled_at + Duration::from_millis(900);
        assert_eq!(
            translate_niri_epoch(10_000, sampled_at, recording_started_at),
            10_900
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
    fn click_effect_uses_the_rendered_cursor_instead_of_the_raw_event_position() {
        let timeline = CursorTimeline::new(vec![
            CursorSample {
                time_ms: 0,
                x: 0.2,
                y: 0.3,
                kind: TahoeCursor::Arrow,
            },
            CursorSample {
                time_ms: 100,
                x: 0.8,
                y: 0.7,
                kind: TahoeCursor::PointingHand,
            },
        ]);
        let sample = cursor_locked_click_sample(
            ClickSample {
                time_ms: 50,
                x: 0.99,
                y: 0.01,
                primary: false,
            },
            &timeline,
            &[],
        )
        .expect("visible clicks should be retained");
        assert!((sample.x - 0.5).abs() < f64::EPSILON);
        assert!((sample.y - 0.5).abs() < f64::EPSILON);
        assert!(!sample.primary);
    }

    #[test]
    fn portal_click_filter_accepts_only_left_and_right_presses() {
        assert_eq!(
            portal_click_button(&evdev::InputEvent::new(
                EventType::KEY.0,
                KeyCode::BTN_LEFT.code(),
                1,
            )),
            Some(true)
        );
        assert_eq!(
            portal_click_button(&evdev::InputEvent::new(
                EventType::KEY.0,
                KeyCode::BTN_RIGHT.code(),
                1,
            )),
            Some(false)
        );
        assert_eq!(
            portal_click_button(&evdev::InputEvent::new(
                EventType::KEY.0,
                KeyCode::BTN_LEFT.code(),
                0,
            )),
            None
        );
        assert_eq!(
            portal_click_button(&evdev::InputEvent::new(
                EventType::KEY.0,
                KeyCode::BTN_MIDDLE.code(),
                1,
            )),
            None
        );
    }

    #[test]
    fn portal_click_dedup_suppresses_only_same_button_bursts() {
        assert!(portal_click_is_duplicate(Some((100, true)), 116, true));
        assert!(!portal_click_is_duplicate(Some((100, true)), 117, true));
        assert!(!portal_click_is_duplicate(Some((100, true)), 105, false));
        assert!(!portal_click_is_duplicate(None, 105, true));
    }

    #[test]
    fn click_effect_is_suppressed_while_the_rendered_cursor_is_hidden() {
        let timeline = CursorTimeline::new(vec![CursorSample {
            time_ms: 50,
            x: 0.5,
            y: 0.5,
            kind: TahoeCursor::Hidden,
        }]);
        assert!(cursor_locked_click_sample(
            ClickSample {
                time_ms: 50,
                x: 0.5,
                y: 0.5,
                primary: true,
            },
            &timeline,
            &[],
        )
        .is_none());
    }

    #[test]
    fn click_effect_is_suppressed_by_a_cursor_hiding_zoom() {
        let timeline = CursorTimeline::new(vec![CursorSample {
            time_ms: 50,
            x: 0.5,
            y: 0.5,
            kind: TahoeCursor::Arrow,
        }]);
        let event =
            crate::zoom::ZoomEvent::standalone(0, 100, 0.5, 0.5, true, ZoomTiming::default());
        assert!(cursor_locked_click_sample(
            ClickSample {
                time_ms: 50,
                x: 0.5,
                y: 0.5,
                primary: true,
            },
            &timeline,
            &[event],
        )
        .is_none());
    }

    #[test]
    fn primary_and_secondary_click_pulses_keep_distinct_colours() {
        let primary = rasterize_click_pulse_tile(true, 0.25);
        let secondary = rasterize_click_pulse_tile(false, 0.25);
        assert_ne!(primary, secondary);
    }

    #[test]
    fn maps_the_full_standard_niri_cursor_set_to_tahoe() {
        assert_eq!(tahoe_cursor_for_niri_name("default"), TahoeCursor::Arrow);
        assert_eq!(
            tahoe_cursor_for_niri_name("pointer"),
            TahoeCursor::PointingHand
        );
        assert_eq!(tahoe_cursor_for_niri_name("text"), TahoeCursor::Text);
        assert_eq!(
            tahoe_cursor_for_niri_name("vertical-text"),
            TahoeCursor::VerticalText
        );
        assert_eq!(tahoe_cursor_for_niri_name("grab"), TahoeCursor::OpenHand);
        assert_eq!(
            tahoe_cursor_for_niri_name("grabbing"),
            TahoeCursor::ClosedHand
        );
        assert_eq!(
            tahoe_cursor_for_niri_name("nwse-resize"),
            TahoeCursor::ResizeNorthWestSouthEast
        );
        assert_eq!(tahoe_cursor_for_niri_name("zoom-in"), TahoeCursor::ZoomIn);
        assert_eq!(
            tahoe_cursor_for_niri_name("unknown-shape"),
            TahoeCursor::Arrow
        );
    }

    #[test]
    fn normalizes_cursor_samples_to_the_selected_monitor() {
        assert_eq!(
            cursor_sample(
                (2_880.0, 540.0),
                TahoeCursor::Text,
                1_920,
                0,
                1_920,
                1_080,
                42,
            ),
            CursorSample {
                time_ms: 42,
                x: 0.5,
                y: 0.5,
                kind: TahoeCursor::Text,
            }
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
    fn window_cast_selection_requires_the_exact_pipewire_node() {
        let casts = serde_json::json!([
            {
                "target": { "Window": { "id": 6 } },
                "pw_node_id": 195
            },
            {
                "target": { "Window": { "id": 14 } },
                "pw_node_id": 222
            }
        ]);
        assert_eq!(niri_window_id_for_node(&casts, 222), Some(14));
        assert_eq!(niri_window_id_for_node(&casts, 999), None);
    }

    #[test]
    fn window_cast_selection_never_falls_back_to_a_stale_cast() {
        let stale_casts = serde_json::json!([{
            "target": { "Window": { "id": 6 } },
            "pw_node_id": 195
        }]);
        assert_eq!(niri_window_id_for_node(&stale_casts, 222), None);
    }

    #[test]
    fn window_crop_is_clamped_to_the_output_with_the_largest_overlap() {
        let layout = serde_json::json!({
            "DP-3": {
                "logical": { "x": 0, "y": 0, "width": 1920, "height": 1080 }
            },
            "HDMI-A-1": {
                "logical": { "x": 1920, "y": 0, "width": 1920, "height": 1080 }
            }
        });
        let crop = clamp_window_crop_to_output_layout(
            CaptureCrop {
                x: 998,
                y: 25,
                width: 940,
                height: 1000,
                excluded_bottom: 0,
            },
            &layout,
        );
        assert_eq!(crop.x, 998);
        assert_eq!(crop.y, 25);
        assert_eq!(crop.width, 922);
        assert_eq!(crop.height, 1000);
    }

    #[test]
    fn cursor_command_file_omits_repeated_stationary_positions() {
        let path = temporary_file("cursor-command-dedup-test", "txt");
        let timeline = CursorTimeline::new(vec![
            CursorSample {
                time_ms: 0,
                x: 0.5,
                y: 0.5,
                kind: TahoeCursor::Arrow,
            },
            CursorSample {
                time_ms: 1_000,
                x: 0.5,
                y: 0.5,
                kind: TahoeCursor::Arrow,
            },
        ]);
        let track = build_cursor_track(&timeline, &[], 1_920, 1_080, 1_000, 120, 0, 100)
            .expect("stationary cursor track should be built");
        write_cursor_commands(&path, &track)
            .expect("stationary cursor command file should be written");
        let contents = std::fs::read_to_string(&path).expect("command file should be readable");
        assert_eq!(contents.lines().count(), 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn cursor_tile_preserves_fractional_position_with_alpha_distribution() {
        let cursor = image::RgbaImage::from_pixel(1, 1, image::Rgba([255, 255, 255, 255]));
        let tile = rasterize_cursor_tile(
            &cursor,
            CursorTileFrame {
                time_ms: 0,
                origin_x: 0,
                origin_y: 0,
                fractional_x: 0.5,
                fractional_y: 0.5,
                visible: true,
                kind: TahoeCursor::Arrow,
            },
            CursorMotionBlur::default(),
        );
        for (x, y) in [
            (CURSOR_TILE_MARGIN as u32, CURSOR_TILE_MARGIN as u32),
            (CURSOR_TILE_MARGIN as u32 + 1, CURSOR_TILE_MARGIN as u32),
            (CURSOR_TILE_MARGIN as u32, CURSOR_TILE_MARGIN as u32 + 1),
            (CURSOR_TILE_MARGIN as u32 + 1, CURSOR_TILE_MARGIN as u32 + 1),
        ] {
            let alpha = tile[((y * CURSOR_TILE_WIDTH + x) * 4 + 3) as usize];
            assert_eq!(alpha, 64);
        }
    }

    #[test]
    fn cursor_smoothing_only_changes_the_rendered_cursor_track() {
        let timeline = CursorTimeline::new(vec![
            CursorSample {
                time_ms: 0,
                x: 0.1,
                y: 0.5,
                kind: TahoeCursor::Arrow,
            },
            CursorSample {
                time_ms: 1_000,
                x: 0.9,
                y: 0.5,
                kind: TahoeCursor::Arrow,
            },
        ]);
        let raw = build_cursor_track(&timeline, &[], 1_920, 1_080, 1_000, 120, 0, 100)
            .expect("raw cursor track should be built");
        let smoothed = build_cursor_track(&timeline, &[], 1_920, 1_080, 1_000, 120, 100, 100)
            .expect("smoothed cursor track should be built");
        let frame = 60;
        assert!(smoothed.frames[frame].origin_x < raw.frames[frame].origin_x);
        assert_eq!(smoothed.frames[frame].kind, raw.frames[frame].kind);
    }

    #[test]
    fn recordly_cursor_motion_blur_uses_the_source_thresholds() {
        let frame = |time_ms, origin_x| CursorTileFrame {
            time_ms,
            origin_x,
            origin_y: 0,
            fractional_x: 0.0,
            fractional_y: 0.0,
            visible: true,
            kind: TahoeCursor::Arrow,
        };
        assert_eq!(
            recordly_cursor_motion_blur(frame(0, 0), frame(80, 0)),
            CursorMotionBlur::default()
        );
        let subtle = recordly_cursor_motion_blur(frame(0, 0), frame(80, 1));
        assert_eq!(subtle.kernel_size, 5);
        assert_eq!(subtle.offset, 0.0);
        let offset = recordly_cursor_motion_blur(frame(0, 0), frame(80, 2));
        assert!((offset.velocity_x - 0.8).abs() < 1e-12);
        assert_eq!(offset.kernel_size, 5);
        assert_eq!(offset.offset, -0.25);
        assert_eq!(
            recordly_cursor_motion_blur(frame(0, 0), frame(80, 3)).kernel_size,
            7
        );
        assert_eq!(
            recordly_cursor_motion_blur(frame(0, 0), frame(80, 8)).kernel_size,
            9
        );
    }

    #[test]
    fn directional_cursor_blur_spreads_alpha_only_along_motion() {
        let cursor = image::RgbaImage::from_pixel(1, 1, image::Rgba([255, 255, 255, 255]));
        let tile = rasterize_cursor_tile(
            &cursor,
            CursorTileFrame {
                time_ms: 8,
                origin_x: 0,
                origin_y: 0,
                fractional_x: 0.0,
                fractional_y: 0.0,
                visible: true,
                kind: TahoeCursor::Arrow,
            },
            CursorMotionBlur {
                velocity_x: 4.0,
                velocity_y: 0.0,
                kernel_size: 9,
                offset: -0.25,
            },
        );
        let alpha_at = |x: u32, y: u32| tile[((y * CURSOR_TILE_WIDTH + x) * 4 + 3) as usize];
        let centre_y = CURSOR_TILE_MARGIN as u32;
        let horizontal_coverage = (0..CURSOR_TILE_WIDTH)
            .filter(|x| alpha_at(*x, centre_y) > 0)
            .count();
        assert!(horizontal_coverage > 1);
        assert!((0..CURSOR_TILE_WIDTH).all(|x| alpha_at(x, centre_y - 1) == 0));
        assert!((0..CURSOR_TILE_WIDTH).all(|x| alpha_at(x, centre_y + 1) == 0));
    }

    #[test]
    fn recordly_tahoe_hotspot_matches_the_source_asset_metadata() {
        let metrics = tahoe_cursor_metrics(TahoeCursor::Arrow).expect("Arrow metrics should load");
        let (x, y) = tahoe_cursor_hotspot_offset(metrics, 1.0);
        assert!((x - 7.23).abs() < 0.01);
        assert!((y - 4.8).abs() < f64::EPSILON);
    }

    #[test]
    fn cursor_uses_the_same_padded_stage_as_the_recording() {
        let point = CursorPoint { x: 0.8, y: 0.8 };
        let metrics = tahoe_cursor_metrics(TahoeCursor::Arrow).expect("Arrow metrics should load");
        let (x, y) = recordly_stage_cursor_position(point, metrics, 1_920, 1_080, 1.0);
        let stage_scale = 994.0 / 1_080.0;
        let (hotspot_x, hotspot_y) = tahoe_cursor_hotspot_offset(metrics, stage_scale);
        assert!((x + hotspot_x - 1_489.8).abs() < 0.001);
        assert!((y + hotspot_y - 838.2).abs() < 0.001);
    }

    #[test]
    fn click_effect_is_centered_on_the_same_padded_stage() {
        let (x, y) = recordly_stage_click_position(CursorPoint { x: 0.5, y: 0.5 }, 1_920, 1_080);
        assert!((x + CLICK_TILE_SIZE as f64 / 2.0 - 960.0).abs() < f64::EPSILON);
        assert!((y + CLICK_TILE_SIZE as f64 / 2.0 - 540.0).abs() < f64::EPSILON);
        let pulse = rasterize_click_pulse_tile(true, 0.0);
        let center = CLICK_TILE_SIZE as usize / 2;
        let alpha = pulse[(center * CLICK_TILE_SIZE as usize + center) * 4 + 3];
        assert!(alpha > 0, "a new pulse must be visible at its centre");
    }

    #[test]
    fn recording_settings_accept_the_supported_high_frame_rates() {
        let settings = RecordingSettings {
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
    fn audio_mix_filter_keeps_system_and_microphone_at_full_gain() {
        assert_eq!(
            audio_mix_filter(&AudioSettings {
                system_enabled: true,
                microphone_enabled: true,
                system_volume: 1.0,
                microphone_volume: 1.0,
                system_device: None,
                microphone_device: None,
            }),
            "[0:a]volume=1.000[system];[1:a]volume=1.000[microphone];[system][microphone]amix=inputs=2:normalize=0[mixed_audio]"
        );
    }

    #[test]
    fn audio_mix_filter_uses_only_the_enabled_source_at_its_selected_level() {
        let audio = AudioSettings {
            system_enabled: true,
            microphone_enabled: false,
            system_volume: 0.35,
            microphone_volume: 1.0,
            system_device: None,
            microphone_device: None,
        };
        assert_eq!(
            audio_mix_filter(&audio),
            "[0:a]volume=0.350[system];[system]amix=inputs=1:normalize=0[mixed_audio]"
        );
        assert_eq!(audio.label(), "system audio");
        assert!(audio.captures_audio());
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

    #[test]
    fn system_monitor_source_is_derived_from_the_default_sink() {
        assert_eq!(
            monitor_source_from_sink("alsa_output.example"),
            "alsa_output.example.monitor"
        );
    }

    #[test]
    fn recordly_scene_is_composited_before_camera_zoom() {
        let card_filter = recordly_card_filter(1_920, 1_080, 16_800, 120, 4, 5);
        assert!(card_filter.contains("tpad=stop_mode=clone:stop_duration=16.808333"));
        assert!(card_filter.contains("[0:v]setpts=PTS-STARTPTS,fps=120"));
        assert!(card_filter.contains("[4:v]format=gray"));
        assert!(card_filter.contains("[5:v]format=rgba[frame_shadow]"));
        assert!(card_filter.contains("[content][frame_mask]alphamerge[content_with_mask]"));
        assert!(card_filter
            .contains("[frame_shadow][content_with_mask]overlay=0:0:format=auto:shortest=1[card]"));
        assert!(!card_filter.contains("maskedmerge"));
        assert!(card_filter.contains("color=black[frame_mask]"));
        assert!(!card_filter.contains("format=yuv420p[frame_mask]"));
        assert!(card_filter.contains("scale=1766:994:flags=lanczos"));
        assert!(card_filter.contains("pad=1920:1080"));

        let wallpaper_filter = wallpaper_background_filter(3, "card", "scene");
        assert!(wallpaper_filter.contains("[3:v]format=rgba[wallpaper]"));
        assert!(wallpaper_filter
            .contains("[wallpaper][card]overlay=0:0:format=auto:shortest=1,format=rgba[scene]"));
        let camera_filter = camera_filter_chain(
            "scene",
            "screen",
            &PathBuf::from("/tmp/camera-commands.txt"),
            1_920,
            1_080,
        );
        let complete_filter = format!("{card_filter};{wallpaper_filter};{camera_filter}");
        assert!(
            complete_filter
                .find("[wallpaper][card]")
                .expect("scene merge")
                < complete_filter.find("crop@camera").expect("scene camera")
        );
        assert!(
            recordly_wallpaper_asset_path(BackgroundStyle::TahoeLight, None)
                .expect("wallpaper path lookup should succeed")
                .is_file()
        );
    }

    #[test]
    fn final_trim_is_part_of_the_single_render_graph() {
        let mut filter = "[scene]null[stage]".to_owned();
        append_final_trim_filter(&mut filter, "stage", Some(3), 1_250, 4_750);
        assert!(filter.contains(
            "[stage]trim=start=1.250000:end=4.750000,setpts=PTS-STARTPTS,format=yuv420p[out]"
        ));
        assert!(filter
            .contains("[3:a:0]atrim=start=1.250000:end=4.750000,asetpts=PTS-STARTPTS[out_audio]"));
    }

    #[test]
    fn custom_background_path_overrides_the_selected_builtin_wallpaper() {
        let custom = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("assets")
            .join("wallpapers")
            .join("tahoe-light.jpg");
        assert_eq!(
            recordly_wallpaper_asset_path(
                BackgroundStyle::TahoeDark,
                Some(custom.to_string_lossy().as_ref())
            )
            .expect("custom background should be accepted"),
            custom
        );
    }

    #[test]
    fn recordly_frame_assets_are_rasterized_before_final_render() {
        let assets = recordly_frame_assets(BackgroundStyle::TahoeDark, None, 320, 180)
            .expect("static frame assets should be created");
        assert_eq!(
            assets
                .mask_path
                .extension()
                .and_then(|value| value.to_str()),
            Some("png")
        );
        assert_eq!(
            assets
                .wallpaper_path
                .extension()
                .and_then(|value| value.to_str()),
            Some("png")
        );
        assert_eq!(
            assets
                .shadow_path
                .extension()
                .and_then(|value| value.to_str()),
            Some("png")
        );
        assert!(assets.mask_path.is_file());
        assert!(assets.wallpaper_path.is_file());
        assert!(assets.shadow_path.is_file());
        let _ = std::fs::remove_file(assets.mask_path);
        let _ = std::fs::remove_file(assets.wallpaper_path);
        let _ = std::fs::remove_file(assets.shadow_path);
    }

    #[test]
    fn solid_backgrounds_render_as_exact_black_and_white_pixels() {
        for (style, expected) in [
            (BackgroundStyle::PureBlack, 0_u8),
            (BackgroundStyle::PureWhite, 255_u8),
        ] {
            let assets = recordly_frame_assets(style, None, 32, 18)
                .expect("solid frame assets should be created");
            let decoded = Command::new("ffmpeg")
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-i",
                    assets.wallpaper_path.to_string_lossy().as_ref(),
                    "-frames:v",
                    "1",
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                    "rgb24",
                    "pipe:1",
                ])
                .output()
                .expect("solid wallpaper should be decodable");
            assert!(decoded.status.success());
            assert!(!decoded.stdout.is_empty());
            assert!(decoded.stdout.iter().all(|channel| *channel == expected));
            let _ = std::fs::remove_file(assets.mask_path);
            let _ = std::fs::remove_file(assets.wallpaper_path);
            let _ = std::fs::remove_file(assets.shadow_path);
        }
    }

    #[test]
    fn ffmpeg_zooms_the_fully_composited_scene() {
        let commands_path = temporary_file("recordly-scene-camera-test", "txt");
        let output_path = temporary_file("recordly-scene-camera-test", "mp4");
        fs::write(
            &commands_path,
            "0.000000-1.000000 [enter] crop@camera w 178, [enter] crop@camera h 100, [enter] crop@camera x 71, [enter] crop@camera y 40;\n",
        )
        .expect("camera command fixture should be written");
        let mut filter = recordly_card_filter(320, 180, 1_000, 30, 2, 3);
        filter.push(';');
        filter.push_str(&wallpaper_background_filter(1, "card", "scene"));
        filter.push_str(";[scene]null[scene0];");
        filter.push_str(&camera_filter_chain(
            "scene0",
            "screen",
            &commands_path,
            320,
            180,
        ));
        let output = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x180:rate=30:duration=1",
                "-f",
                "lavfi",
                "-i",
                "color=c=0x2255aa:size=320x180:rate=30",
                "-f",
                "lavfi",
                "-i",
                "color=c=white:size=294x166:rate=30",
                "-f",
                "lavfi",
                "-i",
                "color=c=black@0.35:size=320x180:rate=30,format=rgba",
                "-filter_complex",
                &filter,
                "-map",
                "[screen]",
                "-frames:v",
                "2",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                output_path.to_string_lossy().as_ref(),
            ])
            .output()
            .expect("FFmpeg card-composite fixture should run");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            fs::metadata(&output_path)
                .map(|metadata| metadata.len() > 0)
                .unwrap_or(false),
            "FFmpeg should produce a readable card-composite video"
        );
        let _ = fs::remove_file(commands_path);
        let _ = fs::remove_file(output_path);
    }

    #[test]
    fn webcam_filter_renders_a_generated_camera_source() {
        let mut webcam = WebcamSettings::default();
        webcam.size_percent = 40;
        webcam.margin = 12;
        let assets = recordly_webcam_frame_assets(320, 180, &webcam)
            .expect("webcam mask should be rasterized");
        let filter = format!(
            "[0:v]setpts=PTS-STARTPTS[stage0]{};[stage_webcam]format=yuv420p[out]",
            webcam_overlay_filter(
                "stage0",
                1,
                2,
                assets.layout,
                true,
                webcam.shadow,
                1_000,
                30,
            )
        );
        let output_path = temporary_file("webcam-filter-probe", "mp4");
        let output = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=#2f2f2f:s=320x180:r=30",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=160x90:r=30",
                "-loop",
                "1",
                "-framerate",
                "30",
                "-i",
                assets.mask_path.to_string_lossy().as_ref(),
                "-filter_complex",
                &filter,
                "-map",
                "[out]",
                "-frames:v",
                "1",
                output_path.to_string_lossy().as_ref(),
            ])
            .output()
            .expect("ffmpeg webcam probe should start");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            source_video_dimensions(&output_path).expect("probe output should be valid video"),
            (320, 180)
        );
        let _ = std::fs::remove_file(assets.mask_path);
        let _ = std::fs::remove_file(output_path);
    }
}
