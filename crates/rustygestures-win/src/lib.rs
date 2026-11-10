//! Windows engine for the M0 spike: low-level mouse hook, re-injection, and trail.
//!
//! On other platforms the session entry point returns an error so the workspace
//! still builds.

use rustygestures_core::{LatencySummary, Stroke};

/// What the engine should do after a finished stroke.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    Continue,
    Stop,
}

/// Numbers printed when the session ends.
#[derive(Clone, Copy, Debug)]
pub struct RunStats {
    pub latency: LatencySummary,
    pub dropped_strokes: u64,
    pub trail_active: bool,
}

/// Options for one capture session.
#[derive(Clone, Copy, Debug, Default)]
pub struct RunOptions {
    /// Sleep this long on the hook thread once a stroke starts, without
    /// refreshing the heartbeat, so the stall watchdog can be observed.
    pub debug_stall_ms: u32,
    /// When non-zero, send random mouse input for this many seconds and then stop.
    pub fuzz_seconds: u64,
}

/// Failure while installing the hook or the overlay.
#[derive(Debug)]
pub struct SessionError(pub String);

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SessionError {}

/// Run the hook until `on_stroke` returns [`Control::Stop`] or the console asks
/// to quit. `on_stroke` runs on the thread that owns the overlay, never inside
/// the hook callback.
pub fn run_session<F>(options: RunOptions, on_stroke: F) -> Result<RunStats, SessionError>
where
    F: FnMut(&Stroke) -> Control,
{
    #[cfg(windows)]
    {
        platform::run(options, on_stroke)
    }
    #[cfg(not(windows))]
    {
        let _ = (options, on_stroke);
        Err(SessionError(
            "capture requires Windows (WH_MOUSE_LL)".to_owned(),
        ))
    }
}

#[cfg(windows)]
mod inject;
#[cfg(windows)]
mod overlay;
#[cfg(windows)]
mod platform;
