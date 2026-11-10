//! Random event sequences must not leave a swallowed press unpaired, and a
//! finished sequence must be back in Idle.

use proptest::prelude::*;
use rustygestures_core::{
    Button, Command, Config, Decision, EventKind, InputEvent, Machine, Neutralize, Output, Phase,
    RecoverKind,
};

#[derive(Clone, Debug)]
enum Step {
    Input {
        kind: EventKind,
        button: Button,
        dx: i16,
        dy: i16,
        dt: u16,
        injected: bool,
        touch_or_pen: bool,
    },
    PollTimeout {
        dt: u16,
    },
    /// The primary release never reached the hook.
    LostRelease,
    Neutralize,
}

fn step_strategy() -> impl Strategy<Value = Step> {
    prop_oneof![
        8 => (
            prop_oneof![
                Just(EventKind::Press),
                Just(EventKind::Release),
                Just(EventKind::Move),
            ],
            prop_oneof![
                Just(Button::Left),
                Just(Button::Right),
                Just(Button::Middle),
                Just(Button::X1),
            ],
            -40i16..80,
            -40i16..80,
            0u16..800,
            prop::bool::weighted(0.1),
            prop::bool::weighted(0.05),
        )
            .prop_map(|(kind, button, dx, dy, dt, injected, touch_or_pen)| Step::Input {
                kind,
                button,
                dx,
                dy,
                dt,
                injected,
                touch_or_pen,
            }),
        2 => (0u16..700).prop_map(|dt| Step::PollTimeout { dt }),
        1 => Just(Step::LostRelease),
        1 => Just(Step::Neutralize),
    ]
}

#[derive(Default)]
struct Books {
    open_swallows: i32,
    /// What the target window was given.
    target_down: bool,
    /// Physical primary button, tracked only from non-injected events.
    physical_down: bool,
    /// Other-button events the machine swallowed.
    swallowed_other: u32,
    /// Those events that have since appeared on an output or a recovery.
    emitted_other: u32,
}

fn note_deferred(books: &mut Books, before: u8, after: u8, emitted: u8) {
    if after > before {
        books.swallowed_other += u32::from(after - before);
    }
    books.emitted_other += u32::from(emitted);
}

fn apply_output(books: &mut Books, ev: &InputEvent, output: &Output) {
    let primary = Button::Right;
    let deferred = output.replay.is_some();
    let real_swallow = output.decision == Decision::Swallow && !deferred;
    if !ev.injected && !ev.touch_or_pen && ev.button == primary {
        match ev.kind {
            EventKind::Press => books.physical_down = true,
            EventKind::Release => books.physical_down = false,
            EventKind::Move => {}
        }
    }
    if output.decision == Decision::Pass
        && ev.kind == EventKind::Press
        && ev.button == primary
        && !ev.injected
        && !ev.touch_or_pen
    {
        books.target_down = true;
    }
    if real_swallow && ev.kind == EventKind::Press && ev.button == primary {
        if books.open_swallows > 0 {
            // A new primary press while one was already swallowed means the
            // release was lost. The old press was not delivered.
            books.open_swallows = 0;
        }
        books.open_swallows += 1;
    }
    if real_swallow
        && ev.kind == EventKind::Release
        && ev.button == primary
        && books.open_swallows > 0
    {
        books.open_swallows -= 1;
    }
    match output.command {
        Command::ReinjectPress { .. } => {
            books.open_swallows -= 1;
            books.target_down = true;
        }
        Command::ReinjectRelease { .. } => {
            books.target_down = false;
        }
        Command::ReinjectClick { .. } | Command::Stroke | Command::None => {}
    }
    if let Some(replay) = output.replay {
        if replay.kind == EventKind::Release && replay.button == primary {
            books.target_down = false;
        }
    } else if output.decision == Decision::Pass
        && ev.kind == EventKind::Release
        && ev.button == primary
    {
        // Passed through, including touch or an injected release. The target
        // got the button-up.
        books.target_down = false;
    }
}

fn dummy_event(time_ms: u32) -> InputEvent {
    InputEvent {
        kind: EventKind::Move,
        button: Button::Right,
        x: 0,
        y: 0,
        time_ms,
        injected: false,
        touch_or_pen: false,
    }
}

fn apply_neutralize(books: &mut Books, action: Neutralize) {
    match action {
        Neutralize::None => {}
        Neutralize::SyntheticClick { .. } | Neutralize::Drop => {
            books.open_swallows = 0;
            books.target_down = false;
        }
        Neutralize::DeliverPress { .. } => {
            books.open_swallows = 0;
            books.target_down = true;
        }
        Neutralize::DeliverRelease { .. } => {
            books.target_down = false;
        }
    }
}

fn reconcile(machine: &mut Machine, books: &mut Books) {
    if machine.delivered_down() && !books.physical_down {
        let action = machine.release_if_delivered_and_up(false);
        apply_neutralize(books, action);
    }
}

fn check(machine: &Machine, books: &Books) {
    assert!(
        (0..=1).contains(&books.open_swallows),
        "open swallows {}",
        books.open_swallows
    );
    assert_eq!(
        books.open_swallows == 1,
        machine.phase() != Phase::Idle,
        "phase {:?} open {}",
        machine.phase(),
        books.open_swallows
    );
    if machine.delivered_down() {
        assert!(
            books.target_down,
            "a delivered press must show on the target"
        );
    }
    if !books.physical_down && machine.phase() == Phase::Idle {
        assert!(
            !books.target_down,
            "target still holds the button after it was physically released"
        );
        assert!(!machine.delivered_down());
    }
    assert_eq!(
        books.swallowed_other,
        books.emitted_other + u32::from(machine.deferred_count()),
        "a swallowed other-button event was dropped"
    );
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 200, ..ProptestConfig::default() })]
    #[test]
    fn random_sequences_hold_invariants(steps in prop::collection::vec(step_strategy(), 0..80)) {
        let mut machine = Machine::new(Config::default());
        let mut books = Books::default();
        let mut x: i32 = 0;
        let mut y: i32 = 0;
        let mut time_ms: u32 = 1_000;
        for step in steps {
            match step {
                Step::Input { kind, button, dx, dy, dt, injected, touch_or_pen } => {
                    x = x.saturating_add(i32::from(dx));
                    y = y.saturating_add(i32::from(dy));
                    time_ms = time_ms.wrapping_add(u32::from(dt));
                    let ev = InputEvent {
                        kind,
                        button,
                        x,
                        y,
                        time_ms,
                        injected,
                        touch_or_pen,
                    };
                    let before = machine.deferred_count();
                    let output = machine.handle(ev);
                    note_deferred(
                        &mut books,
                        before,
                        machine.deferred_count(),
                        output.deferred_len,
                    );
                    apply_output(&mut books, &ev, &output);
                    if output.command == Command::Stroke {
                        prop_assert!(machine.take_stroke().is_some());
                    }
                }
                Step::PollTimeout { dt } => {
                    time_ms = time_ms.wrapping_add(u32::from(dt));
                    let was_pending = machine.phase() == Phase::Pending;
                    let before = machine.deferred_count();
                    let output = machine.poll_timeout(time_ms, books.physical_down);
                    note_deferred(
                        &mut books,
                        before,
                        machine.deferred_count(),
                        output.deferred_len,
                    );
                    if was_pending
                        && machine.phase() == Phase::Idle
                        && output.command == Command::None
                        && !books.physical_down
                    {
                        books.open_swallows = 0;
                        books.target_down = false;
                    }
                    if output.command != Command::None {
                        apply_output(&mut books, &dummy_event(time_ms), &output);
                    }
                }
                Step::LostRelease => {
                    // The release never reaches the machine.
                    books.physical_down = false;
                }
                Step::Neutralize => {
                    let before = machine.deferred_count();
                    let action = machine.recover(RecoverKind::Watchdog, books.physical_down);
                    note_deferred(
                        &mut books,
                        before,
                        machine.deferred_count(),
                        action.deferred_len,
                    );
                    apply_neutralize(&mut books, action.action);
                }
            }
            reconcile(&mut machine, &mut books);
            check(&machine, &books);
        }

        time_ms = time_ms.wrapping_add(10_000);
        let was_pending = machine.phase() == Phase::Pending;
        let before = machine.deferred_count();
        let output = machine.poll_timeout(time_ms, books.physical_down);
        note_deferred(
            &mut books,
            before,
            machine.deferred_count(),
            output.deferred_len,
        );
        if was_pending
            && machine.phase() == Phase::Idle
            && output.command == Command::None
            && !books.physical_down
        {
            books.open_swallows = 0;
        }
        if output.command != Command::None {
            apply_output(&mut books, &dummy_event(time_ms), &output);
        }
        let before = machine.deferred_count();
        let inactive = machine.poll_inactivity(time_ms);
        note_deferred(
            &mut books,
            before,
            machine.deferred_count(),
            inactive.deferred_len,
        );
        apply_neutralize(&mut books, inactive.action);
        reconcile(&mut machine, &mut books);
        if machine.phase() != Phase::Idle {
            if books.physical_down {
                let ev = InputEvent {
                    kind: EventKind::Release,
                    button: Button::Right,
                    x,
                    y,
                    time_ms,
                    injected: false,
                    touch_or_pen: false,
                };
                let before = machine.deferred_count();
                let output = machine.handle(ev);
                note_deferred(
                    &mut books,
                    before,
                    machine.deferred_count(),
                    output.deferred_len,
                );
                apply_output(&mut books, &ev, &output);
            } else {
                let before = machine.deferred_count();
                let recovery = machine.recover(RecoverKind::Inactivity, false);
                note_deferred(
                    &mut books,
                    before,
                    machine.deferred_count(),
                    recovery.deferred_len,
                );
                apply_neutralize(&mut books, recovery.action);
                books.open_swallows = 0;
            }
        }
        reconcile(&mut machine, &mut books);
        let before = machine.deferred_count();
        let action = machine.recover(RecoverKind::Shutdown, books.physical_down);
        note_deferred(
            &mut books,
            before,
            machine.deferred_count(),
            action.deferred_len,
        );
        apply_neutralize(&mut books, action.action);
        if !books.physical_down {
            books.target_down = false;
            books.open_swallows = 0;
        }
        check(&machine, &books);
        prop_assert_eq!(machine.phase(), Phase::Idle);
        if !books.physical_down {
            prop_assert!(!machine.delivered_down());
            prop_assert!(!books.target_down);
        }
        prop_assert_eq!(books.open_swallows, 0);
    }
}

#[test]
fn oracle_sees_a_timeout_while_down_and_a_lost_release_while_up() {
    let mut held = Machine::new(Config::default());
    let mut books = Books {
        physical_down: true,
        ..Books::default()
    };
    let press = InputEvent {
        kind: EventKind::Press,
        button: Button::Right,
        x: 1,
        y: 2,
        time_ms: 0,
        injected: false,
        touch_or_pen: false,
    };
    let output = held.handle(press);
    apply_output(&mut books, &press, &output);
    let output = held.poll_timeout(500, books.physical_down);
    apply_output(&mut books, &press, &output);
    assert!(books.target_down);
    assert!(held.delivered_down());
    check(&held, &books);

    let mut lost = Machine::new(Config::default());
    let mut books = Books::default();
    let output = lost.handle(press);
    books.physical_down = true;
    apply_output(&mut books, &press, &output);
    let left = InputEvent {
        kind: EventKind::Press,
        button: Button::Left,
        x: 3,
        y: 4,
        time_ms: 10,
        injected: false,
        touch_or_pen: false,
    };
    let before = lost.deferred_count();
    let output = lost.handle(left);
    note_deferred(
        &mut books,
        before,
        lost.deferred_count(),
        output.deferred_len,
    );
    apply_output(&mut books, &left, &output);
    books.physical_down = false;
    let again = InputEvent {
        kind: EventKind::Press,
        button: Button::Right,
        x: 30,
        y: 30,
        time_ms: 100,
        injected: false,
        touch_or_pen: false,
    };
    books.physical_down = true;
    let before = lost.deferred_count();
    let output = lost.handle(again);
    note_deferred(
        &mut books,
        before,
        lost.deferred_count(),
        output.deferred_len,
    );
    apply_output(&mut books, &again, &output);
    books.physical_down = false;
    reconcile(&mut lost, &mut books);
    check(&lost, &books);
    assert!(
        !books.target_down,
        "a lost release must not leave the target holding the button"
    );
}
