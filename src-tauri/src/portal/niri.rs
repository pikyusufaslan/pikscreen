use super::*;

/// Keep a portal-provided window rectangle inside one Niri output.
///
/// Niri window decorations can extend a few pixels beyond the output edge.
/// `grim` can screenshot that global rectangle, but wf-recorder intentionally
/// rejects regions which overlap multiple outputs. Picking the output with the
/// largest overlap keeps preview and recording on the same fixed visible area.
pub(super) fn clamp_window_crop_to_niri_output(crop: CaptureCrop) -> CaptureCrop {
    let output = match Command::new("niri").args(["msg", "-j", "outputs"]).output() {
        Ok(output) if output.status.success() => output,
        _ => return crop,
    };
    let Ok(layout) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return crop;
    };
    clamp_window_crop_to_output_layout(crop, &layout)
}

pub(super) fn clamp_window_crop_to_output_layout(
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

pub(super) fn pikscreen_niri_binary() -> PathBuf {
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

pub(super) fn selected_niri_window_id(node_id: u32) -> Result<u64, String> {
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

pub(super) fn niri_window_id_for_node(casts: &serde_json::Value, node_id: u32) -> Option<u64> {
    casts.as_array()?.iter().find_map(|cast| {
        (cast.get("pw_node_id").and_then(serde_json::Value::as_u64) == Some(node_id as u64))
            .then(|| cast.get("target")?.get("Window")?.get("id")?.as_u64())
            .flatten()
    })
}

pub(super) fn focus_niri_window(window_id: u64) -> Result<(), String> {
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

pub(super) fn current_niri_window_geometry(window_id: u64) -> Option<CaptureCrop> {
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

pub(super) fn settled_niri_window_geometry(window_id: u64) -> Option<CaptureCrop> {
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

pub(super) fn detect_quickshell_bottom_bar_height() -> i32 {
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

pub(super) struct NiriCursorClient {
    pub(super) stream: BufReader<UnixStream>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct NiriCursorState {
    pub(super) position: (f64, f64),
    pub(super) kind: TahoeCursor,
}

#[derive(Clone, Debug)]
pub(super) struct NiriClickEvents {
    pub(super) now_ms: u64,
    pub(super) events: Vec<NiriClickEvent>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct NiriClickEvent {
    pub(super) time_ms: u64,
    pub(super) position: (f64, f64),
    pub(super) primary: bool,
}

impl NiriCursorClient {
    pub(super) fn connect() -> Result<Self, String> {
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

    pub(super) fn request(&mut self, request: &[u8]) -> Result<String, String> {
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

    pub(super) fn position(&mut self) -> Result<(f64, f64), String> {
        parse_niri_cursor_reply(&self.request(b"\"CursorPosition\"\n")?)
    }

    pub(super) fn state(&mut self) -> Result<NiriCursorState, String> {
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

    pub(super) fn click_events(&mut self, since_ms: u64) -> Result<NiriClickEvents, String> {
        let request = format!("{{\"ClickEvents\":{{\"since_ms\":{since_ms}}}}}\n");
        parse_niri_click_events_reply(&self.request(request.as_bytes())?)
    }

    pub(super) fn window_geometry(&mut self, id: u64) -> Result<Option<CaptureCrop>, String> {
        let request = format!("{{\"WindowGeometry\":{{\"id\":{id}}}}}\n");
        parse_niri_window_geometry_reply(&self.request(request.as_bytes())?)
    }
}

pub(super) fn sample_niri_click_clock(
    cursor_client: &mut NiriCursorClient,
) -> Result<(u64, Instant), String> {
    let request_started_at = Instant::now();
    let now_ms = cursor_client.click_events(0)?.now_ms;
    let round_trip = request_started_at.elapsed();
    Ok((now_ms, request_started_at + round_trip / 2))
}

pub(super) fn translate_niri_epoch(
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

pub(super) fn parse_niri_cursor_reply(reply: &str) -> Result<(f64, f64), String> {
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

pub(super) fn parse_niri_cursor_state_reply(reply: &str) -> Result<NiriCursorState, String> {
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

pub(super) fn parse_niri_window_geometry_reply(reply: &str) -> Result<Option<CaptureCrop>, String> {
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

pub(super) fn parse_niri_click_events_reply(reply: &str) -> Result<NiriClickEvents, String> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
