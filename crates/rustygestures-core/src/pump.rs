//! Queue of hook decisions, drained after the message pump returns.
//!
//! A low-level hook call can run inside `PeekMessage` and that call can then
//! return false because nothing was posted. Drain only after `peek` returns.
//! Re-entry on the same thread is possible, so callers must not hold `&mut`
//! across `SendInput`. The queue is a fixed array and does not allocate.

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
    ///
    /// Pass-through moves are not queued. Only a command, a replay, or deferred
    /// other-button events occupy a slot.
    pub fn on_hook(&mut self, event: InputEvent) -> crate::Decision {
        let output = self.machine.handle(event);
        let decision = output.decision;
        if output_is_actionable(&output) {
            let _ = self.queue.push(output);
        }
        decision
    }

    #[must_use]
    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }

    /// `true` when a new output was refused. Commands already queued stay.
    #[must_use]
    pub fn take_overflow(&mut self) -> bool {
        let overflow = self.queue.overflowed();
        self.queue.overflow = false;
        overflow
    }

    /// Put an output back at the head when injection has to wait.
    pub fn push_front(&mut self, output: Output) -> bool {
        if self.queue.len == OUTPUT_QUEUE_CAP {
            self.queue.overflow = true;
            return false;
        }
        self.queue.head = (self.queue.head + OUTPUT_QUEUE_CAP - 1) % OUTPUT_QUEUE_CAP;
        self.queue.slots[self.queue.head] = output;
        self.queue.len += 1;
        true
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

    pub fn poll_inactivity(&mut self, now_ms: u32) -> crate::Recovery {
        let _ = CAPTURE_INACTIVITY_MS;
        self.machine.poll_inactivity(now_ms)
    }
}

/// A source of hook deliveries.
///
/// `peek` may call `pump.on_hook` for a sent low-level-hook call and then
/// return false, which is what `PeekMessage` does when nothing was posted.
pub trait HookMessages {
    fn peek(&mut self, pump: &mut CapturePump) -> bool;
}

/// Applies one drained [`Output`]. `false` means the caller must try this
/// output again after pumping messages; it was not applied.
pub trait Injector {
    fn inject(&mut self, output: Output) -> bool;
}

/// Actionable outputs drained after `peek` returns. Fixed storage, no allocation.
#[derive(Clone, Copy)]
pub struct HookBatch {
    slots: [Output; OUTPUT_QUEUE_CAP],
    len: usize,
}

impl HookBatch {
    fn empty() -> Self {
        Self {
            slots: [empty_output(); OUTPUT_QUEUE_CAP],
            len: 0,
        }
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
    pub fn as_slice(&self) -> &[Output] {
        &self.slots[..self.len]
    }
}

fn output_is_actionable(output: &Output) -> bool {
    output.command != Command::None || output.replay.is_some() || output.deferred_len > 0
}

/// Commands queued by `peek`, drained only after `peek` has returned.
///
/// Draining inside the `while peek` body drops every command, because a sent
/// hook call is delivered inside `peek` and `peek` then returns false.
pub fn poll_hook<M: HookMessages>(msgs: &mut M, pump: &mut CapturePump) -> HookBatch {
    while msgs.peek(pump) {}
    let mut batch = HookBatch::empty();
    while let Some(output) = pump.pop() {
        if output_is_actionable(&output) && batch.len < OUTPUT_QUEUE_CAP {
            batch.slots[batch.len] = output;
            batch.len += 1;
        }
    }
    batch
}

/// [`poll_hook`], then hand each output to `injector`.
///
/// The injector does not run inside `peek`. A caller that injects with
/// `SendInput` must use [`poll_hook`] and inject only after that returns, so
/// no `&mut` is held across the call.
pub fn service_hook<M: HookMessages, I: Injector>(
    msgs: &mut M,
    pump: &mut CapturePump,
    injector: &mut I,
) {
    let outputs = poll_hook(msgs, pump);
    for (index, output) in outputs.as_slice().iter().copied().enumerate() {
        if !injector.inject(output) {
            for pending in outputs.as_slice().iter().copied().skip(index).rev() {
                let _ = pump.push_front(pending);
            }
            break;
        }
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

    struct SentThenFalse {
        events: std::collections::VecDeque<InputEvent>,
    }

    impl HookMessages for SentThenFalse {
        fn peek(&mut self, pump: &mut CapturePump) -> bool {
            if let Some(event) = self.events.pop_front() {
                let _ = pump.on_hook(event);
            }
            // A sent hook call is delivered inside peek, then peek returns false.
            false
        }
    }

    struct Record {
        outputs: Vec<Output>,
    }

    impl Injector for Record {
        fn inject(&mut self, output: Output) -> bool {
            self.outputs.push(output);
            true
        }
    }

    #[test]
    fn sent_hook_call_is_applied_when_peek_returns_false() {
        let mut source = SentThenFalse {
            events: [
                event(EventKind::Press, 10, 20, 0),
                event(EventKind::Release, 10, 20, 30),
            ]
            .into(),
        };
        let mut pump = CapturePump::new(Config::default());
        let mut injector = Record {
            outputs: Vec::new(),
        };
        for _ in 0..4 {
            service_hook(&mut source, &mut pump, &mut injector);
            if source.events.is_empty() && pump.queue_len() == 0 {
                break;
            }
        }
        let clicks = injector
            .outputs
            .iter()
            .filter(|output| matches!(output.command, Command::ReinjectClick { .. }))
            .count();
        assert_eq!(
            clicks, 1,
            "a click swallowed inside peek must still be re-injected"
        );
    }

    #[test]
    fn pass_through_moves_are_not_queued() {
        let mut pump = CapturePump::new(Config::default());
        pump.on_hook(event(EventKind::Press, 0, 0, 0));
        pump.on_hook(event(EventKind::Move, 40, 0, 10));
        assert_eq!(pump.queue_len(), 0);
    }

    #[test]
    fn overflow_keeps_commands_already_queued() {
        let mut pump = CapturePump::new(Config::default());
        for i in 0..OUTPUT_QUEUE_CAP {
            pump.on_hook(event(EventKind::Press, i as i32, 0, i as u32));
            pump.on_hook(event(EventKind::Release, i as i32, 0, i as u32 + 1));
        }
        let refused = pump.on_hook(event(EventKind::Press, 9, 9, 10_000));
        let _ = pump.on_hook(event(EventKind::Release, 9, 9, 10_001));
        let _ = refused;
        assert!(pump.take_overflow());
        assert!(
            pump.pop().is_some(),
            "a full queue must keep the commands it already accepted"
        );
    }
}
