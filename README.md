# RustyGestures

RustyGestures is a small open-source mouse-gesture utility for 64-bit Windows, written in Rust and licensed under MIT.

This repository is the **M0 spike**. The hook, the on-screen trail, and the recognizer are in place so they can be tried on a Windows machine. It is not a usable settings app yet: there is no tray, no action runner, and no settings window.

## Layout

- `rustygestures-core` is platform-neutral. It holds the gesture state machine, the Protractor recognizer, and the latency histogram. It builds and tests on Linux as well as Windows.
- `rustygestures-win` is the Windows engine: a `WH_MOUSE_LL` hook on its own thread, click and drag re-injection, and a per-monitor DirectComposition trail.
- `rustygestures-ui` is a placeholder. The settings UI is added later.
- `rustygestures` is the binary.

## Build

Install a stable Rust toolchain with the `x86_64-pc-windows-msvc` target, then:

```text
cargo test --workspace
cargo build --release --target x86_64-pc-windows-msvc
```

The binary speaks a few commands. `rustygestures run` installs the hook, draws the trail while a stroke is captured, and prints the name of a recognized gesture. `train <name>` records samples into `gestures.json`. `record` dumps raw strokes. `list` prints the saved gestures. `latency` runs the hook and prints the callback-time histogram on exit. `run`, `train`, and `record` print that histogram too. `--gestures <file>` selects the template file.

The core tests do not need Windows. The interactive checks (click passthrough, drag, mixed-DPI trail, stuck buttons, elevation) are in [docs/M0-TEST.md](docs/M0-TEST.md). The design notes are in [docs/DESIGN.md](docs/DESIGN.md).

## Clean room

The behaviour is inspired by StrokesPlus.net, using only public descriptions of how that program behaves: its changelog, the exported forum, and archived help pages. No binary was decompiled, disassembled, or unpacked. No source from other mouse-gesture programs was read. RustyGestures does not share a config format or an API with StrokesPlus.net and cannot load its files.

The recognizer follows the published Protractor paper (Li, 2010) and the $1 paper (Wobbrock, Wilson, and Li, 2007), with the near-straight scaling idea from $N (Anthony and Wobbrock, 2010). The hook follows Microsoft's low-level mouse-hook documentation. Contributor rules are in [CLEANROOM.md](CLEANROOM.md).
