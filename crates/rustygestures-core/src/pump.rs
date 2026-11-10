//! Queue of hook decisions, drained after the message pump returns.
//!
//! Windows delivers `WH_MOUSE_LL` inside `PeekMessage` and then returns false
//! when nothing was posted. The hook thread must apply this queue after every
//! pump, not only when `PeekMessage` returns true. The queue is a fixed array
//! so the callback stays allocation-free. It is only touched on the hook thread,
//! including when `SendInput` re-enters the callback.

use crate::{Command, Config, InputEvent, Machine, Output, CAPTURE_INACTIVITY_MS};

/// How many hook decisions can sit between two drains.
pub const OUTPUT_QUEUE_CAP: usize = 32;

/// Same-thread FIFO of [`Output`] values.
#[derive(Clone, Debug)]
pub struct OutputQueue {
    slots: [Output; OUTPUT_QUEUE_CAP],
    head: usize,
    len: usize,
    overflow: bool,
}

impl Default for OutputQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl OutputQueue {
    #[must_use]
    pub fn new() -> Self {
        Self {
            slots: [empty_output(); OUTPUT_QUEUE_CAP],
            head: 0,
            len: 0,
            overflow: false,
        }
    }

    /// Keep an output that asks the engine to do something. `false` means the
    /// queue is full and [`Self::overflowed`] is set.
    pub fn push(&mut self, output: Output) -> bool {
        if output.command == Command::None && output.replay.is_none() && output.deferred_len == 0 {
            return true;
        }
        if self.len == OUTPUT_QUEUE_CAP {
            self.overflow = true;
            return false;
        }
        let index = (self.head + self.len) % OUTPUT_QUEUE_CAP;
        self.slots[index] = output;
        self.len += 1;
        true
    }

    #[must_use]
    pub fn pop(&mut self) -> Option<Output> {
        if self.len == 0 {
            return None;
        }
        let output = self.slots[self.head];
        self.head = (self.head + 1) % OUTPUT_QUEUE_CAP;
        self.len -= 1;
        Some(output)
    }

    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
        self.overflow = false;
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[must_use]
    pub fn overflowed(&self) -> bool {
        self.overflow
    }
}

fn empty_output() -> Output {
    use crate::{Button, Decision, EventKind, Replay, DEFERRED_CAP};
    Output {
        decision: Decision::Pass,
        command: Command::None,
        replay: None,
        deferred: [Replay {
            kind: EventKind::Move,
            button: Button::Left,
            x: 0,
            y: 0,
        }; DEFERRED_CAP],
        deferred_len: 0,
    }
}

/// State machine plus the queue the hook callback fills.
///
/// `on_hook` is what the callback does. `drain` is what the thread does after
/// `PeekMessage` returns, whether or not it returned a posted message.
#[derive(Clone, Debug)]
pub struct CapturePump {
    machine: Machine,
    queue: OutputQueue,
}

impl CapturePump {
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            machine: Machine::new(config),
            queue: OutputQueue::new(),
        }
    }

    #[must_use]
    pub fn machine(&self) -> &Machine {
        &self.machine
    }

    pub fn machine_mut(&mut self) -> &mut Machine {
        &mut self.machine
    }

    /// Record one hook event. Does not inject or publish; the caller drains later.
    pub fn on_hook(&mut self, event: InputEvent) -> crate::Decision {
        let output = self.machine.handle(event);
        let decision = output.decision;
        let _ = self.queue.push(output);
        decision
    }

    #[must_use]
    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }

    /// `true` when a push was dropped. Cleared by [`Self::drain`] once observed.
    #[must_use]
    pub fn take_overflow(&mut self) -> bool {
        let overflow = self.queue.overflowed();
        if overflow {
            self.queue.clear();
        }
        overflow
    }

    #[must_use]
    pub fn pop(&mut self) -> Option<Output> {
        self.queue.pop()
    }

    pub fn clear_queue(&mut self) {
        self.queue.clear();
    }

    /// Cancel-delay check used when the pump wakes with no new hook event.
    pub fn poll_timeout(&mut self, now_ms: u32, physically_down: bool) -> Output {
        self.machine.poll_timeout(now_ms, physically_down)
    }

    pub fn poll_inactivity(&mut self, now_ms: u32) -> crate::Neutralize {
        let _ = CAPTURE_INACTIVITY_MS;
        self.machine.poll_inactivity(now_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Button, Command, Decision, EventKind};

    fn event(kind: EventKind, x: i32, y: i32, time_ms: u32) -> InputEvent {
        InputEvent {
            kind,
            button: Button::Right,
            x,
            y,
            time_ms,
            injected: false,
            touch_or_pen: false,
        }
    }

    fn applied(pump: &mut CapturePump) -> Vec<Output> {
        let mut out = Vec::new();
        while let Some(output) = pump.pop() {
            out.push(output);
        }
        out
    }

    #[test]
    fn click_is_applied_once_after_the_pump_returns_false() {
        let mut pump = CapturePump::new(Config::default());
        // The callback queues both events. PeekMessage then returns false,
        // so nothing has been applied yet.
        pump.on_hook(event(EventKind::Press, 10, 20, 0));
        pump.on_hook(event(EventKind::Release, 10, 20, 30));
        assert_eq!(pump.queue_len(), 1);
        assert_eq!(pump.machine().phase(), crate::Phase::Idle);
        let outputs = applied(&mut pump);
        assert_eq!(outputs.len(), 1);
        assert_eq!(
            outputs[0].command,
            Command::ReinjectClick {
                button: Button::Right,
                x: 10,
                y: 20
            }
        );
        assert!(applied(&mut pump).is_empty());
    }

    #[test]
    fn stroke_is_applied_once_and_survives_a_reentrant_inject() {
        let mut pump = CapturePump::new(Config::default());
        pump.on_hook(event(EventKind::Press, 0, 0, 0));
        pump.on_hook(event(EventKind::Move, 40, 0, 10));
        pump.on_hook(event(EventKind::Release, 50, 0, 20));
        // SendInput of an older command re-enters the callback before drain
        // finishes. The injected event must not overwrite the stroke.
        let mut injected = event(EventKind::Press, 1, 1, 21);
        injected.injected = true;
        pump.on_hook(injected);
        let outputs = applied(&mut pump);
        let strokes: Vec<_> = outputs
            .iter()
            .filter(|output| output.command == Command::Stroke)
            .collect();
        assert_eq!(strokes.len(), 1);
        assert!(pump.machine_mut().take_stroke().is_some());
    }

    #[test]
    fn two_commands_keep_their_order() {
        let mut pump = CapturePump::new(Config::default());
        pump.on_hook(event(EventKind::Press, 0, 0, 0));
        pump.on_hook(event(EventKind::Release, 0, 0, 10));
        pump.on_hook(event(EventKind::Press, 5, 5, 20));
        pump.on_hook(event(EventKind::Move, 40, 5, 30));
        pump.on_hook(event(EventKind::Release, 40, 5, 40));
        let outputs = applied(&mut pump);
        assert_eq!(outputs[0].decision, Decision::Swallow);
        assert!(matches!(outputs[0].command, Command::ReinjectClick { .. }));
        assert_eq!(outputs.last().unwrap().command, Command::Stroke);
    }

    #[test]
    fn overflow_is_reported_and_cleared() {
        let mut queue = OutputQueue::new();
        for i in 0..OUTPUT_QUEUE_CAP {
            assert!(queue.push(Output {
                decision: Decision::Swallow,
                command: Command::ReinjectPress {
                    button: Button::Right,
                    x: i as i32,
                    y: 0,
                },
                ..empty_output()
            }));
        }
        assert!(!queue.push(Output {
            decision: Decision::Swallow,
            command: Command::Stroke,
            ..empty_output()
        }));
        assert!(queue.overflowed());
        assert_eq!(queue.len(), OUTPUT_QUEUE_CAP);
    }
}
