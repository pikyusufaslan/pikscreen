use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum TahoeCursor {
    #[default]
    Arrow,
    ContextMenu,
    Help,
    PointingHand,
    Busy,
    Cell,
    Crosshair,
    Text,
    VerticalText,
    Alias,
    Copy,
    Move,
    NotAllowed,
    OpenHand,
    ClosedHand,
    ResizeEast,
    ResizeNorth,
    ResizeNorthEast,
    ResizeNorthWest,
    ResizeSouth,
    ResizeSouthEast,
    ResizeSouthWest,
    ResizeWest,
    ResizeEastWest,
    ResizeNorthSouth,
    ResizeNorthEastSouthWest,
    ResizeNorthWestSouthEast,
    ResizeLeftRight,
    ResizeUpDown,
    ZoomIn,
    ZoomOut,
    Hidden,
}

impl TahoeCursor {
    pub fn from_standard_name(name: &str) -> Self {
        match name {
            "context-menu" => Self::ContextMenu,
            "help" => Self::Help,
            "pointer" => Self::PointingHand,
            "progress" | "wait" => Self::Busy,
            "cell" => Self::Cell,
            "crosshair" => Self::Crosshair,
            "text" => Self::Text,
            "vertical-text" => Self::VerticalText,
            "alias" => Self::Alias,
            "copy" => Self::Copy,
            "move" | "all-scroll" | "all-resize" => Self::Move,
            "no-drop" | "not-allowed" => Self::NotAllowed,
            "grab" => Self::OpenHand,
            "grabbing" => Self::ClosedHand,
            "e-resize" => Self::ResizeEast,
            "n-resize" => Self::ResizeNorth,
            "ne-resize" => Self::ResizeNorthEast,
            "nw-resize" => Self::ResizeNorthWest,
            "s-resize" => Self::ResizeSouth,
            "se-resize" => Self::ResizeSouthEast,
            "sw-resize" => Self::ResizeSouthWest,
            "w-resize" => Self::ResizeWest,
            "ew-resize" => Self::ResizeEastWest,
            "ns-resize" => Self::ResizeNorthSouth,
            "nesw-resize" => Self::ResizeNorthEastSouthWest,
            "nwse-resize" => Self::ResizeNorthWestSouthEast,
            "col-resize" => Self::ResizeLeftRight,
            "row-resize" => Self::ResizeUpDown,
            "zoom-in" => Self::ZoomIn,
            "zoom-out" => Self::ZoomOut,
            "dnd-ask" => Self::ContextMenu,
            _ => Self::Arrow,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorSample {
    #[serde(alias = "time_ms")]
    pub time_ms: u64,
    pub x: f64,
    pub y: f64,
    #[serde(default)]
    pub kind: TahoeCursor,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CursorPoint {
    pub x: f64,
    pub y: f64,
}

impl From<CursorSample> for CursorPoint {
    fn from(sample: CursorSample) -> Self {
        Self {
            x: sample.x,
            y: sample.y,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CursorTimeline {
    samples: Vec<CursorSample>,
}

impl CursorTimeline {
    pub fn new(mut samples: Vec<CursorSample>) -> Self {
        samples.sort_by_key(|sample| sample.time_ms);
        samples.dedup_by_key(|sample| sample.time_ms);
        Self { samples }
    }

    pub fn at(&self, time_ms: u64) -> Option<CursorPoint> {
        let first = *self.samples.first()?;
        let last = *self.samples.last()?;
        if time_ms <= first.time_ms {
            return Some(first.into());
        }
        if time_ms >= last.time_ms {
            return Some(last.into());
        }

        let right = self
            .samples
            .partition_point(|sample| sample.time_ms < time_ms);
        let before = self.samples[right - 1];
        let after = self.samples[right];
        let span = (after.time_ms - before.time_ms) as f64;
        let progress = (time_ms - before.time_ms) as f64 / span;
        Some(CursorPoint {
            x: before.x + (after.x - before.x) * progress,
            y: before.y + (after.y - before.y) * progress,
        })
    }

    pub fn kind_at(&self, time_ms: u64) -> Option<TahoeCursor> {
        let first = *self.samples.first()?;
        if time_ms <= first.time_ms {
            return Some(first.kind);
        }
        let index = self
            .samples
            .partition_point(|sample| sample.time_ms <= time_ms)
            .saturating_sub(1);
        self.samples.get(index).map(|sample| sample.kind)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CursorSmoother {
    position: CursorPoint,
    velocity: CursorPoint,
    last_target: CursorPoint,
    stationary_ms: u64,
    initialized: bool,
    config: SpringConfig,
}

impl Default for CursorSmoother {
    fn default() -> Self {
        Self::with_amount(35)
    }
}

impl CursorSmoother {
    pub fn with_amount(amount: u8) -> Self {
        Self {
            position: CursorPoint { x: 0.0, y: 0.0 },
            velocity: CursorPoint { x: 0.0, y: 0.0 },
            last_target: CursorPoint { x: 0.0, y: 0.0 },
            stationary_ms: 0,
            initialized: false,
            config: pikscreen_stable_cursor_spring(amount),
        }
    }

    pub fn step(&mut self, target: CursorPoint, delta_ms: u64) -> CursorPoint {
        if !self.initialized {
            self.position = target;
            self.last_target = target;
            self.initialized = true;
            return target;
        }

        // Cursor sampling can keep delivering tiny sub-pixel changes after
        // the physical mouse has stopped. Letting the spring chase those
        // values is visible as a tremble once the zoom camera is stationary.
        // Treat movement smaller than about one source pixel as stillness and
        // lock the pointer once it has been still for four 120 FPS frames.
        const STILLNESS_EPSILON: f64 = 0.0005;
        const SETTLE_SNAP_MS: u64 = 32;
        let target_delta = (target.x - self.last_target.x)
            .abs()
            .max((target.y - self.last_target.y).abs());
        if target_delta <= STILLNESS_EPSILON {
            self.stationary_ms = self.stationary_ms.saturating_add(delta_ms);
        } else {
            self.stationary_ms = 0;
        }
        self.last_target = target;
        if self.stationary_ms >= SETTLE_SNAP_MS {
            self.position = target;
            self.velocity = CursorPoint { x: 0.0, y: 0.0 };
            return target;
        }

        let config = self.config;
        self.position.x = step_recordly_spring(
            self.position.x,
            &mut self.velocity.x,
            target.x,
            delta_ms,
            config,
        );
        self.position.y = step_recordly_spring(
            self.position.y,
            &mut self.velocity.y,
            target.y,
            delta_ms,
            config,
        );
        self.position
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

fn pikscreen_stable_cursor_spring(amount: u8) -> SpringConfig {
    // 35 intentionally maps to the previously tuned 0.18 response. The
    // slider only controls the final cursor rendering, never captured input.
    let smoothing_factor = 0.04_f64 + f64::from(amount.min(100)) / 100.0 * 0.40;
    let normalized = (smoothing_factor / 0.5).clamp(0.0, 1.0);
    let stiffness = (760.0 - normalized * 420.0) * 1.12;
    let mass = 0.85 + normalized * 0.55;
    SpringConfig {
        stiffness,
        damping: 2.04 * (stiffness * mass).sqrt(),
        mass,
        rest_delta: 0.0002,
        rest_speed: 0.01,
    }
}

#[allow(dead_code)]
fn step_recordly_spring(
    value: f64,
    velocity: &mut f64,
    target: f64,
    delta_ms: u64,
    config: SpringConfig,
) -> f64 {
    if (target - value).abs() <= config.rest_delta && velocity.abs() <= config.rest_speed {
        *velocity = 0.0;
        return target;
    }

    let safe_delta_ms = delta_ms.clamp(1, 80) as f64;
    let natural_frequency = (config.stiffness / config.mass).sqrt();
    let damping_ratio = config.damping / (2.0 * (config.stiffness * config.mass).sqrt());
    let initial_delta = target - value;
    let initial_velocity = -*velocity;
    let time_seconds = safe_delta_ms / 1_000.0;
    let current = resolve_recordly_spring_position(
        time_seconds,
        target,
        initial_delta,
        initial_velocity,
        damping_ratio,
        natural_frequency,
    );

    // Same target-crossing guard Recordly uses for critically/overdamped
    // configurations. The default is slightly underdamped, so its subtle
    // settle is preserved.
    if damping_ratio >= 1.0
        && ((value <= target && current > target) || (value >= target && current < target))
    {
        *velocity = 0.0;
        return target;
    }

    let epsilon = 0.0001;
    let ahead = resolve_recordly_spring_position(
        time_seconds + epsilon,
        target,
        initial_delta,
        initial_velocity,
        damping_ratio,
        natural_frequency,
    );
    let next_velocity = (ahead - current) / epsilon;
    if next_velocity.abs() <= config.rest_speed && (target - current).abs() <= config.rest_delta {
        *velocity = 0.0;
        target
    } else {
        *velocity = next_velocity;
        current
    }
}

#[allow(dead_code)]
fn resolve_recordly_spring_position(
    time_seconds: f64,
    target: f64,
    initial_delta: f64,
    initial_velocity: f64,
    damping_ratio: f64,
    natural_frequency: f64,
) -> f64 {
    if damping_ratio < 1.0 {
        let damped_frequency = natural_frequency * (1.0 - damping_ratio * damping_ratio).sqrt();
        let envelope = (-damping_ratio * natural_frequency * time_seconds).exp();
        return target
            - envelope
                * (((initial_velocity + damping_ratio * natural_frequency * initial_delta)
                    / damped_frequency)
                    * (damped_frequency * time_seconds).sin()
                    + initial_delta * (damped_frequency * time_seconds).cos());
    }

    if damping_ratio == 1.0 {
        return target
            - (-natural_frequency * time_seconds).exp()
                * (initial_delta
                    + (initial_velocity + natural_frequency * initial_delta) * time_seconds);
    }

    let damped_frequency = natural_frequency * (damping_ratio * damping_ratio - 1.0).sqrt();
    let envelope = (-damping_ratio * natural_frequency * time_seconds).exp();
    let frequency_time = (damped_frequency * time_seconds).min(300.0);
    target
        - (envelope
            * ((initial_velocity + damping_ratio * natural_frequency * initial_delta)
                * frequency_time.sinh()
                + damped_frequency * initial_delta * frequency_time.cosh())
            / damped_frequency)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_sample_uses_camel_case_and_reads_legacy_time() {
        let sample = CursorSample {
            time_ms: 42,
            x: 0.25,
            y: 0.75,
            kind: TahoeCursor::Text,
        };
        let encoded = serde_json::to_value(sample).expect("cursor sample should serialize");
        assert_eq!(encoded["timeMs"], 42);
        assert!(encoded.get("time_ms").is_none());

        let legacy: CursorSample = serde_json::from_value(serde_json::json!({
            "time_ms": 42,
            "x": 0.25,
            "y": 0.75,
            "kind": "Text"
        }))
        .expect("legacy cursor sample should deserialize");
        assert_eq!(legacy, sample);
    }

    #[test]
    fn interpolates_between_cursor_samples() {
        let timeline = CursorTimeline::new(vec![
            CursorSample {
                time_ms: 0,
                x: 0.1,
                y: 0.2,
                kind: TahoeCursor::Arrow,
            },
            CursorSample {
                time_ms: 100,
                x: 0.9,
                y: 0.6,
                kind: TahoeCursor::Arrow,
            },
        ]);
        assert_eq!(timeline.at(50), Some(CursorPoint { x: 0.5, y: 0.4 }));
    }

    #[test]
    fn holds_the_nearest_endpoint() {
        let timeline = CursorTimeline::new(vec![
            CursorSample {
                time_ms: 100,
                x: 0.2,
                y: 0.3,
                kind: TahoeCursor::Arrow,
            },
            CursorSample {
                time_ms: 200,
                x: 0.8,
                y: 0.7,
                kind: TahoeCursor::Arrow,
            },
        ]);
        assert_eq!(timeline.at(0), Some(CursorPoint { x: 0.2, y: 0.3 }));
        assert_eq!(timeline.at(300), Some(CursorPoint { x: 0.8, y: 0.7 }));
    }

    #[test]
    fn holds_the_latest_cursor_kind_between_position_samples() {
        let timeline = CursorTimeline::new(vec![
            CursorSample {
                time_ms: 0,
                x: 0.1,
                y: 0.2,
                kind: TahoeCursor::Arrow,
            },
            CursorSample {
                time_ms: 100,
                x: 0.9,
                y: 0.6,
                kind: TahoeCursor::Text,
            },
        ]);
        assert_eq!(timeline.kind_at(99), Some(TahoeCursor::Arrow));
        assert_eq!(timeline.kind_at(100), Some(TahoeCursor::Text));
        assert_eq!(timeline.kind_at(150), Some(TahoeCursor::Text));
    }

    #[test]
    fn pikscreen_cursor_spring_is_slightly_overdamped() {
        let config = pikscreen_stable_cursor_spring(35);
        let damping_ratio = config.damping / (2.0 * (config.stiffness * config.mass).sqrt());
        assert!(damping_ratio > 1.0 && damping_ratio < 1.1);
    }

    #[test]
    fn stronger_smoothing_trails_a_target_more_than_the_default() {
        let mut mild = CursorSmoother::with_amount(35);
        let mut strong = CursorSmoother::with_amount(100);
        let start = CursorPoint { x: 0.0, y: 0.0 };
        let target = CursorPoint { x: 1.0, y: 1.0 };
        mild.step(start, 0);
        strong.step(start, 0);
        let mild_position = mild.step(target, 8);
        let strong_position = strong.step(target, 8);
        assert!(strong_position.x < mild_position.x);
        assert!(strong_position.y < mild_position.y);
    }

    #[test]
    fn spring_settles_on_a_stationary_target() {
        let mut smoother = CursorSmoother::default();
        smoother.step(CursorPoint { x: 0.0, y: 0.0 }, 0);
        for _ in 0..120 {
            let current = smoother.step(CursorPoint { x: 1.0, y: 1.0 }, 16);
            assert!(current.x.is_finite());
            assert!(current.y.is_finite());
        }
        let settled = smoother.step(CursorPoint { x: 1.0, y: 1.0 }, 16);
        assert!((settled.x - 1.0).abs() < 0.001);
    }

    #[test]
    fn stable_spring_never_overshoots_a_stationary_target() {
        let mut smoother = CursorSmoother::default();
        smoother.step(CursorPoint { x: 0.0, y: 0.0 }, 0);
        let mut previous = 0.0;
        for _ in 0..180 {
            let current = smoother.step(CursorPoint { x: 1.0, y: 1.0 }, 8);
            assert!(current.x >= previous);
            assert!(current.x <= 1.0);
            previous = current.x;
        }
    }

    #[test]
    fn stationary_cursor_snaps_instead_of_creeping_after_a_zoom() {
        let mut smoother = CursorSmoother::default();
        smoother.step(CursorPoint { x: 0.0, y: 0.0 }, 0);
        smoother.step(CursorPoint { x: 1.0, y: 1.0 }, 8);
        for _ in 0..3 {
            smoother.step(CursorPoint { x: 1.0, y: 1.0 }, 8);
        }
        assert_eq!(
            smoother.step(CursorPoint { x: 1.0, y: 1.0 }, 8),
            CursorPoint { x: 1.0, y: 1.0 }
        );
    }
}
