# M0 manual test

Cloud CI builds the workspace and runs the unit tests. It cannot install a low-level mouse hook or draw on a real desktop. Run this checklist on a Windows 10 22H2 or Windows 11 x64 machine, ideally with two monitors at different DPI scales.

Build:

```text
cargo test --workspace
cargo build --release --target x86_64-pc-windows-msvc
```

The binary is `target\x86_64-pc-windows-msvc\release\rustygestures.exe`.

## Capture

- [ ] `rustygestures.exe run` prints the hook banner and stays running.
- [ ] A right click with almost no movement still clicks the window under the cursor (the press and release are re-injected).
- [ ] Hold the right button still for more than 500 ms, then drag. The drag is delivered to the target. It is not captured as a gesture.
- [ ] Hold the right button and move more than about 10 px. The orange trail appears, and it appears on every monitor the cursor crosses, including a mixed-DPI pair.
- [ ] Release. The trail hides. If `gestures.json` has a matching template, the gesture name is printed. Otherwise the line is `(no match)`.
- [ ] `rustygestures.exe train right --count 3` records three strokes into `gestures.json`. A later `run` recognises a similar stroke.
- [ ] `rustygestures.exe list` prints the saved names. `record --output strokes.json` writes raw points until Ctrl+C.
- [ ] Move the cursor between monitors and change a monitor's scale. The trail windows come back on `WM_DISPLAYCHANGE` / `WM_DPICHANGED` and still line up with the cursor.

## Latency

- [ ] Exit with Ctrl+C. The process prints `latency samples=... p50=...us p99=...us max=...us`.
- [ ] NFR-1: p99 under 50 µs and max under 1 ms on an otherwise idle machine. Record the numbers below.

| samples | p50 µs | p99 µs | max µs | meets NFR-1 |
|---|---|---|---|---|
| | | | | |

`rustygestures.exe latency` is the same hook run, aimed at this measurement.

## Stuck buttons

- [ ] Start a stroke, then kill the process from Task Manager while the button is still down. After that, the right button is not stuck down in other windows.
- [ ] Repeat with Ctrl+C and by closing the console window.
- [ ] Hold a stroke until the process is busy, then release. A stall longer than 250 ms must not leave the button down.

## Elevation matrix

UIPI is expected to block a standard-integrity engine from seeing or injecting input for an elevated window. An elevated engine should work over both. Fill in what you actually observe.

Run elevated `wt.exe` with "Run as administrator". Use a normal Explorer window as the other target. For the elevated engine, start the binary with "Run as administrator".

| Engine | Target | Expected | Observed |
|---|---|---|---|
| Standard | Elevated Windows Terminal | No hook events for that window, and injection does not land | |
| Standard | Normal Explorer window | Clicks pass through and gestures are recognised | |
| Elevated (Run as administrator) | Elevated Windows Terminal | Clicks pass through and gestures are recognised | |
| Elevated (Run as administrator) | Normal Explorer window | Clicks pass through and gestures are recognised | |

Date and machine:

```text
OS:
Monitors / DPI:
Binary:
```
