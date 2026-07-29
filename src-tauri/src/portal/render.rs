use super::*;
use crate::editor::Segment;

pub(super) const RECORDLY_TAHOE_CURSOR_HEIGHT: f64 = 80.0;

pub(super) const RECORDLY_TAHOE_CURSOR_HOTSPOT_X: f64 = 0.14;

pub(super) const RECORDLY_TAHOE_CURSOR_HOTSPOT_Y: f64 = 0.06;

// Some of Tahoe's stateful cursors (notably NotAllowed) are wider than the
// pointer asset once they are scaled for the recording.  Keep a generous,
// fixed transparent tile so every cursor state can use the same raw-video
// stream without clipping or changing the overlay coordinate system.
pub(super) const CURSOR_TILE_WIDTH: u32 = 256;

pub(super) const CURSOR_TILE_HEIGHT: u32 = 256;

pub(super) const CURSOR_TILE_MARGIN: i32 = 64;

pub(super) const RECORDLY_CURSOR_MOTION_BLUR: f64 = 0.4;

pub(super) const RECORDLY_CURSOR_MOTION_BLUR_MULTIPLIER: f64 = 0.08;

pub(super) const RECORDLY_CURSOR_MOTION_BLUR_THRESHOLD: f64 = 0.05;

pub(super) const CLICK_PULSE_DURATION_MS: u64 = 360;

pub(super) const CLICK_TILE_SIZE: u32 = 160;

#[derive(Clone, Debug, Serialize)]
pub(super) struct ProcessingProgress {
    pub(super) stage: String,
    pub(super) percent: u8,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct CursorTileFrame {
    pub(super) time_ms: u64,
    pub(super) origin_x: i32,
    pub(super) origin_y: i32,
    pub(super) fractional_x: f64,
    pub(super) fractional_y: f64,
    pub(super) visible: bool,
    pub(super) kind: TahoeCursor,
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
pub(super) struct CursorMotionBlur {
    pub(super) velocity_x: f64,
    pub(super) velocity_y: f64,
    pub(super) kernel_size: usize,
    pub(super) offset: f64,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct TahoeCursorMetrics {
    pub(super) aspect_ratio: f64,
    pub(super) hotspot_x: f64,
    pub(super) hotspot_y: f64,
}

#[derive(Debug)]
pub(super) struct CursorTrack {
    pub(super) frames: Vec<CursorTileFrame>,
}

#[derive(Debug)]
pub(super) struct ClickEffectTrack {
    pub(super) path: PathBuf,
    pub(super) commands_path: PathBuf,
    pub(super) start_ms: u64,
}

pub(super) fn source_video_dimensions(source_path: &PathBuf) -> Result<(u32, u32), String> {
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

pub(super) fn render_final_video(
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
    segments: &[Segment],
    cursor_strategy: CursorStrategy,
    settings: RecordingSettings,
) -> Result<(), String> {
    let scene = settings.background_enabled;
    let (_, content_height) = recordly_padded_size(width, height, scene);
    let frame_assets = if scene {
        Some(recordly_frame_assets(
            settings.background,
            settings.custom_background_path.as_deref(),
            width,
            height,
        )?)
    } else {
        None
    };
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
                    scene,
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
            scene,
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
            scene,
        )?;
    }
    let cursor_input_index = cursor_render.as_ref().map(|_| 1);
    let click_input_start = 1 + usize::from(cursor_input_index.is_some());
    let audio_input_index = click_input_start + click_tracks.len();
    // Wallpaper, mask and shadow are only fed to ffmpeg when the scene is on,
    // so everything after them shifts by three.
    let wallpaper_input_index = audio_input_index + usize::from(audio_path.is_some());
    let mask_input_index = wallpaper_input_index + 1;
    let shadow_input_index = wallpaper_input_index + 2;
    let webcam_input_index = wallpaper_input_index + if scene { 3 } else { 0 };
    let webcam_mask_input_index = webcam_input_index + 1;
    let mut filter = if scene {
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
        filter
    } else {
        full_frame_scene_filter(width, height, duration_ms, settings.fps)
    };
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
    append_segment_filter(
        &mut filter,
        &stage_label,
        audio_path.is_some().then_some(audio_input_index),
        segments,
    );
    let output_duration_ms: u64 = segments.iter().map(Segment::duration_ms).sum();
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
    if let Some(frame_assets) = frame_assets.as_ref() {
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
    }
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

/// Cut the composed timeline down to the segments that survive, in order.
///
/// The scene is built over the whole recording, so every overlay is already
/// where it belongs in source time; taking pieces out of the end is what makes
/// a cut. One segment is the old single trim, and is emitted without a split so
/// the untouched case produces the graph it always did.
pub(super) fn append_segment_filter(
    filter: &mut String,
    stage_label: &str,
    audio_input_index: Option<usize>,
    segments: &[Segment],
) {
    let seconds = |ms: u64| ms as f64 / 1_000.0;

    if let [only] = segments {
        filter.push_str(&format!(
            ";[{stage_label}]trim=start={:.6}:end={:.6},setpts=PTS-STARTPTS,format=yuv420p[out]",
            seconds(only.start_ms),
            seconds(only.end_ms)
        ));
    } else {
        let count = segments.len();
        filter.push_str(&format!(";[{stage_label}]split={count}"));
        for index in 0..count {
            filter.push_str(&format!("[seg{index}]"));
        }
        for (index, segment) in segments.iter().enumerate() {
            filter.push_str(&format!(
                ";[seg{index}]trim=start={:.6}:end={:.6},setpts=PTS-STARTPTS[cut{index}]",
                seconds(segment.start_ms),
                seconds(segment.end_ms)
            ));
        }
        filter.push(';');
        for index in 0..count {
            filter.push_str(&format!("[cut{index}]"));
        }
        filter.push_str(&format!("concat=n={count}:v=1:a=0,format=yuv420p[out]"));
    }

    let Some(audio_input_index) = audio_input_index else {
        return;
    };
    if let [only] = segments {
        filter.push_str(&format!(
            ";[{audio_input_index}:a:0]atrim=start={:.6}:end={:.6},asetpts=PTS-STARTPTS[out_audio]",
            seconds(only.start_ms),
            seconds(only.end_ms)
        ));
        return;
    }
    let count = segments.len();
    filter.push_str(&format!(";[{audio_input_index}:a:0]asplit={count}"));
    for index in 0..count {
        filter.push_str(&format!("[aseg{index}]"));
    }
    for (index, segment) in segments.iter().enumerate() {
        filter.push_str(&format!(
            ";[aseg{index}]atrim=start={:.6}:end={:.6},asetpts=PTS-STARTPTS[acut{index}]",
            seconds(segment.start_ms),
            seconds(segment.end_ms)
        ));
    }
    filter.push(';');
    for index in 0..count {
        filter.push_str(&format!("[acut{index}]"));
    }
    filter.push_str(&format!("concat=n={count}:v=0:a=1[out_audio]"));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct WebcamLayout {
    pub(super) size: u32,
    pub(super) x: u32,
    pub(super) y: u32,
    pub(super) roundness: u32,
}

pub(super) struct WebcamFrameAssets {
    pub(super) layout: WebcamLayout,
    pub(super) mask_path: PathBuf,
}

pub(super) fn recordly_webcam_layout(
    width: u32,
    height: u32,
    webcam: &WebcamSettings,
) -> WebcamLayout {
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

pub(super) fn recordly_webcam_frame_assets(
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

pub(super) fn webcam_overlay_filter(
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

pub(super) fn camera_filter_chain(
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

pub(super) fn emit_processing_progress(app: &AppHandle, stage: &str, percent: f64) {
    let _ = app.emit(
        "processing-progress",
        ProcessingProgress {
            stage: stage.to_owned(),
            percent: percent.round().clamp(0.0, 100.0) as u8,
        },
    );
}

pub(super) fn run_ffmpeg_with_progress(
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

pub(super) fn build_cursor_track(
    timeline: &CursorTimeline,
    events: &[crate::zoom::ZoomEvent],
    width: u32,
    height: u32,
    duration_ms: u64,
    fps: u32,
    cursor_smoothing: u8,
    cursor_size: u8,
    scene: bool,
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
                scene,
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

pub(super) fn build_click_effect_tracks(
    scene: bool,
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
        write_click_effect_commands(
            &commands_path,
            tracks.len(),
            sample,
            width,
            height,
            fps,
            scene,
        )?;
        tracks.push(ClickEffectTrack {
            path,
            commands_path,
            start_ms: sample.time_ms,
        });
    }
    Ok(tracks)
}

pub(super) fn cursor_locked_click_sample(
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

pub(super) fn write_click_effect_frames(
    path: &PathBuf,
    primary: bool,
    fps: u32,
) -> Result<(), String> {
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

pub(super) fn rasterize_click_pulse_tile(primary: bool, progress: f64) -> Vec<u8> {
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

pub(super) fn write_click_effect_commands(
    path: &PathBuf,
    index: usize,
    sample: ClickSample,
    width: u32,
    height: u32,
    fps: u32,
    scene: bool,
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
        let (x, y) = recordly_stage_click_position(point, width, height, scene);
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

pub(super) fn recordly_cursor_motion_blur(
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

pub(super) fn rasterize_cursor_tile(
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

pub(super) fn apply_recordly_cursor_motion_blur(
    source: Vec<f32>,
    blur: CursorMotionBlur,
) -> Vec<f32> {
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

pub(super) fn premultiplied_alpha_bounds(pixels: &[f32]) -> Option<(i32, i32, i32, i32)> {
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

pub(super) fn sample_premultiplied_rgba(pixels: &[f32], x: f64, y: f64) -> [f32; 4] {
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

pub(super) fn premultiplied_rgba_to_bytes(pixels: &[f32]) -> Vec<u8> {
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

pub(super) fn write_cursor_commands(path: &PathBuf, track: &CursorTrack) -> Result<(), String> {
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

pub(super) fn write_camera_commands(
    path: &PathBuf,
    events: &[crate::zoom::ZoomEvent],
    width: u32,
    height: u32,
    duration_ms: u64,
    fps: u32,
    scene: bool,
) -> Result<(), String> {
    let file = File::create(path)
        .map_err(|error| format!("Could not create camera command file: {error}"))?;
    let mut file = BufWriter::new(file);
    for interval in camera_command_intervals(scene, events, width, height, duration_ms, fps) {
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
pub(super) struct CameraCommandInterval {
    pub(super) start_ms: u64,
    pub(super) end_ms: u64,
    pub(super) rect: CameraCropRect,
}

pub(super) fn camera_command_intervals(
    scene: bool,
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
        let rect = camera_crop_rect(transform, width, height, scene);
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
pub(super) struct CameraCropRect {
    pub(super) x: u32,
    pub(super) y: u32,
    pub(super) width: u32,
    pub(super) height: u32,
}

pub(super) fn camera_crop_rect(
    transform: CameraTransform,
    width: u32,
    height: u32,
    scene: bool,
) -> CameraCropRect {
    let crop_width = ((width as f64 / transform.scale).round() as u32).clamp(1, width);
    let crop_height = ((height as f64 / transform.scale).round() as u32).clamp(1, height);
    let (content_width, content_height) = recordly_padded_size(width, height, scene);
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

pub(super) fn tahoe_cursor_for_niri_name(name: &str) -> TahoeCursor {
    TahoeCursor::from_standard_name(name)
}

pub(super) fn tahoe_cursor_asset_file(kind: TahoeCursor) -> &'static str {
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

pub(super) fn tahoe_cursor_asset_path(kind: TahoeCursor) -> Result<PathBuf, String> {
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

pub(super) fn tahoe_cursor_metrics(kind: TahoeCursor) -> Result<TahoeCursorMetrics, String> {
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

pub(super) fn scaled_cursor_asset_paths(
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

pub(super) fn recordly_wallpaper_asset_path(
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

pub(super) fn recordly_card_filter(
    width: u32,
    height: u32,
    duration_ms: u64,
    fps: u32,
    mask_input_index: usize,
    shadow_input_index: usize,
) -> String {
    let (content_width, content_height) = recordly_padded_size(width, height, true);
    let padded_duration = duration_ms as f64 / 1_000.0 + 1.0 / fps.max(1) as f64;
    format!(
        "[0:v]setpts=PTS-STARTPTS,fps={fps},tpad=stop_mode=clone:stop_duration={padded_duration:.6},scale={content_width}:{content_height}:flags=lanczos,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2:color=black,format=rgba[content];[{mask_input_index}:v]format=gray,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2:color=black[frame_mask];[content][frame_mask]alphamerge[content_with_mask];[{shadow_input_index}:v]format=rgba[frame_shadow];[frame_shadow][content_with_mask]overlay=0:0:format=auto:shortest=1[card]"
    )
}

/// The recording as the whole frame: no inset card, no rounded mask, no
/// shadow and no wallpaper behind it.
pub(super) fn full_frame_scene_filter(
    width: u32,
    height: u32,
    duration_ms: u64,
    fps: u32,
) -> String {
    let padded_duration = duration_ms as f64 / 1_000.0 + 1.0 / fps.max(1) as f64;
    format!(
        "[0:v]setpts=PTS-STARTPTS,fps={fps},tpad=stop_mode=clone:stop_duration={padded_duration:.6},scale={width}:{height}:flags=lanczos,format=rgba[scene]"
    )
}

pub(super) fn wallpaper_background_filter(
    wallpaper_input_index: usize,
    card_label: &str,
    output_label: &str,
) -> String {
    format!(
        "[{wallpaper_input_index}:v]format=rgba[wallpaper];[wallpaper][{card_label}]overlay=0:0:format=auto:shortest=1,format=rgba[{output_label}]"
    )
}

pub(super) struct RecordlyFrameAssets {
    pub(super) mask_path: PathBuf,
    pub(super) wallpaper_path: PathBuf,
    pub(super) shadow_path: PathBuf,
}

pub(super) fn recordly_frame_assets(
    background: BackgroundStyle,
    custom_background_path: Option<&str>,
    width: u32,
    height: u32,
) -> Result<RecordlyFrameAssets, String> {
    let (content_width, content_height) = recordly_padded_size(width, height, true);
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

pub(super) fn rasterize_wallpaper(
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

pub(super) fn rasterize_solid_wallpaper(
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

pub(super) fn rasterize_static_svg(
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

/// Size of the area the recording occupies inside the output frame.
///
/// With the scene on this is Recordly's inset card; with it off the recording
/// fills the frame. Everything positioned over the recording - cursor, click
/// pulses, camera crops - measures against this, so the two layouts stay in
/// agreement by asking here rather than each applying the inset themselves.
pub(super) fn recordly_padded_size(width: u32, height: u32, scene: bool) -> (u32, u32) {
    if !scene {
        return (width, height);
    }
    // Port of Recordly's DEFAULT_PADDING (20) and PADDING_SCALE_FACTOR (0.2):
    // each linked side consumes 4% of the canvas dimension.
    (
        (width as f64 * 0.92).round() as u32,
        (height as f64 * 0.92).round() as u32,
    )
}

pub(super) fn ffmpeg_filter_threads() -> String {
    std::thread::available_parallelism()
        .map(|threads| threads.get().clamp(1, 12))
        .unwrap_or(4)
        .to_string()
}

pub(super) fn recordly_squircle_path(width: f64, height: f64, radius: f64) -> String {
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

pub(super) fn tahoe_cursor_hotspot_offset(
    metrics: TahoeCursorMetrics,
    stage_scale: f64,
) -> (f64, f64) {
    let cursor_height = RECORDLY_TAHOE_CURSOR_HEIGHT * stage_scale;
    let width = cursor_height * metrics.aspect_ratio;
    (width * metrics.hotspot_x, cursor_height * metrics.hotspot_y)
}

pub(super) fn recordly_stage_cursor_position(
    point: CursorPoint,
    metrics: TahoeCursorMetrics,
    width: u32,
    height: u32,
    cursor_scale: f64,
    scene: bool,
) -> (f64, f64) {
    let (content_width, content_height) = recordly_padded_size(width, height, scene);
    let inset_x = (width.saturating_sub(content_width)) as f64 / 2.0;
    let inset_y = (height.saturating_sub(content_height)) as f64 / 2.0;
    let stage_scale = content_height as f64 / height as f64;
    let (hotspot_x, hotspot_y) = tahoe_cursor_hotspot_offset(metrics, stage_scale * cursor_scale);
    (
        inset_x + point.x * content_width as f64 - hotspot_x,
        inset_y + point.y * content_height as f64 - hotspot_y,
    )
}

pub(super) fn recordly_stage_click_position(
    point: CursorPoint,
    width: u32,
    height: u32,
    scene: bool,
) -> (f64, f64) {
    let (content_width, content_height) = recordly_padded_size(width, height, scene);
    let inset_x = (width.saturating_sub(content_width)) as f64 / 2.0;
    let inset_y = (height.saturating_sub(content_height)) as f64 / 2.0;
    (
        inset_x + point.x * content_width as f64 - CLICK_TILE_SIZE as f64 / 2.0,
        inset_y + point.y * content_height as f64 - CLICK_TILE_SIZE as f64 / 2.0,
    )
}

pub(super) fn cursor_is_hidden(events: &[crate::zoom::ZoomEvent], time_ms: u64) -> bool {
    events.iter().any(|event| {
        event.hide_cursor
            && time_ms >= event.start_ms
            && time_ms < event.end_ms(ZoomTiming::default())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turning_the_scene_off_gives_the_recording_the_whole_frame() {
        assert_eq!(recordly_padded_size(1_920, 1_080, true), (1_766, 994));
        assert_eq!(recordly_padded_size(1_920, 1_080, false), (1_920, 1_080));
    }

    #[test]
    fn the_scene_off_filter_composites_nothing_behind_the_recording() {
        let filter = full_frame_scene_filter(1_920, 1_080, 4_000, 120);

        assert!(filter.contains("scale=1920:1080"));
        assert!(filter.ends_with("[scene]"));
        // No card, so nothing to mask, shadow or lay over a wallpaper.
        assert!(!filter.contains("alphamerge"));
        assert!(!filter.contains("overlay"));
        assert!(!filter.contains("[wallpaper]"));
        // tpad extends the last frame and is unrelated to the card's padding.
        assert!(!filter.contains("pad=1920:1080"));
    }

    #[test]
    fn overlays_drop_the_inset_when_the_scene_is_off() {
        let centre = CursorPoint { x: 0.5, y: 0.5 };
        let edge = CursorPoint { x: 1.0, y: 1.0 };
        let tile = CLICK_TILE_SIZE as f64 / 2.0;

        // The centre of the recording is the centre of the frame either way.
        assert_eq!(
            recordly_stage_click_position(centre, 1_920, 1_080, false).0,
            1_920.0 / 2.0 - tile
        );
        assert_eq!(
            recordly_stage_click_position(centre, 1_920, 1_080, true).0,
            1_920.0 / 2.0 - tile
        );
        // Its right edge is the frame's edge only once the inset is gone.
        assert_eq!(
            recordly_stage_click_position(edge, 1_920, 1_080, false).0,
            1_920.0 - tile
        );
        assert!(recordly_stage_click_position(edge, 1_920, 1_080, true).0 < 1_920.0 - tile);
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
        let rect = camera_crop_rect(transform, 1_920, 1_080, true);
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
            true,
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
        let intervals = camera_command_intervals(true, &events, 1_920, 1_080, 5_000, 120);
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
        write_camera_commands(&path, &events, 1_920, 1_080, 4_500, 120, true)
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
        let track = build_cursor_track(&timeline, &[], 1_920, 1_080, 1_000, 120, 0, 100, true)
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
        let raw = build_cursor_track(&timeline, &[], 1_920, 1_080, 1_000, 120, 0, 100, true)
            .expect("raw cursor track should be built");
        let smoothed = build_cursor_track(&timeline, &[], 1_920, 1_080, 1_000, 120, 100, 100, true)
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
        let (x, y) = recordly_stage_cursor_position(point, metrics, 1_920, 1_080, 1.0, true);
        let stage_scale = 994.0 / 1_080.0;
        let (hotspot_x, hotspot_y) = tahoe_cursor_hotspot_offset(metrics, stage_scale);
        assert!((x + hotspot_x - 1_489.8).abs() < 0.001);
        assert!((y + hotspot_y - 838.2).abs() < 0.001);
    }

    #[test]
    fn click_effect_is_centered_on_the_same_padded_stage() {
        let (x, y) =
            recordly_stage_click_position(CursorPoint { x: 0.5, y: 0.5 }, 1_920, 1_080, true);
        assert!((x + CLICK_TILE_SIZE as f64 / 2.0 - 960.0).abs() < f64::EPSILON);
        assert!((y + CLICK_TILE_SIZE as f64 / 2.0 - 540.0).abs() < f64::EPSILON);
        let pulse = rasterize_click_pulse_tile(true, 0.0);
        let center = CLICK_TILE_SIZE as usize / 2;
        let alpha = pulse[(center * CLICK_TILE_SIZE as usize + center) * 4 + 3];
        assert!(alpha > 0, "a new pulse must be visible at its centre");
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
    fn one_segment_renders_as_the_single_trim_it_always_was() {
        let mut filter = "[scene]null[stage]".to_owned();
        append_segment_filter(
            &mut filter,
            "stage",
            Some(3),
            &[Segment {
                start_ms: 1_250,
                end_ms: 4_750,
            }],
        );
        assert!(filter.contains(
            "[stage]trim=start=1.250000:end=4.750000,setpts=PTS-STARTPTS,format=yuv420p[out]"
        ));
        assert!(filter
            .contains("[3:a:0]atrim=start=1.250000:end=4.750000,asetpts=PTS-STARTPTS[out_audio]"));
        // Nothing to join, so no split or concat is introduced.
        assert!(!filter.contains("split"));
        assert!(!filter.contains("concat"));
    }

    #[test]
    fn cuts_are_stitched_back_together_in_order() {
        let mut filter = "[scene]null[stage]".to_owned();
        append_segment_filter(
            &mut filter,
            "stage",
            Some(3),
            &[
                Segment {
                    start_ms: 0,
                    end_ms: 1_000,
                },
                Segment {
                    start_ms: 4_000,
                    end_ms: 6_500,
                },
            ],
        );

        assert!(filter.contains("[stage]split=2[seg0][seg1]"));
        assert!(filter.contains("[seg0]trim=start=0.000000:end=1.000000,setpts=PTS-STARTPTS[cut0]"));
        assert!(filter.contains("[seg1]trim=start=4.000000:end=6.500000,setpts=PTS-STARTPTS[cut1]"));
        assert!(filter.contains("[cut0][cut1]concat=n=2:v=1:a=0,format=yuv420p[out]"));

        // The audio follows the same cuts.
        assert!(filter.contains("[3:a:0]asplit=2[aseg0][aseg1]"));
        assert!(filter.contains("[acut0][acut1]concat=n=2:v=0:a=1[out_audio]"));
    }

    #[test]
    fn a_silent_recording_gets_no_audio_branch() {
        let mut filter = "[scene]null[stage]".to_owned();
        append_segment_filter(
            &mut filter,
            "stage",
            None,
            &[
                Segment {
                    start_ms: 0,
                    end_ms: 1_000,
                },
                Segment {
                    start_ms: 2_000,
                    end_ms: 3_000,
                },
            ],
        );

        assert!(filter.contains("concat=n=2:v=1:a=0"));
        assert!(!filter.contains("out_audio"));
        assert!(!filter.contains("atrim"));
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
