use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Marker {
    #[serde(alias = "time_ms")]
    pub time_ms: u64,
    pub x: f64,
    pub y: f64,
    #[serde(default, alias = "target_scale")]
    pub target_scale: Option<f64>,
    /// `None` means a normal tap. A value is recorded when Alt is held and
    /// then released.
    #[serde(alias = "held_until_ms")]
    pub held_until_ms: Option<u64>,
    /// Double-Alt gestures intentionally remove the rendered cursor only for
    /// this zoom event.
    #[serde(alias = "hide_cursor")]
    pub hide_cursor: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct ZoomTiming {
    pub zoom_in_ms: u64,
    pub hold_ms: u64,
    pub zoom_out_ms: u64,
    pub target_scale: f64,
}

impl Default for ZoomTiming {
    fn default() -> Self {
        Self {
            // Recordly's default Focused motion preset uses 200 ms camera
            // transfers. Keep PikScreen's normal tap gesture at three seconds
            // by spending the remaining time at the selected focus.
            zoom_in_ms: 190,
            hold_ms: 2_620,
            zoom_out_ms: 190,
            target_scale: 1.8,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct ZoomEvent {
    pub start_ms: u64,
    /// The camera reaches this event's target at the end of its transfer.
    pub transition_end_ms: u64,
    pub hold_until_ms: u64,
    /// This is exclusive: a following marker starts its own camera transfer
    /// at this exact timestamp, rather than forcing a return to 1x first.
    pub end_ms: u64,
    pub center_x: f64,
    pub center_y: f64,
    /// Zoom depth is captured with the gesture so every later render pass uses
    /// the same framing even when it evaluates the event with default timing.
    pub target_scale: f64,
    pub hide_cursor: bool,
    from_scale: f64,
    from_crop_x: f64,
    from_crop_y: f64,
}

impl ZoomEvent {
    pub fn standalone(
        start_ms: u64,
        hold_until_ms: u64,
        center_x: f64,
        center_y: f64,
        hide_cursor: bool,
        timing: ZoomTiming,
    ) -> Self {
        Self {
            start_ms,
            transition_end_ms: start_ms.saturating_add(timing.zoom_in_ms),
            hold_until_ms,
            end_ms: hold_until_ms.saturating_add(timing.zoom_out_ms),
            center_x,
            center_y,
            target_scale: timing.target_scale,
            hide_cursor,
            from_scale: 1.0,
            from_crop_x: 0.0,
            from_crop_y: 0.0,
        }
    }

    pub fn end_ms(self, _timing: ZoomTiming) -> u64 {
        self.end_ms
    }

    pub fn scale_at(self, time_ms: u64, timing: ZoomTiming) -> f64 {
        CameraTransform::at(self, time_ms, timing).scale
    }

    pub fn progress_at(self, time_ms: u64, timing: ZoomTiming) -> f64 {
        if time_ms < self.start_ms || time_ms >= self.end_ms(timing) {
            return 0.0;
        }
        (self.scale_at(time_ms, timing) - 1.0) / (self.target_scale - 1.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraTransform {
    pub scale: f64,
    pub crop_x: f64,
    pub crop_y: f64,
}

#[derive(Clone, Copy, Debug)]
struct SpringState {
    value: f64,
    velocity: f64,
    initialized: bool,
}

impl SpringState {
    fn new(value: f64) -> Self {
        Self {
            value,
            velocity: 0.0,
            initialized: false,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct SpringConfig {
    stiffness: f64,
    damping: f64,
    mass: f64,
    rest_delta: f64,
    rest_speed: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct RecordlyCameraSpring {
    scale: SpringState,
    crop_x: SpringState,
    crop_y: SpringState,
}

impl Default for RecordlyCameraSpring {
    fn default() -> Self {
        Self {
            scale: SpringState::new(1.0),
            crop_x: SpringState::new(0.0),
            crop_y: SpringState::new(0.0),
        }
    }
}

impl RecordlyCameraSpring {
    pub fn step(&mut self, target: CameraTransform, delta_ms: f64) -> CameraTransform {
        let config = recordly_camera_spring_config();
        CameraTransform {
            scale: step_spring_value(&mut self.scale, target.scale, delta_ms, config),
            crop_x: step_spring_value(&mut self.crop_x, target.crop_x, delta_ms, config),
            crop_y: step_spring_value(&mut self.crop_y, target.crop_y, delta_ms, config),
        }
    }
}

impl CameraTransform {
    pub fn at(event: ZoomEvent, time_ms: u64, timing: ZoomTiming) -> Self {
        if time_ms < event.start_ms || time_ms >= event.end_ms(timing) {
            return Self::identity();
        }
        let from = Self {
            scale: event.from_scale,
            crop_x: event.from_crop_x,
            crop_y: event.from_crop_y,
        };
        let target = target_transform(event.center_x, event.center_y, event.target_scale);
        if time_ms <= event.transition_end_ms {
            let progress = recordly_ease_out_zoom(normalized_progress(
                time_ms,
                event.start_ms,
                event.transition_end_ms,
            ));
            if from == Self::identity() {
                return Self::from_identity_towards_target(event, progress);
            }
            return Self::interpolate(from, target, progress);
        }
        if time_ms <= event.hold_until_ms {
            return target;
        }
        let remaining = 1.0
            - recordly_ease_out_zoom(normalized_progress(
                time_ms,
                event.hold_until_ms,
                event.end_ms(timing),
            ));
        Self::from_identity_towards_target(event, remaining)
    }

    fn identity() -> Self {
        Self {
            scale: 1.0,
            crop_x: 0.0,
            crop_y: 0.0,
        }
    }

    fn interpolate(from: Self, to: Self, progress: f64) -> Self {
        let progress = progress.clamp(0.0, 1.0);
        Self {
            scale: from.scale + (to.scale - from.scale) * progress,
            crop_x: from.crop_x + (to.crop_x - from.crop_x) * progress,
            crop_y: from.crop_y + (to.crop_y - from.crop_y) * progress,
        }
    }

    fn from_identity_towards_target(event: ZoomEvent, progress: f64) -> Self {
        let scale = 1.0 + (event.target_scale - 1.0) * progress;
        let crop = |focus: f64| {
            ((focus * event.target_scale - 0.5) * progress / scale).clamp(0.0, 1.0 - 1.0 / scale)
        };
        Self {
            scale,
            crop_x: crop(event.center_x),
            crop_y: crop(event.center_y),
        }
    }

    pub fn map_normalized_point(self, x: f64, y: f64) -> (f64, f64) {
        (
            ((x - self.crop_x) * self.scale).clamp(0.0, 1.0),
            ((y - self.crop_y) * self.scale).clamp(0.0, 1.0),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CropRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

pub fn build_zoom_events(markers: &[Marker], timing: ZoomTiming) -> Vec<ZoomEvent> {
    let mut ordered = markers.to_vec();
    ordered.sort_by_key(|marker| marker.time_ms);
    let mut events: Vec<ZoomEvent> = Vec::new();

    for marker in ordered {
        let target_scale = marker
            .target_scale
            .filter(|scale| scale.is_finite() && (1.2..=2.4).contains(scale))
            .unwrap_or(timing.target_scale);
        let (x, y) = clamp_focus(marker.x, marker.y, target_scale);
        let transition_end_ms = marker.time_ms.saturating_add(timing.zoom_in_ms);
        let held_until_ms = marker
            .held_until_ms
            .map(|release_ms| release_ms.max(transition_end_ms))
            .unwrap_or_else(|| transition_end_ms.saturating_add(timing.hold_ms));
        let from = events
            .last()
            .map(|event| CameraTransform::at(*event, marker.time_ms, timing))
            .unwrap_or_else(CameraTransform::identity);
        if let Some(active) = events.last_mut() {
            if marker.time_ms < active.end_ms {
                active.end_ms = marker.time_ms;
            }
        }
        events.push(ZoomEvent {
            start_ms: marker.time_ms,
            transition_end_ms,
            hold_until_ms: held_until_ms,
            end_ms: held_until_ms.saturating_add(timing.zoom_out_ms),
            center_x: x,
            center_y: y,
            target_scale,
            hide_cursor: marker.hide_cursor,
            from_scale: from.scale,
            from_crop_x: from.crop_x,
            from_crop_y: from.crop_y,
        });
    }
    events
}

pub fn camera_transform_at(
    events: &[ZoomEvent],
    time_ms: u64,
    timing: ZoomTiming,
) -> CameraTransform {
    events
        .iter()
        .rev()
        .find(|event| time_ms >= event.start_ms && time_ms < event.end_ms(timing))
        .map(|event| CameraTransform::at(*event, time_ms, timing))
        .unwrap_or_else(CameraTransform::identity)
}

fn target_transform(center_x: f64, center_y: f64, target_scale: f64) -> CameraTransform {
    let crop = |focus: f64| (focus - 0.5 / target_scale).clamp(0.0, 1.0 - 1.0 / target_scale);
    CameraTransform {
        scale: target_scale,
        crop_x: crop(center_x),
        crop_y: crop(center_y),
    }
}

pub fn crop_rect(source_width: u32, source_height: u32, scale: f64, x: f64, y: f64) -> CropRect {
    let safe_scale = scale.max(1.0);
    let width = ((source_width as f64 / safe_scale).round() as u32).clamp(1, source_width);
    let height = ((source_height as f64 / safe_scale).round() as u32).clamp(1, source_height);
    let center_x = x.clamp(0.0, 1.0) * source_width as f64;
    let center_y = y.clamp(0.0, 1.0) * source_height as f64;
    let max_x = source_width.saturating_sub(width) as f64;
    let max_y = source_height.saturating_sub(height) as f64;
    CropRect {
        x: (center_x - width as f64 / 2.0).clamp(0.0, max_x).round() as u32,
        y: (center_y - height as f64 / 2.0).clamp(0.0, max_y).round() as u32,
        width,
        height,
    }
}

fn clamp_focus(x: f64, y: f64, scale: f64) -> (f64, f64) {
    let margin = 1.0 / (2.0 * scale.max(1.0));
    (x.clamp(margin, 1.0 - margin), y.clamp(margin, 1.0 - margin))
}

fn normalized_progress(value: u64, start: u64, end: u64) -> f64 {
    if end <= start {
        return 1.0;
    }
    (value.saturating_sub(start) as f64 / end.saturating_sub(start) as f64).clamp(0.0, 1.0)
}

pub fn recordly_ease_out_zoom(progress: f64) -> f64 {
    // Recordly's cubic-bezier(0.16, 1, 0.3, 1), solved the same way its
    // renderer does: Newton iterations followed by a short binary refinement.
    let progress = progress.clamp(0.0, 1.0);
    let mut t = progress;
    for _ in 0..8 {
        let x = 0.48 * t - 0.06 * t * t + 0.58 * t * t * t;
        let derivative = 0.48 - 0.12 * t + 1.74 * t * t;
        if derivative.abs() < 1e-6 {
            break;
        }
        t = (t - (x - progress) / derivative).clamp(0.0, 1.0);
    }

    let mut lower = 0.0;
    let mut upper = 1.0;
    for _ in 0..10 {
        let x = 0.48 * t - 0.06 * t * t + 0.58 * t * t * t;
        if (x - progress).abs() < 1e-6 {
            break;
        }
        if x < progress {
            lower = t;
        } else {
            upper = t;
        }
        t = (lower + upper) / 2.0;
    }

    1.0 - (1.0 - t).powi(3)
}

fn recordly_camera_spring_config() -> SpringConfig {
    // Recordly's default zoomSmoothness=0.5 produces the base 100/21/1
    // spring. Its camera defaults then apply 1.0 stiffness, 1.13 damping,
    // and 1.12 mass multipliers.
    SpringConfig {
        stiffness: 100.0,
        damping: 21.0 * 1.13,
        mass: 1.12,
        rest_delta: 0.0005,
        rest_speed: 0.015,
    }
}

fn step_spring_value(
    state: &mut SpringState,
    target: f64,
    delta_ms: f64,
    config: SpringConfig,
) -> f64 {
    let safe_delta_ms = if !delta_ms.is_finite() || delta_ms <= 0.0 {
        1_000.0 / 60.0
    } else {
        delta_ms.clamp(1.0, 80.0)
    };
    if !state.initialized || !state.value.is_finite() {
        state.value = target;
        state.velocity = 0.0;
        state.initialized = true;
        return state.value;
    }
    if (target - state.value).abs() <= config.rest_delta
        && state.velocity.abs() <= config.rest_speed
    {
        state.value = target;
        state.velocity = 0.0;
        return state.value;
    }

    let undamped_frequency = (config.stiffness / config.mass).sqrt();
    let damping_ratio = config.damping / (2.0 * (config.stiffness * config.mass).sqrt());
    let initial_delta = target - state.value;
    let initial_velocity = -state.velocity;
    let seconds = safe_delta_ms / 1_000.0;
    let current = resolve_spring_position(
        seconds,
        target,
        initial_delta,
        initial_velocity,
        damping_ratio,
        undamped_frequency,
    );

    if damping_ratio >= 1.0 {
        let crossed = (state.value <= target && current > target)
            || (state.value >= target && current < target);
        if crossed {
            state.value = target;
            state.velocity = 0.0;
            return state.value;
        }
    }

    let epsilon = 0.0001;
    let ahead = resolve_spring_position(
        seconds + epsilon,
        target,
        initial_delta,
        initial_velocity,
        damping_ratio,
        undamped_frequency,
    );
    let velocity = (ahead - current) / epsilon;
    if velocity.abs() <= config.rest_speed && (target - current).abs() <= config.rest_delta {
        state.value = target;
        state.velocity = 0.0;
    } else {
        state.value = current;
        state.velocity = velocity;
    }
    state.value
}

fn resolve_spring_position(
    seconds: f64,
    target: f64,
    initial_delta: f64,
    initial_velocity: f64,
    damping_ratio: f64,
    undamped_frequency: f64,
) -> f64 {
    if damping_ratio < 1.0 {
        let damped_frequency = undamped_frequency * (1.0 - damping_ratio.powi(2)).sqrt();
        let envelope = (-damping_ratio * undamped_frequency * seconds).exp();
        return target
            - envelope
                * (((initial_velocity + damping_ratio * undamped_frequency * initial_delta)
                    / damped_frequency)
                    * (damped_frequency * seconds).sin()
                    + initial_delta * (damped_frequency * seconds).cos());
    }
    if (damping_ratio - 1.0).abs() < f64::EPSILON {
        return target
            - (-undamped_frequency * seconds).exp()
                * (initial_delta
                    + (initial_velocity + undamped_frequency * initial_delta) * seconds);
    }

    let damped_frequency = undamped_frequency * (damping_ratio.powi(2) - 1.0).sqrt();
    let envelope = (-damping_ratio * undamped_frequency * seconds).exp();
    let frequency_time = (damped_frequency * seconds).min(300.0);
    target
        - envelope
            * ((initial_velocity + damping_ratio * undamped_frequency * initial_delta)
                * frequency_time.sinh()
                + damped_frequency * initial_delta * frequency_time.cosh())
            / damped_frequency
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_uses_camel_case_for_editor_ipc_and_reads_legacy_manifests() {
        let marker = Marker {
            time_ms: 1_250,
            x: 0.2,
            y: 0.7,
            target_scale: Some(2.0),
            held_until_ms: Some(2_400),
            hide_cursor: true,
        };
        let encoded = serde_json::to_value(marker).expect("marker should serialize");
        assert_eq!(encoded["timeMs"], 1_250);
        assert_eq!(encoded["targetScale"], 2.0);
        assert_eq!(encoded["heldUntilMs"], 2_400);
        assert_eq!(encoded["hideCursor"], true);
        assert!(encoded.get("time_ms").is_none());

        let legacy: Marker = serde_json::from_value(serde_json::json!({
            "time_ms": 1250,
            "x": 0.2,
            "y": 0.7,
            "target_scale": 2.0,
            "held_until_ms": 2400,
            "hide_cursor": true
        }))
        .expect("legacy marker should deserialize");
        assert_eq!(legacy, marker);
    }

    #[test]
    fn zoom_uses_the_approved_timing_and_scale() {
        let timing = ZoomTiming::default();
        let event = ZoomEvent::standalone(1_000, 3_100, 0.5, 0.5, false, timing);
        assert_eq!(event.scale_at(999, timing), 1.0);
        assert_eq!(event.scale_at(1_000 + timing.zoom_in_ms, timing), 1.8);
        assert_eq!(event.scale_at(3_100, timing), 1.8);
        assert_eq!(event.scale_at(3_100 + timing.zoom_out_ms, timing), 1.0);
    }

    #[test]
    fn event_keeps_its_selected_zoom_depth_for_later_rendering() {
        let selected = ZoomTiming {
            target_scale: 2.25,
            ..ZoomTiming::default()
        };
        let event = build_zoom_events(
            &[Marker {
                time_ms: 1_000,
                x: 0.5,
                y: 0.5,
                target_scale: None,
                held_until_ms: None,
                hide_cursor: false,
            }],
            selected,
        )[0];

        assert_eq!(event.target_scale, 2.25);
        assert_eq!(
            CameraTransform::at(
                event,
                1_000 + ZoomTiming::default().zoom_in_ms,
                ZoomTiming::default()
            )
            .scale,
            2.25
        );
    }

    #[test]
    fn marker_specific_depth_overrides_the_recording_default() {
        let event = build_zoom_events(
            &[Marker {
                time_ms: 1_000,
                x: 0.5,
                y: 0.5,
                target_scale: Some(2.2),
                held_until_ms: None,
                hide_cursor: false,
            }],
            ZoomTiming::default(),
        )[0];

        assert_eq!(event.target_scale, 2.2);
    }

    #[test]
    fn a_new_marker_continues_the_active_camera_without_returning_to_one_x() {
        let timing = ZoomTiming::default();
        let events = build_zoom_events(
            &[
                Marker {
                    time_ms: 1_000,
                    x: 0.2,
                    y: 0.3,
                    target_scale: None,
                    held_until_ms: None,
                    hide_cursor: false,
                },
                Marker {
                    time_ms: 1_500,
                    x: 0.8,
                    y: 0.7,
                    target_scale: None,
                    held_until_ms: None,
                    hide_cursor: false,
                },
            ],
            timing,
        );
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].end_ms(timing), 1_500);
        assert_eq!(events[1].center_x, 0.7222222222222222);
        assert_eq!(events[1].center_y, 0.7);
        let transfer_start = CameraTransform::at(events[1], 1_500, timing);
        assert_eq!(transfer_start.scale, timing.target_scale);
        assert_eq!(
            events[1].hold_until_ms,
            1_500 + timing.zoom_in_ms + timing.hold_ms
        );
    }

    #[test]
    fn separated_markers_do_not_stretch_the_previous_zoom_out() {
        let timing = ZoomTiming::default();
        let markers = [
            Marker {
                time_ms: 1_000,
                x: 0.3,
                y: 0.4,
                target_scale: None,
                held_until_ms: None,
                hide_cursor: false,
            },
            Marker {
                time_ms: 8_000,
                x: 0.7,
                y: 0.6,
                target_scale: None,
                held_until_ms: None,
                hide_cursor: false,
            },
        ];

        let events = build_zoom_events(&markers, timing);

        assert_eq!(events[0].end_ms, 4_000);
        assert_eq!(
            camera_transform_at(&events, 6_000, timing),
            CameraTransform::identity()
        );
    }

    #[test]
    fn crop_is_centered_when_the_target_is_not_near_an_edge() {
        assert_eq!(
            crop_rect(1_920, 1_080, 2.4, 0.5, 0.5),
            CropRect {
                x: 560,
                y: 315,
                width: 800,
                height: 450
            }
        );
    }

    #[test]
    fn crop_stays_inside_the_source_at_edges() {
        assert_eq!(
            crop_rect(1_920, 1_080, 2.4, 0.0, 1.0),
            CropRect {
                x: 0,
                y: 630,
                width: 800,
                height: 450
            }
        );
    }

    #[test]
    fn recordly_easing_is_fast_out_and_settles_at_one() {
        assert_eq!(recordly_ease_out_zoom(0.0), 0.0);
        assert_eq!(recordly_ease_out_zoom(1.0), 1.0);
        assert!(recordly_ease_out_zoom(0.25) > 0.5);
    }

    #[test]
    fn camera_moves_towards_focus_instead_of_locking_it_immediately() {
        let timing = ZoomTiming::default();
        let event = ZoomEvent::standalone(0, 2_100, 0.7, 0.4, false, timing);
        let mid = CameraTransform::at(event, timing.zoom_in_ms / 2, timing);
        let full = CameraTransform::at(event, timing.zoom_in_ms, timing);
        assert!(mid.crop_x > 0.0);
        assert!(mid.crop_x < full.crop_x);
        assert!((full.crop_x - (0.7 - 0.5 / 1.8)).abs() < 1e-9);
    }

    #[test]
    fn a_standalone_zoom_preserves_the_original_recordly_camera_path() {
        let timing = ZoomTiming::default();
        let event = ZoomEvent::standalone(0, 2_100, 0.7, 0.4, false, timing);
        let midpoint = timing.zoom_in_ms / 2;
        let progress = recordly_ease_out_zoom(normalized_progress(midpoint, 0, timing.zoom_in_ms));
        let expected_scale = 1.0 + (timing.target_scale - 1.0) * progress;
        let expected_crop_x = ((0.7 * timing.target_scale - 0.5) * progress / expected_scale)
            .clamp(0.0, 1.0 - 1.0 / expected_scale);
        let actual = CameraTransform::at(event, midpoint, timing);
        assert!((actual.scale - expected_scale).abs() < 1e-9);
        assert!((actual.crop_x - expected_crop_x).abs() < 1e-9);
    }

    #[test]
    fn a_standalone_zoom_uses_the_same_target_aware_path_when_exiting() {
        let timing = ZoomTiming::default();
        let event = ZoomEvent::standalone(0, 2_100, 0.7, 0.4, false, timing);
        let time_ms = 2_100 + timing.zoom_out_ms / 2;
        let remaining = 1.0
            - recordly_ease_out_zoom(normalized_progress(
                time_ms,
                2_100,
                2_100 + timing.zoom_out_ms,
            ));
        let expected_scale = 1.0 + (timing.target_scale - 1.0) * remaining;
        let expected_crop_x = ((0.7 * timing.target_scale - 0.5) * remaining / expected_scale)
            .clamp(0.0, 1.0 - 1.0 / expected_scale);
        let actual = CameraTransform::at(event, time_ms, timing);
        assert!((actual.scale - expected_scale).abs() < 1e-9);
        assert!((actual.crop_x - expected_crop_x).abs() < 1e-9);
    }

    #[test]
    fn focus_is_clamped_to_recordly_crop_bounds() {
        let timing = ZoomTiming::default();
        let events = build_zoom_events(
            &[Marker {
                time_ms: 0,
                x: 0.0,
                y: 1.0,
                target_scale: None,
                held_until_ms: None,
                hide_cursor: false,
            }],
            timing,
        );
        assert!((events[0].center_x - 1.0 / 3.6).abs() < 1e-9);
        assert!((events[0].center_y - (1.0 - 1.0 / 3.6)).abs() < 1e-9);
    }

    #[test]
    fn held_marker_keeps_the_zoom_until_alt_is_released() {
        let timing = ZoomTiming::default();
        let event = build_zoom_events(
            &[Marker {
                time_ms: 1_000,
                x: 0.5,
                y: 0.5,
                target_scale: None,
                held_until_ms: Some(5_000),
                hide_cursor: false,
            }],
            timing,
        )[0];
        assert_eq!(event.hold_until_ms, 5_000);
        assert_eq!(event.scale_at(5_000, timing), timing.target_scale);
        assert_eq!(event.scale_at(5_000 + timing.zoom_out_ms, timing), 1.0);
    }

    #[test]
    fn double_alt_marker_hides_the_cursor_for_its_zoom() {
        let event = build_zoom_events(
            &[Marker {
                time_ms: 1_000,
                x: 0.5,
                y: 0.5,
                target_scale: None,
                held_until_ms: None,
                hide_cursor: true,
            }],
            ZoomTiming::default(),
        )[0];
        assert!(event.hide_cursor);
    }

    #[test]
    fn camera_maps_a_focus_point_to_the_stage_centre_when_fully_zoomed() {
        let timing = ZoomTiming::default();
        let event = ZoomEvent::standalone(0, 2_100, 0.7, 0.4, false, timing);
        let transform = CameraTransform::at(event, timing.zoom_in_ms, timing);
        let (x, y) = transform.map_normalized_point(0.7, 0.4);
        assert!((x - 0.5).abs() < 1e-9);
        assert!((y - 0.5).abs() < 1e-9);
    }

    #[test]
    fn camera_is_exactly_stable_during_the_zoom_hold() {
        let timing = ZoomTiming::default();
        let event = ZoomEvent::standalone(1_000, 5_000, 0.7, 0.4, false, timing);
        let settled = CameraTransform::at(event, 1_000 + timing.zoom_in_ms, timing);
        for time_ms in [1_000 + timing.zoom_in_ms + 1, 3_337, 4_999, 5_000] {
            let current = CameraTransform::at(event, time_ms, timing);
            assert!((current.scale - settled.scale).abs() < 1e-12);
            assert!((current.crop_x - settled.crop_x).abs() < 1e-12);
            assert!((current.crop_y - settled.crop_y).abs() < 1e-12);
        }
    }

    #[test]
    fn recordly_camera_spring_smooths_the_eased_target_without_overshoot() {
        let timing = ZoomTiming::default();
        let event = ZoomEvent::standalone(0, 2_100, 0.7, 0.4, false, timing);
        let mut spring = RecordlyCameraSpring::default();
        let mut previous = CameraTransform::identity();
        for frame in 0..=240 {
            let time_ms = frame * 1_000 / 120;
            let target = CameraTransform::at(event, time_ms, timing);
            let current = spring.step(target, 1_000.0 / 120.0);
            assert!(current.scale >= 1.0);
            assert!(current.scale <= timing.target_scale);
            assert!(current.crop_x >= 0.0);
            assert!(current.scale + 1e-9 >= previous.scale || time_ms > event.hold_until_ms);
            previous = current;
        }
        let raw = CameraTransform::at(event, timing.zoom_in_ms / 2, timing);
        let mut spring = RecordlyCameraSpring::default();
        let mut smoothed = CameraTransform::identity();
        for frame in 0..=(timing.zoom_in_ms / 2 * 120 / 1_000) {
            let time_ms = frame * 1_000 / 120;
            smoothed = spring.step(CameraTransform::at(event, time_ms, timing), 1_000.0 / 120.0);
        }
        assert!(smoothed.scale < raw.scale);
    }
}
