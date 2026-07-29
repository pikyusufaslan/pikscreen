use std::{
    fs,
    io::{BufRead, BufReader, Result as IoResult, Write},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::process::{CommandExt, ExitStatusExt},
    },
    path::{Path, PathBuf},
    process::{Child, ChildStderr, Command, ExitStatus, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

const CHILD_PIPEWIRE_FD: i32 = 33;
const PROBE_TIMEOUT: Duration = Duration::from_secs(6);
const STOP_TIMEOUT: Duration = Duration::from_secs(3);
const STARTUP_FRAME_TIMEOUT: Duration = Duration::from_secs(3);
const DEBUG_LOG_PATH: &str = "/tmp/pikscreen-window-pipewire.log";

/// Keep the tail of a recording's output rather than all of it: a capture can
/// run for hours, and only the most recent lines explain a failure.
const CHILD_LOG_CAPACITY: usize = 128 * 1024;
/// pipewiresrc logs its stream transitions at debug level.  Reaching this one
/// means PipeWire negotiated a format, buffers are allocated and frames are
/// moving, which is what "the recording started" actually means.
const STREAMING_MARKER: &str = "got stream state streaming";
/// Both the pipewiresrc warning and the gst-launch error line carry the reason
/// after this prefix.
const STREAM_ERROR_MARKER: &str = "stream error: ";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowPipelineProfile {
    DmaBufGlVaapi,
    MemFdX264,
}

impl WindowPipelineProfile {
    pub const ALL: [Self; 2] = [Self::DmaBufGlVaapi, Self::MemFdX264];

    pub fn label(self) -> &'static str {
        match self {
            Self::DmaBufGlVaapi => "DMA-BUF/OpenGL + VA-API",
            Self::MemFdX264 => "PipeWire MemFd + x264",
        }
    }
}

pub struct WindowRecorder {
    child: Child,
    log: ChildLog,
    profile: WindowPipelineProfile,
    output_path: PathBuf,
}

#[derive(Debug)]
struct ChildResult {
    status: ExitStatus,
    stderr: String,
}

/// A child's stderr, drained from the moment it starts.
///
/// A pipe holds 64 KB. A chatty `GST_DEBUG` fills that in well under a second,
/// and once it is full the child blocks in `write(2)` before it ever produces a
/// frame, which reads from the outside as a source that silently never
/// delivers. Reading the pipe continuously is what keeps that from happening;
/// the stream transitions are picked up on the way past.
#[derive(Clone)]
pub struct ChildLog {
    inner: Arc<Mutex<ChildLogInner>>,
}

#[derive(Default)]
struct ChildLogInner {
    text: String,
    streaming: bool,
    error: Option<String>,
}

impl ChildLog {
    pub fn drain(stderr: ChildStderr) -> Self {
        let inner = Arc::new(Mutex::new(ChildLogInner::default()));
        let writer = Arc::clone(&inner);
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                writer
                    .lock()
                    .unwrap_or_else(|err| err.into_inner())
                    .push(&line);
            }
        });
        Self { inner }
    }

    pub fn detached() -> Self {
        Self {
            inner: Arc::new(Mutex::new(ChildLogInner::default())),
        }
    }

    pub fn snapshot(&self) -> String {
        self.inner
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .text
            .clone()
    }

    fn saw_streaming(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .streaming
    }

    fn stream_error(&self) -> Option<String> {
        self.inner
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .error
            .clone()
    }
}

impl ChildLogInner {
    fn push(&mut self, line: &str) {
        if line.contains(STREAMING_MARKER) {
            self.streaming = true;
        }
        if self.error.is_none() {
            if let Some(reason) = line.split(STREAM_ERROR_MARKER).nth(1) {
                let reason = reason.trim();
                if !reason.is_empty() {
                    self.error = Some(reason.to_owned());
                }
            }
        }

        self.text.push_str(line);
        self.text.push('\n');
        while self.text.len() > CHILD_LOG_CAPACITY {
            // Drop whole lines from the front. A newline is always a character
            // boundary, so the remainder stays valid UTF-8.
            match self.text.find('\n') {
                Some(end) => drop(self.text.drain(..=end)),
                None => {
                    self.text.clear();
                    break;
                }
            }
        }
    }
}

pub fn probe_profile(
    pipewire_fd: OwnedFd,
    node_id: u32,
    profile: WindowPipelineProfile,
    width: u32,
    height: u32,
    output_path: &Path,
) -> Result<(), String> {
    let (width, height) = even_dimensions(width, height);
    let _ = fs::remove_file(output_path);
    let args = recording_pipeline_args(
        node_id,
        profile,
        width,
        height,
        30,
        24,
        "superfast",
        2_048,
        output_path,
        Some(2),
    );
    let (child, log) = spawn_gst(pipewire_fd, &args)?;
    let result = wait_for_child(child, &log, PROBE_TIMEOUT)?;
    if !result.status.success() {
        return Err(format!(
            "{} probe exited with {}: {}",
            profile.label(),
            result.status,
            compact_log(&result.stderr)
        ));
    }
    let size = fs::metadata(output_path)
        .map(|metadata| metadata.len())
        .unwrap_or_default();
    if size == 0 {
        return Err(format!(
            "{} probe produced no readable MP4 data",
            profile.label()
        ));
    }
    verify_video(output_path).map_err(|error| format!("{} probe {error}", profile.label()))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_recorder(
    pipewire_fd: OwnedFd,
    node_id: u32,
    profile: WindowPipelineProfile,
    width: u32,
    height: u32,
    fps: u32,
    crf: u32,
    preset: &str,
    max_bitrate_kbps: u32,
    output_path: &Path,
) -> Result<WindowRecorder, String> {
    let (width, height) = even_dimensions(width, height);
    // A failed profile can leave an empty Matroska file behind.  Each retry
    // must start from a clean target so a later profile is not mistaken for a
    // working stream merely because the previous muxer wrote its header.
    let _ = fs::remove_file(output_path);
    let args = recording_pipeline_args(
        node_id,
        profile,
        width,
        height,
        fps,
        crf,
        preset,
        max_bitrate_kbps,
        output_path,
        None,
    );
    let (child, log) = spawn_gst(pipewire_fd, &args)?;
    Ok(WindowRecorder {
        child,
        log,
        profile,
        output_path: output_path.to_path_buf(),
    })
}

pub fn ensure_recorder_started(recorder: &mut WindowRecorder) -> Result<(), String> {
    let started_at = Instant::now();
    loop {
        // The stream reports a negotiation failure before the process reacts to
        // it, and it names the reason. Check it first so a broken pipeline
        // fails in milliseconds with something actionable instead of after the
        // full timeout with "no first frame".
        if let Some(error) = recorder.log.stream_error() {
            return Err(fail_startup(
                recorder,
                &format!("PipeWire stream failed: {error}"),
            ));
        }

        if let Some(status) = recorder
            .child
            .try_wait()
            .map_err(|error| format!("Could not inspect the window PipeWire pipeline: {error}"))?
        {
            let diagnostic = compact_log(&recorder.log.snapshot());
            append_debug_log(&format!(
                "{} exited during startup with {status}: {diagnostic}",
                recorder.profile.label()
            ));
            append_debug_blob(
                &format!("{} full stderr", recorder.profile.label()),
                &recorder.log.snapshot(),
            );
            return Err(format!(
                "{} recorder failed during startup with {}: {}",
                recorder.profile.label(),
                status,
                diagnostic
            ));
        }

        // Reaching the streaming state means the format is negotiated and
        // buffers are flowing, which holds even for an idle window that niri
        // only draws once.
        if recorder.log.saw_streaming() {
            return Ok(());
        }

        // Secondary signal, so a future change to GST_DEBUG cannot silently
        // turn startup detection off: a streamable Matroska file grows as soon
        // as the first encoded frame reaches the muxer.
        let has_frame_data = fs::metadata(&recorder.output_path)
            .map(|metadata| metadata.len() > 1_024)
            .unwrap_or(false);
        if has_frame_data {
            return Ok(());
        }

        if started_at.elapsed() >= STARTUP_FRAME_TIMEOUT {
            return Err(fail_startup(
                recorder,
                &format!(
                    "PipeWire stream never started within {:.1}s",
                    STARTUP_FRAME_TIMEOUT.as_secs_f64()
                ),
            ));
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// Stop a recorder that failed to start and turn its output into one message
/// for the UI, keeping the untruncated version in the debug log.
fn fail_startup(recorder: &mut WindowRecorder, reason: &str) -> String {
    let _ = signal_child(&recorder.child, libc::SIGINT);
    let _ = wait_for_child_inner(&mut recorder.child, &recorder.log, Duration::from_secs(1));
    let raw = recorder.log.snapshot();
    let label = recorder.profile.label();
    append_debug_log(&format!("{label} {reason}: {}", compact_log(&raw)));
    append_debug_blob(&format!("{label} full stderr"), &raw);
    format!(
        "{label} recorder could not start: {reason}. {}",
        compact_log(&raw)
    )
}

pub fn stop_recorder(mut recorder: WindowRecorder) -> Result<(), String> {
    signal_child(&recorder.child, libc::SIGINT)?;
    let result = match wait_for_child_inner(&mut recorder.child, &recorder.log, STOP_TIMEOUT) {
        Ok(result) => result,
        Err(timeout_error) => {
            return verify_video(&recorder.output_path).map_err(|verify_error| {
                format!(
                    "{timeout_error}. Recorded source {} {verify_error}",
                    recorder.output_path.display()
                )
            });
        }
    };
    if result.status.success() || result.status.signal() == Some(libc::SIGINT) {
        verify_video(&recorder.output_path)
            .map_err(|error| format!("Recorded source {} {error}", recorder.output_path.display()))
    } else {
        Err(format!(
            "{} recorder exited with {}: {}",
            recorder.profile.label(),
            result.status,
            compact_log(&result.stderr)
        ))
    }
}

pub fn preview_dimensions(width: u32, height: u32) -> (u32, u32) {
    let width = width.max(2);
    let height = height.max(2);
    let preview_width = width.min(960) & !1;
    let preview_height = ((height as u64 * preview_width as u64 / width as u64) as u32).max(2) & !1;
    (preview_width.max(2), preview_height.max(2))
}

pub fn even_dimensions(width: u32, height: u32) -> (u32, u32) {
    ((width.max(2) & !1).max(2), (height.max(2) & !1).max(2))
}

#[allow(clippy::too_many_arguments)]
fn recording_pipeline_args(
    node_id: u32,
    profile: WindowPipelineProfile,
    width: u32,
    height: u32,
    fps: u32,
    crf: u32,
    preset: &str,
    max_bitrate_kbps: u32,
    output_path: &Path,
    num_buffers: Option<u32>,
) -> Vec<String> {
    let mut args = pipeline_prefix(node_id);
    args.extend(profile_conversion_args(profile));
    let raw_format = match profile {
        WindowPipelineProfile::DmaBufGlVaapi => "NV12",
        WindowPipelineProfile::MemFdX264 => "I420",
    };
    args.extend([
        "!".to_owned(),
        "videoconvert".to_owned(),
        "!".to_owned(),
        "videoscale".to_owned(),
        "!".to_owned(),
        // A compositor screencast is damage driven, so the PipeWire stream
        // advertises framerate=0/1.  videoconvert and videoscale pass the
        // framerate through untouched, so pinning one here leaves nothing to
        // intersect and the pipeline dies during preroll with
        // "no more input formats".  The compositor below sets the constant
        // rate the encoder needs, and force-live keeps it emitting while an
        // idle window sends no new frames.
        format!("video/x-raw,format={raw_format},width={width},height={height}"),
        "!".to_owned(),
        "compositor".to_owned(),
        "force-live=true".to_owned(),
        "start-time-selection=first".to_owned(),
        "background=black".to_owned(),
        "!".to_owned(),
        format!("video/x-raw,format={raw_format},width={width},height={height},framerate={fps}/1"),
    ]);
    if let Some(num_buffers) = num_buffers {
        args.extend([
            "!".to_owned(),
            "identity".to_owned(),
            format!("eos-after={}", identity_eos_after(num_buffers)),
            "!".to_owned(),
        ]);
    } else {
        args.push("!".to_owned());
    }
    match profile {
        WindowPipelineProfile::DmaBufGlVaapi => args.extend([
            "vapostproc".to_owned(),
            "!".to_owned(),
            "video/x-raw(memory:VAMemory),format=NV12".to_owned(),
            "!".to_owned(),
            "vah264enc".to_owned(),
            "rate-control=cqp".to_owned(),
            format!("qpi={crf}"),
            format!("qpp={crf}"),
            "target-usage=7".to_owned(),
            format!("key-int-max={}", keyframe_interval(fps)),
            "!".to_owned(),
        ]),
        WindowPipelineProfile::MemFdX264 => args.extend([
            "x264enc".to_owned(),
            format!("speed-preset={preset}"),
            "tune=zerolatency".to_owned(),
            "pass=qual".to_owned(),
            format!("quantizer={crf}"),
            format!("bitrate={max_bitrate_kbps}"),
            format!("key-int-max={}", keyframe_interval(fps)),
            "!".to_owned(),
        ]),
    }
    args.extend([
        "h264parse".to_owned(),
        "config-interval=-1".to_owned(),
        "!".to_owned(),
        "video/x-h264,stream-format=avc,alignment=au".to_owned(),
        "!".to_owned(),
    ]);
    if output_path
        .extension()
        .is_some_and(|extension| extension == "mkv")
    {
        args.extend(["matroskamux".to_owned(), "streamable=true".to_owned()]);
    } else {
        args.extend(["mp4mux".to_owned(), "faststart=false".to_owned()]);
    }
    args.extend([
        "!".to_owned(),
        "filesink".to_owned(),
        format!("location={}", output_path.display()),
    ]);
    args
}

/// Frames between keyframes in the intermediate recording.
///
/// A decoder has to start from the previous keyframe, so how far apart they sit
/// is how much work a seek costs. At two seconds the editor's playhead decoded
/// up to two seconds of frames for every move and the preview went black in
/// between. A quarter second keeps scrubbing responsive; this file is only an
/// intermediate that the final export re-encodes from, so the extra size does
/// not reach the output.
fn keyframe_interval(fps: u32) -> u32 {
    (fps / 4).max(1)
}

fn identity_eos_after(output_frames: u32) -> u32 {
    output_frames.saturating_add(1)
}

fn pipeline_prefix(node_id: u32) -> Vec<String> {
    let mut args = vec![
        "-q".to_owned(),
        "-e".to_owned(),
        "pipewiresrc".to_owned(),
        format!("fd={CHILD_PIPEWIRE_FD}"),
        format!("path={node_id}"),
        "do-timestamp=true".to_owned(),
    ];
    args.extend(["!".to_owned(), "queue".to_owned()]);
    // No memory:DMABuf caps filter here.  niri offers the stream as BGRx/BGRA
    // with a DRM modifier, which does not intersect a pinned DMA_DRM format,
    // and pinning memory:DMABuf alone fails the same way.  glupload picks the
    // DMA-BUF import path on its own when the producer exports one.
    args
}

fn profile_conversion_args(profile: WindowPipelineProfile) -> Vec<String> {
    match profile {
        WindowPipelineProfile::DmaBufGlVaapi => vec![
            "!".to_owned(),
            "glupload".to_owned(),
            "!".to_owned(),
            "glcolorconvert".to_owned(),
            "!".to_owned(),
            "gldownload".to_owned(),
        ],
        WindowPipelineProfile::MemFdX264 => Vec::new(),
    }
}

fn spawn_gst(pipewire_fd: OwnedFd, args: &[String]) -> Result<(Child, ChildLog), String> {
    let mut command = Command::new("gst-launch-1.0");
    append_debug_log(&format!("spawn gst-launch-1.0 {}", args.join(" ")));
    command
        .args(args)
        // Keep PipeWire's stream setup in the error returned to the UI. The
        // portal remote is intentionally private, so this is the useful
        // diagnostic path when a chosen window fails after the picker closes.
        // GST_CAPS is deliberately absent: a single caps query on this source
        // prints tens of kilobytes and pushes the pipewiresrc lines that carry
        // the actual failure out of every capped log.
        .env("GST_DEBUG", "pipewiresrc:6")
        .env("GST_DEBUG_NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(move || prepare_child_process(&pipewire_fd));
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("Could not start the window PipeWire pipeline: {error}"))?;
    let log = match child.stderr.take() {
        Some(stderr) => ChildLog::drain(stderr),
        None => ChildLog::detached(),
    };
    Ok((child, log))
}

fn prepare_child_process(pipewire_fd: &OwnedFd) -> IoResult<()> {
    inherit_pipewire_fd(pipewire_fd)?;
    reset_child_signal_state()
}

fn reset_child_signal_state() -> IoResult<()> {
    unsafe {
        let mut empty = std::mem::zeroed::<libc::sigset_t>();
        if libc::sigemptyset(&mut empty) == -1 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut()) == -1 {
            return Err(std::io::Error::last_os_error());
        }
        for signal in [libc::SIGINT, libc::SIGTERM] {
            if libc::signal(signal, libc::SIG_DFL) == libc::SIG_ERR {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

fn inherit_pipewire_fd(pipewire_fd: &OwnedFd) -> IoResult<()> {
    if unsafe { libc::dup2(pipewire_fd.as_raw_fd(), CHILD_PIPEWIRE_FD) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let flags = unsafe { libc::fcntl(CHILD_PIPEWIRE_FD, libc::F_GETFD) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(CHILD_PIPEWIRE_FD, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn wait_for_child(
    mut child: Child,
    log: &ChildLog,
    timeout: Duration,
) -> Result<ChildResult, String> {
    wait_for_child_inner(&mut child, log, timeout)
}

fn wait_for_child_inner(
    child: &mut Child,
    log: &ChildLog,
    timeout: Duration,
) -> Result<ChildResult, String> {
    let started_at = Instant::now();
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("Could not inspect the window pipeline: {error}"))?
        {
            return Ok(ChildResult {
                status,
                stderr: log.snapshot(),
            });
        }
        if started_at.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "Window PipeWire pipeline timed out after {:.1}s: {}",
                timeout.as_secs_f64(),
                compact_log(&log.snapshot())
            ));
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn signal_child(child: &Child, signal: i32) -> Result<(), String> {
    if unsafe { libc::kill(child.id() as i32, signal) } == 0
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    {
        Ok(())
    } else {
        Err(format!(
            "Could not stop the window PipeWire pipeline: {}",
            std::io::Error::last_os_error()
        ))
    }
}

fn compact_log(log: &str) -> String {
    let compact = log
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" | ");
    if compact.len() > 4_000 {
        // Startup failures often put the useful caps rejection at the end of
        // the trace, while the stream setup is at the beginning. Keep both.
        format!(
            "{} … {}",
            &compact[..800],
            &compact[compact.len() - 3_000..]
        )
    } else if compact.is_empty() {
        "no diagnostic output".to_owned()
    } else {
        compact
    }
}

/// Append a child's untouched output. `compact_log` caps what the UI carries,
/// but a stream that never negotiates only says so once, so the log file keeps
/// the whole thing.
fn append_debug_blob(label: &str, blob: &str) {
    if let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(DEBUG_LOG_PATH)
    {
        let _ = writeln!(file, "----- {label} begin -----");
        let _ = file.write_all(blob.as_bytes());
        let _ = writeln!(file, "\n----- {label} end -----");
    }
}

fn append_debug_log(line: &str) {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    if let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(DEBUG_LOG_PATH)
    {
        let _ = writeln!(file, "[{timestamp}] {line}");
    }
}

fn verify_video(path: &Path) -> Result<(), String> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-count_frames",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_type,nb_read_frames",
            "-of",
            "default=noprint_wrappers=1",
        ])
        .arg(path)
        .output()
        .map_err(|error| format!("could not be inspected: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "is unreadable: {}",
            compact_log(&String::from_utf8_lossy(&output.stderr))
        ));
    }
    validate_video_probe(&String::from_utf8_lossy(&output.stdout))
}

fn validate_video_probe(probe: &str) -> Result<(), String> {
    let mut video_stream = false;
    let mut frame_count = None;
    for line in probe.lines().map(str::trim) {
        if line == "codec_type=video" {
            video_stream = true;
        } else if let Some(value) = line.strip_prefix("nb_read_frames=") {
            if value != "N/A" {
                frame_count = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| format!("reported an invalid frame count: {value}"))?,
                );
            }
        }
    }
    if !video_stream {
        return Err("has no readable video stream".to_owned());
    }
    if let Some(frame_count @ 0..=1) = frame_count {
        return Err(format!(
            "is frozen: ffprobe decoded only {frame_count} video frame(s)"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    fn scan(lines: &[&str]) -> ChildLogInner {
        let mut log = ChildLogInner::default();
        for line in lines {
            log.push(line);
        }
        log
    }

    #[test]
    fn startup_completes_when_the_stream_reaches_streaming() {
        let log = scan(&[
            "0:00:00.011 DEBUG pipewiresrc on_state_changed:<pipewiresrc0> got stream state connecting",
            "0:00:00.023 DEBUG pipewiresrc on_state_changed:<pipewiresrc0> got stream state paused",
            "0:00:00.310 DEBUG pipewiresrc on_state_changed:<pipewiresrc0> got stream state streaming",
        ]);

        assert!(log.streaming);
        assert_eq!(log.error, None);
    }

    #[test]
    fn a_stream_that_only_connects_is_not_treated_as_started() {
        let log = scan(&[
            "0:00:00.011 DEBUG pipewiresrc on_state_changed:<pipewiresrc0> got stream state connecting",
            "0:00:00.023 DEBUG pipewiresrc on_state_changed:<pipewiresrc0> got stream state paused",
        ]);

        assert!(!log.streaming);
    }

    #[test]
    fn negotiation_failure_is_captured_with_its_reason() {
        let log = scan(&[
            "0:00:00.014 WARN pipewiresrc on_state_changed:<pipewiresrc0> error: stream error: no more input formats",
            "ERROR: from element /GstPipeline:pipeline0/GstPipeWireSrc:pipewiresrc0: stream error: target not found",
        ]);

        // The first reason is the real one; the later line is the same failure
        // surfacing again through gst-launch.
        assert_eq!(log.error.as_deref(), Some("no more input formats"));
    }

    #[test]
    fn ordinary_output_carries_no_verdict() {
        let log = scan(&["Setting pipeline to PAUSED ...", "Redistribute latency..."]);

        assert!(!log.streaming);
        assert_eq!(log.error, None);
        assert!(log.text.contains("Redistribute latency..."));
    }

    #[test]
    fn the_buffer_keeps_the_tail_of_a_long_recording() {
        let mut log = ChildLogInner::default();
        for index in 0..40_000 {
            log.push(&format!("line {index}"));
        }

        assert!(log.text.len() <= CHILD_LOG_CAPACITY);
        assert!(log.text.contains("line 39999"));
        assert!(!log.text.contains("line 0\n"));
        // Dropping happens on line boundaries, so the tail stays parseable.
        assert!(log.text.starts_with("line "));
    }

    #[test]
    fn a_verdict_survives_the_buffer_dropping_the_line_that_set_it() {
        let mut log = ChildLogInner::default();
        log.push("got stream state streaming");
        for index in 0..40_000 {
            log.push(&format!("filler {index}"));
        }

        assert!(!log.text.contains("got stream state streaming"));
        assert!(log.streaming);
    }

    #[test]
    fn rounds_recording_dimensions_down_to_even_pixels() {
        assert_eq!(even_dimensions(1_921, 1_081), (1_920, 1_080));
        assert_eq!(even_dimensions(1, 1), (2, 2));
    }

    #[test]
    fn video_probe_rejects_an_explicit_single_frame_source() {
        assert_eq!(
            validate_video_probe("codec_type=video\nnb_read_frames=1\n"),
            Err("is frozen: ffprobe decoded only 1 video frame(s)".to_owned())
        );
    }

    #[test]
    fn video_probe_accepts_a_moving_source() {
        assert_eq!(
            validate_video_probe("codec_type=video\nnb_read_frames=874\n"),
            Ok(())
        );
    }

    #[test]
    fn video_probe_keeps_readability_fallback_when_count_is_unavailable() {
        assert_eq!(
            validate_video_probe("codec_type=video\nnb_read_frames=N/A\n"),
            Ok(())
        );
    }

    #[test]
    fn window_recording_targets_the_portal_node_instead_of_a_screen_rectangle() {
        let args = recording_pipeline_args(
            42,
            WindowPipelineProfile::DmaBufGlVaapi,
            1_920,
            1_080,
            120,
            18,
            "medium",
            2_048,
            Path::new("/tmp/window.mp4"),
            None,
        );
        assert!(args.contains(&"path=42".to_owned()));
        assert!(args.contains(&"glupload".to_owned()));
        assert!(args.contains(&"vah264enc".to_owned()));
        assert!(!args.iter().any(|arg| arg == "-g"));
    }

    /// The source pad of pipewiresrc carries whatever niri advertises: a
    /// damage driven stream with framerate=0/1, and BGRx/BGRA in DMA-BUF
    /// memory rather than DMA_DRM.  Constraining either one before an element
    /// that can convert it makes negotiation fail with "no more input
    /// formats", which reaches the UI as a missing first frame.
    #[test]
    fn window_recording_leaves_the_source_caps_convertible() {
        for profile in WindowPipelineProfile::ALL {
            let args = recording_pipeline_args(
                7,
                profile,
                1_920,
                1_080,
                120,
                18,
                "medium",
                2_048,
                Path::new("/tmp/window.mkv"),
                None,
            );

            let converter = args
                .iter()
                .position(|arg| arg == "compositor")
                .expect("every profile needs a rate converter for an idle window");
            let pinned_before_converter = args[..converter]
                .iter()
                .any(|arg| arg.contains("framerate=") || arg.contains("DMABuf"));

            assert!(
                !pinned_before_converter,
                "{:?} pins caps pipewiresrc cannot satisfy: {args:?}",
                profile
            );
            assert!(args.contains(&"force-live=true".to_owned()));
        }
    }

    #[test]
    fn memfd_fallback_accepts_any_cpu_readable_raw_format() {
        let args = recording_pipeline_args(
            9,
            WindowPipelineProfile::MemFdX264,
            800,
            600,
            60,
            20,
            "superfast",
            100_000,
            Path::new("/tmp/window.mp4"),
            Some(2),
        );
        assert!(!args.iter().any(|arg| arg == "video/x-raw,format=BGRx"));
        assert!(args.contains(&"videoconvert".to_owned()));
        assert!(args.contains(&"x264enc".to_owned()));
        assert!(args.contains(&"pass=qual".to_owned()));
        assert!(args.contains(&"quantizer=20".to_owned()));
        assert!(args.contains(&"bitrate=100000".to_owned()));
        assert!(args.contains(&"video/x-raw,format=I420,width=800,height=600".to_owned()));
        assert!(args
            .contains(&"video/x-raw,format=I420,width=800,height=600,framerate=60/1".to_owned()));
        assert!(args.contains(&"compositor".to_owned()));
        assert!(args.contains(&"identity".to_owned()));
        assert!(args.contains(&"eos-after=3".to_owned()));
        assert!(!args.contains(&"num-buffers=2".to_owned()));
        assert!(!args.contains(&"single-segment=true".to_owned()));
    }

    #[test]
    fn memfd_recording_negotiates_the_requested_cpu_format_and_fps() {
        let args = recording_pipeline_args(
            9,
            WindowPipelineProfile::MemFdX264,
            1_920,
            1_080,
            120,
            18,
            "veryfast",
            100_000,
            Path::new("/tmp/monitor.mkv"),
            None,
        );

        let output_caps = args
            .iter()
            .position(|arg| arg.contains("framerate=120/1"))
            .expect("recording should negotiate the requested output FPS");
        let encoder = args
            .iter()
            .position(|arg| arg == "x264enc")
            .expect("recording should retain its encoder");

        assert!(output_caps < encoder);
        assert!(args.contains(
            &"video/x-raw,format=I420,width=1920,height=1080,framerate=120/1".to_owned()
        ));
        assert!(args.contains(&"compositor".to_owned()));
        assert!(!args.iter().any(|arg| arg.starts_with("keepalive-time=")));
        assert!(!args.iter().any(|arg| arg.starts_with("eos-after=")));
    }

    #[test]
    fn keyframes_stay_close_enough_to_scrub_through() {
        // A quarter second at each supported rate.
        assert_eq!(keyframe_interval(120), 30);
        assert_eq!(keyframe_interval(60), 15);
        assert_eq!(keyframe_interval(30), 7);
        // Never zero, whatever the rate.
        assert_eq!(keyframe_interval(1), 1);
        assert_eq!(keyframe_interval(0), 1);
    }

    #[test]
    fn both_profiles_ask_the_encoder_for_those_keyframes() {
        for profile in WindowPipelineProfile::ALL {
            let args = recording_pipeline_args(
                7,
                profile,
                1_920,
                1_080,
                120,
                18,
                "medium",
                2_048,
                Path::new("/tmp/window.mkv"),
                None,
            );
            assert!(
                args.contains(&"key-int-max=30".to_owned()),
                "{profile:?} should carry the scrubbable interval: {args:?}"
            );
        }
    }

    #[test]
    fn identity_limit_accounts_for_the_unforwarded_eos_buffer() {
        assert_eq!(identity_eos_after(2), 3);
    }

    #[test]
    fn window_recording_uses_a_streamable_recovery_container() {
        let args = recording_pipeline_args(
            9,
            WindowPipelineProfile::MemFdX264,
            800,
            600,
            60,
            20,
            "superfast",
            100_000,
            Path::new("/tmp/window.mkv"),
            None,
        );
        assert!(args.contains(&"matroskamux".to_owned()));
        assert!(args.contains(&"streamable=true".to_owned()));
        assert!(!args.contains(&"mp4mux".to_owned()));
    }

    #[test]
    fn child_unblocks_sigint_before_exec() {
        std::thread::spawn(|| unsafe {
            let mut blocked = std::mem::zeroed::<libc::sigset_t>();
            assert_eq!(libc::sigemptyset(&mut blocked), 0);
            assert_eq!(libc::sigaddset(&mut blocked, libc::SIGINT), 0);
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, std::ptr::null_mut(),),
                0
            );

            let mut command = Command::new("sh");
            command.args(["-c", "kill -INT $$; exit 99"]);
            command.pre_exec(reset_child_signal_state);
            let status = command.status().expect("child process should start");

            assert_eq!(
                libc::pthread_sigmask(libc::SIG_UNBLOCK, &blocked, std::ptr::null_mut(),),
                0
            );
            assert_eq!(status.signal(), Some(libc::SIGINT));
        })
        .join()
        .expect("signal-mask test thread should not panic");
    }
}
