use super::*;

pub(super) const PORTAL_CLICK_DEDUP_MS: u64 = 16;

pub(super) fn listen_for_cursor_samples(
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

pub(super) fn listen_for_hyprland_input_bridge(
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

pub(super) fn push_cursor_sample(samples: &mut Vec<CursorSample>, sample: CursorSample) {
    if let Some(previous) = samples
        .last_mut()
        .filter(|previous| previous.time_ms == sample.time_ms)
    {
        *previous = sample;
    } else {
        samples.push(sample);
    }
}

pub(super) fn hyprland_click_sample(
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

pub(super) fn listen_for_portal_cursor_events(
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

pub(super) fn portal_cursor_sample(
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

pub(super) fn gnome_cursor_sample(
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

pub(super) fn listen_for_gnome_cursor_samples(
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

pub(super) fn listen_for_portal_clicks(active: Arc<Mutex<Option<ActiveCapture>>>, id: u64) {
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

pub(super) fn pointer_button_devices() -> Vec<Device> {
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

pub(super) fn portal_click_button(event: &evdev::InputEvent) -> Option<bool> {
    if event.event_type() != EventType::KEY || event.value() != 1 {
        return None;
    }
    match KeyCode::new(event.code()) {
        KeyCode::BTN_LEFT => Some(true),
        KeyCode::BTN_RIGHT => Some(false),
        _ => None,
    }
}

pub(super) fn portal_click_is_duplicate(
    previous: Option<(u64, bool)>,
    time_ms: u64,
    primary: bool,
) -> bool {
    previous.is_some_and(|(previous_time_ms, previous_primary)| {
        previous_primary == primary
            && time_ms.saturating_sub(previous_time_ms) <= PORTAL_CLICK_DEDUP_MS
    })
}

pub(super) fn cursor_sample_for_geometry(
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

pub(super) fn hyprland_input_sample(
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

pub(super) fn hidden_cursor_sample(position: (f64, f64), time_ms: u64) -> CursorSample {
    CursorSample {
        time_ms,
        x: position.0,
        y: position.1,
        kind: TahoeCursor::Hidden,
    }
}

pub(super) fn point_is_inside((x, y): (f64, f64), geometry: CaptureCrop) -> bool {
    x >= geometry.x as f64
        && y >= geometry.y as f64
        && x < (geometry.x + geometry.width) as f64
        && y < (geometry.y + geometry.height) as f64
}

pub(super) fn cursor_sample(
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

pub(super) fn click_sample(
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
