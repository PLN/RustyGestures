# RustyGestures: requirements, features and design

*Rev 3.1, 2026-11-09. Author: PLN. Repo: `github.com/PLN/RustyGestures`. Status: M0 spike in progress.*

RustyGestures is a lean, open-source mouse-gesture utility for Windows, written in Rust and released under the MIT license.

**Logo concept:** an old, rusted PS/2-era ball mouse.

## Changelog

- **Rev 3.1 (2026-11-09):** Installer to `Program Files` for 0.1 confirmed; no open questions remain.
- **Rev 3 (2026-11-09):** Applied PLN's answers to the open questions.
  - **License:** MIT for the whole project. All dependencies must be permissive.
  - **Settings UI:** Slint dropped in favour of **egui/eframe** (MIT OR Apache-2.0); see §6.
  - **Code signing:** skipped. It's now an optional item for 1.0 or later.
  - **Elevated mode:** an opt-in **Run elevated** mode moves into 0.1, using a Task Scheduler task and an admin-only config folder. uiAccess stays a later option that needs signing.
  - **Secure Desktop:** explicitly out of scope.
  - **ARM64:** dropped. The target is x64 only.
  - **Repo:** `PLN/RustyGestures`.
  - **Identity:** copyright and git identity set to PLN only.
  - The roadmap and open questions were updated to match.
- **Rev 2 (2026-11-09):** Stripped down to basics.
  - Renamed to RustyGestures.
  - Built-in actions only; scripting moved to "Later".
  - All StrokesPlus.net compatibility removed.
  - Architecture reduced to engine + settings UI.
- **Rev 1 (2026-11-09):** First draft: a full-parity clone with scripting and an importer.

---

## 0. Summary

| Decision | Pick | Why |
|---|---|---|
| Product | Lean gesture tool: **gestures and a few triggers → built-in actions** | That's the core value. Built-in actions cover what people bind. |
| Language | **Rust** (`windows` crate) | No garbage collector on the hook path. First-party Win32/COM bindings. Small binary. |
| Recognizer | **Protractor** (orientation-sensitive), with $1-style preprocessing | Published, simple, microsecond-fast, trainable from a few samples. |
| Architecture | **One binary, two processes**: always-on engine + settings UI started on demand | The hook path stays tiny, and there's no script host. |
| Overlay | Per-monitor click-through windows, **DirectComposition + Direct2D** | Smooth trail, correct per-monitor DPI, zero cost when idle. |
| Settings UI | **egui/eframe** | MIT/Apache, pure Rust, quick to build forms and a gesture canvas, AccessKit accessibility. |
| Elevation | Opt-in **Run elevated** (Task Scheduler, highest privileges) in 0.1; uiAccess later | Gestures work over elevated terminals and IDEs without code signing. |
| Config | Versioned **TOML** + `gestures.json`, atomic writes, hot reload | Human-readable and diffable. |
| License | **MIT** | PLN's decision. All dependencies must be permissive (MIT, Apache-2.0, BSD, Zlib, ISC). |
| Platform | Windows 10 22H2+ / Windows 11, **x64 only** | |

---

## 1. Background, goals, non-goals

### 1.1 Inspiration and clean-room statement

The behaviour is inspired by StrokesPlus.net, a free, closed-source Windows mouse-gesture utility. Its final release was 0.5.8.0, on 2024-10-20, and its author has since moved on to a paid successor.

**Sources used.** Feature ideas came only from public, behavioural sources:
- its [change log](https://raw.githubusercontent.com/roblarky/roblarky.github.io/HEAD/ChangeLog.txt);
- the forum export in [roblarky/roblarky.github.io](https://github.com/roblarky/roblarky.github.io);
- the [archived help pages](https://web.archive.org/web/20240509182702/https://www.strokesplus.net/).

**What was not done.**
- No binaries were decompiled, disassembled or unpacked.
- No code was read from [minyoad/StrokesPlus](https://github.com/minyoad/StrokesPlus), the old C++ source, which is cited as background only.
- The successor was not studied.

**No compatibility.** RustyGestures is **not compatible** with StrokesPlus.net in any way: no shared config format, no importer, no API. Behaviour is described in our own words.

**For contributors.** `CLEANROOM.md` and the PR template restate these rules. Recognition and hooking code is written only from the published papers and the Microsoft docs.

### 1.2 License, identity, name

**License.** The whole project is **MIT**. The `LICENSE` file reads `Copyright (c) 2026 PLN`.
- **Dependency rule:** every dependency must be under a permissive, MIT-compatible license (MIT, Apache-2.0, BSD-2/3, Zlib, ISC, Unicode-3.0).
- **No copyleft or source-available licenses:** no GPL, LGPL, MPL-only or custom licenses, either in-tree or linked into the shipped binary.
- **Enforcement:** `cargo-deny` in CI uses a license allow-list.
- **Third-party notices:** the release bundles them in `THIRD_PARTY_LICENSES` (generated with `cargo-about`).

**Identity.** Only the PLN handle is used in code, commits, PRs, docs and release metadata. The git identity is `PLN <78331+PLN@users.noreply.github.com>`.

**Name.** "RustyGestures" was free on GitHub and crates.io as of 2026-11-09. Trademark registers haven't been checked. Don't use "StrokesPlus", "S+" or "StrokeIt" in any branding.

### 1.3 Goals

- **G1:** Reliable, low-latency mouse gestures that **never lose or stick input**.
- **G2:** Everything configured in a settings UI; no code required.
- **G3:** Lean: ≤ 15 MB RAM when idle, ~0% CPU, one small binary, a portable mode.
- **G4:** Gestures also work over **elevated windows**, such as admin terminals and IDEs, through the opt-in elevated mode.
- **G5:** No telemetry and no network access by default.

### 1.4 Non-goals

- **Secure Desktop:** the UAC consent prompt, Ctrl+Alt+Del and the lock screen are explicitly out of scope. They run on a separate desktop that no user-mode hook can reach.
- **Platforms:** no ARM64 build, and no macOS or Linux. The core crate stays platform-neutral anyway.
- **Scope:**
  - no scripting in core (see §7);
  - no StrokesPlus.net compatibility;
  - no plug-ins, macro recording, text expansion or touch floaters;
  - no accounts, cloud sync or encrypted settings.
- **Low-level tricks:** no kernel drivers, and no bypassing anti-cheat.

---

## 2. Features

Priorities: **MVP** = 0.1. **v1** = needed for 1.0. **Later** = optional, after 1.0.

### 2.1 Gestures and triggers

| Feature | Behaviour | Priority |
|---|---|---|
| Primary stroke button | Hold (default: right) and move to draw a gesture. A click without movement passes through normally. | MVP |
| Cancel delay / relay | If the button is held still past N ms, the press is replayed so right-drag still works. | MVP |
| Movement threshold | Pixel distance before a press counts as a gesture. | MVP |
| Gesture trail | Width, colour and opacity; can be turned off. | MVP |
| Training | Draw to create a gesture or add samples to one. Multiple samples per gesture; rename, delete, preview. | MVP |
| No-match feedback | Optional sound or a brief on-screen "no match". | MVP |
| Key modifiers | Holding Ctrl, Alt, Shift or Win while drawing selects a different action. | MVP |
| Secondary stroke button | For example middle or X1, with its own action set. | v1 |
| Rocker | Hold left and click right, or the reverse, to run an action (e.g. back/forward). | v1 |
| Wheel while holding the stroke button | Wheel up/down runs a repeatable action, such as switching tabs or changing volume. | v1 |
| Hint | While drawing, shows the name of the action that would match. | v1 |
| Hot keys | A key combination runs any built-in action (`RegisterHotKey`). | v1 |

### 2.2 Scoping

| Feature | Behaviour | Priority |
|---|---|---|
| Global actions | Gesture → action, grouped in categories. | MVP |
| App profiles | Match by exe, window class or title. All non-empty fields must match. Patterns are globs (case-insensitive) or optional regex. A window picker fills them in. App actions override global ones, and a profile can turn off the global fallback. | MVP |
| Ignore list | Same matching as app profiles. Capture is off in those windows, optionally only when the window is full screen. | MVP |
| Ignore key | Hold it to bypass capture temporarily. | MVP |
| Enable/disable | From the tray or a hot key. Capture is suspended automatically while the session is locked. | MVP |
| Disable on full-screen apps | Global option. | v1 |

### 2.3 Built-in actions (the whole action model)

Each action is a typed record edited in the UI. An action can also be a **sequence** of these, with optional short delays between steps.

| Group | Actions | Priority |
|---|---|---|
| Keyboard | Send a hot key (e.g. `Ctrl+Shift+T`); send a key sequence; type a text snippet (Unicode via `SendInput`, or paste through the clipboard and then restore it) | MVP |
| Window (target: the window under the gesture start, or the foreground window) | Minimise; maximise/restore toggle; restore; close; always-on-top toggle; move to next/previous monitor; centre; activate | MVP |
| Launch | Program (path, args, working dir), URL, file or folder; optional "activate if already running". Launches **un-elevated by default**, even from the elevated engine; "run as admin" is an explicit option (§4.6). | MVP |
| Media and volume | Play/pause, next, previous, stop, volume up/down/mute | MVP |
| App control | Enable/disable capture, open settings, toggle training mode | MVP |
| Mouse | Click (left, right, middle or X; single or double) at the current point or the gesture start; scroll by N; move to a point | v1 |
| Window extras | Snap left/right, minimise all others, send to back, transparency toggle | v1 |
| Clipboard | Set clipboard text | v1 |

Launch arguments accept placeholders: `{gesture}`, `{exe}`, `{title}`, `{hwnd}`, `{x}`, `{y}`.

### 2.4 App and platform

| Feature | Priority |
|---|---|
| Tray icon and menu (enable, training, settings, exit) | MVP |
| Settings UI: gestures, actions, app profiles, ignore list, options | MVP |
| Portable mode (config beside the exe; standard mode only) | MVP |
| Safe mode (`--safe` starts with capture disabled) | MVP |
| Multi-monitor and mixed DPI (Per-Monitor-V2) | MVP |
| Auto-start with Windows (standard mode) | MVP |
| **Run elevated** mode (opt-in; §4.6) | MVP |
| Export and import of RustyGestures config | v1 |
| Light/dark theme following the system; translations | v1 |
| Opt-in update check (GitHub Releases, notification only) | v1 |
| Code signing and a uiAccess build | Later (optional) |

---

## 3. Requirements

### 3.1 Functional

- **FR-1 (start a gesture):** With capture enabled and no ignore rule matching, a stroke-button press plus movement past `min_move_px` (default 10 px) enters gesture mode. The press and release are not delivered to the target.
- **FR-2 (normal click):** A press and release that never crosses the threshold reaches the target as a normal click within 30 ms of the release.
- **FR-3 (drag):** Holding the button past `cancel_delay_ms` (default 500) without moving re-delivers the press at its original point. The drag then passes through.
- **FR-4 (run the action):** On release, the stroke is recognised and the action is chosen by the precedence in §4.4.
- **FR-5 (hot reload):** Config edits apply within 1 s, without a restart.
- **FR-6 (elevated windows):** In Run-elevated mode, FR-1 to FR-4 also work when the target window is elevated.

### 3.2 Non-functional

| Id | Requirement | Target |
|---|---|---|
| NFR-1 | Hook callback time | p99 < 50 µs, max < 1 ms. Windows **silently removes** low-level hooks that exceed `LowLevelHooksTimeout`, which is capped at 1000 ms since Win10 1709 ([docs](https://learn.microsoft.com/en-us/windows/win32/winmsg/lowlevelmouseproc)). |
| NFR-2 | Inside the hook callback | No heap allocation, no blocking locks, no I/O, no calls that send messages to other windows |
| NFR-3 | Release → action start | < 30 ms p95 |
| NFR-4 | **Never swallow input permanently** | Every suppressed press is either re-injected or paired with its suppressed release. A watchdog releases any held state if the engine stalls for more than 250 ms during a capture. |
| NFR-5 | Hook-loss detection | If `GetLastInputInfo` shows input but the hook has gone silent, re-install the hook. |
| NFR-6 | Idle footprint (UI closed) | ≤ 15 MB private working set, no timer wake-ups, ~0% CPU |
| NFR-7 | Startup and size | Ready in < 300 ms; exe < 10 MB |
| NFR-8 | Platforms | Win10 22H2 and Win11 23H2+, x64 |
| NFR-9 | Privacy | No telemetry. Keystrokes are never logged or stored. |

---

## 4. Architecture

### 4.1 Process model

```
rustygestures.exe              (engine, always running; normal or elevated)
  hook thread  WH_MOUSE_LL (+ WH_KEYBOARD_LL only when modifiers/ignore key need it)
               tiny state machine -> lock-free ring buffer -> engine
  main thread  tray, overlays (DComp/D2D), foreground tracking (WinEvent hook),
               RegisterHotKey, display/session changes, config file watcher
  worker       recognizer, action resolver, action executor (SendInput, window ops, launch)

rustygestures.exe --settings   (settings UI, egui/eframe, separate process, on demand;
                                launched by the engine at the engine's own integrity level)
```

- **No script host.** Actions are short, bounded native code.
- **Separate UI process.** The UI's memory is only used while it's open, and a UI bug can't touch the hook path.
- **How the two talk:**
  - The UI writes config files atomically, and the engine picks them up via `ReadDirectoryChangesW`.
  - A few control messages (open settings, toggle training, reload) go to the engine's hidden message window.
  - Training happens in the UI's own canvas. Tray training mode passes points to the UI with `WM_COPYDATA`.
  - There is no pipe or network listener.

### 4.2 Input capture

- **Hooks.** `WH_MOUSE_LL` is required, because Raw Input can't *suppress* the stroke-button press. `WH_KEYBOARD_LL` is installed only when modifiers or the ignore key need it. Hot keys use `RegisterHotKey`.
- **Pass-through events.** Our own injected events carry a magic `dwExtraInfo` value and always pass through. So do touch- and pen-generated mouse events (signature `0xFF515700`).
- **State machine.** A pure, allocation-free function: `Idle → Pending → Capturing → Idle`, plus rocker and wheel states in v1. The hook reads only snapshots that the main thread prepares: enabled flag, ignore key, foreground-app ignore match.
  - **Pending + move** past the threshold → Capturing.
  - **Pending + release, or timeout** → the engine re-injects the press (and release) with `SendInput`.
  - **Capturing + release** → swallowed, and the stroke is posted.
- **Window under the cursor.** `WindowFromPoint` can send `WM_NCHITTEST` to the target's thread, so the hook never calls it. The engine resolves the window asynchronously and relays the press immediately if that window is ignored. That costs about 1–5 ms, to be measured in M0.
- **Invariants (property-tested):**
  - No unpaired swallowed press or release.
  - On shutdown, lock, hook re-install or watchdog trigger, any pending press is released and the state returns to Idle.
  - If the process dies, Windows removes its hooks, so input can't get stuck.

### 4.3 Recognizer

**Algorithm.** *Protractor* ([Li 2010, CHI](https://dl.acm.org/doi/10.1145/1753326.1753654)), orientation-sensitive, with *$1* preprocessing ([Wobbrock, Wilson & Li 2007, UIST](https://faculty.washington.edu/wobbrock/pubs/uist-07.01.pdf)):
- resample to 64 points and translate to the centroid;
- aspect-preserving scaling for near-straight strokes (the 1D handling from *$N*, Anthony & Wobbrock 2010);
- rotation limited to ±30°.

**Matching.** The best template is accepted if its score is ≥ 0.80 **and** it beats the best *other* gesture by ≥ 0.05. Training warns about confusable pairs.

**Cost and IP.** Matching takes microseconds for hundreds of templates. These are published algorithms ([ACE Lab $-family](https://depts.washington.edu/acelab/proj/dollar/index.html)), implemented from the papers. No patent search was done.

### 4.4 App matching and action resolution

- **Foreground cache.** `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)` keeps a cache: window → exe (via `QueryFullProcessImageNameW`), class and title → profile. If the exe can't be read, the window matches `(protected)`.
- **Precedence:**
  1. The matching profile's actions, filtered by gesture, button and modifiers.
  2. Global actions, unless the profile turns off the fallback.
  3. No-match feedback.
- **Execution.** Optionally activate the window under the gesture start, then run the action's steps on the worker thread.

### 4.5 Overlay

- **Windows:** one per monitor, created hidden with `WS_EX_LAYERED | TRANSPARENT | TOPMOST | NOACTIVATE | TOOLWINDOW`, and shown only while capturing.
- **Rendering:** DirectComposition + Direct2D, with dirty-rect updates. Hint and no-match text uses DirectWrite.
- **Fallback:** without a GPU, `UpdateLayeredWindow` + `tiny-skia` (BSD-3-Clause).

### 4.6 Elevation (UIPI)

**Why it matters.** User Interface Privilege Isolation stops a process from injecting input into, or controlling, windows of a **higher** integrity level. A normal-integrity RustyGestures therefore can't act on elevated terminals or IDEs. Developers run those all the time, so 0.1 ships an opt-in elevated mode.

**Why elevation is enough.** UIPI only restricts lower → higher integrity:
- [`SendInput`](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-sendinput) may inject "only into applications that are at an equal or lesser integrity level".
- An elevated (High-IL) engine can therefore inject into both elevated and normal windows.
- Its `WH_MOUSE_LL`/`WH_KEYBOARD_LL` hooks are called for input headed to windows at or below its own level, which is every window on the user's desktop.
- Window operations (`ShowWindow`, `SetWindowPos`, `PostMessage(WM_CLOSE)`) are subject to the same rule and work on both.
- The M0 exit criteria include an empirical test on Win10 and Win11 VMs: an elevated engine, gestures over an elevated `wt.exe` and over a normal Explorer window, and each action type.
- `SendInput` fails silently when UIPI blocks it, so standard mode detects elevated targets with `GetTokenInformation(TokenIntegrityLevel)` on the target process. It then shows "target is elevated; enable Run elevated" instead of failing silently.

**Out of scope.** The Secure Desktop (UAC consent prompt, Ctrl+Alt+Del, lock screen) can't be reached by any user-mode hook, with or without elevation.

| Mode | How | Over elevated windows | When |
|---|---|---|---|
| Standard (default) | Normal process; HKCU `Run` auto-start; config in `%APPDATA%`; portable mode possible | No (detected and explained) | 0.1 |
| **Run elevated** (opt-in) | A Task Scheduler task "at logon of user X, run with highest privileges" starts the engine with no UAC prompt at each logon. Turning the mode on or off is a one-time UAC-elevated step that creates or deletes the task and migrates the config. | Yes | 0.1 |
| uiAccess | `uiAccess="true"` in the manifest. The binary must be Authenticode-signed by a trusted certificate and installed in a secure location (`Program Files`). Gives UIPI bypass for input without full admin rights. **Local dev builds:** sign with a self-signed cert imported into *Trusted Root* on the dev machine only, never in releases. | Yes | Later, once code signing exists |

**Rules in Run-elevated mode:**
- **Config location.** The engine reads config **only** from `%ProgramData%\RustyGestures\<user-SID>\`. The ACL there is Administrators and SYSTEM full control, the user read-only, inheritance off. The engine checks the ACL at startup and refuses to start elevated if the folder (or any file in it) is writable by non-admins. Portable mode is not available in this mode. See §8 for why.
- **Settings UI.** The UI is launched **by the elevated engine**, so it's elevated too, with no extra prompt, and writes the protected config directly. If the user starts it any other way while elevated mode is on, it relaunches itself with `runas` (UAC prompt).
- **Launch actions run un-elevated by default.** Otherwise every program started by a gesture would inherit admin rights. The engine creates the child with `PROC_THREAD_ATTRIBUTE_PARENT_PROCESS` set to the user's `explorer.exe`, so it gets the shell's medium-integrity token. If that fails, it falls back to asking the shell to launch the program (`IShellDispatch2::ShellExecute` on the desktop's shell view). "Run as admin" is a per-action opt-in.
- **Mutual exclusion.** Only one engine instance runs per session. A named mutex in the session namespace prevents a standard and an elevated engine from hooking at the same time.

### 4.7 Configuration and code layout

- **Location:**
  - Standard mode: `%APPDATA%\RustyGestures\`, or `<exe dir>\data\` with `portable.toml`.
  - Elevated mode: `%ProgramData%\RustyGestures\<SID>\` (§4.6).
  - `--config <dir>` overrides both in standard mode only.
- **Files:** `settings.toml`, `actions.toml` (profiles, ignore list, actions, hot keys) and `gestures.json`.
- **Robustness:** every file has `schema = <int>`, with migrations in code. Writes are atomic (`ReplaceFileW`) with 10 rotating backups. If an edit is invalid, the last good config stays active.
- **Crates:**
  - `rustygestures-core`: platform-neutral config model, recognizer, matcher, state machine.
  - `rustygestures-win`: hooks, input, window ops, overlay, tray, elevation helpers.
  - `rustygestures-ui`: egui/eframe.
  - `rustygestures`: the single binary.

---

## 5. Language: Rust

- **Hook path:** native, with no runtime in between. In Go, callbacks enter its runtime on a locked OS thread, where garbage-collection stop-the-world pauses add jitter to every input event.
- **Windows APIs:** Microsoft's [`windows`](https://github.com/microsoft/windows-rs) crate (MIT OR Apache-2.0) covers hooks, Direct2D, DirectComposition, DirectWrite, Task Scheduler COM and security APIs. Go's `x/sys/windows` covers only a subset.
- **Size:** roughly half of Go's memory and binary size for this kind of tool.

Go would build faster, but Win32/COM interop is most of the work here, and that's where Go is weakest.

---

## 6. Settings UI: egui/eframe (Slint dropped)

### Slint licensing verdict

[Slint](https://github.com/slint-ui/slint/blob/master/LICENSE.md) is offered under GPL-3.0, a Slint Royalty-free License 2.0, or a commercial license.
- **What the royalty-free license allows:** it is free for desktop apps, including open-source ones, on condition that the app shows the `AboutSlint` widget or a Slint badge on its download page.
- **Its restrictions:** no embedded use, no redistributing Slint on its own, and no apps that expose Slint's APIs.
- **So it's legally usable.** An MIT app *can* ship with Slint under these terms; the attribution alone would be acceptable.
- **But it fails our dependency rule.** It is a custom, non-OSI, source-available license. The shipped binary would not be purely permissive, every fork would inherit Slint's terms, and `cargo-deny` would need a special exception. The only other free option, GPL-3.0, would make the distributed binary GPL.

**Verdict:** Slint doesn't fit a "whole project MIT, permissive deps only" rule, so it is dropped.

### Pick: egui/eframe

[egui](https://github.com/emilk/egui)/eframe 0.37 (Oct 2026) is licensed MIT OR Apache-2.0.
- Pure Rust, with an immediate-mode style that suits forms, lists and a gesture-drawing canvas.
- Accessibility through the built-in AccessKit integration.
- Adds a few MB, and its code isn't touched by the engine unless `--settings` runs, so it doesn't affect idle memory.
- **Tradeoffs:** it doesn't look native, and IME is weaker. Both are acceptable for a settings window that has little text entry.

**Alternatives rejected:**
- **Native Win32 via `windows`/`winsafe`** (MIT): smallest and fully native, but much slower to build rich editors and the gesture canvas.
- **native-windows-gui** (MIT): low maintenance activity.

The tray menu, hint text and overlay stay native Win32/Direct2D in the engine.

---

## 7. Later / optional extensions

Not planned for 1.0:
- **Gesture context for external tools:** a "Run script" action passing gesture context through env vars (`RUSTYGESTURES_GESTURE`, `_EXE`, `_TITLE`, `_HWND`, `_X`, `_Y`) to AutoHotkey, PowerShell and the like.
- **Embedded JS engine** (e.g. QuickJS-NG via `rquickjs`, MIT), only if needed. It would bring back a trust model and limits.
- **Code signing and uiAccess** (§4.6).
- **More recognition features:** exclusion zones, regions, and multistroke or touch gestures (e.g. *$Q*, [Vatavu et al. 2018](https://dl.acm.org/doi/10.1145/3229434.3229465)).

---

## 8. Security and safety

- **No code execution in core.** Actions are typed and bounded. Importing someone else's config is still flagged, because Launch actions run programs.
- **Elevated-mode config ACL (the main risk).**
  - **The risk:** an elevated engine runs Launch and SendInput actions with admin rights, based on the config it reads. If that config were user-writable, any unelevated malware could add a "launch X as admin" action and gain elevation, which is a UAC bypass.
  - **The mitigations:** the admin-only `%ProgramData%` location, the startup ACL check with refusal to run, no `--config` override and no portable mode when elevated, and un-elevated Launch by default (§4.6).
- **The Task Scheduler task:** created with a fixed path to the installed binary. That binary must also sit in a folder only admins can write, otherwise a user-writable exe would itself be an elevation path. So the elevated mode requires installing to `Program Files` or another admin-protected folder. The UI checks the exe folder's ACL before creating the task.
- **Control messages:** accepted only from the same user and session. The elevated engine also rejects `WM_COPYDATA` from lower integrity (UIPI blocks it by default; we don't add `ChangeWindowMessageFilterEx` exceptions).
- **Keyboard data:** keystrokes are never logged or stored, and the keyboard hook is installed only when needed.
- **AV heuristics:** low-level hooks plus SendInput look like a keylogger, and unsigned builds make this worse. Mitigations: reproducible builds, Defender submissions, a plain README explanation. Signing comes later.
- **Telemetry and supply chain:** no telemetry; the network is used only for the opt-in update check. `cargo-deny` checks licenses and advisories, `cargo-audit` runs in CI, and each release ships checksums and an SBOM.

---

## 9. Testing

- **Recognizer:** a recorded-stroke corpus from several mice, touchpads and DPIs; golden accuracy and confusion-matrix tests; `criterion` benchmarks.
- **State machine:** unit tests and `proptest` over random event sequences, covering the invariants and timeouts.
- **Latency:** an in-hook QPC histogram, with a CI gate on p99 and max under a synthetic `SendInput` flood.
- **Integration (self-hosted Win10/Win11 x64 VMs with autologon):**
  - mixed-DPI monitors, lock/unlock, RDP, and the software-render fallback;
  - the **elevation matrix:** {standard, elevated engine} × {normal, elevated target} × each action type;
  - the ACL refusal path, and un-elevated Launch from the elevated engine.

  GitHub-hosted runners handle builds and unit tests.
- **Robustness:** fuzzing of the config parser, and a 24-hour soak test.

---

## 10. Roadmap

| Milestone | Scope | Exit criteria |
|---|---|---|
| **M0 spike** | Hook + state machine with relay/cancel; DComp trail on mixed-DPI multi-monitor; Protractor with CLI training; latency instrumentation; **elevation check** (elevated engine over elevated and normal targets) | NFR-1 met; no stuck buttons in 1 h of fuzzed input; elevation matrix confirmed |
| **0.1 MVP** | All MVP rows in §2: primary button, trail, training, global actions + app profiles, ignore list/key, key modifiers, MVP built-in actions, tray, egui settings UI, portable/safe mode, hot reload, **Run elevated** mode with ACL-protected config and un-elevated Launch; unsigned x64 portable zip + simple installer to Program Files | PLN can use it daily, incl. over elevated terminals and IDEs |
| **0.2** | Secondary button, rocker, wheel actions, hints, hot keys, mouse/window-extra/clipboard actions, full-screen disable, config export/import, themes | Feature-complete for 1.0 |
| **1.0** | Hardening, translations, opt-in update check, docs, soak test, `THIRD_PARTY_LICENSES`; *optional:* code signing and a uiAccess build if a signing route exists by then | All MVP and v1 rows done |

## 11. Open questions for PLN

None open. Decided 2026-11-09: 0.1 ships a simple installer to `Program Files` alongside the portable zip; Run-elevated mode requires the installed copy (§8), and portable-only users stay in standard mode.

## Not verified

- **Elevation behaviour:** the hook and `SendInput` behaviour from an elevated engine follows Microsoft's documented UIPI rules, but hasn't been tested yet (planned in M0).
- **Fallback launch:** the `IShellDispatch2` launch path works in practice, but our implementation still needs testing.
- **Legal:** no trademark or patent searches were done.
