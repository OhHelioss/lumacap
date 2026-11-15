# Contributing to Lumacap

Thanks for helping! Lumacap is a single-file Rust app (`src/main.rs`) using Media Foundation and GDI/GDI+.

## Build
Windows 10/11 with stable Rust (MSVC toolchain): `cargo build --release`, or run `build.bat`. The exe ends up in `target\release\lumacap.exe`.

## Guidelines
- Keep it lightweight: no new heavy dependencies, no web views, no background services.
- The video thread must never block on UI or disk; heavy work goes to its own thread.
- Every GDI object you create must be deleted (and deselected first). `diag.txt` in `%APPDATA%\lumacap` shows the GDI object count: it must stay flat.
- Never call `DrawTextW` with an empty buffer.
- Test on real hardware if you touch capture, encoding or hotkeys, and say what you tested in the PR.

## Bugs
Include your GPU, capture device, Windows version and the contents of `%APPDATA%\lumacap\crash.txt` / `diag.txt` if present.
