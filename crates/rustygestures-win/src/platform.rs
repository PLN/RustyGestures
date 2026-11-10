//! Hook thread, stall watchdog, and the engine loop that owns the overlay.
//!
//! The low-level hook callback only enqueues a decision. `PeekMessage` runs
//! that callback as a sent message and then returns false, so the queue is
//! drained after `service_hook`'s peek returns, whether or not a posted
//! message was removed. `SendInput` can re-enter this thread, so no `&mut`
//! is held across it, and this thread never spins on the inject lock.

use std::cell::Cell;
use std::sync::atomic::{
    AtomicBool, AtomicI32, AtomicIsize, AtomicPtr, AtomicU64, AtomicU8, Ordering,
};
use std::sync::Mutex;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rustygestures_core::{
    is_injected_extra, is_touch_or_pen_extra, poll_hook, Button, CapturePump, Command, Config,
    Decision, EventKind, HookMessages, InputEvent, LatencyHistogram, LiveTrail, Neutralize, Output,
    Phase, Point, RecoverKind, Recovery, Replay, SpscRing, Stroke, DEFERRED_CAP, MAX_STROKE_POINTS,
    WATCHDOG_STALL_MS,
};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
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
use windows::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
    RIDEV_INPUTSINK, RID_INPUT, RIM_TYPEMOUSE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetSystemMetrics, GetWindowRect, MsgWaitForMultipleObjects, PeekMessageW, RegisterClassExW,
    SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WindowFromPoint, HHOOK, HMENU, MSG,
    MSLLHOOKSTRUCT, PM_REMOVE, QS_ALLINPUT, RI_MOUSE_RIGHT_BUTTON_DOWN, RI_MOUSE_RIGHT_BUTTON_UP,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, WH_MOUSE_LL,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_INPUT, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN,
    WM_MBUTTONUP, WM_MOUSEMOVE, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_WTSSESSION_CHANGE, WM_XBUTTONDOWN,
    WM_XBUTTONUP, WNDCLASSEXW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE, WTS_CONSOLE_DISCONNECT,
    WTS_REMOTE_DISCONNECT, WTS_SESSION_LOCK, WTS_SESSION_LOGOFF, XBUTTON2,
};

use crate::inject::{button_from_u8, inject_click, inject_press, inject_release, inject_replay};
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
    /// Right button, from non-injected hook events and raw input. Injected
    /// events (`dwExtraInfo`) do not change it.
    physical_down: AtomicBool,
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
    /// Auto-reset. Signalled on session lock or disconnect, never for shutdown.
    session_event: isize,
    fuzz_hwnd: AtomicIsize,
    fuzz_left: AtomicI32,
    fuzz_top: AtomicI32,
    fuzz_right: AtomicI32,
    fuzz_bottom: AtomicI32,
    fuzz_failed: AtomicBool,
}

thread_local! {
    static ACTIVE_PUMP: Cell<*mut CapturePump> = const { Cell::new(std::ptr::null_mut()) };
}

#[derive(Clone, Copy)]
enum PendingInject {
    Recovery(Recovery),
    Output(Output),
}

static LOCAL: AtomicPtr<HookLocal> = AtomicPtr::new(std::ptr::null_mut());
static SHARED_PTR: AtomicPtr<Shared> = AtomicPtr::new(std::ptr::null_mut());

struct HookLocal {
    shared: &'static Shared,
    pump: CapturePump,
    hook: HHOOK,
    seen_recovery: u64,
    stall_used: bool,
    pending: Option<PendingInject>,
}

struct SessionGuard {
    shared: &'static Shared,
    hook: Option<JoinHandle<()>>,
    watchdog: Option<JoinHandle<()>>,
    fuzz: Option<JoinHandle<()>>,
    fuzz_window: HWND,
    handler_installed: bool,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        signal(self.shared.stop_event);
        if let Some(thread) = self.hook.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.watchdog.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.fuzz.take() {
            let _ = thread.join();
        }
        if !self.fuzz_window.0.is_null() {
            unsafe {
                let _ = DestroyWindow(self.fuzz_window);
            }
        }
        if self.handler_installed {
            unsafe {
                let _ = SetConsoleCtrlHandler(Some(ctrl_handler), false);
            }
        }
        SHARED_PTR.store(std::ptr::null_mut(), Ordering::Release);
        shutdown_handles(self.shared);
    }
}

pub(crate) fn run<F>(options: RunOptions, mut on_stroke: F) -> Result<RunStats, SessionError>
where
    F: FnMut(&Stroke) -> Control,
{
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let freq = qpc_frequency().map_err(|err| SessionError(err.to_string()))?;
    let (stop_event, done_event, ready_event, session_event) = create_events()?;
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
        physical_down: AtomicBool::new(false),
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
        session_event: session_event.0 as isize,
        fuzz_hwnd: AtomicIsize::new(0),
        fuzz_left: AtomicI32::new(0),
        fuzz_top: AtomicI32::new(0),
        fuzz_right: AtomicI32::new(0),
        fuzz_bottom: AtomicI32::new(0),
        fuzz_failed: AtomicBool::new(false),
    }));
    let mut guard = SessionGuard {
        shared,
        hook: None,
        watchdog: None,
        fuzz: None,
        fuzz_window: HWND::default(),
        handler_installed: false,
    };
    SHARED_PTR.store(std::ptr::from_ref(shared) as *mut Shared, Ordering::Release);
    unsafe {
        SetConsoleCtrlHandler(Some(ctrl_handler), true)
            .map_err(|err| SessionError(err.to_string()))?;
    }
    guard.handler_installed = true;

    guard.hook = Some(
        thread::Builder::new()
            .name("rustygestures-hook".to_owned())
            .spawn(move || hook_main(shared))
            .map_err(|err| SessionError(err.to_string()))?,
    );
    unsafe {
        WaitForSingleObject(HANDLE(shared.ready_event as *mut _), 5000);
    }
    if shared.hook_status.load(Ordering::Acquire) != HOOK_READY {
        let message = shared
            .hook_error
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_else(|_| "SetWindowsHookExW failed".to_owned());
        return Err(SessionError(message));
    }
    println!("hook installed (right button, 10 px, 500 ms cancel). Ctrl+C to exit.");

    guard.watchdog = Some(
        thread::Builder::new()
            .name("rustygestures-watchdog".to_owned())
            .spawn(move || watchdog_main(shared))
            .map_err(|err| SessionError(err.to_string()))?,
    );

    if options.fuzz_seconds > 0 {
        println!(
            "fuzzing for {}s inside a tool-owned window. Run this in a VM. Ctrl+C stops early.",
            options.fuzz_seconds
        );
        let window = create_fuzz_window(shared)?;
        guard.fuzz_window = window;
        let seconds = options.fuzz_seconds;
        guard.fuzz = Some(
            thread::Builder::new()
                .name("rustygestures-fuzz".to_owned())
                .spawn(move || fuzz_sender(shared, seconds))
                .map_err(|err| SessionError(err.to_string()))?,
        );
    }

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
    if let Some(thread) = guard.hook.take() {
        let _ = thread.join();
    }
    if let Some(thread) = guard.watchdog.take() {
        let _ = thread.join();
    }
    if let Some(thread) = guard.fuzz.take() {
        let _ = thread.join();
    }
    session.close();
    let stats = RunStats {
        latency: shared.histogram.summary(),
        dropped_strokes: shared.dropped.load(Ordering::Relaxed),
        trail_active,
    };
    if options.fuzz_seconds > 0 {
        let idle = shared.phase.load(Ordering::Acquire) == Phase::Idle as u8;
        let delivered = shared.delivered.load(Ordering::Acquire);
        let physical = shared.physical_down.load(Ordering::Acquire);
        let failed = shared.fuzz_failed.load(Ordering::Acquire);
        if !idle || delivered || physical || failed {
            return Err(SessionError(format!(
                "fuzz ended with the engine not idle or the button down (idle={idle} delivered={delivered} physical={physical} confined={})",
                !failed
            )));
        }
    }
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
        pending: None,
    });
    let ptr = Box::into_raw(local);
    LOCAL.store(ptr, Ordering::Release);
    let installed =
        unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), Some(HINSTANCE(module.0)), 0) };
    match installed {
        Ok(hook) => unsafe { (*ptr).hook = hook },
        Err(err) => {
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
    let raw_window = install_raw_input();
    shared.hook_status.store(HOOK_READY, Ordering::Release);
    shared.heartbeat.store(qpc(), Ordering::Release);
    signal(shared.ready_event);

    while !shared.stop.load(Ordering::Acquire) {
        if !flush_pending(ptr) {
            let _ = peek_posted(ptr);
            continue;
        }
        adopt_local(unsafe { &mut *ptr });
        if !flush_pending(ptr) {
            let _ = peek_posted(ptr);
            continue;
        }
        if shared.session_change.swap(false, Ordering::AcqRel) {
            let recovery = {
                let local = unsafe { &mut *ptr };
                let down = physical_down(local.shared);
                local.pump.machine_mut().recover(RecoverKind::Session, down)
            };
            if !store_or_apply(ptr, PendingInject::Recovery(recovery)) {
                let _ = peek_posted(ptr);
                continue;
            }
            publish(unsafe { &mut *ptr });
        }
        {
            let outputs = {
                let local = unsafe { &mut *ptr };
                poll_hook(&mut WinSource, &mut local.pump)
            };
            for (index, output) in outputs.iter().copied().enumerate() {
                if try_apply_output(ptr, output) {
                    continue;
                }
                let local = unsafe { &mut *ptr };
                for pending in outputs.iter().copied().skip(index).rev() {
                    let _ = local.pump.push_front(pending);
                }
                break;
            }
        }
        if !flush_pending(ptr) {
            let _ = peek_posted(ptr);
            continue;
        }
        reconcile_release(ptr);
        adopt_local(unsafe { &mut *ptr });
        if !flush_pending(ptr) {
            let _ = peek_posted(ptr);
            continue;
        }
        let stall = {
            let local = unsafe { &mut *ptr };
            let down = physical_down(local.shared);
            let now = tick_ms();
            if local.pump.take_overflow() {
                let recovery = local
                    .pump
                    .machine_mut()
                    .recover(RecoverKind::Overflow, down);
                if !store_or_apply(ptr, PendingInject::Recovery(recovery)) {
                    continue;
                }
            }
            let inactive = {
                let local = unsafe { &mut *ptr };
                local.pump.poll_inactivity(now)
            };
            if !store_or_apply(ptr, PendingInject::Recovery(inactive)) {
                continue;
            }
            let timeout = {
                let local = unsafe { &mut *ptr };
                local.pump.poll_timeout(now, down)
            };
            if !store_or_apply(ptr, PendingInject::Output(timeout)) {
                continue;
            }
            reconcile_release(ptr);
            let local = unsafe { &mut *ptr };
            let stall = local.shared.debug_stall_ms > 0
                && !local.stall_used
                && local.pump.machine().phase() == Phase::Capturing;
            if stall {
                local.stall_used = true;
            }
            publish(local);
            stall
        };
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
        adopt_local(unsafe { &mut *ptr });
        let _ = flush_pending(ptr);
        let recovery = {
            let local = unsafe { &mut *ptr };
            let down = physical_down(local.shared);
            local
                .pump
                .machine_mut()
                .recover(RecoverKind::Shutdown, down)
        };
        unsafe {
            (*ptr).pending = Some(PendingInject::Recovery(recovery));
        }
        for _ in 0..20 {
            if flush_pending(ptr) {
                break;
            }
            let _ = peek_posted(ptr);
            thread::sleep(Duration::from_millis(1));
        }
        let local = unsafe { &mut *ptr };
        publish(local);
        unsafe {
            let _ = UnhookWindowsHookEx(local.hook);
        }
    }
    if let Some(hwnd) = raw_window {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
    LOCAL.store(std::ptr::null_mut(), Ordering::Release);
    drop(unsafe { Box::from_raw(ptr) });
    signal(shared.done_event);
}

struct WinSource;

impl HookMessages for WinSource {
    fn peek(&mut self, pump: &mut CapturePump) -> bool {
        ACTIVE_PUMP.with(|slot| slot.set(pump));
        let posted = peek_posted_messages();
        ACTIVE_PUMP.with(|slot| slot.set(std::ptr::null_mut()));
        posted
    }
}

fn peek_posted(_ptr: *mut HookLocal) -> bool {
    peek_posted_messages()
}

fn peek_posted_messages() -> bool {
    let mut message = MSG::default();
    let posted = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool();
    if posted {
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    posted
}

fn adopt_local(local: &mut HookLocal) {
    let generation = local.shared.recovery_gen.load(Ordering::Acquire);
    if generation == local.seen_recovery {
        return;
    }
    local.seen_recovery = generation;
    let delivered = local.shared.adopt_delivered.load(Ordering::Acquire);
    let swallow = local.shared.adopt_swallow.load(Ordering::Acquire);
    let recovery = local.pump.machine_mut().adopt_recovery(delivered, swallow);
    local.pump.clear_queue();
    stash_recovery(local, recovery);
    publish(local);
}

fn adopt_pump(local_ptr: *mut HookLocal, pump: *mut CapturePump) {
    let shared = unsafe { (*local_ptr).shared };
    let seen = unsafe { (*local_ptr).seen_recovery };
    let generation = shared.recovery_gen.load(Ordering::Acquire);
    if generation == seen {
        return;
    }
    unsafe {
        (*local_ptr).seen_recovery = generation;
    }
    let delivered = shared.adopt_delivered.load(Ordering::Acquire);
    let swallow = shared.adopt_swallow.load(Ordering::Acquire);
    let recovery = unsafe { (*pump).machine_mut().adopt_recovery(delivered, swallow) };
    unsafe {
        (*pump).clear_queue();
    }
    stash_recovery_ptr(local_ptr, recovery);
    publish_pump(shared, unsafe { &*pump });
}

fn stash_recovery(local: &mut HookLocal, recovery: Recovery) {
    if !recovery_needs_inject(&recovery) {
        return;
    }
    match local.pending {
        Some(PendingInject::Recovery(existing)) => {
            local.pending = Some(PendingInject::Recovery(merge_recovery(existing, recovery)));
        }
        Some(PendingInject::Output(_)) | None => {
            if local.pending.is_none() {
                local.pending = Some(PendingInject::Recovery(recovery));
            }
        }
    }
}

fn stash_recovery_ptr(local_ptr: *mut HookLocal, recovery: Recovery) {
    let local = unsafe { &mut *local_ptr };
    stash_recovery(local, recovery);
}

fn merge_recovery(mut existing: Recovery, extra: Recovery) -> Recovery {
    let space = DEFERRED_CAP.saturating_sub(existing.deferred_len as usize);
    let add = (extra.deferred_len as usize).min(space);
    for replay in extra.deferred.iter().take(add) {
        existing.deferred[existing.deferred_len as usize] = *replay;
        existing.deferred_len += 1;
    }
    if existing.action == Neutralize::None || existing.action == Neutralize::Drop {
        existing.action = extra.action;
    }
    existing
}

fn recovery_needs_inject(recovery: &Recovery) -> bool {
    recovery.deferred_len > 0
        || matches!(
            recovery.action,
            Neutralize::DeliverPress { .. }
                | Neutralize::DeliverRelease { .. }
                | Neutralize::SyntheticClick { .. }
        )
}

fn flush_pending(ptr: *mut HookLocal) -> bool {
    let pending = unsafe { (*ptr).pending.take() };
    let Some(pending) = pending else {
        return true;
    };
    if apply_pending(ptr, pending) {
        publish(unsafe { &mut *ptr });
        true
    } else {
        unsafe {
            (*ptr).pending = Some(pending);
        }
        false
    }
}

fn store_or_apply(ptr: *mut HookLocal, inject: PendingInject) -> bool {
    if !flush_pending(ptr) {
        unsafe {
            if (*ptr).pending.is_none() {
                (*ptr).pending = Some(inject);
            }
        }
        return false;
    }
    if apply_pending(ptr, inject) {
        let local = unsafe { &mut *ptr };
        publish(local);
        true
    } else {
        unsafe {
            (*ptr).pending = Some(inject);
        }
        false
    }
}

fn apply_pending(ptr: *mut HookLocal, pending: PendingInject) -> bool {
    match pending {
        PendingInject::Recovery(recovery) => try_apply_recovery(unsafe { (*ptr).shared }, recovery),
        PendingInject::Output(output) => try_apply_output(ptr, output),
    }
}

fn try_apply_output(ptr: *mut HookLocal, output: Output) -> bool {
    let shared = unsafe { (*ptr).shared };
    if output_needs_inject(&output) && !try_lock(shared) {
        return false;
    }
    // SendInput can re-enter this thread. The lock is held, but nothing
    // borrows HookLocal across the call, and a failed lock does not spin.
    send_output(&output);
    if output_needs_inject(&output) {
        unlock(shared);
    }
    if output.command == Command::Stroke {
        let local = unsafe { &mut *ptr };
        push_stroke(local);
    }
    true
}

fn output_needs_inject(output: &Output) -> bool {
    output.deferred_len > 0
        || output.replay.is_some()
        || matches!(
            output.command,
            Command::ReinjectClick { .. }
                | Command::ReinjectPress { .. }
                | Command::ReinjectRelease { .. }
        )
}

fn send_output(output: &Output) {
    let deferred_inside_click =
        matches!(output.command, Command::ReinjectClick { .. }) && output.deferred_len > 0;
    match output.command {
        Command::None | Command::Stroke => {}
        Command::ReinjectClick { button, x, y } if deferred_inside_click => {
            inject_press(button, x, y);
            for replay in output.deferred.iter().take(output.deferred_len as usize) {
                inject_replay(*replay);
            }
            inject_release(button, x, y);
        }
        Command::ReinjectClick { button, x, y } => inject_click(button, x, y),
        Command::ReinjectPress { button, x, y } => inject_press(button, x, y),
        Command::ReinjectRelease { button, x, y } => inject_release(button, x, y),
    }
    if !deferred_inside_click {
        for replay in output.deferred.iter().take(output.deferred_len as usize) {
            inject_replay(*replay);
        }
    }
    if let Some(replay) = output.replay {
        inject_replay(replay);
    }
}

fn try_apply_recovery(shared: &Shared, recovery: Recovery) -> bool {
    if !recovery_needs_inject(&recovery) {
        return true;
    }
    if !try_lock(shared) {
        return false;
    }
    send_recovery(&recovery);
    unlock(shared);
    true
}

fn send_recovery(recovery: &Recovery) {
    match recovery.action {
        Neutralize::None | Neutralize::Drop => {
            replay_deferred(recovery.deferred, recovery.deferred_len);
        }
        Neutralize::DeliverPress { button, x, y } => {
            inject_press(button, x, y);
            replay_deferred(recovery.deferred, recovery.deferred_len);
        }
        Neutralize::DeliverRelease { button, x, y } => {
            inject_release(button, x, y);
            replay_deferred(recovery.deferred, recovery.deferred_len);
        }
        Neutralize::SyntheticClick { button, x, y } => {
            inject_press(button, x, y);
            replay_deferred(recovery.deferred, recovery.deferred_len);
            inject_release(button, x, y);
        }
    }
}

fn replay_deferred(deferred: [Replay; DEFERRED_CAP], len: u8) {
    for replay in deferred.iter().take(len as usize) {
        inject_replay(*replay);
    }
}

fn reconcile_release(ptr: *mut HookLocal) {
    if unsafe { (*ptr).pending.is_some() } {
        return;
    }
    let down = physical_down(unsafe { (*ptr).shared });
    let action = {
        let local = unsafe { &mut *ptr };
        local.pump.machine_mut().release_if_delivered_and_up(down)
    };
    if action == Neutralize::None {
        return;
    }
    let _ = store_or_apply(
        ptr,
        PendingInject::Recovery(Recovery {
            action,
            ..Recovery::empty()
        }),
    );
}

fn try_lock(shared: &Shared) -> bool {
    if shared
        .injecting
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return false;
    }
    shared.heartbeat.store(qpc(), Ordering::Release);
    true
}

fn unlock(shared: &Shared) {
    shared.heartbeat.store(qpc(), Ordering::Release);
    shared.injecting.store(false, Ordering::Release);
}

fn physical_down(shared: &Shared) -> bool {
    shared.physical_down.load(Ordering::Acquire)
}

fn note_physical(shared: &Shared, event: &InputEvent) {
    if event.injected || event.touch_or_pen || event.button != Button::Right {
        return;
    }
    match event.kind {
        EventKind::Press => shared.physical_down.store(true, Ordering::Release),
        EventKind::Release => shared.physical_down.store(false, Ordering::Release),
        EventKind::Move => {}
    }
}

fn publish_pump(shared: &Shared, pump: &CapturePump) {
    let phase = pump.machine().phase();
    shared.phase.store(phase as u8, Ordering::Release);
    if phase == Phase::Capturing {
        shared.trail.publish(pump.machine().current_points());
    } else {
        shared.trail.publish(&[]);
    }
    if let Some((button, x, y)) = pump.machine().held_point() {
        shared.button.store(button as u8, Ordering::Release);
        shared.x.store(x, Ordering::Release);
        shared.y.store(y, Ordering::Release);
    }
    shared
        .delivered
        .store(pump.machine().delivered_down(), Ordering::Release);
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
    let hook = unsafe { (*local_ptr).hook };
    let swallow = code >= 0 && queue_event(local_ptr, wparam, lparam);
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

fn queue_event(local_ptr: *mut HookLocal, wparam: WPARAM, lparam: LPARAM) -> bool {
    let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
    let Some(event) = decode(wparam.0 as u32, info) else {
        return false;
    };
    let shared = unsafe { (*local_ptr).shared };
    note_physical(shared, &event);
    let pump = ACTIVE_PUMP.with(|slot| slot.get());
    unsafe {
        if pump.is_null() {
            adopt_local(&mut *local_ptr);
            (*local_ptr).pump.on_hook(event) == Decision::Swallow
        } else {
            adopt_pump(local_ptr, pump);
            (*pump).on_hook(event) == Decision::Swallow
        }
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
        if !try_lock(shared) {
            continue;
        }
        let phase = shared.phase.load(Ordering::Acquire);
        let delivered = shared.delivered.load(Ordering::Acquire);
        let button = button_from_u8(shared.button.load(Ordering::Acquire));
        let x = shared.x.load(Ordering::Acquire);
        let y = shared.y.load(Ordering::Acquire);
        let down = physical_down(shared);
        if phase == Phase::Idle as u8 && !delivered {
            unlock(shared);
            continue;
        }
        let (delivered_after, swallow, action) = match phase {
            value if value == Phase::Capturing as u8 => (false, true, Neutralize::Drop),
            value if value == Phase::Pending as u8 && down => {
                (true, false, Neutralize::DeliverPress { button, x, y })
            }
            value if value == Phase::Pending as u8 => (false, true, Neutralize::Drop),
            _ if delivered && !down => (false, true, Neutralize::DeliverRelease { button, x, y }),
            _ => (delivered, false, Neutralize::None),
        };
        send_recovery(&Recovery {
            action,
            ..Recovery::empty()
        });
        if action != Neutralize::None || delivered_after != delivered {
            shared
                .adopt_delivered
                .store(delivered_after, Ordering::Release);
            shared.adopt_swallow.store(swallow, Ordering::Release);
            shared.phase.store(Phase::Idle as u8, Ordering::Release);
            shared.delivered.store(delivered_after, Ordering::Release);
            shared.recovery_gen.fetch_add(1, Ordering::Release);
        }
        unlock(shared);
    }
}

fn fuzz_sender(shared: &'static Shared, seconds: u64) {
    let start = Instant::now();
    let mut state = 0x00c0_ffee_u64;
    let mut x = shared.fuzz_left.load(Ordering::Acquire).saturating_add(20);
    let mut y = shared.fuzz_top.load(Ordering::Acquire).saturating_add(20);
    let mut down = false;
    while !shared.stop.load(Ordering::Acquire) && start.elapsed().as_secs() < seconds {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        let roll = (state >> 33) % 6;
        let left = shared.fuzz_left.load(Ordering::Acquire);
        let top = shared.fuzz_top.load(Ordering::Acquire);
        let right = shared.fuzz_right.load(Ordering::Acquire);
        let bottom = shared.fuzz_bottom.load(Ordering::Acquire);
        x = (x + ((state >> 8) as i32 % 31) - 15)
            .clamp(left.saturating_add(1), right.saturating_sub(2));
        y = (y + ((state >> 16) as i32 % 31) - 15)
            .clamp(top.saturating_add(1), bottom.saturating_sub(2));
        match roll {
            0 if !down => {
                if !fuzz_point_is_ours(shared, x, y) {
                    shared.fuzz_failed.store(true, Ordering::Release);
                    break;
                }
                crate::inject::inject_plain(EventKind::Press, Button::Right, x, y);
                down = true;
            }
            1 if down => {
                crate::inject::inject_plain(EventKind::Release, Button::Right, x, y);
                down = false;
            }
            _ => {
                crate::inject::inject_plain(EventKind::Move, Button::Right, x, y);
            }
        }
        thread::sleep(Duration::from_millis(8));
    }
    if down {
        crate::inject::inject_plain(EventKind::Release, Button::Right, x, y);
        thread::sleep(Duration::from_millis(200));
    }
    shared.stop.store(true, Ordering::Release);
    signal(shared.stop_event);
}

fn fuzz_point_is_ours(shared: &Shared, x: i32, y: i32) -> bool {
    let ours = shared.fuzz_hwnd.load(Ordering::Acquire);
    if ours == 0 {
        return false;
    }
    let hit = unsafe { WindowFromPoint(POINT { x, y }) };
    hit.0 as isize == ours
}

fn create_fuzz_window(shared: &Shared) -> Result<HWND, SessionError> {
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(fuzz_proc),
        hInstance: instance_handle(),
        lpszClassName: w!("RustyGesturesFuzz"),
        ..Default::default()
    };
    let atom = unsafe { RegisterClassExW(&class) };
    if atom == 0 {
        return Err(SessionError(
            "RegisterClassExW failed for the fuzz window".to_owned(),
        ));
    }
    let x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) }.max(1);
    let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) }.max(1);
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST,
            w!("RustyGesturesFuzz"),
            w!("RustyGestures fuzz"),
            WS_POPUP | WS_VISIBLE,
            x,
            y,
            width,
            height,
            None,
            None::<HMENU>,
            Some(instance_handle()),
            None,
        )
    }
    .map_err(|err| SessionError(format!("CreateWindowExW failed for the fuzz window: {err}")))?;
    let mut rect = RECT::default();
    if let Err(err) = unsafe { GetWindowRect(hwnd, &mut rect) } {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        return Err(SessionError(err.to_string()));
    }
    shared.fuzz_hwnd.store(hwnd.0 as isize, Ordering::Release);
    shared.fuzz_left.store(rect.left, Ordering::Release);
    shared.fuzz_top.store(rect.top, Ordering::Release);
    shared.fuzz_right.store(rect.right, Ordering::Release);
    shared.fuzz_bottom.store(rect.bottom, Ordering::Release);
    Ok(hwnd)
}

unsafe extern "system" fn fuzz_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn install_raw_input() -> Option<HWND> {
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(raw_proc),
        hInstance: instance_handle(),
        lpszClassName: w!("RustyGesturesRaw"),
        ..Default::default()
    };
    let atom = unsafe { RegisterClassExW(&class) };
    if atom == 0 {
        return None;
    }
    let parent = HWND(-3isize as *mut std::ffi::c_void);
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("RustyGesturesRaw"),
            w!("RustyGestures raw"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            Some(parent),
            None::<HMENU>,
            Some(instance_handle()),
            None,
        )
    }
    .ok()?;
    let devices = [RAWINPUTDEVICE {
        usUsagePage: 0x01,
        usUsage: 0x02,
        dwFlags: RIDEV_INPUTSINK,
        hwndTarget: hwnd,
    }];
    if unsafe { RegisterRawInputDevices(&devices, std::mem::size_of::<RAWINPUTDEVICE>() as u32) }
        .is_err()
    {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        return None;
    }
    Some(hwnd)
}

unsafe extern "system" fn raw_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_INPUT {
        note_raw_input(lparam);
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn note_raw_input(lparam: LPARAM) {
    let shared_ptr = SHARED_PTR.load(Ordering::Acquire);
    if shared_ptr.is_null() {
        return;
    }
    let handle = HRAWINPUT(lparam.0 as *mut std::ffi::c_void);
    let header = std::mem::size_of::<RAWINPUTHEADER>() as u32;
    let mut size = 0u32;
    let query = unsafe { GetRawInputData(handle, RID_INPUT, None, &mut size, header) };
    if query == u32::MAX || size == 0 || size > 1024 {
        return;
    }
    // Aligned for RAWINPUT. A byte buffer would be the wrong alignment.
    let mut buffer = [0u64; 128];
    let got = unsafe {
        GetRawInputData(
            handle,
            RID_INPUT,
            Some(buffer.as_mut_ptr().cast()),
            &mut size,
            header,
        )
    };
    if got == u32::MAX || got < std::mem::size_of::<RAWINPUT>() as u32 {
        return;
    }
    let raw = unsafe { &*buffer.as_ptr().cast::<RAWINPUT>() };
    if raw.header.dwType != RIM_TYPEMOUSE.0 {
        return;
    }
    let mouse = unsafe { raw.data.mouse };
    let extra = mouse.ulExtraInformation as usize;
    if is_injected_extra(extra) || is_touch_or_pen_extra(extra) {
        return;
    }
    let flags = u32::from(unsafe { mouse.Anonymous.Anonymous.usButtonFlags });
    let shared = unsafe { &*shared_ptr };
    if flags & RI_MOUSE_RIGHT_BUTTON_DOWN != 0 {
        shared.physical_down.store(true, Ordering::Release);
    }
    if flags & RI_MOUSE_RIGHT_BUTTON_UP != 0 {
        shared.physical_down.store(false, Ordering::Release);
    }
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
                signal((*ptr).session_event);
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
    let handles = [
        HANDLE(shared.stop_event as *mut _),
        HANDLE(shared.session_event as *mut _),
    ];
    unsafe {
        MsgWaitForMultipleObjects(Some(&handles), false, wait_ms, QS_ALLINPUT);
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
        let _ = CloseHandle(HANDLE(shared.session_event as *mut _));
    }
}

fn create_events() -> Result<(HANDLE, HANDLE, HANDLE, HANDLE), SessionError> {
    let stop = create_event(true)?;
    let done = match create_event(true) {
        Ok(handle) => handle,
        Err(err) => {
            close_handle(stop);
            return Err(err);
        }
    };
    let ready = match create_event(true) {
        Ok(handle) => handle,
        Err(err) => {
            close_handle(stop);
            close_handle(done);
            return Err(err);
        }
    };
    let session = match create_event(false) {
        Ok(handle) => handle,
        Err(err) => {
            close_handle(stop);
            close_handle(done);
            close_handle(ready);
            return Err(err);
        }
    };
    Ok((stop, done, ready, session))
}

fn create_event(manual: bool) -> Result<HANDLE, SessionError> {
    unsafe { CreateEventW(None, manual, false, PCWSTR::null()) }
        .map_err(|err| SessionError(err.to_string()))
}

fn close_handle(handle: HANDLE) {
    unsafe {
        let _ = CloseHandle(handle);
    }
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
