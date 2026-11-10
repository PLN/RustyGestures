//! Allocation-free gesture capture state machine.
//!
//! The hook callback calls [`Machine::handle`]. Re-injection and the cancel
//! timeout are applied by the caller, outside the callback. [`Machine::poll_timeout`]
//! is that timeout check. [`Machine::neutralize`] closes any held press on
//! shutdown or when the stall watchdog trips.

/// `dwExtraInfo` stamped on every `SendInput` event this program injects.
/// It does not collide with the touch/pen signature below.
pub const INJECTED_EXTRA_INFO: usize = 0x5247_5354;

/// Mask documented for touch- and pen-synthesized mouse events.
pub const TOUCH_PEN_EXTRA_MASK: usize = 0xFFFF_FF00;

/// Signature in `dwExtraInfo` for touch- and pen-synthesized mouse events.
pub const TOUCH_PEN_EXTRA_SIGNATURE: usize = 0xFF51_5700;

/// Movement, in pixels, before a press becomes a stroke.
pub const DEFAULT_MIN_MOVE_PX: i32 = 10;

/// How long a press may sit still before it is replayed so a drag still works.
pub const DEFAULT_CANCEL_DELAY_MS: u32 = 500;

/// If the engine stops making progress for this long during a capture, held
/// input is released.
pub const WATCHDOG_STALL_MS: u32 = 250;

/// Largest stroke kept by the state machine. Extra points replace the last one
/// so the stroke still ends at the cursor.
pub const MAX_STROKE_POINTS: usize = 256;

#[must_use]
pub fn is_injected_extra(extra: usize) -> bool {
    extra == INJECTED_EXTRA_INFO
}

#[must_use]
pub fn is_touch_or_pen_extra(extra: usize) -> bool {
    extra & TOUCH_PEN_EXTRA_MASK == TOUCH_PEN_EXTRA_SIGNATURE
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Button {
    Left = 0,
    Right = 1,
    Middle = 2,
    X1 = 3,
    X2 = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Phase {
    Idle = 0,
    Pending = 1,
    Capturing = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    Press,
    Release,
    Move,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputEvent {
    pub kind: EventKind,
    pub button: Button,
    pub x: i32,
    pub y: i32,
    /// Millisecond timestamp. Wrapping `u32` matches the hook's tick clock.
    pub time_ms: u32,
    /// `true` when the event carries [`INJECTED_EXTRA_INFO`].
    pub injected: bool,
    /// `true` for touch- or pen-generated mouse events.
    pub touch_or_pen: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub primary: Button,
    pub min_move_px: i32,
    pub cancel_delay_ms: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            primary: Button::Right,
            min_move_px: DEFAULT_MIN_MOVE_PX,
            cancel_delay_ms: DEFAULT_CANCEL_DELAY_MS,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Deliver the event to the target.
    Pass,
    /// Do not deliver the event.
    Swallow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    None,
    /// Press and release were both swallowed. Inject both at the origin.
    ReinjectClick {
        button: Button,
        x: i32,
        y: i32,
    },
    /// The press sat still past the cancel delay. Inject the press only.
    ReinjectPress {
        button: Button,
        x: i32,
        y: i32,
    },
    /// The target still has the button down and the real release will not arrive.
    ReinjectRelease {
        button: Button,
        x: i32,
        y: i32,
    },
    /// A stroke was finished. Read it with [`Machine::take_stroke`].
    Stroke,
}

/// An event that was swallowed only so it can be injected *after* [`Command`].
///
/// Used when the cancel delay elapses on the same event that must still reach
/// the target. The press has to be injected first; the original event is
/// replayed immediately afterwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Replay {
    pub kind: EventKind,
    pub button: Button,
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Output {
    pub decision: Decision,
    pub command: Command,
    pub replay: Option<Replay>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stroke {
    pub points: [Point; MAX_STROKE_POINTS],
    pub len: u16,
    pub button: Button,
}

impl Stroke {
    #[must_use]
    pub fn as_slice(&self) -> &[Point] {
        &self.points[..self.len as usize]
    }
}

/// What the caller must inject so a held press is not left unpaired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Neutralize {
    None,
    /// A suppressed press is closed with a synthetic press and release.
    SyntheticClick {
        button: Button,
        x: i32,
        y: i32,
    },
    /// A press was already delivered. Inject the matching release.
    SyntheticRelease {
        button: Button,
        x: i32,
        y: i32,
    },
}

#[derive(Clone, Debug)]
pub struct Machine {
    config: Config,
    phase: Phase,
    button: Button,
    origin_x: i32,
    origin_y: i32,
    origin_ms: u32,
    points: [Point; MAX_STROKE_POINTS],
    len: u16,
    delivered_down: bool,
    delivered_button: Button,
    delivered_x: i32,
    delivered_y: i32,
    stroke_ready: bool,
    last_stroke: Stroke,
}

impl Machine {
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            config,
            phase: Phase::Idle,
            button: config.primary,
            origin_x: 0,
            origin_y: 0,
            origin_ms: 0,
            points: [Point { x: 0, y: 0 }; MAX_STROKE_POINTS],
            len: 0,
            delivered_down: false,
            delivered_button: config.primary,
            delivered_x: 0,
            delivered_y: 0,
            stroke_ready: false,
            last_stroke: Stroke {
                points: [Point { x: 0, y: 0 }; MAX_STROKE_POINTS],
                len: 0,
                button: config.primary,
            },
        }
    }

    #[must_use]
    pub fn phase(&self) -> Phase {
        self.phase
    }

    #[must_use]
    pub fn delivered_down(&self) -> bool {
        self.delivered_down
    }

    /// Button and point the watchdog should re-inject if the engine stalls.
    #[must_use]
    pub fn held_point(&self) -> Option<(Button, i32, i32)> {
        if self.phase != Phase::Idle {
            Some((self.button, self.origin_x, self.origin_y))
        } else if self.delivered_down {
            Some((self.delivered_button, self.delivered_x, self.delivered_y))
        } else {
            None
        }
    }

    #[must_use]
    pub fn current_points(&self) -> &[Point] {
        &self.points[..self.len as usize]
    }

    /// Take the stroke produced by the last [`Command::Stroke`].
    #[must_use]
    pub fn take_stroke(&mut self) -> Option<Stroke> {
        if self.stroke_ready {
            self.stroke_ready = false;
            Some(self.last_stroke)
        } else {
            None
        }
    }

    /// Feed one input event. The returned command is performed by the caller
    /// outside the hook callback.
    pub fn handle(&mut self, ev: InputEvent) -> Output {
        if ev.injected || ev.touch_or_pen {
            self.note_delivered_release(&ev);
            return pass();
        }

        // A move that crosses the threshold is a stroke, even if this event's
        // timestamp is also past the cancel delay. The delay is for a press
        // that is still sitting still.
        if self.phase == Phase::Pending
            && ev.kind == EventKind::Move
            && self.past_threshold(ev.x, ev.y)
        {
            self.begin_capture(ev.x, ev.y);
            return swallow(Command::None);
        }

        if self.phase == Phase::Pending && self.timed_out(ev.time_ms) {
            return self.timeout_then(ev);
        }

        self.dispatch(ev)
    }

    /// Cancel delay elapsed with no further input. Returns [`Command::ReinjectPress`]
    /// once, then [`Command::None`] until the next press.
    pub fn poll_timeout(&mut self, now_ms: u32) -> Command {
        if self.phase != Phase::Pending || !self.timed_out(now_ms) {
            return Command::None;
        }
        let button = self.button;
        let x = self.origin_x;
        let y = self.origin_y;
        self.mark_delivered(button, x, y);
        self.phase = Phase::Idle;
        self.len = 0;
        Command::ReinjectPress { button, x, y }
    }

    /// Milliseconds until [`Self::poll_timeout`] would fire, if a press is pending.
    #[must_use]
    pub fn ms_until_timeout(&self, now_ms: u32) -> Option<u32> {
        if self.phase != Phase::Pending {
            return None;
        }
        let elapsed = now_ms.wrapping_sub(self.origin_ms);
        Some(self.config.cancel_delay_ms.saturating_sub(elapsed))
    }

    /// Drop capture or a delivered press so input cannot stay stuck.
    ///
    /// Shutdown and the stall watchdog call this. The returned action is
    /// injected by the caller. After it returns, the machine is [`Phase::Idle`]
    /// and [`Self::delivered_down`] is false.
    pub fn neutralize(&mut self) -> Neutralize {
        if self.phase != Phase::Idle {
            let button = self.button;
            let x = self.origin_x;
            let y = self.origin_y;
            self.phase = Phase::Idle;
            self.len = 0;
            self.delivered_down = false;
            return Neutralize::SyntheticClick { button, x, y };
        }
        if self.delivered_down {
            let button = self.delivered_button;
            let x = self.delivered_x;
            let y = self.delivered_y;
            self.delivered_down = false;
            return Neutralize::SyntheticRelease { button, x, y };
        }
        Neutralize::None
    }

    /// The watchdog already injected the corrective input. Forget local state
    /// without asking for a second injection.
    pub fn force_idle(&mut self) {
        self.phase = Phase::Idle;
        self.len = 0;
        self.delivered_down = false;
        self.stroke_ready = false;
    }

    fn timeout_then(&mut self, ev: InputEvent) -> Output {
        let button = self.button;
        let x = self.origin_x;
        let y = self.origin_y;
        self.mark_delivered(button, x, y);
        self.phase = Phase::Idle;
        self.len = 0;
        let follow = self.dispatch(ev);
        if follow.decision == Decision::Pass {
            Output {
                decision: Decision::Swallow,
                command: Command::ReinjectPress { button, x, y },
                replay: Some(Replay {
                    kind: ev.kind,
                    button: ev.button,
                    x: ev.x,
                    y: ev.y,
                }),
            }
        } else {
            Output {
                decision: follow.decision,
                command: Command::ReinjectPress { button, x, y },
                replay: None,
            }
        }
    }

    fn dispatch(&mut self, ev: InputEvent) -> Output {
        match ev.kind {
            EventKind::Press => self.on_press(ev),
            EventKind::Release => self.on_release(ev),
            EventKind::Move => self.on_move(ev),
        }
    }

    fn on_press(&mut self, ev: InputEvent) -> Output {
        if ev.button != self.config.primary || self.phase != Phase::Idle {
            return pass();
        }
        // A second down without a release is not a new stroke. The target
        // already has this button, or a duplicate down arrived.
        if self.delivered_down && ev.button == self.delivered_button {
            return pass();
        }
        self.phase = Phase::Pending;
        self.button = ev.button;
        self.origin_x = ev.x;
        self.origin_y = ev.y;
        self.origin_ms = ev.time_ms;
        self.len = 0;
        self.stroke_ready = false;
        swallow(Command::None)
    }

    fn on_release(&mut self, ev: InputEvent) -> Output {
        let cleared = self.note_delivered_release(&ev);
        if ev.button != self.config.primary {
            return pass();
        }
        match self.phase {
            Phase::Idle => {
                let _ = cleared;
                pass()
            }
            Phase::Pending => {
                let button = self.button;
                let x = self.origin_x;
                let y = self.origin_y;
                self.phase = Phase::Idle;
                self.len = 0;
                Output {
                    decision: Decision::Swallow,
                    command: Command::ReinjectClick { button, x, y },
                    replay: None,
                }
            }
            Phase::Capturing => {
                self.push_point(ev.x, ev.y);
                self.finish_stroke();
                self.phase = Phase::Idle;
                Output {
                    decision: Decision::Swallow,
                    command: Command::Stroke,
                    replay: None,
                }
            }
        }
    }

    fn on_move(&mut self, ev: InputEvent) -> Output {
        match self.phase {
            Phase::Idle => pass(),
            Phase::Pending => {
                if self.past_threshold(ev.x, ev.y) {
                    self.begin_capture(ev.x, ev.y);
                }
                swallow(Command::None)
            }
            Phase::Capturing => {
                self.push_point(ev.x, ev.y);
                swallow(Command::None)
            }
        }
    }

    fn begin_capture(&mut self, x: i32, y: i32) {
        self.phase = Phase::Capturing;
        self.len = 0;
        self.push_point(self.origin_x, self.origin_y);
        self.push_point(x, y);
    }

    fn push_point(&mut self, x: i32, y: i32) {
        let point = Point { x, y };
        let index = self.len as usize;
        if index < MAX_STROKE_POINTS {
            self.points[index] = point;
            self.len += 1;
        } else {
            self.points[MAX_STROKE_POINTS - 1] = point;
        }
    }

    fn finish_stroke(&mut self) {
        self.last_stroke = Stroke {
            points: self.points,
            len: self.len,
            button: self.button,
        };
        self.stroke_ready = true;
        self.len = 0;
    }

    fn past_threshold(&self, x: i32, y: i32) -> bool {
        let dx = i64::from(x) - i64::from(self.origin_x);
        let dy = i64::from(y) - i64::from(self.origin_y);
        let min = i64::from(self.config.min_move_px);
        dx * dx + dy * dy >= min * min
    }

    fn timed_out(&self, now_ms: u32) -> bool {
        now_ms.wrapping_sub(self.origin_ms) >= self.config.cancel_delay_ms
    }

    fn mark_delivered(&mut self, button: Button, x: i32, y: i32) {
        self.delivered_down = true;
        self.delivered_button = button;
        self.delivered_x = x;
        self.delivered_y = y;
    }

    fn note_delivered_release(&mut self, ev: &InputEvent) -> bool {
        if ev.kind == EventKind::Release
            && self.delivered_down
            && ev.button == self.delivered_button
        {
            self.delivered_down = false;
            true
        } else {
            false
        }
    }
}

fn pass() -> Output {
    Output {
        decision: Decision::Pass,
        command: Command::None,
        replay: None,
    }
}

fn swallow(command: Command) -> Output {
    Output {
        decision: Decision::Swallow,
        command,
        replay: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine() -> Machine {
        Machine::new(Config::default())
    }

    fn input(kind: EventKind, button: Button, x: i32, y: i32, time_ms: u32) -> InputEvent {
        InputEvent {
            kind,
            button,
            x,
            y,
            time_ms,
            injected: false,
            touch_or_pen: false,
        }
    }

    #[test]
    fn injected_magic_is_not_a_touch_signature() {
        assert!(!is_touch_or_pen_extra(INJECTED_EXTRA_INFO));
        assert!(is_injected_extra(INJECTED_EXTRA_INFO));
        assert!(is_touch_or_pen_extra(TOUCH_PEN_EXTRA_SIGNATURE));
        assert!(is_touch_or_pen_extra(TOUCH_PEN_EXTRA_SIGNATURE | 0x12));
        assert!(!is_touch_or_pen_extra(0));
    }

    #[test]
    fn click_without_movement_reinjects_press_and_release() {
        let mut m = machine();
        let press = m.handle(input(EventKind::Press, Button::Right, 10, 20, 0));
        assert_eq!(press.decision, Decision::Swallow);
        assert_eq!(press.command, Command::None);
        assert_eq!(m.phase(), Phase::Pending);
        let release = m.handle(input(EventKind::Release, Button::Right, 12, 21, 40));
        assert_eq!(release.decision, Decision::Swallow);
        assert_eq!(
            release.command,
            Command::ReinjectClick {
                button: Button::Right,
                x: 10,
                y: 20
            }
        );
        assert_eq!(m.phase(), Phase::Idle);
        assert!(!m.delivered_down());
    }

    #[test]
    fn subthreshold_move_stays_a_click() {
        let mut m = machine();
        m.handle(input(EventKind::Press, Button::Right, 0, 0, 0));
        let moved = m.handle(input(EventKind::Move, Button::Right, 9, 0, 10));
        assert_eq!(moved.decision, Decision::Swallow);
        assert_eq!(m.phase(), Phase::Pending);
        let release = m.handle(input(EventKind::Release, Button::Right, 9, 0, 20));
        assert_eq!(
            release.command,
            Command::ReinjectClick {
                button: Button::Right,
                x: 0,
                y: 0
            }
        );
    }

    #[test]
    fn threshold_is_euclidean() {
        let mut m = machine();
        m.handle(input(EventKind::Press, Button::Right, 0, 0, 0));
        // 7²+7² = 98 < 100.
        m.handle(input(EventKind::Move, Button::Right, 7, 7, 5));
        assert_eq!(m.phase(), Phase::Pending);
        // 8²+8² = 128 >= 100.
        m.handle(input(EventKind::Move, Button::Right, 8, 8, 6));
        assert_eq!(m.phase(), Phase::Capturing);
        assert_eq!(m.current_points().len(), 2);
        assert_eq!(m.current_points()[0], Point { x: 0, y: 0 });
        assert_eq!(m.current_points()[1], Point { x: 8, y: 8 });
    }

    #[test]
    fn stroke_on_release_returns_to_idle() {
        let mut m = machine();
        m.handle(input(EventKind::Press, Button::Right, 0, 0, 0));
        m.handle(input(EventKind::Move, Button::Right, 30, 0, 10));
        m.handle(input(EventKind::Move, Button::Right, 40, 5, 15));
        let release = m.handle(input(EventKind::Release, Button::Right, 50, 5, 20));
        assert_eq!(release.decision, Decision::Swallow);
        assert_eq!(release.command, Command::Stroke);
        assert_eq!(m.phase(), Phase::Idle);
        let stroke = m.take_stroke().unwrap();
        assert_eq!(stroke.button, Button::Right);
        let points = stroke.as_slice();
        assert_eq!(points[0], Point { x: 0, y: 0 });
        assert_eq!(*points.last().unwrap(), Point { x: 50, y: 5 });
        assert!(points.len() >= 3);
    }

    #[test]
    fn timeout_reinjects_press_and_the_later_release_passes() {
        let mut m = machine();
        m.handle(input(EventKind::Press, Button::Right, 4, 5, 1_000));
        assert_eq!(m.poll_timeout(1_499), Command::None);
        assert_eq!(m.ms_until_timeout(1_000), Some(500));
        let command = m.poll_timeout(1_500);
        assert_eq!(
            command,
            Command::ReinjectPress {
                button: Button::Right,
                x: 4,
                y: 5
            }
        );
        assert_eq!(m.phase(), Phase::Idle);
        assert!(m.delivered_down());
        assert_eq!(m.poll_timeout(2_000), Command::None);
        let release = m.handle(input(EventKind::Release, Button::Right, 8, 9, 1_800));
        assert_eq!(release.decision, Decision::Pass);
        assert_eq!(release.command, Command::None);
        assert!(!m.delivered_down());
    }

    #[test]
    fn release_past_the_deadline_is_replayed_after_the_press() {
        let mut m = machine();
        m.handle(input(EventKind::Press, Button::Right, 1, 2, 0));
        let release = m.handle(input(EventKind::Release, Button::Right, 1, 2, 500));
        assert_eq!(release.decision, Decision::Swallow);
        assert_eq!(
            release.command,
            Command::ReinjectPress {
                button: Button::Right,
                x: 1,
                y: 2
            }
        );
        assert_eq!(
            release.replay,
            Some(Replay {
                kind: EventKind::Release,
                button: Button::Right,
                x: 1,
                y: 2
            })
        );
        assert_eq!(m.phase(), Phase::Idle);
        assert!(!m.delivered_down());
    }

    #[test]
    fn move_past_threshold_still_captures_after_the_deadline() {
        let mut m = machine();
        m.handle(input(EventKind::Press, Button::Right, 0, 0, 0));
        let moved = m.handle(input(EventKind::Move, Button::Right, 40, 0, 800));
        assert_eq!(moved.command, Command::None);
        assert!(moved.replay.is_none());
        assert_eq!(m.phase(), Phase::Capturing);
        assert!(!m.delivered_down());
    }

    #[test]
    fn small_move_past_the_deadline_is_replayed() {
        let mut m = machine();
        m.handle(input(EventKind::Press, Button::Right, 0, 0, 0));
        let moved = m.handle(input(EventKind::Move, Button::Right, 3, 0, 600));
        assert_eq!(
            moved.command,
            Command::ReinjectPress {
                button: Button::Right,
                x: 0,
                y: 0
            }
        );
        assert_eq!(
            moved.replay,
            Some(Replay {
                kind: EventKind::Move,
                button: Button::Right,
                x: 3,
                y: 0
            })
        );
        assert!(m.delivered_down());
        assert_eq!(m.phase(), Phase::Idle);
    }

    #[test]
    fn injected_and_touch_events_never_capture() {
        let mut m = machine();
        let mut injected = input(EventKind::Press, Button::Right, 0, 0, 0);
        injected.injected = true;
        assert_eq!(m.handle(injected).decision, Decision::Pass);
        assert_eq!(m.phase(), Phase::Idle);
        let mut touch = input(EventKind::Press, Button::Right, 0, 0, 0);
        touch.touch_or_pen = true;
        assert_eq!(m.handle(touch).decision, Decision::Pass);
        assert_eq!(m.phase(), Phase::Idle);

        m.handle(input(EventKind::Press, Button::Right, 0, 0, 0));
        m.handle(input(EventKind::Move, Button::Right, 30, 0, 10));
        let mut noise = input(EventKind::Move, Button::Right, 100, 100, 12);
        noise.injected = true;
        assert_eq!(m.handle(noise).decision, Decision::Pass);
        assert_eq!(m.current_points().len(), 2);
    }

    #[test]
    fn other_buttons_pass_through() {
        let mut m = machine();
        let press = m.handle(input(EventKind::Press, Button::Left, 0, 0, 0));
        assert_eq!(press.decision, Decision::Pass);
        assert_eq!(m.phase(), Phase::Idle);
        m.handle(input(EventKind::Press, Button::Right, 0, 0, 0));
        m.handle(input(EventKind::Move, Button::Right, 20, 0, 5));
        let other = m.handle(input(EventKind::Press, Button::Middle, 3, 3, 6));
        assert_eq!(other.decision, Decision::Pass);
        assert_eq!(m.phase(), Phase::Capturing);
    }

    #[test]
    fn neutralize_pending_and_capturing_pairs_the_press() {
        let mut pending = machine();
        pending.handle(input(EventKind::Press, Button::Right, 8, 9, 0));
        assert_eq!(
            pending.neutralize(),
            Neutralize::SyntheticClick {
                button: Button::Right,
                x: 8,
                y: 9
            }
        );
        assert_eq!(pending.phase(), Phase::Idle);
        assert!(!pending.delivered_down());

        let mut capturing = machine();
        capturing.handle(input(EventKind::Press, Button::Right, 1, 1, 0));
        capturing.handle(input(EventKind::Move, Button::Right, 40, 1, 10));
        assert_eq!(
            capturing.neutralize(),
            Neutralize::SyntheticClick {
                button: Button::Right,
                x: 1,
                y: 1
            }
        );
        assert_eq!(capturing.phase(), Phase::Idle);
    }

    #[test]
    fn neutralize_after_timeout_releases_the_delivered_press() {
        let mut m = machine();
        m.handle(input(EventKind::Press, Button::Right, 2, 3, 0));
        assert!(matches!(m.poll_timeout(500), Command::ReinjectPress { .. }));
        assert_eq!(
            m.neutralize(),
            Neutralize::SyntheticRelease {
                button: Button::Right,
                x: 2,
                y: 3
            }
        );
        assert!(!m.delivered_down());
        assert_eq!(m.neutralize(), Neutralize::None);
    }

    #[test]
    fn force_idle_does_not_ask_for_input() {
        let mut m = machine();
        m.handle(input(EventKind::Press, Button::Right, 0, 0, 0));
        m.force_idle();
        assert_eq!(m.phase(), Phase::Idle);
        assert_eq!(m.neutralize(), Neutralize::None);
    }

    #[test]
    fn timeout_wraps_the_tick_clock() {
        let mut m = machine();
        m.handle(input(EventKind::Press, Button::Right, 0, 0, u32::MAX - 10));
        assert_eq!(m.poll_timeout(u32::MAX), Command::None);
        let command = m.poll_timeout(489);
        assert!(matches!(command, Command::ReinjectPress { .. }));
    }

    #[test]
    fn full_buffer_keeps_the_newest_point() {
        let mut m = machine();
        m.handle(input(EventKind::Press, Button::Right, 0, 0, 0));
        m.handle(input(EventKind::Move, Button::Right, 10, 0, 1));
        for i in 0..MAX_STROKE_POINTS + 5 {
            m.handle(input(EventKind::Move, Button::Right, 10 + i as i32, 1, 2));
        }
        assert_eq!(m.current_points().len(), MAX_STROKE_POINTS);
        let release = m.handle(input(EventKind::Release, Button::Right, 9_000, 9, 3));
        assert_eq!(release.command, Command::Stroke);
        let stroke = m.take_stroke().unwrap();
        assert_eq!(stroke.len as usize, MAX_STROKE_POINTS);
        assert_eq!(stroke.as_slice().last().unwrap().x, 9_000);
    }
}
