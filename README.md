# Lumacap

A tiny, fast, open-source recorder for capture cards on Windows. One exe, no installer, no accounts. Built and tested with the Elgato HD60 S, but it works with any UVC capture device.

> Lumacap is an independent project and is **not affiliated with or endorsed by Elgato / Corsair**.

## Features
- Live low-latency preview, fullscreen (F11)
- GPU-accelerated H.264 / HEVC recording to MP4 (optional crash-safe fragmented MP4), 30 or 60 fps
- Instant replay buffer: save the last N seconds
- Game and microphone audio mixing, optional separate mic track, pause/resume
- Screenshots (PNG)
- Library with thumbnails, durations, trim and export (lossless remux)
- Custom global hotkeys (defaults: F9 record, F10 screenshot, F8 replay, F7 pause)
- Low-disk-space warning and auto-stop, optional auto-split of long recordings (15/30/60 min)
- Minimize to tray and start with Windows
- Light/dark theme and accent colours

## Download

**[Download the latest `lumacap.exe`](https://github.com/OhHelioss/lumacap/releases/latest)** (no installer, just run it). Windows SmartScreen may warn because the exe is not code-signed: choose *More info → Run anyway*.

## Build it yourself

Or build it yourself:

```
git clone https://github.com/OhHelioss/lumacap
cd lumacap
cargo build --release
```
(or run `build.bat`). Requires Windows 10/11 and Rust (MSVC).

## Files
Settings and diagnostics live in `%APPDATA%\lumacap`. Recordings go to `Videos\Lumacap`, screenshots to `Pictures\Lumacap` by default.

## Notes
- Output is MP4 only (Media Foundation has no MKV muxer).
- HEVC needs a hardware encoder.

## License
MIT, see [LICENSE](LICENSE). Contributions welcome: see [CONTRIBUTING.md](CONTRIBUTING.md).
