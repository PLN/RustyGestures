//! Platform-neutral pieces of RustyGestures.
//!
//! The hook callback runs [`Machine`] and pushes finished strokes through
//! [`SpscRing`]. Nothing in this crate depends on Windows.

mod gestures;
mod histogram;
mod recognizer;
mod ring;
mod state;
mod trail;

pub use gestures::{
    load_gesture_file, save_gesture_file, save_stroke_log, GestureFile, GestureIoError,
    GestureName, NamedGesture, RawPoint, RawStroke, StrokeLog, GESTURE_SCHEMA,
};
pub use histogram::{LatencyHistogram, LatencySummary};
pub use recognizer::{GestureSet, Recognition, RESAMPLE_POINTS};
pub use ring::{SpscRing, RING_CAP};
pub use state::{
    is_injected_extra, is_touch_or_pen_extra, Button, Command, Config, Decision, EventKind,
    InputEvent, Machine, Neutralize, Output, Phase, Point, Replay, Stroke, DEFAULT_CANCEL_DELAY_MS,
    DEFAULT_MIN_MOVE_PX, INJECTED_EXTRA_INFO, MAX_STROKE_POINTS, TOUCH_PEN_EXTRA_MASK,
    TOUCH_PEN_EXTRA_SIGNATURE, WATCHDOG_STALL_MS,
};
pub use trail::LiveTrail;
