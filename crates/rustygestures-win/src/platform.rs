//! Hook thread, stall watchdog, and the engine loop that owns the overlay.
//!
//! The low-level hook callback only enqueues a decision. `PeekMessage` runs
//! that callback as a sent message and then returns false, so the queue is
//! drained after every pump whether or not `PeekMessage` returned a posted
//! message.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU64, AtomicU8, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use rustygestures_core::{
    is_injected_extra, is_touch_or_pen_extra, Button, CapturePump, Command, Config, Decision,
    EventKind, InputEvent, LatencyHistogram, LiveTrail, Neutralize, Output, Phase, Point,
    RecoverKind, SpscRing, Stroke, MAX_STROKE_POINTS, WATCHDOG_STALL_MS,
};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Console::{
    SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT, CTRL_LOGOFF_EVENT,
    CTRL_SHUTDOWN_EVENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    MsgWaitForMultipleObjects, PeekMessageW, RegisterClassExW, SetWindowsHookExW, TranslateMessage,
    UnhookWindowsHookEx, HHOOK, HMENU, MSG, MSLLHOOKSTRUCT, PM_REMOVE, QS_ALLINPUT, WH_MOUSE_LL,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WM_WTSSESSION_CHANGE, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSEXW,
    WTS_CONSOLE_DISCONNECT, WTS_REMOTE_DISCONNECT, WTS_SESSION_LOCK, WTS_SESSION_LOGOFF, XBUTTON2,
};

use crate::inject::{
    button_from_u8, button_is_down, inject_click, inject_press, inject_release, inject_replay,
};
use crate::overlay::Overlay;
use crate::{Control, RunOptions, RunStats, SessionError};

const HOOK_PENDING: u8 = 0;
const HOOK_READY: u8 = 1;
const HOOK_FAILED: u8 = 2;

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
    stop: AtomicBool,
    session_change: AtomicBool,
    dropped: AtomicU64,
    hook_status: AtomicU8,
    hook_error: Mutex<String>,
    recovery_gen: AtomicU64,
    adopt_delivered: AtomicBool,
    adopt_swallow: AtomicBool,
    qpc_freq: u64,
    debug_stall_ms: u32,
    stop_event: isize,
    done_event: isize,
    ready_event: isize,
}

static LOCAL: AtomicPtr<HookLocal> = AtomicPtr::new(std::ptr::null_mut());
static SHARED_PTR: AtomicPtr<Shared> = AtomicPtr::new(std::ptr::null_mut());

struct HookLocal {
    shared: &'static Shared,
    pump: CapturePump,
    hook: HHOOK,
    seen_recovery: u64,
    stall_used: bool,
}

pub(crate) fn run<F>(options: RunOptions, mut on_stroke: F) -> Result<RunStats, SessionError>
where
    F: FnMut(&Stroke) -> Control,
{
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let freq = qpc_frequency().map_err(|err| SessionError(err.to_string()))?;
    let stop_event = create_event().map_err(|err| SessionError(err.to_string()))?;
    let done_event = create_event().map_err(|err| SessionError(err.to_string()))?;
    let ready_event = create_event().map_err(|err| SessionError(err.to_string()))?;
    let shared: &'static Shared = Box::leak(Box::new(Shared {
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
        stop: AtomicBool::new(false),
        session_change: AtomicBool::new(false),
        dropped: AtomicU64::new(0),
        hook_status: AtomicU8::new(HOOK_PENDING),
        hook_error: Mutex::new(String::new()),
        recovery_gen: AtomicU64::new(0),
        adopt_delivered: AtomicBool::new(false),
        adopt_swallow: AtomicBool::new(false),
        qpc_freq: freq,
        debug_stall_ms: options.debug_stall_ms,
        stop_event: stop_event.0 as isize,
        done_event: done_event.0 as isize,
        ready_event: ready_event.0 as isize,
    }));
    SHARED_PTR.store(std::ptr::from_ref(shared) as *mut Shared, Ordering::Release);
    unsafe {
        SetConsoleCtrlHandler(Some(ctrl_handler), true)
            .map_err(|err| SessionError(err.to_string()))?;
    }

    let hook_thread = thread::Builder::new()
        .name("rustygestures-hook".to_owned())
        .spawn(move || hook_main(shared))
        .map_err(|err| SessionError(err.to_string()))?;
    unsafe {
        WaitForSingleObject(HANDLE(shared.ready_event as *mut _), 5000);
    }
    if shared.hook_status.load(Ordering::Acquire) != HOOK_READY {
        let message = shared
            .hook_error
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_else(|_| "SetWindowsHookExW failed".to_owned());
        shared.stop.store(true, Ordering::Release);
        signal(shared.stop_event);
        let _ = hook_thread.join();
        shutdown_handles(shared);
        return Err(SessionError(message));
    }
    println!("hook installed (right button, 10 px, 500 ms cancel). Ctrl+C to exit.");

    let watchdog = thread::Builder::new()
        .name("rustygestures-watchdog".to_owned())
        .spawn(move || watchdog_main(shared))
        .map_err(|err| SessionError(err.to_string()))?;

    let fuzz = if options.fuzz_seconds > 0 {
        println!(
            "fuzzing for {}s with random SendInput. Ctrl+C stops early.",
            options.fuzz_seconds
        );
        let seconds = options.fuzz_seconds;
        Some(
            thread::Builder::new()
                .name("rustygestures-fuzz".to_owned())
                .spawn(move || fuzz_sender(shared, seconds))
                .map_err(|err| SessionError(err.to_string()))?,
        )
    } else {
        None
    };

    let mut session = install_session_window();
    let mut overlay = Overlay::new().map_err(|err| SessionError(err.to_string()))?;
    let trail_active = overlay.trail_active();
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
        wait_for_stop_or_message(shared, wait);
    }

    shared.stop.store(true, Ordering::Release);
    signal(shared.stop_event);
    let _ = hook_thread.join();
    let _ = watchdog.join();
    if let Some(fuzz) = fuzz {
        let _ = fuzz.join();
    }
    session.close();
    unsafe {
        let _ = SetConsoleCtrlHandler(Some(ctrl_handler), false);
    }
    let stats = RunStats {
        latency: shared.histogram.summary(),
        dropped_strokes: shared.dropped.load(Ordering::Relaxed),
        trail_active,
    };
    shutdown_handles(shared);
    let _ = overlay;
    Ok(stats)
}

fn hook_main(shared: &'static Shared) {
    let module = match unsafe { GetModuleHandleW(None) } {
        Ok(module) => module,
        Err(err) => {
            if let Ok(mut slot) = shared.hook_error.lock() {
                *slot = format!("GetModuleHandleW failed: {err}");
            }
            shared.hook_status.store(HOOK_FAILED, Ordering::Release);
            signal(shared.ready_event);
            signal(shared.done_event);
            return;
        }
    };
    let local = Box::new(HookLocal {
        shared,
        pump: CapturePump::new(Config::default()),
        hook: HHOOK::default(),
        seen_recovery: 0,
        stall_used: false,
    });
    let ptr = Box::into_raw(local);
    LOCAL.store(ptr, Ordering::Release);
    let installed =
        unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), Some(HINSTANCE(module.0)), 0) };
    match installed {
        Ok(hook) => unsafe { (*ptr).hook = hook },
        Err(err) => {
            eprintln!("rustygestures: SetWindowsHookExW failed: {err}");
            if let Ok(mut slot) = shared.hook_error.lock() {
                *slot = format!("SetWindowsHookExW failed: {err}");
            }
            shared.hook_status.store(HOOK_FAILED, Ordering::Release);
            LOCAL.store(std::ptr::null_mut(), Ordering::Release);
            drop(unsafe { Box::from_raw(ptr) });
            signal(shared.ready_event);
            signal(shared.done_event);
            return;
        }
    }
    shared.hook_status.store(HOOK_READY, Ordering::Release);
    shared.heartbeat.store(qpc(), Ordering::Release);
    signal(shared.ready_event);

    while !shared.stop.load(Ordering::Acquire) {
        adopt_external_recovery(ptr);
        if shared.session_change.swap(false, Ordering::AcqRel) {
            let action = {
                let local = unsafe { &mut *ptr };
                let down = button_is_down(Button::Right);
                local.pump.machine_mut().recover(RecoverKind::Session, down)
            };
            apply_neutralize_ptr(ptr, action);
            publish(unsafe { &mut *ptr });
        }
        // The callback runs inside PeekMessage. Drain afterwards, even when
        // PeekMessage returns false because nothing was posted.
        let mut message = MSG::default();
        while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        drain(ptr);
        let (overflow_action, inactive, timeout, stall) = {
            let local = unsafe { &mut *ptr };
            let overflow = local.pump.take_overflow();
            let now = tick_ms();
            let down = button_is_down(Button::Right);
            let overflow_action = if overflow {
                local
                    .pump
                    .machine_mut()
                    .recover(RecoverKind::Overflow, down)
            } else {
                Neutralize::None
            };
            let inactive = local.pump.poll_inactivity(now);
            let timeout = local.pump.poll_timeout(now, down);
            let stall = local.shared.debug_stall_ms > 0
                && !local.stall_used
                && local.pump.machine().phase() == Phase::Capturing;
            if stall {
                local.stall_used = true;
            }
            (overflow_action, inactive, timeout, stall)
        };
        apply_neutralize_ptr(ptr, overflow_action);
        apply_neutralize_ptr(ptr, inactive);
        apply_output_ptr(ptr, timeout);
        {
            let local = unsafe { &mut *ptr };
            publish(local);
        }
        if stall {
            thread::sleep(Duration::from_millis(u64::from(shared.debug_stall_ms)));
        }
        shared.heartbeat.store(qpc(), Ordering::Release);
        let wait = {
            let local = unsafe { &mut *ptr };
            local
                .pump
                .machine()
                .ms_until_timeout(tick_ms())
                .map(|left| left.clamp(1, 50))
                .unwrap_or(50)
        };
        wait_for_stop_or_message(shared, wait);
    }

    {
        let action = {
            let local = unsafe { &mut *ptr };
            let down = button_is_down(Button::Right);
            local
                .pump
                .machine_mut()
                .recover(RecoverKind::Shutdown, down)
        };
        apply_neutralize_ptr(ptr, action);
        let local = unsafe { &mut *ptr };
        publish(local);
        unsafe {
            let _ = UnhookWindowsHookEx(local.hook);
        }
    }
    LOCAL.store(std::ptr::null_mut(), Ordering::Release);
    drop(unsafe { Box::from_raw(ptr) });
    signal(shared.done_event);
}

fn adopt_external_recovery(ptr: *mut HookLocal) {
    let local = unsafe { &mut *ptr };
    let generation = local.shared.recovery_gen.load(Ordering::Acquire);
    if generation == local.seen_recovery {
        return;
    }
    local.seen_recovery = generation;
    let delivered = local.shared.adopt_delivered.load(Ordering::Acquire);
    let swallow = local.shared.adopt_swallow.load(Ordering::Acquire);
    local.pump.machine_mut().adopt_recovery(delivered, swallow);
    local.pump.clear_queue();
    publish(local);
}

fn drain(ptr: *mut HookLocal) {
    loop {
        let output = {
            let local = unsafe { &mut *ptr };
            local.pump.pop()
        };
        let Some(output) = output else {
            break;
        };
        // SendInput re-enters the callback. Do not hold a reference to the
        // hook state across that call.
        apply_output_ptr(ptr, output);
        let local = unsafe { &mut *ptr };
        publish(local);
    }
}

fn apply_output_ptr(ptr: *mut HookLocal, output: Output) {
    let shared = unsafe { (*ptr).shared };
    let deferred_inside_click =
        matches!(output.command, Command::ReinjectClick { .. }) && output.deferred_len > 0;
    match output.command {
        Command::None => {}
        Command::ReinjectClick { button, x, y } if deferred_inside_click => {
            with_inject_lock(shared, || {
                inject_press(button, x, y);
                for replay in output.deferred.iter().take(output.deferred_len as usize) {
                    inject_replay(*replay);
                }
                inject_release(button, x, y);
            });
        }
        Command::ReinjectClick { button, x, y } => {
            with_inject_lock(shared, || inject_click(button, x, y));
        }
        Command::ReinjectPress { button, x, y } => {
            with_inject_lock(shared, || inject_press(button, x, y));
        }
        Command::ReinjectRelease { button, x, y } => {
            with_inject_lock(shared, || inject_release(button, x, y));
        }
        Command::Stroke => {
            let local = unsafe { &mut *ptr };
            push_stroke(local);
        }
    }
    if !deferred_inside_click {
        for replay in output.deferred.iter().take(output.deferred_len as usize) {
            let replay = *replay;
            with_inject_lock(shared, || inject_replay(replay));
        }
    }
    if let Some(replay) = output.replay {
        with_inject_lock(shared, || inject_replay(replay));
    }
}

fn apply_neutralize_ptr(ptr: *mut HookLocal, action: Neutralize) {
    let shared = unsafe { (*ptr).shared };
    match action {
        Neutralize::None | Neutralize::Drop => {}
        Neutralize::DeliverPress { button, x, y } => {
            with_inject_lock(shared, || inject_press(button, x, y));
        }
        Neutralize::DeliverRelease { button, x, y } => {
            with_inject_lock(shared, || inject_release(button, x, y));
        }
        Neutralize::SyntheticClick { button, x, y } => {
            with_inject_lock(shared, || inject_click(button, x, y));
        }
    }
}

fn with_inject_lock(shared: &Shared, inject: impl FnOnce()) {
    while shared
        .injecting
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        std::hint::spin_loop();
    }
    shared.heartbeat.store(qpc(), Ordering::Release);
    inject();
    shared.heartbeat.store(qpc(), Ordering::Release);
    shared.injecting.store(false, Ordering::Release);
}

fn publish(local: &HookLocal) {
    let phase = local.pump.machine().phase();
    local.shared.phase.store(phase as u8, Ordering::Release);
    if phase == Phase::Capturing {
        local
            .shared
            .trail
            .publish(local.pump.machine().current_points());
    } else {
        local.shared.trail.publish(&[]);
    }
    if let Some((button, x, y)) = local.pump.machine().held_point() {
        local.shared.button.store(button as u8, Ordering::Release);
        local.shared.x.store(x, Ordering::Release);
        local.shared.y.store(y, Ordering::Release);
    }
    local
        .shared
        .delivered
        .store(local.pump.machine().delivered_down(), Ordering::Release);
}

fn push_stroke(local: &mut HookLocal) {
    let Some(stroke) = local.pump.machine_mut().take_stroke() else {
        return;
    };
    if !local.shared.ring.push(&stroke) {
        local.shared.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let started = qpc();
    let local_ptr = LOCAL.load(Ordering::Acquire);
    if local_ptr.is_null() {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    let (swallow, hook) = {
        let local = &mut *local_ptr;
        if code < 0 {
            (false, local.hook)
        } else {
            (queue_event(local, wparam, lparam), local.hook)
        }
    };
    let elapsed = qpc().saturating_sub(started);
    {
        let local = &mut *local_ptr;
        local
            .shared
            .histogram
            .record_ns(ticks_to_ns(elapsed, local.shared.qpc_freq));
        local.shared.heartbeat.store(qpc(), Ordering::Release);
    }
    if code < 0 || !swallow {
        CallNextHookEx(Some(hook), code, wparam, lparam)
    } else {
        LRESULT(1)
    }
}

fn queue_event(local: &mut HookLocal, wparam: WPARAM, lparam: LPARAM) -> bool {
    let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
    let Some(event) = decode(wparam.0 as u32, info) else {
        return false;
    };
    local.pump.on_hook(event) == Decision::Swallow
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

fn watchdog_main(shared: &'static Shared) {
    while !shared.stop.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(50));
        if shared.stop.load(Ordering::Acquire) {
            break;
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
        let button = button_from_u8(shared.button.load(Ordering::Acquire));
        let x = shared.x.load(Ordering::Acquire);
        let y = shared.y.load(Ordering::Acquire);
        let down = button_is_down(button);
        with_inject_lock(shared, || {
            let (delivered_after, swallow, action) = match phase {
                value if value == Phase::Capturing as u8 => (false, true, Neutralize::Drop),
                value if value == Phase::Pending as u8 && down => {
                    (true, false, Neutralize::DeliverPress { button, x, y })
                }
                value if value == Phase::Pending as u8 => (false, true, Neutralize::Drop),
                _ if delivered && !down => {
                    (false, true, Neutralize::DeliverRelease { button, x, y })
                }
                _ => (delivered, false, Neutralize::None),
            };
            match action {
                Neutralize::DeliverPress { button, x, y } => inject_press(button, x, y),
                Neutralize::DeliverRelease { button, x, y } => inject_release(button, x, y),
                Neutralize::SyntheticClick { button, x, y } => inject_click(button, x, y),
                Neutralize::None | Neutralize::Drop => {}
            }
            shared
                .adopt_delivered
                .store(delivered_after, Ordering::Release);
            shared.adopt_swallow.store(swallow, Ordering::Release);
            shared.phase.store(Phase::Idle as u8, Ordering::Release);
            shared.delivered.store(delivered_after, Ordering::Release);
            shared.recovery_gen.fetch_add(1, Ordering::Release);
        });
    }
}

fn fuzz_sender(shared: &'static Shared, seconds: u64) {
    let start = Instant::now();
    let mut state = 0x00c0_ffee_u64;
    let mut x = 200i32;
    let mut y = 200i32;
    let mut down = false;
    while !shared.stop.load(Ordering::Acquire) && start.elapsed().as_secs() < seconds {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        let roll = (state >> 33) % 6;
        match roll {
            0 if !down => {
                crate::inject::inject_plain(EventKind::Press, Button::Right, x, y);
                down = true;
            }
            1 if down => {
                crate::inject::inject_plain(EventKind::Release, Button::Right, x, y);
                down = false;
            }
            _ => {
                x = (x + ((state >> 8) as i32 % 31) - 15).clamp(0, 2000);
                y = (y + ((state >> 16) as i32 % 31) - 15).clamp(0, 1200);
                crate::inject::inject_plain(EventKind::Move, Button::Right, x, y);
            }
        }
        thread::sleep(Duration::from_millis(8));
    }
    if down {
        crate::inject::inject_plain(EventKind::Release, Button::Right, x, y);
    }
    shared.stop.store(true, Ordering::Release);
    signal(shared.stop_event);
}

struct SessionWindow {
    hwnd: HWND,
}

impl SessionWindow {
    fn close(&mut self) {
        if !self.hwnd.0.is_null() {
            unsafe {
                let _ = WTSUnRegisterSessionNotification(self.hwnd);
                let _ = DestroyWindow(self.hwnd);
            }
            self.hwnd = HWND::default();
        }
    }
}

impl Drop for SessionWindow {
    fn drop(&mut self) {
        self.close();
    }
}

fn install_session_window() -> SessionWindow {
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(session_proc),
        hInstance: instance_handle(),
        lpszClassName: w!("RustyGesturesSession"),
        ..Default::default()
    };
    let atom = unsafe { RegisterClassExW(&class) };
    if atom == 0 {
        return SessionWindow {
            hwnd: HWND::default(),
        };
    }
    let parent = HWND(-3isize as *mut std::ffi::c_void);
    let hwnd = unsafe {
        CreateWindowExW(
            Default::default(),
            w!("RustyGesturesSession"),
            w!("RustyGestures"),
            Default::default(),
            0,
            0,
            0,
            0,
            Some(parent),
            None::<HMENU>,
            Some(instance_handle()),
            None,
        )
    };
    let Ok(hwnd) = hwnd else {
        return SessionWindow {
            hwnd: HWND::default(),
        };
    };
    if unsafe { WTSRegisterSessionNotification(hwnd, 0) }.is_err() {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        return SessionWindow {
            hwnd: HWND::default(),
        };
    }
    SessionWindow { hwnd }
}

fn instance_handle() -> HINSTANCE {
    match unsafe { GetModuleHandleW(None) } {
        Ok(module) => HINSTANCE(module.0),
        Err(_) => HINSTANCE::default(),
    }
}

unsafe extern "system" fn session_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_WTSSESSION_CHANGE {
        let kind = wparam.0 as u32;
        if matches!(
            kind,
            WTS_SESSION_LOCK | WTS_SESSION_LOGOFF | WTS_CONSOLE_DISCONNECT | WTS_REMOTE_DISCONNECT
        ) {
            let ptr = SHARED_PTR.load(Ordering::Acquire);
            if !ptr.is_null() {
                (*ptr).session_change.store(true, Ordering::Release);
                signal((*ptr).stop_event);
            }
        }
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
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
    if event == 0 {
        return;
    }
    unsafe {
        let _ = SetEvent(HANDLE(event as *mut _));
    }
}

fn shutdown_handles(shared: &Shared) {
    unsafe {
        let _ = CloseHandle(HANDLE(shared.stop_event as *mut _));
        let _ = CloseHandle(HANDLE(shared.done_event as *mut _));
        let _ = CloseHandle(HANDLE(shared.ready_event as *mut _));
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
