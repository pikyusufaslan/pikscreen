use std::{
    fs,
    io::{Read, Result as IoResult},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::process::{CommandExt, ExitStatusExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

const CHILD_PIPEWIRE_FD: i32 = 33;
const PROBE_TIMEOUT: Duration = Duration::from_secs(6);
const STOP_TIMEOUT: Duration = Duration::from_secs(3);

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
    profile: WindowPipelineProfile,
    output_path: PathBuf,
}

#[derive(Debug)]
struct ChildResult {
    status: ExitStatus,
    stderr: String,
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
    let child = spawn_gst(pipewire_fd, &args)?;
    let result = wait_for_child(child, PROBE_TIMEOUT)?;
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
    let child = spawn_gst(pipewire_fd, &args)?;
    Ok(WindowRecorder {
        child,
        profile,
        output_path: output_path.to_path_buf(),
    })
}

pub fn ensure_recorder_started(recorder: &mut WindowRecorder) -> Result<(), String> {
    thread::sleep(Duration::from_millis(200));
    let Some(status) = recorder
        .child
        .try_wait()
        .map_err(|error| format!("Could not inspect the window PipeWire pipeline: {error}"))?
    else {
        return Ok(());
    };
    Err(format!(
        "{} recorder failed during startup with {}: {}",
        recorder.profile.label(),
        status,
        compact_log(&read_child_stderr(&mut recorder.child))
    ))
}

pub fn stop_recorder(mut recorder: WindowRecorder) -> Result<(), String> {
    signal_child(&recorder.child, libc::SIGINT)?;
    let result = match wait_for_child_inner(&mut recorder.child, STOP_TIMEOUT) {
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
    let mut args = pipeline_prefix(node_id, profile);
    args.extend(profile_conversion_args(profile));
    args.extend([
        "!".to_owned(),
        "videoconvert".to_owned(),
        "!".to_owned(),
        "videoscale".to_owned(),
        "!".to_owned(),
        format!("video/x-raw,format=NV12,width={width},height={height}"),
        "!".to_owned(),
        "compositor".to_owned(),
        "force-live=true".to_owned(),
        "start-time-selection=first".to_owned(),
        "background=black".to_owned(),
        "!".to_owned(),
        format!("video/x-raw,format=NV12,width={width},height={height},framerate={fps}/1"),
        "!".to_owned(),
    ]);
    if let Some(num_buffers) = num_buffers {
        args.extend([
            "identity".to_owned(),
            format!("eos-after={}", identity_eos_after(num_buffers)),
            "!".to_owned(),
        ]);
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
            format!("key-int-max={}", fps.saturating_mul(2)),
            "!".to_owned(),
        ]),
        WindowPipelineProfile::MemFdX264 => args.extend([
            "x264enc".to_owned(),
            format!("speed-preset={preset}"),
            "tune=zerolatency".to_owned(),
            "pass=qual".to_owned(),
            format!("quantizer={crf}"),
            format!("bitrate={max_bitrate_kbps}"),
            format!("key-int-max={}", fps.saturating_mul(2)),
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

fn identity_eos_after(output_frames: u32) -> u32 {
    output_frames.saturating_add(1)
}

fn pipeline_prefix(node_id: u32, profile: WindowPipelineProfile) -> Vec<String> {
    let mut args = vec![
        "-q".to_owned(),
        "-e".to_owned(),
        "pipewiresrc".to_owned(),
        format!("fd={CHILD_PIPEWIRE_FD}"),
        format!("path={node_id}"),
        "do-timestamp=true".to_owned(),
    ];
    args.extend(["!".to_owned(), "queue".to_owned(), "!".to_owned()]);
    args.push(match profile {
        WindowPipelineProfile::DmaBufGlVaapi => {
            "video/x-raw(memory:DMABuf),format=DMA_DRM".to_owned()
        }
        WindowPipelineProfile::MemFdX264 => "video/x-raw,format=BGRx".to_owned(),
    });
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

fn spawn_gst(pipewire_fd: OwnedFd, args: &[String]) -> Result<Child, String> {
    let mut command = Command::new("gst-launch-1.0");
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(move || prepare_child_process(&pipewire_fd));
    }
    command
        .spawn()
        .map_err(|error| format!("Could not start the window PipeWire pipeline: {error}"))
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

fn wait_for_child(mut child: Child, timeout: Duration) -> Result<ChildResult, String> {
    wait_for_child_inner(&mut child, timeout)
}

fn wait_for_child_inner(child: &mut Child, timeout: Duration) -> Result<ChildResult, String> {
    let started_at = Instant::now();
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("Could not inspect the window pipeline: {error}"))?
        {
            return Ok(ChildResult {
                status,
                stderr: read_child_stderr(child),
            });
        }
        if started_at.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "Window PipeWire pipeline timed out after {:.1}s: {}",
                timeout.as_secs_f64(),
                compact_log(&read_child_stderr(child))
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

fn read_child_stderr(child: &mut Child) -> String {
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    stderr
}

fn compact_log(log: &str) -> String {
    let compact = log
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" | ");
    if compact.len() > 2_000 {
        format!("{}…", &compact[..2_000])
    } else if compact.is_empty() {
        "no diagnostic output".to_owned()
    } else {
        compact
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
        assert!(args.contains(&"video/x-raw(memory:DMABuf),format=DMA_DRM".to_owned()));
        assert!(args.contains(&"vah264enc".to_owned()));
        assert!(!args.iter().any(|arg| arg == "-g"));
    }

    #[test]
    fn memfd_fallback_forces_cpu_readable_video() {
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
        assert!(args.contains(&"video/x-raw,format=BGRx".to_owned()));
        assert!(args.contains(&"x264enc".to_owned()));
        assert!(args.contains(&"pass=qual".to_owned()));
        assert!(args.contains(&"quantizer=20".to_owned()));
        assert!(args.contains(&"bitrate=100000".to_owned()));
        assert!(args.contains(&"compositor".to_owned()));
        assert!(args.contains(&"force-live=true".to_owned()));
        assert!(args.contains(&"start-time-selection=first".to_owned()));
        assert!(args.contains(&"identity".to_owned()));
        assert!(args.contains(&"eos-after=3".to_owned()));
        assert!(!args.contains(&"num-buffers=2".to_owned()));
        assert!(!args.contains(&"single-segment=true".to_owned()));
    }

    #[test]
    fn portal_recording_repeats_the_latest_real_frame_on_a_live_clock() {
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

        let compositor = args
            .iter()
            .position(|arg| arg == "compositor")
            .expect("recording should have a live compositor");
        let output_caps = args
            .iter()
            .position(|arg| arg.contains("framerate=120/1"))
            .expect("recording should negotiate the requested output FPS");
        let encoder = args
            .iter()
            .position(|arg| arg == "x264enc")
            .expect("recording should retain its encoder");

        assert!(compositor < output_caps);
        assert!(output_caps < encoder);
        assert!(args.contains(&"force-live=true".to_owned()));
        assert!(args.contains(&"start-time-selection=first".to_owned()));
        assert!(!args.iter().any(|arg| arg.starts_with("keepalive-time=")));
        assert!(!args.iter().any(|arg| arg.starts_with("eos-after=")));
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
