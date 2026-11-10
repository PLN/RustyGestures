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
    target_down: bool,
}

fn apply_output(books: &mut Books, ev: &InputEvent, output: &Output) {
    let primary = Button::Right;
    let deferred = output.replay.is_some();
    let real_swallow = output.decision == Decision::Swallow && !deferred;
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
        // The release reached the target, so a delivered press is paired.
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
    assert_eq!(
        books.target_down,
        machine.delivered_down(),
        "phase {:?} open {} target {} delivered {}",
        machine.phase(),
        books.open_swallows,
        books.target_down,
        machine.delivered_down()
    );
    assert!(!(books.target_down && books.open_swallows > 0));
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
                    let output = machine.handle(ev);
                    apply_output(&mut books, &ev, &output);
                    if output.command == Command::Stroke {
                        prop_assert!(machine.take_stroke().is_some());
                    }
                }
                Step::PollTimeout { dt } => {
                    time_ms = time_ms.wrapping_add(u32::from(dt));
                    let output = machine.poll_timeout(time_ms, true);
                    if output.command != Command::None {
                        apply_output(&mut books, &dummy_event(time_ms), &output);
                    }
                }
                Step::LostRelease => {
                    let action = match machine.phase() {
                        Phase::Capturing => machine.recover(RecoverKind::Inactivity, false),
                        Phase::Pending => machine.recover(RecoverKind::Watchdog, false),
                        Phase::Idle if machine.delivered_down() => {
                            machine.recover(RecoverKind::Session, false)
                        }
                        Phase::Idle => Neutralize::None,
                    };
                    apply_neutralize(&mut books, action);
                }
                Step::Neutralize => {
                    let action = machine.recover(RecoverKind::Watchdog, true);
                    apply_neutralize(&mut books, action);
                }
            }
            check(&machine, &books);
        }

        time_ms = time_ms.wrapping_add(10_000);
        let output = machine.poll_timeout(time_ms, true);
        if output.command != Command::None {
            apply_output(&mut books, &dummy_event(time_ms), &output);
        }
        let inactive = machine.poll_inactivity(time_ms);
        apply_neutralize(&mut books, inactive);
        if machine.phase() != Phase::Idle {
            let ev = InputEvent {
                kind: EventKind::Release,
                button: Button::Right,
                x,
                y,
                time_ms,
                injected: false,
                touch_or_pen: false,
            };
            let output = machine.handle(ev);
            apply_output(&mut books, &ev, &output);
        }
        if machine.delivered_down() {
            let ev = InputEvent {
                kind: EventKind::Release,
                button: Button::Right,
                x,
                y,
                time_ms,
                injected: false,
                touch_or_pen: false,
            };
            let output = machine.handle(ev);
            apply_output(&mut books, &ev, &output);
        }
        let action = machine.recover(RecoverKind::Shutdown, false);
        apply_neutralize(&mut books, action);
        check(&machine, &books);
        prop_assert_eq!(machine.phase(), Phase::Idle);
        prop_assert!(!machine.delivered_down());
        prop_assert_eq!(books.open_swallows, 0);
    }
}
