use pipewire as pw;
use pw::{properties::properties, spa};
use spa::{
    buffer::meta::{MetaBitmap, MetaCursor},
    pod::Pod,
};
use std::{
    env, fs,
    io::Cursor,
    os::fd::OwnedFd,
    path::PathBuf,
    sync::mpsc::{self, Receiver, SyncSender},
    thread::{self, JoinHandle},
    time::Duration,
};

use crate::cursor::TahoeCursor;

const MAX_CURSOR_WIDTH: usize = 256;
const MAX_CURSOR_HEIGHT: usize = 256;
const CURSOR_META_BYTES: usize = std::mem::size_of::<spa::sys::spa_meta_cursor>()
    + std::mem::size_of::<spa::sys::spa_meta_bitmap>()
    + MAX_CURSOR_WIDTH * MAX_CURSOR_HEIGHT * 4;
const COLLECTOR_START_TIMEOUT: Duration = Duration::from_secs(3);
const STREAM_SIZE_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortalCursorBitmap {
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortalCursorObservation {
    pub cursor_id: u32,
    pub x: i32,
    pub y: i32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    pub stream_width: u32,
    pub stream_height: u32,
    pub bitmap_width: Option<u32>,
    pub bitmap_height: Option<u32>,
    pub bitmap: Option<PortalCursorBitmap>,
}

impl PortalCursorObservation {
    pub fn is_inside_stream(&self) -> bool {
        self.x >= 0
            && self.y >= 0
            && self.x < self.stream_width as i32
            && self.y < self.stream_height as i32
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PortalCursorMatch {
    pub kind: TahoeCursor,
    pub name: String,
    pub score: f64,
}

pub struct PortalCursorClassifier {
    theme_name: String,
    candidates: Vec<CursorCandidate>,
}

#[derive(Clone)]
struct CursorCandidate {
    kind: TahoeCursor,
    name: String,
    width: u32,
    height: u32,
    hotspot_x: u32,
    hotspot_y: u32,
    alpha: Vec<u8>,
}

const STANDARD_CURSOR_NAMES: &[&str] = &[
    "default",
    "context-menu",
    "help",
    "pointer",
    "progress",
    "wait",
    "cell",
    "crosshair",
    "text",
    "vertical-text",
    "alias",
    "copy",
    "move",
    "no-drop",
    "not-allowed",
    "grab",
    "grabbing",
    "e-resize",
    "n-resize",
    "ne-resize",
    "nw-resize",
    "s-resize",
    "se-resize",
    "sw-resize",
    "w-resize",
    "ew-resize",
    "ns-resize",
    "nesw-resize",
    "nwse-resize",
    "col-resize",
    "row-resize",
    "all-scroll",
    "zoom-in",
    "zoom-out",
    "dnd-ask",
    "all-resize",
];

impl PortalCursorClassifier {
    pub fn from_environment() -> Self {
        let theme_name = active_cursor_theme_name();
        let theme = xcursor::CursorTheme::load(&theme_name);
        let mut candidates = Vec::new();
        for name in STANDARD_CURSOR_NAMES {
            let Some(path) = theme.load_icon(name) else {
                continue;
            };
            let Some(images) = fs::read(path)
                .ok()
                .and_then(|bytes| xcursor::parser::parse_xcursor(&bytes))
            else {
                continue;
            };
            for image in images {
                if image.pixels_rgba.len()
                    != image.width.saturating_mul(image.height).saturating_mul(4) as usize
                {
                    continue;
                }
                candidates.push(CursorCandidate {
                    kind: TahoeCursor::from_standard_name(name),
                    name: (*name).to_owned(),
                    width: image.width,
                    height: image.height,
                    hotspot_x: image.xhot,
                    hotspot_y: image.yhot,
                    alpha: image
                        .pixels_rgba
                        .chunks_exact(4)
                        .map(|pixel| pixel[3])
                        .collect(),
                });
            }
        }
        Self {
            theme_name,
            candidates,
        }
    }

    pub fn theme_name(&self) -> &str {
        &self.theme_name
    }

    pub fn candidate_count(&self) -> usize {
        self.candidates.len()
    }

    pub fn classify(
        &self,
        bitmap: &PortalCursorBitmap,
        hotspot_x: i32,
        hotspot_y: i32,
    ) -> Option<PortalCursorMatch> {
        let alpha = bitmap_alpha(bitmap)?;
        if alpha.iter().all(|value| *value == 0) {
            return None;
        }
        let mut best: Option<PortalCursorMatch> = None;
        for candidate in &self.candidates {
            let exact_size = candidate.width == bitmap.width && candidate.height == bitmap.height;
            let candidate_alpha = if exact_size {
                candidate.alpha.clone()
            } else {
                resize_alpha(
                    &candidate.alpha,
                    candidate.width,
                    candidate.height,
                    bitmap.width,
                    bitmap.height,
                )?
            };
            let alpha_score = mean_alpha_distance(&alpha, &candidate_alpha)?;
            let expected_hotspot_x =
                candidate.hotspot_x as f64 * bitmap.width as f64 / candidate.width.max(1) as f64;
            let expected_hotspot_y =
                candidate.hotspot_y as f64 * bitmap.height as f64 / candidate.height.max(1) as f64;
            let hotspot_score = ((hotspot_x as f64 - expected_hotspot_x).abs()
                / bitmap.width.max(1) as f64
                + (hotspot_y as f64 - expected_hotspot_y).abs() / bitmap.height.max(1) as f64)
                * 0.25;
            let scaling_penalty = if exact_size { 0.0 } else { 0.015 };
            let score = alpha_score + hotspot_score + scaling_penalty;
            if best.as_ref().is_none_or(|current| score < current.score) {
                best = Some(PortalCursorMatch {
                    kind: candidate.kind,
                    name: candidate.name.clone(),
                    score,
                });
            }
        }
        best.filter(|matched| matched.score <= 0.16)
    }
}

fn active_cursor_theme_name() -> String {
    if let Some(theme) = env::var_os("XCURSOR_THEME").filter(|value| !value.is_empty()) {
        return theme.to_string_lossy().into_owned();
    }
    let config_path = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|path| path.join("kcminputrc"));
    let Some(contents) = config_path.and_then(|path| fs::read_to_string(path).ok()) else {
        return "default".to_owned();
    };
    let mut in_mouse_section = false;
    for line in contents.lines().map(str::trim) {
        if line.starts_with('[') && line.ends_with(']') {
            in_mouse_section = line == "[Mouse]";
            continue;
        }
        if in_mouse_section {
            if let Some(theme) = line.strip_prefix("cursorTheme=") {
                if !theme.trim().is_empty() {
                    return theme.trim().to_owned();
                }
            }
        }
    }
    "default".to_owned()
}

#[derive(Debug)]
pub enum PortalCursorEvent {
    Observation(PortalCursorObservation),
    Error(String),
}

pub struct PortalCursorCollector {
    stop: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

pub fn probe_stream_size(pipewire_fd: OwnedFd, node_id: u32) -> Result<(u32, u32), String> {
    let (result_sender, result_receiver) = mpsc::channel();
    let (stop_sender, stop_receiver) = pw::channel::channel();
    let thread = thread::Builder::new()
        .name("pikscreen-portal-size-probe".to_owned())
        .spawn(move || {
            if let Err(error) =
                run_stream_size_probe(pipewire_fd, node_id, result_sender.clone(), stop_receiver)
            {
                let _ = result_sender.send(Err(error));
            }
        })
        .map_err(|error| format!("Could not start the PipeWire size probe: {error}"))?;

    let result = result_receiver.recv_timeout(STREAM_SIZE_PROBE_TIMEOUT);
    let _ = stop_sender.send(());
    thread
        .join()
        .map_err(|_| "PipeWire size probe panicked.".to_owned())?;
    match result {
        Ok(result) => result,
        Err(error) => Err(format!(
            "PipeWire did not report the selected source size within {:.1}s: {error}",
            STREAM_SIZE_PROBE_TIMEOUT.as_secs_f64()
        )),
    }
}

fn run_stream_size_probe(
    pipewire_fd: OwnedFd,
    node_id: u32,
    result_sender: mpsc::Sender<Result<(u32, u32), String>>,
    stop_receiver: pw::channel::Receiver<()>,
) -> Result<(), String> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|error| format!("Could not create the PipeWire size loop: {error}"))?;
    let context = pw::context::ContextRc::new(&mainloop, None)
        .map_err(|error| format!("Could not create the PipeWire size context: {error}"))?;
    let core = context
        .connect_fd_rc(pipewire_fd, None)
        .map_err(|error| format!("Could not connect the PipeWire size remote: {error}"))?;
    let stream = pw::stream::StreamBox::new(
        &core,
        "pikscreen-stream-size",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|error| format!("Could not create the PipeWire size stream: {error}"))?;

    let stop_loop = mainloop.clone();
    let _stop_receiver = stop_receiver.attach(mainloop.loop_(), move |_| stop_loop.quit());
    let error_loop = mainloop.clone();
    let error_sender = result_sender.clone();
    let format_loop = mainloop.clone();
    let format_sender = result_sender.clone();
    let _listener = stream
        .add_local_listener::<()>()
        .state_changed(move |_, _, _, state| {
            if let pw::stream::StreamState::Error(error) = state {
                let _ = error_sender.send(Err(format!("PipeWire size stream failed: {error}")));
                error_loop.quit();
            }
        })
        .param_changed(move |_, _, id, param| {
            let Some(param) = param else {
                return;
            };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = spa::param::format_utils::parse_format(param)
            else {
                return;
            };
            if media_type != spa::param::format::MediaType::Video
                || media_subtype != spa::param::format::MediaSubtype::Raw
            {
                return;
            }
            let mut format = spa::param::video::VideoInfoRaw::default();
            if format.parse(param).is_err() {
                return;
            }
            let size = format.size();
            if size.width >= 2 && size.height >= 2 {
                let _ = format_sender.send(Ok((size.width, size.height)));
                format_loop.quit();
            }
        })
        .register()
        .map_err(|error| format!("Could not register the PipeWire size listener: {error}"))?;

    let format_bytes = stream_size_format_param_bytes()?;
    let format_pod = Pod::from_bytes(&format_bytes)
        .ok_or_else(|| "Could not build the PipeWire size format pod.".to_owned())?;
    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT,
            &mut [format_pod],
        )
        .map_err(|error| format!("Could not connect the PipeWire size stream: {error}"))?;
    mainloop.run();
    Ok(())
}

impl PortalCursorCollector {
    pub fn stop(mut self) -> Result<(), String> {
        let _ = self.stop.send(());
        self.join()
    }

    fn join(&mut self) -> Result<(), String> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        thread
            .join()
            .map_err(|_| "PipeWire cursor metadata collector panicked.".to_owned())
    }
}

impl Drop for PortalCursorCollector {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        let _ = self.join();
    }
}

pub fn spawn_collector(
    pipewire_fd: OwnedFd,
    node_id: u32,
    requested_width: u32,
    requested_height: u32,
) -> Result<(PortalCursorCollector, Receiver<PortalCursorEvent>), String> {
    let (event_sender, event_receiver) = mpsc::channel();
    let (startup_sender, startup_receiver) = mpsc::sync_channel(1);
    let (stop_sender, stop_receiver) = pw::channel::channel();
    let thread = thread::Builder::new()
        .name("pikscreen-portal-cursor".to_owned())
        .spawn(move || {
            if let Err(error) = run_collector(
                pipewire_fd,
                node_id,
                requested_width,
                requested_height,
                event_sender.clone(),
                startup_sender.clone(),
                stop_receiver,
            ) {
                let _ = startup_sender.send(Err(error.clone()));
                let _ = event_sender.send(PortalCursorEvent::Error(error));
            }
        })
        .map_err(|error| format!("Could not start the cursor metadata thread: {error}"))?;

    match startup_receiver.recv_timeout(COLLECTOR_START_TIMEOUT) {
        Ok(Ok(())) => Ok((
            PortalCursorCollector {
                stop: stop_sender,
                thread: Some(thread),
            },
            event_receiver,
        )),
        Ok(Err(error)) => {
            let _ = stop_sender.send(());
            let _ = thread.join();
            Err(error)
        }
        Err(error) => {
            let _ = stop_sender.send(());
            let _ = thread.join();
            Err(format!(
                "PipeWire cursor metadata collector did not start within {:.1}s: {error}",
                COLLECTOR_START_TIMEOUT.as_secs_f64()
            ))
        }
    }
}

fn run_collector(
    pipewire_fd: OwnedFd,
    node_id: u32,
    requested_width: u32,
    requested_height: u32,
    event_sender: mpsc::Sender<PortalCursorEvent>,
    startup_sender: SyncSender<Result<(), String>>,
    stop_receiver: pw::channel::Receiver<()>,
) -> Result<(), String> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|error| format!("Could not create the PipeWire cursor loop: {error}"))?;
    let context = pw::context::ContextRc::new(&mainloop, None)
        .map_err(|error| format!("Could not create the PipeWire cursor context: {error}"))?;
    let core = context
        .connect_fd_rc(pipewire_fd, None)
        .map_err(|error| format!("Could not connect the PipeWire cursor remote: {error}"))?;
    let stream = pw::stream::StreamBox::new(
        &core,
        "pikscreen-cursor-metadata",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|error| format!("Could not create the PipeWire cursor stream: {error}"))?;

    let stop_loop = mainloop.clone();
    let _stop_receiver = stop_receiver.attach(mainloop.loop_(), move |_| stop_loop.quit());
    let listener_sender = event_sender.clone();
    let listener_startup = startup_sender.clone();
    let _listener = stream
        .add_local_listener_with_user_data(CursorStreamState {
            width: requested_width,
            height: requested_height,
        })
        .state_changed(move |_, _, _, state| {
            if let pw::stream::StreamState::Error(error) = state {
                let message = format!("PipeWire cursor metadata stream failed: {error}");
                let _ = listener_startup.send(Err(message.clone()));
                let _ = listener_sender.send(PortalCursorEvent::Error(message));
            }
        })
        .param_changed(|stream, state, id, param| {
            let Some(param) = param else {
                return;
            };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            if let Ok((media_type, media_subtype)) = spa::param::format_utils::parse_format(param) {
                if media_type == spa::param::format::MediaType::Video
                    && media_subtype == spa::param::format::MediaSubtype::Raw
                {
                    let mut format = spa::param::video::VideoInfoRaw::default();
                    if format.parse(param).is_ok() {
                        state.width = format.size().width;
                        state.height = format.size().height;
                    }
                }
            }
            if let Some(bytes) = cursor_meta_param_bytes() {
                if let Some(pod) = Pod::from_bytes(&bytes) {
                    let _ = stream.update_params(&mut [pod]);
                }
            }
        })
        .process(move |stream, state| {
            let Some(buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(cursor) = buffer.find_meta::<MetaCursor>() else {
                return;
            };
            if state.width == 0 || state.height == 0 {
                return;
            }
            let position = cursor.position();
            let hotspot = cursor.hotspot();
            let bitmap = cursor.bitmap();
            let (bitmap_width, bitmap_height) = bitmap
                .map(|bitmap| (Some(bitmap.size().width), Some(bitmap.size().height)))
                .unwrap_or((None, None));
            let bitmap = bitmap.and_then(copy_cursor_bitmap);
            let _ = event_sender.send(PortalCursorEvent::Observation(PortalCursorObservation {
                cursor_id: cursor.id(),
                x: position.x,
                y: position.y,
                hotspot_x: hotspot.x,
                hotspot_y: hotspot.y,
                stream_width: state.width,
                stream_height: state.height,
                bitmap_width,
                bitmap_height,
                bitmap,
            }));
        })
        .register()
        .map_err(|error| format!("Could not register the PipeWire cursor listener: {error}"))?;

    let format_bytes = cursor_format_param_bytes(requested_width, requested_height)?;
    let format_pod = Pod::from_bytes(&format_bytes)
        .ok_or_else(|| "Could not build the PipeWire cursor format pod.".to_owned())?;
    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT,
            &mut [format_pod],
        )
        .map_err(|error| format!("Could not connect the PipeWire cursor stream: {error}"))?;
    let _ = startup_sender.send(Ok(()));
    mainloop.run();
    Ok(())
}

struct CursorStreamState {
    width: u32,
    height: u32,
}

fn copy_cursor_bitmap(bitmap: &MetaBitmap) -> Option<PortalCursorBitmap> {
    let size = bitmap.size();
    if size.width == 0
        || size.height == 0
        || size.width as usize > MAX_CURSOR_WIDTH
        || size.height as usize > MAX_CURSOR_HEIGHT
    {
        return None;
    }
    let data = bitmap.bitmap_data()?;
    let pixels = pack_bitmap_rows(data, size.width, size.height, bitmap.stride())?;
    Some(PortalCursorBitmap {
        format: bitmap.format().0,
        width: size.width,
        height: size.height,
        pixels,
    })
}

fn pack_bitmap_rows(data: &[u8], width: u32, height: u32, stride: i32) -> Option<Vec<u8>> {
    let row_bytes = width.checked_mul(4)? as usize;
    let stride_bytes = stride.unsigned_abs() as usize;
    let height = height as usize;
    if row_bytes == 0
        || height == 0
        || stride_bytes < row_bytes
        || data.len() < stride_bytes.checked_mul(height)?
    {
        return None;
    }
    let mut pixels = Vec::with_capacity(row_bytes.checked_mul(height)?);
    for output_y in 0..height {
        let source_y = if stride < 0 {
            height - 1 - output_y
        } else {
            output_y
        };
        let start = source_y.checked_mul(stride_bytes)?;
        pixels.extend_from_slice(data.get(start..start + row_bytes)?);
    }
    Some(pixels)
}

fn bitmap_alpha(bitmap: &PortalCursorBitmap) -> Option<Vec<u8>> {
    let format = bitmap.format;
    let alpha_index = if format == spa::param::video::VideoFormat::BGRA.0
        || format == spa::param::video::VideoFormat::RGBA.0
    {
        3
    } else if format == spa::param::video::VideoFormat::ARGB.0
        || format == spa::param::video::VideoFormat::ABGR.0
    {
        0
    } else {
        return None;
    };
    let expected = bitmap.width.checked_mul(bitmap.height)?.checked_mul(4)? as usize;
    if bitmap.pixels.len() != expected {
        return None;
    }
    Some(
        bitmap
            .pixels
            .chunks_exact(4)
            .map(|pixel| pixel[alpha_index])
            .collect(),
    )
}

fn resize_alpha(
    source: &[u8],
    source_width: u32,
    source_height: u32,
    target_width: u32,
    target_height: u32,
) -> Option<Vec<u8>> {
    if source.len() != source_width.checked_mul(source_height)? as usize
        || source_width == 0
        || source_height == 0
        || target_width == 0
        || target_height == 0
    {
        return None;
    }
    let mut resized = Vec::with_capacity(target_width.checked_mul(target_height)? as usize);
    for y in 0..target_height {
        let source_y = (y as u64 * source_height as u64 / target_height as u64) as u32;
        for x in 0..target_width {
            let source_x = (x as u64 * source_width as u64 / target_width as u64) as u32;
            resized.push(source[(source_y * source_width + source_x) as usize]);
        }
    }
    Some(resized)
}

fn mean_alpha_distance(left: &[u8], right: &[u8]) -> Option<f64> {
    if left.is_empty() || left.len() != right.len() {
        return None;
    }
    let difference = left
        .iter()
        .zip(right)
        .map(|(left, right)| left.abs_diff(*right) as u64)
        .sum::<u64>();
    Some(difference as f64 / (left.len() as f64 * 255.0))
}

fn cursor_format_param_bytes(width: u32, height: u32) -> Result<Vec<u8>, String> {
    let format = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::RGBx,
            spa::param::video::VideoFormat::BGRA,
            spa::param::video::VideoFormat::RGBA,
            spa::param::video::VideoFormat::ARGB,
            spa::param::video::VideoFormat::ABGR,
            spa::param::video::VideoFormat::NV12,
            spa::param::video::VideoFormat::I420,
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle { width, height },
            spa::utils::Rectangle {
                width: 2,
                height: 2,
            },
            spa::utils::Rectangle {
                width: 32767,
                height: 32767,
            }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 60, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 240, denom: 1 }
        ),
    );
    spa::pod::serialize::PodSerializer::serialize(
        Cursor::new(Vec::new()),
        &spa::pod::Value::Object(format),
    )
    .map(|serialized| serialized.0.into_inner())
    .map_err(|error| format!("Could not serialize the PipeWire cursor format: {error}"))
}

fn stream_size_format_param_bytes() -> Result<Vec<u8>, String> {
    let format = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::RGBx,
            spa::param::video::VideoFormat::BGRA,
            spa::param::video::VideoFormat::RGBA,
            spa::param::video::VideoFormat::NV12,
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle {
                width: 1920,
                height: 1080,
            },
            spa::utils::Rectangle {
                width: 2,
                height: 2,
            },
            spa::utils::Rectangle {
                width: 32767,
                height: 32767,
            }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 60, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 240, denom: 1 }
        ),
    );
    spa::pod::serialize::PodSerializer::serialize(
        Cursor::new(Vec::new()),
        &spa::pod::Value::Object(format),
    )
    .map(|serialized| serialized.0.into_inner())
    .map_err(|error| format!("Could not serialize the PipeWire size format: {error}"))
}

fn cursor_meta_param_bytes() -> Option<Vec<u8>> {
    let metadata = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamMeta.as_raw(),
        id: spa::param::ParamType::Meta.as_raw(),
        properties: vec![
            spa::pod::Property::new(
                spa::sys::SPA_PARAM_META_type,
                spa::pod::Value::Id(spa::utils::Id(spa::sys::SPA_META_Cursor)),
            ),
            spa::pod::Property::new(
                spa::sys::SPA_PARAM_META_size,
                spa::pod::Value::Int(CURSOR_META_BYTES as i32),
            ),
        ],
    };
    spa::pod::serialize::PodSerializer::serialize(
        Cursor::new(Vec::new()),
        &spa::pod::Value::Object(metadata),
    )
    .ok()
    .map(|serialized| serialized.0.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_observation_detects_stream_bounds() {
        let mut observation = PortalCursorObservation {
            cursor_id: 7,
            x: 959,
            y: 539,
            hotspot_x: 4,
            hotspot_y: 3,
            stream_width: 960,
            stream_height: 540,
            bitmap_width: Some(48),
            bitmap_height: Some(48),
            bitmap: None,
        };
        assert!(observation.is_inside_stream());
        observation.x = 960;
        assert!(!observation.is_inside_stream());
        observation.x = -1;
        assert!(!observation.is_inside_stream());
    }

    #[test]
    fn cursor_metadata_pod_reserves_the_full_sprite() {
        let bytes = cursor_meta_param_bytes().expect("cursor metadata pod");
        assert!(bytes.len() > std::mem::size_of::<spa::sys::spa_pod>());
        assert!(CURSOR_META_BYTES >= MAX_CURSOR_WIDTH * MAX_CURSOR_HEIGHT * 4);
    }

    #[test]
    fn cursor_format_pod_accepts_the_portal_source_range() {
        let bytes = cursor_format_param_bytes(960, 540).expect("cursor format pod");
        assert!(Pod::from_bytes(&bytes).is_some());
        let value = spa::pod::deserialize::PodDeserializer::deserialize_any_from(&bytes)
            .expect("deserializable cursor format pod");
        assert!(format!("{value:?}").contains("32767"));
    }

    #[test]
    fn stream_size_probe_format_accepts_flexible_dimensions() {
        let bytes = stream_size_format_param_bytes().expect("size probe format pod");
        let value = spa::pod::deserialize::PodDeserializer::deserialize_any_from(&bytes)
            .expect("deserializable size probe pod")
            .1;
        let spa::pod::Value::Object(object) = value else {
            panic!("size probe should serialize an object");
        };
        let size = object
            .properties
            .iter()
            .find(|property| {
                property.key == spa::param::format::FormatProperties::VideoSize.as_raw()
            })
            .expect("video size property");
        assert!(matches!(
            size.value,
            spa::pod::Value::Choice(spa::pod::ChoiceValue::Rectangle(spa::utils::Choice(
                _,
                spa::utils::ChoiceEnum::Range { .. }
            )))
        ));
    }

    #[test]
    fn bitmap_rows_drop_padding_and_respect_negative_stride() {
        let data = [
            1, 2, 3, 4, 5, 6, 7, 8, 90, 91, 92, 93, 9, 10, 11, 12, 13, 14, 15, 16, 94, 95, 96, 97,
        ];
        assert_eq!(
            pack_bitmap_rows(&data, 2, 2, 12),
            Some(vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16])
        );
        assert_eq!(
            pack_bitmap_rows(&data, 2, 2, -12),
            Some(vec![9, 10, 11, 12, 13, 14, 15, 16, 1, 2, 3, 4, 5, 6, 7, 8])
        );
    }

    #[test]
    fn extracts_alpha_from_all_supported_cursor_formats() {
        let cases = [
            (spa::param::video::VideoFormat::BGRA.0, vec![1, 2, 3, 77]),
            (spa::param::video::VideoFormat::RGBA.0, vec![3, 2, 1, 77]),
            (spa::param::video::VideoFormat::ARGB.0, vec![77, 3, 2, 1]),
            (spa::param::video::VideoFormat::ABGR.0, vec![77, 1, 2, 3]),
        ];
        for (format, pixels) in cases {
            assert_eq!(
                bitmap_alpha(&PortalCursorBitmap {
                    format,
                    width: 1,
                    height: 1,
                    pixels,
                }),
                Some(vec![77])
            );
        }
    }

    #[test]
    fn classifier_selects_the_matching_semantic_shape() {
        let classifier = PortalCursorClassifier {
            theme_name: "test".to_owned(),
            candidates: vec![
                CursorCandidate {
                    kind: TahoeCursor::Arrow,
                    name: "default".to_owned(),
                    width: 2,
                    height: 2,
                    hotspot_x: 0,
                    hotspot_y: 0,
                    alpha: vec![255, 0, 0, 255],
                },
                CursorCandidate {
                    kind: TahoeCursor::Text,
                    name: "text".to_owned(),
                    width: 2,
                    height: 2,
                    hotspot_x: 1,
                    hotspot_y: 1,
                    alpha: vec![0, 255, 255, 0],
                },
            ],
        };
        let bitmap = bitmap_from_alpha(2, 2, &[0, 255, 255, 0]);
        let matched = classifier.classify(&bitmap, 1, 1).expect("text match");
        assert_eq!(matched.kind, TahoeCursor::Text);
        assert_eq!(matched.name, "text");
        assert!(matched.score < f64::EPSILON);
    }

    #[test]
    fn classifier_handles_a_compositor_scaled_cursor() {
        let classifier = PortalCursorClassifier {
            theme_name: "test".to_owned(),
            candidates: vec![CursorCandidate {
                kind: TahoeCursor::PointingHand,
                name: "pointer".to_owned(),
                width: 2,
                height: 2,
                hotspot_x: 1,
                hotspot_y: 0,
                alpha: vec![255, 0, 0, 255],
            }],
        };
        let scaled = resize_alpha(&[255, 0, 0, 255], 2, 2, 4, 4).expect("scaled alpha");
        let bitmap = bitmap_from_alpha(4, 4, &scaled);
        let matched = classifier
            .classify(&bitmap, 2, 0)
            .expect("scaled pointer match");
        assert_eq!(matched.kind, TahoeCursor::PointingHand);
        assert!(matched.score <= 0.015 + f64::EPSILON);
    }

    fn bitmap_from_alpha(width: u32, height: u32, alpha: &[u8]) -> PortalCursorBitmap {
        PortalCursorBitmap {
            format: spa::param::video::VideoFormat::BGRA.0,
            width,
            height,
            pixels: alpha.iter().flat_map(|alpha| [0, 0, 0, *alpha]).collect(),
        }
    }
}
