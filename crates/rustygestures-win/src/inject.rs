//! `SendInput` helpers. Called outside the hook callback.

use rustygestures_core::{Button, EventKind, Replay, INJECTED_EXTRA_INFO};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, MOUSE_EVENT_FLAGS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    XBUTTON1, XBUTTON2,
};

pub(crate) fn button_from_u8(value: u8) -> Button {
    match value {
        0 => Button::Left,
        1 => Button::Right,
        2 => Button::Middle,
        3 => Button::X1,
        _ => Button::X2,
    }
}

pub(crate) fn inject_click(button: Button, x: i32, y: i32) {
    let (dx, dy) = absolute(x, y);
    let inputs = [
        mouse(dx, dy, down_flag(button), mouse_data(button)),
        mouse(dx, dy, up_flag(button), mouse_data(button)),
    ];
    send(&inputs);
}

pub(crate) fn inject_press(button: Button, x: i32, y: i32) {
    let (dx, dy) = absolute(x, y);
    let inputs = [mouse(dx, dy, down_flag(button), mouse_data(button))];
    send(&inputs);
}

pub(crate) fn inject_release(button: Button, x: i32, y: i32) {
    let (dx, dy) = absolute(x, y);
    let inputs = [mouse(dx, dy, up_flag(button), mouse_data(button))];
    send(&inputs);
}

/// Button-up at the cursor. No `MOUSEEVENTF_MOVE` or `ABSOLUTE`, so a recovery
/// release does not warp back to the press origin.
pub(crate) fn inject_release_in_place(button: Button) {
    let mut input = button_only(up_flag(button), mouse_data(button));
    input.Anonymous.mi.dwExtraInfo = INJECTED_EXTRA_INFO;
    send(&[input]);
}

/// Same in-place up, but not tagged, so the hook treats it as the user's release.
pub(crate) fn inject_plain_release_in_place(button: Button) {
    let mut input = button_only(up_flag(button), mouse_data(button));
    input.Anonymous.mi.dwExtraInfo = 0;
    send(&[input]);
}

pub(crate) fn inject_replay(replay: Replay) {
    let (dx, dy) = absolute(replay.x, replay.y);
    let (flags, data) = match replay.kind {
        EventKind::Move => (MOUSEEVENTF_MOVE, 0),
        EventKind::Press => (down_flag(replay.button), mouse_data(replay.button)),
        EventKind::Release => (up_flag(replay.button), mouse_data(replay.button)),
    };
    let inputs = [mouse(dx, dy, flags, data)];
    send(&inputs);
}

/// Input that is not tagged as ours, so the hook treats it as a real user event.
pub(crate) fn inject_plain(kind: EventKind, button: Button, x: i32, y: i32) {
    let (dx, dy) = absolute(x, y);
    let (flags, data) = match kind {
        EventKind::Move => (MOUSEEVENTF_MOVE, 0),
        EventKind::Press => (down_flag(button), mouse_data(button)),
        EventKind::Release => (up_flag(button), mouse_data(button)),
    };
    let mut input = mouse(dx, dy, flags, data);
    input.Anonymous.mi.dwExtraInfo = 0;
    send(&[input]);
}

fn send(inputs: &[INPUT]) {
    unsafe {
        SendInput(inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

fn mouse(dx: i32, dy: i32, flags: MOUSE_EVENT_FLAGS, mouse_data: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: mouse_data,
                dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK | flags,
                time: 0,
                dwExtraInfo: INJECTED_EXTRA_INFO,
            },
        },
    }
}

fn button_only(flags: MOUSE_EVENT_FLAGS, mouse_data: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: mouse_data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn down_flag(button: Button) -> MOUSE_EVENT_FLAGS {
    match button {
        Button::Left => MOUSEEVENTF_LEFTDOWN,
        Button::Right => MOUSEEVENTF_RIGHTDOWN,
        Button::Middle => MOUSEEVENTF_MIDDLEDOWN,
        Button::X1 | Button::X2 => MOUSEEVENTF_XDOWN,
    }
}

fn up_flag(button: Button) -> MOUSE_EVENT_FLAGS {
    match button {
        Button::Left => MOUSEEVENTF_LEFTUP,
        Button::Right => MOUSEEVENTF_RIGHTUP,
        Button::Middle => MOUSEEVENTF_MIDDLEUP,
        Button::X1 | Button::X2 => MOUSEEVENTF_XUP,
    }
}

fn mouse_data(button: Button) -> u32 {
    match button {
        Button::X1 => u32::from(XBUTTON1),
        Button::X2 => u32::from(XBUTTON2),
        _ => 0,
    }
}

/// Map a screen pixel into the 0..65535 virtual-desktop range `SendInput` uses.
fn absolute(x: i32, y: i32) -> (i32, i32) {
    let origin_x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let origin_y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    (norm(x, origin_x, width), norm(y, origin_y, height))
}

fn norm(value: i32, origin: i32, span: i32) -> i32 {
    if span <= 1 {
        return 0;
    }
    let relative = i64::from(value) - i64::from(origin);
    let denom = i64::from(span - 1);
    // Round to nearest so the mapped pixel is not systematically one short.
    let numer = relative * 65535;
    let rounded = if numer >= 0 {
        (numer + denom / 2) / denom
    } else {
        (numer - denom / 2) / denom
    };
    rounded.clamp(0, 65535) as i32
}
