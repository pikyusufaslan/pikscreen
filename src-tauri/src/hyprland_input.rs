use std::{
    env, fs, io::ErrorKind, os::unix::net::UnixDatagram, path::PathBuf, process::Command,
    time::Duration,
};

const PACKET_MAGIC: [u8; 8] = *b"ZMHYPR1\0";
const PACKET_VERSION: u32 = 1;
const PACKET_SIZE: usize = 120;
const PLUGIN_NAME: &str = "pikscreen-hyprland-input";
const SOCKET_FILE: &str = "pikscreen-hyprland-input-v1.sock";

#[cfg(test)]
pub const EVENT_SNAPSHOT: u32 = 0;
#[cfg(test)]
pub const EVENT_CURSOR_SHAPE: u32 = 1;
pub const EVENT_MOUSE_BUTTON: u32 = 2;
pub const EVENT_KEYBOARD_KEY: u32 = 3;
pub const MODIFIER_SHIFT: u32 = 1;
#[cfg(test)]
pub const MODIFIER_ALT: u32 = 2;
pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const KEY_LEFTALT: u32 = 56;
pub const KEY_RIGHTALT: u32 = 100;

#[derive(Clone, Debug, PartialEq)]
pub struct HyprlandInputPacket {
    pub monotonic_ns: u64,
    pub kind: u32,
    pub code: u32,
    pub pressed: bool,
    pub modifiers: u32,
    pub position: (f64, f64),
    pub shape: String,
}

impl HyprlandInputPacket {
    pub fn recording_time_ms(&self, recording_epoch_ns: u64) -> u64 {
        self.monotonic_ns
            .saturating_sub(recording_epoch_ns)
            .saturating_div(1_000_000)
    }

    pub fn shift_held(&self) -> bool {
        self.modifiers & MODIFIER_SHIFT != 0
    }
}

pub struct HyprlandInputBridge {
    socket: UnixDatagram,
    socket_path: PathBuf,
}

impl HyprlandInputBridge {
    pub fn connect() -> Result<Self, String> {
        let socket_path = runtime_socket_path()?;
        match fs::remove_file(&socket_path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "Could not replace PikScreen's Hyprland input socket {}: {error}",
                    socket_path.display()
                ));
            }
        }
        let socket = UnixDatagram::bind(&socket_path).map_err(|error| {
            format!(
                "Could not bind PikScreen's Hyprland input socket {}: {error}",
                socket_path.display()
            )
        })?;
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .map_err(|error| format!("Could not configure the Hyprland input socket: {error}"))?;
        let bridge = Self {
            socket,
            socket_path,
        };
        ensure_plugin_loaded()?;
        Ok(bridge)
    }

    pub fn recv(&self) -> Result<HyprlandInputPacket, BridgeReceiveError> {
        let mut bytes = [0_u8; 256];
        let size = match self.socket.recv(&mut bytes) {
            Ok(size) => size,
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return Err(BridgeReceiveError::Timeout);
            }
            Err(error) => return Err(BridgeReceiveError::Io(error)),
        };
        decode_packet(&bytes[..size]).map_err(BridgeReceiveError::Invalid)
    }
}

impl Drop for HyprlandInputBridge {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket_path);
    }
}

#[derive(Debug)]
pub enum BridgeReceiveError {
    Timeout,
    Invalid(String),
    Io(std::io::Error),
}

pub fn monotonic_now_ns() -> Result<u64, String> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let result = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) };
    if result != 0 {
        return Err(format!(
            "Could not read the monotonic recording clock: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok((time.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(time.tv_nsec as u64))
}

fn runtime_socket_path() -> Result<PathBuf, String> {
    let runtime_dir = env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| "Hyprland input capture requires XDG_RUNTIME_DIR.".to_owned())?;
    Ok(PathBuf::from(runtime_dir).join(SOCKET_FILE))
}

fn plugin_paths() -> (PathBuf, PathBuf, PathBuf) {
    let tools = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tools");
    let plugin = env::var_os("PIKSCREEN_HYPRLAND_INPUT_PLUGIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| tools.join("pikscreen-hyprland-input.so"));
    let abi = PathBuf::from(format!("{}.abi", plugin.display()));
    (tools.join("build-hyprland-input.sh"), plugin, abi)
}

fn ensure_plugin_loaded() -> Result<(), String> {
    let loaded = hyprctl(&["plugin", "list"])?;
    if loaded.contains(PLUGIN_NAME) {
        return Ok(());
    }
    let (build_script, plugin, abi_file) = plugin_paths();
    let current_abi = hyprland_abi()?;
    let built_abi = fs::read_to_string(&abi_file)
        .ok()
        .map(|abi| abi.trim().to_owned());
    if !plugin.is_file() || built_abi.as_deref() != Some(current_abi.as_str()) {
        let output = Command::new("bash")
            .arg(&build_script)
            .output()
            .map_err(|error| {
                format!(
                    "Could not run the Hyprland input plugin build {}: {error}",
                    build_script.display()
                )
            })?;
        if !output.status.success() {
            return Err(format!(
                "Could not build the Hyprland input plugin: {}",
                command_error(&output)
            ));
        }
    }
    let plugin = plugin.canonicalize().map_err(|error| {
        format!(
            "Could not resolve the Hyprland input plugin {}: {error}",
            plugin.display()
        )
    })?;
    hyprctl(&["plugin", "load", plugin.to_string_lossy().as_ref()])?;
    let loaded = hyprctl(&["plugin", "list"])?;
    if loaded.contains(PLUGIN_NAME) {
        Ok(())
    } else {
        Err("Hyprland did not report PikScreen's input plugin after loading it.".to_owned())
    }
}

fn hyprland_abi() -> Result<String, String> {
    let version = hyprctl(&["version"])?;
    version
        .lines()
        .find_map(|line| line.strip_prefix("Version ABI string: "))
        .map(str::trim)
        .filter(|abi| !abi.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "Hyprland did not report its plugin ABI string.".to_owned())
}

fn hyprctl(args: &[&str]) -> Result<String, String> {
    let output = Command::new("hyprctl")
        .args(args)
        .output()
        .map_err(|error| format!("Could not run hyprctl {}: {error}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "hyprctl {} failed: {}",
            args.join(" "),
            command_error(&output)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn command_error(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    if detail.is_empty() {
        output.status.to_string()
    } else {
        detail.to_owned()
    }
}

fn decode_packet(bytes: &[u8]) -> Result<HyprlandInputPacket, String> {
    if bytes.len() != PACKET_SIZE {
        return Err(format!(
            "Hyprland input plugin sent a {}-byte packet; expected {PACKET_SIZE}.",
            bytes.len()
        ));
    }
    if bytes[..8] != PACKET_MAGIC {
        return Err("Hyprland input plugin sent an invalid packet magic.".to_owned());
    }
    let version = read_u32(bytes, 8)?;
    let size = read_u32(bytes, 12)? as usize;
    if version != PACKET_VERSION || size != PACKET_SIZE {
        return Err(format!(
            "Hyprland input packet version/size mismatch: {version}/{size}."
        ));
    }
    let x = read_f64(bytes, 40)?;
    let y = read_f64(bytes, 48)?;
    if !x.is_finite() || !y.is_finite() {
        return Err("Hyprland input plugin sent non-finite cursor coordinates.".to_owned());
    }
    let shape_bytes = &bytes[56..120];
    let shape_end = shape_bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(shape_bytes.len());
    let shape = std::str::from_utf8(&shape_bytes[..shape_end])
        .map_err(|error| format!("Hyprland input plugin sent an invalid shape name: {error}"))?
        .to_owned();
    Ok(HyprlandInputPacket {
        monotonic_ns: read_u64(bytes, 16)?,
        kind: read_u32(bytes, 24)?,
        code: read_u32(bytes, 28)?,
        pressed: read_u32(bytes, 32)? != 0,
        modifiers: read_u32(bytes, 36)?,
        position: (x, y),
        shape: if shape.is_empty() {
            "default".to_owned()
        } else {
            shape
        },
    })
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    bytes
        .get(offset..offset + 4)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u32::from_ne_bytes)
        .ok_or_else(|| "Hyprland input packet ended inside a u32 field.".to_owned())
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, String> {
    bytes
        .get(offset..offset + 8)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u64::from_ne_bytes)
        .ok_or_else(|| "Hyprland input packet ended inside a u64 field.".to_owned())
}

fn read_f64(bytes: &[u8], offset: usize) -> Result<f64, String> {
    bytes
        .get(offset..offset + 8)
        .and_then(|bytes| bytes.try_into().ok())
        .map(f64::from_ne_bytes)
        .ok_or_else(|| "Hyprland input packet ended inside an f64 field.".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet_bytes() -> [u8; PACKET_SIZE] {
        let mut bytes = [0_u8; PACKET_SIZE];
        bytes[..8].copy_from_slice(&PACKET_MAGIC);
        bytes[8..12].copy_from_slice(&PACKET_VERSION.to_ne_bytes());
        bytes[12..16].copy_from_slice(&(PACKET_SIZE as u32).to_ne_bytes());
        bytes[16..24].copy_from_slice(&5_500_000_000_u64.to_ne_bytes());
        bytes[24..28].copy_from_slice(&EVENT_CURSOR_SHAPE.to_ne_bytes());
        bytes[28..32].copy_from_slice(&BTN_LEFT.to_ne_bytes());
        bytes[32..36].copy_from_slice(&1_u32.to_ne_bytes());
        bytes[36..40].copy_from_slice(&(MODIFIER_SHIFT | MODIFIER_ALT).to_ne_bytes());
        bytes[40..48].copy_from_slice(&640.25_f64.to_ne_bytes());
        bytes[48..56].copy_from_slice(&360.5_f64.to_ne_bytes());
        bytes[56..63].copy_from_slice(b"pointer");
        bytes
    }

    #[test]
    fn decodes_version_one_input_packets() {
        assert_eq!(
            decode_packet(&packet_bytes()),
            Ok(HyprlandInputPacket {
                monotonic_ns: 5_500_000_000,
                kind: EVENT_CURSOR_SHAPE,
                code: BTN_LEFT,
                pressed: true,
                modifiers: MODIFIER_SHIFT | MODIFIER_ALT,
                position: (640.25, 360.5),
                shape: "pointer".to_owned(),
            })
        );
    }

    #[test]
    fn rejects_wrong_packet_size_and_magic() {
        assert!(decode_packet(&packet_bytes()[..119])
            .expect_err("short packet must fail")
            .contains("119-byte"));
        let mut invalid = packet_bytes();
        invalid[0] = b'X';
        assert!(decode_packet(&invalid)
            .expect_err("bad magic must fail")
            .contains("invalid packet magic"));
    }

    #[test]
    fn translates_monotonic_time_to_recording_time() {
        let packet = decode_packet(&packet_bytes()).expect("packet");
        assert_eq!(packet.recording_time_ms(4_250_000_000), 1_250);
        assert_eq!(packet.recording_time_ms(6_000_000_000), 0);
    }
}
