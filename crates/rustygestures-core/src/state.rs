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

/// No hook event for this long while capturing means the release was lost.
pub const CAPTURE_INACTIVITY_MS: u32 = 2_000;

/// Other-button events held back during Pending so they are injected after the
/// primary press, preserving order.
pub const DEFERRED_CAP: usize = 8;

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
    /// Other-button events swallowed during Pending. Apply them after `command`
    /// and before `replay`.
    pub deferred: [Replay; DEFERRED_CAP],
    pub deferred_len: u8,
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

/// Why the engine is leaving a capture without a normal release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoverKind {
    /// The hook thread stopped making progress.
    Watchdog,
    /// Process exit. A click is used only when the physical button is already up.
    Shutdown,
    /// Session lock, disconnect, or switch. Do not leave the target holding a button.
    Session,
    /// No events while capturing.
    Inactivity,
    /// The output queue overflowed.
    Overflow,
}

/// What the caller must inject so a held press is not left unpaired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Neutralize {
    None,
    /// The physical button is still down. Inject the press only.
    DeliverPress {
        button: Button,
        x: i32,
        y: i32,
    },
    /// The target has the button down and the real release will not arrive.
    DeliverRelease {
        button: Button,
        x: i32,
        y: i32,
    },
    /// Shutdown, and the physical button is already up. Press and release.
    SyntheticClick {
        button: Button,
        x: i32,
        y: i32,
    },
    /// Inject nothing. The next primary release is swallowed.
    Drop,
}

impl Recovery {
    /// No injection and no deferred events.
    #[must_use]
    pub fn empty() -> Self {
        Self::none()
    }

    fn none() -> Self {
        Self {
            action: Neutralize::None,
            deferred: [Replay {
                kind: EventKind::Move,
                button: Button::Left,
                x: 0,
                y: 0,
            }; DEFERRED_CAP],
            deferred_len: 0,
        }
    }
}

/// Recovery action plus other-button events that were swallowed and must be replayed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Recovery {
    pub action: Neutralize,
    pub deferred: [Replay; DEFERRED_CAP],
    pub deferred_len: u8,
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
    swallow_next: bool,
    last_event_ms: u32,
    deferred: [Replay; DEFERRED_CAP],
    deferred_len: u8,
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
            swallow_next: false,
            last_event_ms: 0,
            deferred: [Replay {
                kind: EventKind::Move,
                button: Button::Left,
                x: 0,
                y: 0,
            }; DEFERRED_CAP],
            deferred_len: 0,
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

    /// Other-button events still waiting to be replayed.
    #[must_use]
    pub fn deferred_count(&self) -> u8 {
        self.deferred_len
    }

    /// Timestamp of the last non-injected event, for stillness checks.
    #[must_use]
    pub fn last_event_ms(&self) -> u32 {
        self.last_event_ms
    }

    #[must_use]
    pub fn swallow_next_release(&self) -> bool {
        self.swallow_next
    }

    pub fn set_swallow_next_release(&mut self, value: bool) {
        self.swallow_next = value;
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
        self.last_event_ms = ev.time_ms;

        // A move that crosses the threshold is a stroke, even if this event's
        // timestamp is also past the cancel delay. The delay is for a press
        // that is still sitting still.
        if self.phase == Phase::Pending
            && ev.kind == EventKind::Move
            && self.past_threshold(ev.x, ev.y)
        {
            self.begin_capture(ev.x, ev.y);
            return self.with_deferred(swallow(Command::None));
        }

        if self.phase == Phase::Pending && self.timed_out(ev.time_ms) {
            return self.timeout_then(ev);
        }

        if self.phase == Phase::Pending
            && ev.kind != EventKind::Move
            && ev.button != self.config.primary
        {
            return self.defer_other(ev);
        }

        self.dispatch(ev)
    }

    /// Cancel delay elapsed with no further input.
    ///
    /// `physically_down` is the button state tracked from non-injected input.
    /// When it is already up, the release was lost: inject nothing.
    pub fn poll_timeout(&mut self, now_ms: u32, physically_down: bool) -> Output {
        if self.phase != Phase::Pending || !self.timed_out(now_ms) {
            return pass();
        }
        let button = self.button;
        let x = self.origin_x;
        let y = self.origin_y;
        self.phase = Phase::Idle;
        self.len = 0;
        if !physically_down {
            self.delivered_down = false;
            self.swallow_next = true;
            return self.with_deferred(pass());
        }
        self.mark_delivered(button, x, y);
        self.with_deferred(swallow(Command::ReinjectPress { button, x, y }))
    }

    /// Capturing with no events for [`CAPTURE_INACTIVITY_MS`] drops the stroke
    /// and injects nothing.
    pub fn poll_inactivity(&mut self, now_ms: u32) -> Recovery {
        if self.phase != Phase::Capturing {
            return Recovery::none();
        }
        if now_ms.wrapping_sub(self.last_event_ms) < CAPTURE_INACTIVITY_MS {
            return Recovery::none();
        }
        self.recover(RecoverKind::Inactivity, false)
    }

    /// A delivered press whose physical button is already up needs a release.
    pub fn release_if_delivered_and_up(&mut self, physically_down: bool) -> Neutralize {
        if self.delivered_down && !physically_down {
            let button = self.delivered_button;
            let x = self.delivered_x;
            let y = self.delivered_y;
            self.delivered_down = false;
            self.swallow_next = true;
            Neutralize::DeliverRelease { button, x, y }
        } else {
            Neutralize::None
        }
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

    /// Leave Pending or Capturing without a normal release.
    ///
    /// `physically_down` is the real button. Capturing never injects: the
    /// target was not given the press, and the next primary release is
    /// swallowed. Pending injects a press only when the button is still down
    /// and the caller is the watchdog or shutdown. A session change never
    /// leaves the target holding the button.
    pub fn recover(&mut self, kind: RecoverKind, physically_down: bool) -> Recovery {
        let phase = self.phase;
        let button = self.button;
        let x = self.origin_x;
        let y = self.origin_y;
        let delivered = self.delivered_down;
        let delivered_button = self.delivered_button;
        let delivered_x = self.delivered_x;
        let delivered_y = self.delivered_y;
        let (deferred, deferred_len) = self.take_deferred_events();

        self.phase = Phase::Idle;
        self.len = 0;
        self.stroke_ready = false;

        let action = if phase == Phase::Capturing {
            self.delivered_down = false;
            self.swallow_next = true;
            Neutralize::Drop
        } else if phase == Phase::Pending {
            let hold =
                matches!(kind, RecoverKind::Watchdog | RecoverKind::Shutdown) && physically_down;
            if hold {
                self.mark_delivered(button, x, y);
                Neutralize::DeliverPress { button, x, y }
            } else if kind == RecoverKind::Shutdown && !physically_down {
                self.delivered_down = false;
                self.swallow_next = true;
                Neutralize::SyntheticClick { button, x, y }
            } else {
                self.delivered_down = false;
                self.swallow_next = true;
                Neutralize::Drop
            }
        } else if delivered {
            let release_it = !physically_down
                || matches!(
                    kind,
                    RecoverKind::Session | RecoverKind::Inactivity | RecoverKind::Overflow
                );
            if release_it {
                self.delivered_down = false;
                self.swallow_next = true;
                Neutralize::DeliverRelease {
                    button: delivered_button,
                    x: delivered_x,
                    y: delivered_y,
                }
            } else {
                Neutralize::None
            }
        } else {
            Neutralize::None
        };
        Recovery {
            action,
            deferred,
            deferred_len,
        }
    }

    /// The watchdog already injected. Adopt that outcome without a second injection.
    /// Deferred other-button events are returned so the caller can replay them.
    pub fn adopt_recovery(&mut self, delivered: bool, swallow_next: bool) -> Recovery {
        let (deferred, deferred_len) = self.take_deferred_events();
        self.phase = Phase::Idle;
        self.len = 0;
        self.stroke_ready = false;
        self.delivered_down = delivered;
        self.swallow_next = swallow_next;
        Recovery {
            action: Neutralize::None,
            deferred,
            deferred_len,
        }
    }

    fn take_deferred_events(&mut self) -> ([Replay; DEFERRED_CAP], u8) {
        let deferred = self.deferred;
        let len = self.deferred_len;
        self.deferred_len = 0;
        (deferred, len)
    }

    fn timeout_then(&mut self, ev: InputEvent) -> Output {
        let button = self.button;
        let x = self.origin_x;
        let y = self.origin_y;
        self.mark_delivered(button, x, y);
        self.phase = Phase::Idle;
        self.len = 0;
        let follow = self.dispatch(ev);
        let mut output = if follow.decision == Decision::Pass {
            Output {
                decision: Decision::Swallow,
                command: Command::ReinjectPress { button, x, y },
                replay: Some(Replay {
                    kind: ev.kind,
                    button: ev.button,
                    x: ev.x,
                    y: ev.y,
                }),
                ..pass()
            }
        } else {
            Output {
                decision: follow.decision,
                command: Command::ReinjectPress { button, x, y },
                replay: None,
                ..pass()
            }
        };
        output = self.with_deferred(output);
        output
    }

    fn dispatch(&mut self, ev: InputEvent) -> Output {
        match ev.kind {
            EventKind::Press => self.on_press(ev),
            EventKind::Release => self.on_release(ev),
            EventKind::Move => self.on_move(ev),
        }
    }

    fn on_press(&mut self, ev: InputEvent) -> Output {
        if ev.button != self.config.primary {
            return pass();
        }
        if self.phase != Phase::Idle {
            // The release never arrived. Nothing was delivered, so drop the
            // stale stroke and start again from this press.
            let flushed = self.with_deferred(swallow(Command::None));
            self.phase = Phase::Idle;
            self.len = 0;
            self.stroke_ready = false;
            self.swallow_next = false;
            self.begin_pending(ev);
            return flushed;
        }
        // A second down without a release is not a new stroke. The target
        // already has this button, or a duplicate down arrived.
        if self.delivered_down && ev.button == self.delivered_button {
            return pass();
        }
        self.begin_pending(ev);
        swallow(Command::None)
    }

    fn begin_pending(&mut self, ev: InputEvent) {
        self.phase = Phase::Pending;
        self.button = ev.button;
        self.origin_x = ev.x;
        self.origin_y = ev.y;
        self.origin_ms = ev.time_ms;
        self.len = 0;
        self.stroke_ready = false;
        self.deferred_len = 0;
        self.swallow_next = false;
    }

    fn on_release(&mut self, ev: InputEvent) -> Output {
        if ev.button == self.config.primary && self.phase == Phase::Idle && self.swallow_next {
            self.swallow_next = false;
            self.delivered_down = false;
            return swallow(Command::None);
        }
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
                self.with_deferred(Output {
                    decision: Decision::Swallow,
                    command: Command::ReinjectClick { button, x, y },
                    replay: None,
                    ..pass()
                })
            }
            Phase::Capturing => {
                self.push_point(ev.x, ev.y);
                self.finish_stroke();
                self.phase = Phase::Idle;
                Output {
                    decision: Decision::Swallow,
                    command: Command::Stroke,
                    ..pass()
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

    fn defer_other(&mut self, ev: InputEvent) -> Output {
        if (self.deferred_len as usize) < DEFERRED_CAP {
            let index = self.deferred_len as usize;
            self.deferred[index] = Replay {
                kind: ev.kind,
                button: ev.button,
                x: ev.x,
                y: ev.y,
            };
            self.deferred_len += 1;
            swallow(Command::None)
        } else {
            // The buffer is full. Deliver this one now rather than drop it.
            pass()
        }
    }

    fn with_deferred(&mut self, mut output: Output) -> Output {
        output.deferred = self.deferred;
        output.deferred_len = self.deferred_len;
        self.deferred_len = 0;
        output
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

fn empty_replay() -> Replay {
    Replay {
        kind: EventKind::Move,
        button: Button::Left,
        x: 0,
        y: 0,
    }
}

fn pass() -> Output {
    Output {
        decision: Decision::Pass,
        command: Command::None,
        replay: None,
        deferred: [empty_replay(); DEFERRED_CAP],
        deferred_len: 0,
    }
}

fn swallow(command: Command) -> Output {
    Output {
        decision: Decision::Swallow,
        command,
        ..pass()
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
        assert_eq!(m.poll_timeout(1_499, true).command, Command::None);
        assert_eq!(m.ms_until_timeout(1_000), Some(500));
        let command = m.poll_timeout(1_500, true).command;
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
        assert_eq!(m.poll_timeout(2_000, true).command, Command::None);
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
    fn watchdog_pending_injects_press_only_while_held() {
        let mut pending = machine();
        pending.handle(input(EventKind::Press, Button::Right, 8, 9, 0));
        assert_eq!(
            pending.recover(RecoverKind::Watchdog, true).action,
            Neutralize::DeliverPress {
                button: Button::Right,
                x: 8,
                y: 9
            }
        );
        assert_eq!(pending.phase(), Phase::Idle);
        assert!(pending.delivered_down());

        let mut up = machine();
        up.handle(input(EventKind::Press, Button::Right, 8, 9, 0));
        assert_eq!(
            up.recover(RecoverKind::Watchdog, false).action,
            Neutralize::Drop
        );
        assert!(!up.delivered_down());
        assert!(up.swallow_next_release());
    }

    #[test]
    fn capturing_recovery_injects_nothing() {
        let mut capturing = machine();
        capturing.handle(input(EventKind::Press, Button::Right, 1, 1, 0));
        capturing.handle(input(EventKind::Move, Button::Right, 40, 1, 10));
        assert_eq!(
            capturing.recover(RecoverKind::Watchdog, true).action,
            Neutralize::Drop
        );
        assert_eq!(capturing.phase(), Phase::Idle);
        assert!(!capturing.delivered_down());
        let release = capturing.handle(input(EventKind::Release, Button::Right, 40, 1, 20));
        assert_eq!(release.decision, Decision::Swallow);
        assert_eq!(release.command, Command::None);
        assert!(!capturing.swallow_next_release());
    }

    #[test]
    fn shutdown_click_only_when_the_button_is_up() {
        let mut up = machine();
        up.handle(input(EventKind::Press, Button::Right, 2, 3, 0));
        assert_eq!(
            up.recover(RecoverKind::Shutdown, false).action,
            Neutralize::SyntheticClick {
                button: Button::Right,
                x: 2,
                y: 3
            }
        );
        let mut down = machine();
        down.handle(input(EventKind::Press, Button::Right, 2, 3, 0));
        assert_eq!(
            down.recover(RecoverKind::Shutdown, true).action,
            Neutralize::DeliverPress {
                button: Button::Right,
                x: 2,
                y: 3
            }
        );
    }

    #[test]
    fn session_change_does_not_deliver_a_press() {
        let mut pending = machine();
        pending.handle(input(EventKind::Press, Button::Right, 4, 5, 0));
        assert_eq!(
            pending.recover(RecoverKind::Session, true).action,
            Neutralize::Drop
        );
        assert!(!pending.delivered_down());
    }

    #[test]
    fn lost_release_then_a_new_press_starts_clean() {
        let mut m = machine();
        m.handle(input(EventKind::Press, Button::Right, 0, 0, 0));
        m.handle(input(EventKind::Move, Button::Right, 50, 0, 10));
        assert_eq!(m.phase(), Phase::Capturing);
        let press = m.handle(input(EventKind::Press, Button::Right, 300, 300, 5_000));
        assert_eq!(press.decision, Decision::Swallow);
        assert_eq!(press.command, Command::None);
        assert_eq!(m.phase(), Phase::Pending);
        assert!(!m.delivered_down());
        let release = m.handle(input(EventKind::Release, Button::Right, 300, 300, 5_050));
        assert_eq!(
            release.command,
            Command::ReinjectClick {
                button: Button::Right,
                x: 300,
                y: 300
            }
        );
        assert_eq!(m.phase(), Phase::Idle);
    }

    #[test]
    fn timeout_with_the_button_already_up_injects_nothing() {
        let mut m = machine();
        m.handle(input(EventKind::Press, Button::Right, 1, 2, 0));
        let output = m.poll_timeout(500, false);
        assert_eq!(output.command, Command::None);
        assert_eq!(m.phase(), Phase::Idle);
        assert!(!m.delivered_down());
        assert!(m.swallow_next_release());
    }

    #[test]
    fn other_button_during_pending_is_deferred() {
        let mut m = machine();
        m.handle(input(EventKind::Press, Button::Right, 0, 0, 0));
        let left = m.handle(input(EventKind::Press, Button::Left, 1, 1, 10));
        assert_eq!(left.decision, Decision::Swallow);
        assert_eq!(left.deferred_len, 0);
        let release = m.handle(input(EventKind::Release, Button::Right, 0, 0, 20));
        assert_eq!(
            release.command,
            Command::ReinjectClick {
                button: Button::Right,
                x: 0,
                y: 0
            }
        );
        assert_eq!(release.deferred_len, 1);
        assert_eq!(release.deferred[0].button, Button::Left);
        assert_eq!(release.deferred[0].kind, EventKind::Press);
    }

    #[test]
    fn inactivity_drops_a_capture() {
        let mut m = machine();
        m.handle(input(EventKind::Press, Button::Right, 0, 0, 0));
        m.handle(input(EventKind::Move, Button::Right, 40, 0, 10));
        assert_eq!(
            m.poll_inactivity(10 + CAPTURE_INACTIVITY_MS - 1).action,
            Neutralize::None
        );
        assert_eq!(
            m.poll_inactivity(10 + CAPTURE_INACTIVITY_MS).action,
            Neutralize::Drop
        );
        assert_eq!(m.phase(), Phase::Idle);
        assert!(!m.delivered_down());
    }

    #[test]
    fn recover_replays_deferred_other_button_events() {
        let mut session = machine();
        session.handle(input(EventKind::Press, Button::Right, 0, 0, 0));
        session.handle(input(EventKind::Press, Button::Left, 4, 5, 10));
        session.handle(input(EventKind::Release, Button::Left, 4, 6, 20));
        let recovered = session.recover(RecoverKind::Session, true);
        assert_eq!(recovered.action, Neutralize::Drop);
        assert_eq!(recovered.deferred_len, 2);
        assert_eq!(recovered.deferred[0].kind, EventKind::Press);
        assert_eq!(recovered.deferred[0].button, Button::Left);
        assert_eq!(recovered.deferred[1].kind, EventKind::Release);
        assert_eq!(recovered.deferred[1].button, Button::Left);

        let mut shutdown = machine();
        shutdown.handle(input(EventKind::Press, Button::Right, 1, 1, 0));
        shutdown.handle(input(EventKind::Press, Button::Left, 2, 2, 5));
        shutdown.handle(input(EventKind::Release, Button::Left, 2, 2, 6));
        let recovered = shutdown.recover(RecoverKind::Shutdown, false);
        assert_eq!(
            recovered.action,
            Neutralize::SyntheticClick {
                button: Button::Right,
                x: 1,
                y: 1
            }
        );
        assert_eq!(recovered.deferred_len, 2);
        assert_eq!(recovered.deferred[0].button, Button::Left);
        assert_eq!(recovered.deferred[1].kind, EventKind::Release);
    }

    #[test]
    fn adopt_recovery_does_not_invent_a_command() {
        let mut m = machine();
        m.handle(input(EventKind::Press, Button::Right, 0, 0, 0));
        let _ = m.adopt_recovery(false, true);
        assert_eq!(m.phase(), Phase::Idle);
        assert!(m.swallow_next_release());
        assert_eq!(
            m.recover(RecoverKind::Watchdog, true).action,
            Neutralize::None
        );
    }

    #[test]
    fn adopt_recovery_returns_deferred_events_for_replay() {
        let mut m = machine();
        m.handle(input(EventKind::Press, Button::Right, 0, 0, 0));
        m.handle(input(EventKind::Press, Button::Left, 3, 4, 10));
        m.handle(input(EventKind::Release, Button::Left, 3, 4, 20));
        let recovered = m.adopt_recovery(false, true);
        assert_eq!(recovered.action, Neutralize::None);
        assert_eq!(recovered.deferred_len, 2);
        assert_eq!(recovered.deferred[0].kind, EventKind::Press);
        assert_eq!(recovered.deferred[1].kind, EventKind::Release);
        assert_eq!(m.deferred_count(), 0);
    }

    #[test]
    fn lost_release_new_press_replays_the_deferred_other_button() {
        let mut m = machine();
        m.handle(input(EventKind::Press, Button::Right, 0, 0, 0));
        m.handle(input(EventKind::Press, Button::Left, 8, 9, 10));
        // The right-button release never arrives.
        let again = m.handle(input(EventKind::Press, Button::Right, 4, 4, 100));
        assert_eq!(again.decision, Decision::Swallow);
        assert_eq!(again.deferred_len, 1);
        assert_eq!(again.deferred[0].button, Button::Left);
        assert_eq!(again.deferred[0].kind, EventKind::Press);
        assert_eq!(m.phase(), Phase::Pending);
        assert!(!m.delivered_down());
        assert_eq!(m.deferred_count(), 0);
    }

    #[test]
    fn timeout_wraps_the_tick_clock() {
        let mut m = machine();
        m.handle(input(EventKind::Press, Button::Right, 0, 0, u32::MAX - 10));
        assert_eq!(m.poll_timeout(u32::MAX, true).command, Command::None);
        let command = m.poll_timeout(489, true).command;
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
