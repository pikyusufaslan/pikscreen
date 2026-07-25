use serde::Deserialize;
use std::{
    env,
    io::{Read, Write},
    net::Shutdown,
    os::unix::{fs::FileTypeExt, net::UnixStream},
    path::{Path, PathBuf},
    time::Duration,
};

const REQUEST_TIMEOUT: Duration = Duration::from_millis(250);

pub struct HyprlandCursorClient {
    socket_path: PathBuf,
}

impl HyprlandCursorClient {
    pub fn connect() -> Result<Self, String> {
        let runtime_dir = env::var_os("XDG_RUNTIME_DIR")
            .ok_or_else(|| "Hyprland cursor capture requires XDG_RUNTIME_DIR.".to_owned())?;
        let signature = env::var_os("HYPRLAND_INSTANCE_SIGNATURE").ok_or_else(|| {
            "Hyprland cursor capture requires HYPRLAND_INSTANCE_SIGNATURE.".to_owned()
        })?;
        let socket_path =
            request_socket_path(Path::new(&runtime_dir), &signature.to_string_lossy());
        let metadata = socket_path.metadata().map_err(|error| {
            format!(
                "Could not find Hyprland's request socket {}: {error}",
                socket_path.display()
            )
        })?;
        if !metadata.file_type().is_socket() {
            return Err(format!(
                "Hyprland request path is not a socket: {}",
                socket_path.display()
            ));
        }
        Ok(Self { socket_path })
    }

    pub fn position(&self) -> Result<(f64, f64), String> {
        let mut stream = UnixStream::connect(&self.socket_path).map_err(|error| {
            format!(
                "Could not connect to Hyprland's request socket {}: {error}",
                self.socket_path.display()
            )
        })?;
        stream
            .set_read_timeout(Some(REQUEST_TIMEOUT))
            .map_err(|error| format!("Could not bound the Hyprland cursor read: {error}"))?;
        stream
            .set_write_timeout(Some(REQUEST_TIMEOUT))
            .map_err(|error| format!("Could not bound the Hyprland cursor write: {error}"))?;
        stream
            .write_all(b"j/cursorpos")
            .map_err(|error| format!("Could not request Hyprland's cursor position: {error}"))?;
        stream
            .shutdown(Shutdown::Write)
            .map_err(|error| format!("Could not finish the Hyprland cursor request: {error}"))?;
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .map_err(|error| format!("Could not read Hyprland's cursor position: {error}"))?;
        parse_cursor_position(&response)
    }
}

pub fn session_available() -> bool {
    env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some_and(|value| !value.is_empty())
}

fn request_socket_path(runtime_dir: &Path, signature: &str) -> PathBuf {
    runtime_dir
        .join("hypr")
        .join(signature)
        .join(".socket.sock")
}

#[derive(Deserialize)]
struct CursorPosition {
    x: f64,
    y: f64,
}

fn parse_cursor_position(response: &str) -> Result<(f64, f64), String> {
    let position: CursorPosition = serde_json::from_str(response)
        .map_err(|error| format!("Hyprland returned invalid cursor-position JSON: {error}"))?;
    if !position.x.is_finite() || !position.y.is_finite() {
        return Err("Hyprland returned non-finite cursor coordinates.".to_owned());
    }
    Ok((position.x, position.y))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_socket_uses_the_current_runtime_instance() {
        assert_eq!(
            request_socket_path(Path::new("/run/user/1000"), "instance-42"),
            PathBuf::from("/run/user/1000/hypr/instance-42/.socket.sock")
        );
    }

    #[test]
    fn parses_integer_and_fractional_cursor_positions() {
        assert_eq!(
            parse_cursor_position("{\"x\":2573,\"y\":596}"),
            Ok((2573.0, 596.0))
        );
        assert_eq!(
            parse_cursor_position("{\"x\":12.5,\"y\":-8.25}"),
            Ok((12.5, -8.25))
        );
    }

    #[test]
    fn rejects_incomplete_cursor_positions() {
        assert!(parse_cursor_position("{\"x\":20}")
            .expect_err("a missing coordinate must fail")
            .contains("invalid cursor-position JSON"));
    }
}
