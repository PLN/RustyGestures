//! Hook thread, stall watchdog, and the engine loop that owns the overlay.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rustygestures_core::{
    is_injected_extra, is_touch_or_pen_extra, Button, Command, Config, Decision, EventKind,
    InputEvent, LatencyHistogram, LiveTrail, Machine, Output, Phase, Point, SpscRing, Stroke,
    MAX_STROKE_POINTS, WATCHDOG_STALL_MS,
};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HANDLE, HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Console::{
    SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT, CTRL_LOGOFF_EVENT,
    CTRL_SHUTDOWN_EVENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, MsgWaitForMultipleObjects, PeekMessageW, SetWindowsHookExW,
    TranslateMessage, UnhookWindowsHookEx, HHOOK, MSG, MSLLHOOKSTRUCT, PM_REMOVE, QS_ALLINPUT,
    WH_MOUSE_LL, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_XBUTTONDOWN, WM_XBUTTONUP, XBUTTON2,
};

use crate::inject::{button_from_u8, inject_click, inject_press, inject_release, inject_replay};
use crate::overlay::Overlay;
use crate::{Control, RunStats, SessionError};

struct Shared {
    histogram: LatencyHistogram,
    ring: SpscRing,
    trail: LiveTrail,
    phase: AtomicU8,
    delivered: AtomicBool,
    button: AtomicU8,
    x: AtomicI32,
    y: AtomicI32,
    heartbeat: AtomicU64,
    injecting: AtomicBool,
    neutralize: AtomicBool,
    stop: AtomicBool,
    dropped: AtomicU64,
    qpc_freq: u64,
    stop_event: isize,
    done_event: isize,
}

static LOCAL: AtomicPtr<HookLocal> = AtomicPtr::new(std::ptr::null_mut());
static SHARED_PTR: AtomicPtr<Shared> = AtomicPtr::new(std::ptr::null_mut());

struct HookLocal {
    shared: Arc<Shared>,
    machine: Machine,
    hook: HHOOK,
    queued: Option<Output>,
}

pub(crate) fn run<F>(mut on_stroke: F) -> Result<RunStats, SessionError>
where
    F: FnMut(&Stroke) -> Control,
{
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let freq = qpc_frequency().map_err(|err| SessionError(err.to_string()))?;
    let stop_event = create_event().map_err(|err| SessionError(err.to_string()))?;
    let done_event = create_event().map_err(|err| SessionError(err.to_string()))?;
    let shared = Arc::new(Shared {
        histogram: LatencyHistogram::new(),
        ring: SpscRing::new(),
        trail: LiveTrail::new(),
        phase: AtomicU8::new(Phase::Idle as u8),
        delivered: AtomicBool::new(false),
        button: AtomicU8::new(Button::Right as u8),
        x: AtomicI32::new(0),
        y: AtomicI32::new(0),
        heartbeat: AtomicU64::new(qpc()),
        injecting: AtomicBool::new(false),
        neutralize: AtomicBool::new(false),
        stop: AtomicBool::new(false),
        dropped: AtomicU64::new(0),
        qpc_freq: freq,
        stop_event: stop_event.0 as isize,
        done_event: done_event.0 as isize,
    });
    SHARED_PTR.store(Arc::as_ptr(&shared) as *mut Shared, Ordering::Release);
    unsafe {
        SetConsoleCtrlHandler(Some(ctrl_handler), true)
            .map_err(|err| SessionError(err.to_string()))?;
    }

    let overlay = Overlay::new().map_err(|err| SessionError(err.to_string()))?;
    let trail_active = overlay.trail_active();

    let hook_shared = Arc::clone(&shared);
    let hook_thread = thread::Builder::new()
        .name("rustygestures-hook".to_owned())
        .spawn(move || hook_main(hook_shared))
        .map_err(|err| SessionError(err.to_string()))?;

    let watch_shared = Arc::clone(&shared);
    let watchdog = thread::Builder::new()
        .name("rustygestures-watchdog".to_owned())
        .spawn(move || watchdog_main(watch_shared))
        .map_err(|err| SessionError(err.to_string()))?;

    let mut overlay = overlay;
    let mut scratch = [Point { x: 0, y: 0 }; MAX_STROKE_POINTS];
    let mut flat = Vec::new();
    loop {
        if shared.stop.load(Ordering::Acquire) {
            break;
        }
        pump_messages();
        let mut stop_now = false;
        while let Some(stroke) = shared.ring.pop() {
            if on_stroke(&stroke) == Control::Stop {
                stop_now = true;
                break;
            }
        }
        if stop_now {
            shared.stop.store(true, Ordering::Release);
            signal(shared.stop_event);
            break;
        }
        let capturing = shared.phase.load(Ordering::Acquire) == Phase::Capturing as u8;
        let count = shared.trail.read(&mut scratch);
        flat.clear();
        flat.extend(scratch.iter().take(count).map(|point| (point.x, point.y)));
        overlay.sync(capturing, &flat);
        let wait = if capturing { 16 } else { 50 };
        wait_for_stop_or_message(&shared, wait);
    }

    shared.stop.store(true, Ordering::Release);
    signal(shared.stop_event);
    let _ = hook_thread.join();
    let _ = watchdog.join();
    unsafe {
        let _ = SetConsoleCtrlHandler(Some(ctrl_handler), false);
    }
    SHARED_PTR.store(std::ptr::null_mut(), Ordering::Release);
    drop(overlay);

    Ok(RunStats {
        latency: shared.histogram.summary(),
        dropped_strokes: shared.dropped.load(Ordering::Relaxed),
        trail_active,
    })
}

fn hook_main(shared: Arc<Shared>) {
    let module = match unsafe { GetModuleHandleW(None) } {
        Ok(module) => module,
        Err(_) => {
            signal(shared.done_event);
            return;
        }
    };
    let mut local = HookLocal {
        shared: Arc::clone(&shared),
        machine: Machine::new(Config::default()),
        hook: HHOOK::default(),
        queued: None,
    };
    LOCAL.store(&mut local, Ordering::Release);
    let installed =
        unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), Some(HINSTANCE(module.0)), 0) };
    match installed {
        Ok(hook) => local.hook = hook,
        Err(err) => {
            eprintln!("rustygestures: SetWindowsHookExW failed ({err})");
            LOCAL.store(std::ptr::null_mut(), Ordering::Release);
            signal(shared.done_event);
            return;
        }
    }
    shared.heartbeat.store(qpc(), Ordering::Release);

    while !shared.stop.load(Ordering::Acquire) {
        if shared.neutralize.swap(false, Ordering::AcqRel) {
            local.queued = None;
            local.machine.force_idle();
            publish(&local);
        }
        let mut message = MSG::default();
        while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            drain(&mut local);
        }
        let now = tick_ms();
        let command = local.machine.poll_timeout(now);
        if command != Command::None {
            apply_command(&mut local, command);
            publish(&local);
        }
        shared.heartbeat.store(qpc(), Ordering::Release);
        let wait = local
            .machine
            .ms_until_timeout(tick_ms())
            .map(|left| left.clamp(1, 50))
            .unwrap_or(50);
        wait_for_stop_or_message(&shared, wait);
    }

    if shared.neutralize.swap(false, Ordering::AcqRel) {
        local.machine.force_idle();
    } else {
        match local.machine.neutralize() {
            rustygestures_core::Neutralize::None => {}
            rustygestures_core::Neutralize::SyntheticClick { button, x, y } => {
                guarded(&shared, || inject_click(button, x, y));
            }
            rustygestures_core::Neutralize::SyntheticRelease { button, x, y } => {
                guarded(&shared, || inject_release(button, x, y));
            }
        }
    }
    publish(&local);
    unsafe {
        let _ = UnhookWindowsHookEx(local.hook);
    }
    LOCAL.store(std::ptr::null_mut(), Ordering::Release);
    signal(shared.done_event);
}

fn drain(local: &mut HookLocal) {
    while let Some(output) = local.queued.take() {
        apply_output(local, output);
        publish(local);
    }
}

fn apply_output(local: &mut HookLocal, output: Output) {
    apply_command(local, output.command);
    if let Some(replay) = output.replay {
        let shared = Arc::clone(&local.shared);
        guarded(&shared, || inject_replay(replay));
    }
}

fn apply_command(local: &mut HookLocal, command: Command) {
    let shared = Arc::clone(&local.shared);
    match command {
        Command::None => {}
        Command::ReinjectClick { button, x, y } => guarded(&shared, || inject_click(button, x, y)),
        Command::ReinjectPress { button, x, y } => guarded(&shared, || inject_press(button, x, y)),
        Command::ReinjectRelease { button, x, y } => {
            guarded(&shared, || inject_release(button, x, y));
        }
        Command::Stroke => push_stroke(local),
    }
}

fn guarded(shared: &Shared, inject: impl FnOnce()) {
    shared.injecting.store(true, Ordering::Release);
    shared.heartbeat.store(qpc(), Ordering::Release);
    inject();
    shared.heartbeat.store(qpc(), Ordering::Release);
    shared.injecting.store(false, Ordering::Release);
}

fn publish(local: &HookLocal) {
    let phase = local.machine.phase();
    local.shared.phase.store(phase as u8, Ordering::Release);
    if phase == Phase::Capturing {
        local.shared.trail.publish(local.machine.current_points());
    } else {
        local.shared.trail.publish(&[]);
    }
    if let Some((button, x, y)) = local.machine.held_point() {
        local.shared.button.store(button as u8, Ordering::Release);
        local.shared.x.store(x, Ordering::Release);
        local.shared.y.store(y, Ordering::Release);
    }
    local
        .shared
        .delivered
        .store(local.machine.delivered_down(), Ordering::Release);
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let started = qpc();
    let local_ptr = LOCAL.load(Ordering::Acquire);
    if local_ptr.is_null() {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    let local = &mut *local_ptr;
    let result = if code < 0 {
        CallNextHookEx(Some(local.hook), code, wparam, lparam)
    } else if local.shared.neutralize.swap(false, Ordering::AcqRel) {
        local.machine.force_idle();
        dispatch_event(local, code, wparam, lparam)
    } else {
        dispatch_event(local, code, wparam, lparam)
    };
    let elapsed = qpc().saturating_sub(started);
    local
        .shared
        .histogram
        .record_ns(ticks_to_ns(elapsed, local.shared.qpc_freq));
    local.shared.heartbeat.store(qpc(), Ordering::Release);
    result
}

fn dispatch_event(local: &mut HookLocal, code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
    let Some(event) = decode(wparam.0 as u32, info) else {
        return unsafe { CallNextHookEx(Some(local.hook), code, wparam, lparam) };
    };
    let output = local.machine.handle(event);
    let swallow = output.decision == Decision::Swallow;
    local.queued = Some(output);
    if swallow {
        LRESULT(1)
    } else {
        unsafe { CallNextHookEx(Some(local.hook), code, wparam, lparam) }
    }
}

fn decode(message: u32, info: &MSLLHOOKSTRUCT) -> Option<InputEvent> {
    let (button, kind) = match message {
        WM_LBUTTONDOWN => (Button::Left, EventKind::Press),
        WM_LBUTTONUP => (Button::Left, EventKind::Release),
        WM_RBUTTONDOWN => (Button::Right, EventKind::Press),
        WM_RBUTTONUP => (Button::Right, EventKind::Release),
        WM_MBUTTONDOWN => (Button::Middle, EventKind::Press),
        WM_MBUTTONUP => (Button::Middle, EventKind::Release),
        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let which = ((info.mouseData >> 16) & 0xffff) as u16;
            let button = if which == XBUTTON2 {
                Button::X2
            } else {
                Button::X1
            };
            let kind = if message == WM_XBUTTONDOWN {
                EventKind::Press
            } else {
                EventKind::Release
            };
            (button, kind)
        }
        WM_MOUSEMOVE => (Button::Left, EventKind::Move),
        _ => return None,
    };
    Some(InputEvent {
        kind,
        button,
        x: info.pt.x,
        y: info.pt.y,
        time_ms: info.time,
        injected: is_injected_extra(info.dwExtraInfo),
        touch_or_pen: is_touch_or_pen_extra(info.dwExtraInfo),
    })
}

fn watchdog_main(shared: Arc<Shared>) {
    while !shared.stop.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(50));
        if shared.stop.load(Ordering::Acquire) {
            break;
        }
        if shared.injecting.load(Ordering::Acquire) {
            continue;
        }
        let phase = shared.phase.load(Ordering::Acquire);
        let delivered = shared.delivered.load(Ordering::Acquire);
        if phase == Phase::Idle as u8 && !delivered {
            continue;
        }
        let age_ms = ticks_to_ms(
            qpc().saturating_sub(shared.heartbeat.load(Ordering::Acquire)),
            shared.qpc_freq,
        );
        if age_ms <= u64::from(WATCHDOG_STALL_MS) {
            continue;
        }
        if shared
            .injecting
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            continue;
        }
        let button = button_from_u8(shared.button.load(Ordering::Acquire));
        let x = shared.x.load(Ordering::Acquire);
        let y = shared.y.load(Ordering::Acquire);
        if phase != Phase::Idle as u8 {
            inject_click(button, x, y);
        } else if delivered {
            inject_release(button, x, y);
        }
        shared.phase.store(Phase::Idle as u8, Ordering::Release);
        shared.delivered.store(false, Ordering::Release);
        shared.neutralize.store(true, Ordering::Release);
        shared.injecting.store(false, Ordering::Release);
        shared.heartbeat.store(qpc(), Ordering::Release);
    }
}

unsafe extern "system" fn ctrl_handler(kind: u32) -> windows::core::BOOL {
    if matches!(
        kind,
        CTRL_C_EVENT
            | CTRL_BREAK_EVENT
            | CTRL_CLOSE_EVENT
            | CTRL_LOGOFF_EVENT
            | CTRL_SHUTDOWN_EVENT
    ) {
        let ptr = SHARED_PTR.load(Ordering::Acquire);
        if !ptr.is_null() {
            let shared = &*ptr;
            shared.stop.store(true, Ordering::Release);
            signal(shared.stop_event);
            unsafe {
                WaitForSingleObject(HANDLE(shared.done_event as *mut _), 2000);
            }
        }
        windows::core::BOOL(1)
    } else {
        windows::core::BOOL(0)
    }
}

fn pump_messages() {
    let mut message = MSG::default();
    while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

fn wait_for_stop_or_message(shared: &Shared, wait_ms: u32) {
    let handle = HANDLE(shared.stop_event as *mut _);
    unsafe {
        MsgWaitForMultipleObjects(Some(&[handle]), false, wait_ms, QS_ALLINPUT);
    }
}

fn signal(event: isize) {
    unsafe {
        let _ = SetEvent(HANDLE(event as *mut _));
    }
}

fn create_event() -> windows::core::Result<HANDLE> {
    unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
}

fn qpc() -> u64 {
    let mut ticks = 0i64;
    unsafe {
        let _ = QueryPerformanceCounter(&mut ticks);
    }
    ticks.max(0) as u64
}

fn qpc_frequency() -> windows::core::Result<u64> {
    let mut freq = 0i64;
    unsafe { QueryPerformanceFrequency(&mut freq)? };
    Ok(freq.max(1) as u64)
}

fn tick_ms() -> u32 {
    (unsafe { GetTickCount64() } & 0xffff_ffff) as u32
}

fn ticks_to_ns(ticks: u64, freq: u64) -> u64 {
    if freq == 0 {
        return 0;
    }
    ((ticks as u128) * 1_000_000_000 / u128::from(freq)) as u64
}

fn ticks_to_ms(ticks: u64, freq: u64) -> u64 {
    if freq == 0 {
        return 0;
    }
    (ticks as u128 * 1000 / u128::from(freq)) as u64
}

/// Push a finished stroke. Split out so the ring push can borrow the machine mutably.
fn push_stroke(local: &mut HookLocal) {
    let Some(stroke) = local.machine.take_stroke() else {
        return;
    };
    if !local.shared.ring.push(&stroke) {
        local.shared.dropped.fetch_add(1, Ordering::Relaxed);
    }
}
