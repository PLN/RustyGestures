# M0 manual test

Cloud CI builds the workspace and runs the unit tests. It cannot install a low-level mouse hook or draw on a real desktop. Run this checklist on Windows 10 22H2 or Windows 11 x64. A second monitor at a different DPI scale is needed for the mixed-DPI rows.

## Prerequisite

Install the MSVC Build Tools (the C++ build tools workload) so `link.exe` and the Windows SDK are on PATH. Then install a stable Rust toolchain with the Windows target:

```text
rustup target add x86_64-pc-windows-msvc
cargo test --locked --workspace
cargo build --locked --release --target x86_64-pc-windows-msvc
```

The binary is `target\x86_64-pc-windows-msvc\release\rustygestures.exe`. Run it from a terminal whose current directory is the repo, and pass `--gestures` with a full path. Do not start it by right-clicking the exe and choosing Run as administrator: that launches it with no arguments, from `C:\Windows\System32`, and the console closes as soon as clap prints help.

If a button gets stuck, press Ctrl+C in this console (the keyboard is not hooked), or Ctrl+Shift+Esc and end `rustygestures.exe` in Task Manager. A left click in another window is also safe. Then press and release the right button once.

## Capture

From PowerShell, in the repo:

```text
Set-Location C:\path\to\RustyGestures
target\x86_64-pc-windows-msvc\release\rustygestures.exe run --gestures C:\path\to\RustyGestures\gestures.json
```

- [ ] The process prints `hook installed` and stays running. If the hook cannot be installed, it prints the `SetWindowsHookExW` error and exits non-zero instead of pretending to run.
- [ ] A right click with almost no movement still right-clicks the window under the cursor.
- [ ] Hold the right button still for more than 500 ms, then drag. The drag is delivered. It is not captured as a gesture.
- [ ] Hold the right button and move more than about 10 px. The orange trail appears on every monitor the cursor crosses, including a mixed-DPI pair.
- [ ] Release. The trail hides. With a matching template the gesture name is printed. Otherwise the line is `(no match)`.
- [ ] `rustygestures.exe train right --count 3 --gestures C:\path\to\RustyGestures\gestures.json` records three strokes. Ctrl+C before the third stroke saves the samples taken so far and says so. A later `run` recognises a similar stroke.
- [ ] `rustygestures.exe list --gestures C:\path\to\RustyGestures\gestures.json` prints the saved names.
- [ ] `rustygestures.exe record --output C:\path\to\RustyGestures\strokes.json` writes raw points until Ctrl+C.
- [ ] Change a monitor's scale or unplug one. The trail windows come back and still line up with the cursor.

## Lost release

| Step | What to do | Expected |
|---|---|---|
| Release over an elevated window | Standard engine. Start a right-button stroke over Explorer, move onto an elevated Windows Terminal, and release there. | The engine returns to idle. The next right click in Explorer works. Nothing stays held down. |
| Ctrl+Alt+Del mid-stroke | Start a stroke, press Ctrl+Alt+Del, cancel back to the desktop, then right-click. | The stroke is dropped. The next right click works. |
| UAC mid-stroke | Start a stroke and trigger a UAC prompt. Dismiss it, then right-click. | Same as the row above. |
| Hold still, then drag | Hold the right button still for 600 ms, then drag across a window and release. | The target sees the drag. The drop is where the button was released, not back at the press. |
| Swapped buttons | In Settings, turn on Mouse → "Swap mouse buttons", then start the engine. The hook's right-button messages are the logical button. Raw input's right-button bits are the physical button, and the engine reads `SM_SWAPBUTTON` once at startup so the two stay paired. Right-click, then do a short right drag. | The logical right button still clicks and starts a drag. Change the setting only while the engine is not running. |

## Latency

- [ ] Exit with Ctrl+C. The process prints `latency samples=... p50=...us p99=...us max=...us`.
- [ ] NFR-1: p99 under 50 µs and max under 1 ms on an otherwise idle machine.

| samples | p50 µs | p99 µs | max µs | meets NFR-1 |
|---|---|---|---|---|
| | | | | |

`rustygestures.exe latency --gestures C:\path\to\RustyGestures\gestures.json` is the same hook run.

## Stuck buttons and the watchdog

- [ ] Start a stroke and kill the process from Task Manager while the button is still down. The right button is not stuck afterwards.
- [ ] Repeat with Ctrl+C and by closing the console window.
- [ ] Watchdog: train a gesture first, then `rustygestures.exe run --debug-stall-ms 400 --gestures C:\path\to\RustyGestures\gestures.json`. Start a stroke (move about 20 px), stop moving, and wait for the line `debug stall: sleeping 400 ms`. Holding still without that move stays in Pending, so the stall does not run. Do not move during the sleep: a stall while the mouse is moving can exceed `LowLevelHooksTimeout` and Windows will remove the hook. The console also prints `watchdog: recovered (drop)`. The stalled stroke is dropped and no click is injected at the origin. Then draw the trained gesture. Its name is printed. A bare right click afterwards is not enough, because that also works when the hook is gone.

## One hour of fuzzed input

This is the M0 "no stuck buttons" soak. Run it in a virtual machine, from PowerShell. The tool opens a full-screen window it owns and injects random moves, presses, and releases through `SendInput` only while `WindowFromPoint` reports that window, and it checks again before every injection. If the pointer is over something else, the soak aborts. The fuzz window takes focus, so Ctrl+C may not reach the console. Press Esc in that window to abort.

```text
Set-Location C:\path\to\RustyGestures
target\x86_64-pc-windows-msvc\release\rustygestures.exe fuzz --seconds 3600 --gestures C:\path\to\RustyGestures\gestures.json
```

- [ ] It runs for an hour and prints `fuzz finished` and a latency line. If the engine is not idle, the button is still down, or the pointer left the tool window, the process exits non-zero and does not print `fuzz finished`.
- [ ] Afterwards a normal right click and a normal drag both work. No button is stuck.
- [ ] Repeat on the other of Win10 and Win11 if both are available. Record each machine below.

## Elevation matrix

Open an elevated Windows Terminal with Run as administrator. In PowerShell, `Set-Location` to the repo, and pass the exe path and `--gestures`. That is the elevated engine. A normal PowerShell window, same command, is the standard engine. The elevated target is `wt.exe` started as administrator. The normal target is an Explorer window.

| Engine | Target | Expected | Observed |
|---|---|---|---|
| Standard | Elevated Windows Terminal | No hook events for that window, and injection does not land | |
| Standard | Normal Explorer window | Clicks pass through and gestures are recognised | |
| Elevated engine | Elevated Windows Terminal | Clicks pass through and gestures are recognised | |
| Elevated engine | Normal Explorer window | Clicks pass through and gestures are recognised | |

Elevated engine, from the admin terminal:

```text
Set-Location C:\path\to\RustyGestures
target\x86_64-pc-windows-msvc\release\rustygestures.exe run --gestures C:\path\to\RustyGestures\gestures.json
```

Date and machines:

```text
OS:
Monitors / DPI:
Binary:
Second OS, if the soak was repeated:
```
