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
- Custom global hotkeys (defaults: F9 record, F10 screenshot, F8 replay, F7 pause, F12 mute mic)
- Quality presets (Small / Balanced / High / Max), capture-card picker
- Low-disk-space warning and auto-stop, optional auto-split of long recordings (15/30/60 min)
- Minimize to tray and start with Windows
- Built-in live streaming over RTMP/RTMPS (Twitch, YouTube, Facebook, Kick or any custom server) with the GPU encoder, auto-reconnect and a live bitrate readout, no OBS needed
- Scene tab: digital zoom with pan and a webcam picture-in-picture (experimental)
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

## Streaming
Settings > Stream: pick Twitch, YouTube or Facebook (or choose Custom and type your server address, e.g. the `rtmps://...` address Kick shows in its dashboard), click **Edit** on the key row and paste your stream key, set a bitrate, then switch on **Go live**. **Test connection** checks the address and key without sending video. If the connection drops, Lumacap retries for about a minute. Your key is stored only in `%APPDATA%\lumacap\settings.ini` and is sent only to the server you chose. Streaming is new and untested on many setups, so please report problems as issues.

## Scene (experimental)
Settings > Scene: zoom (100% to 300%) with pan, and a webcam overlay in any corner. Both are applied to the preview, recordings, screenshots and streams.

## Supported capture cards
Lumacap talks to the card through the standard Windows capture APIs, so it should work with Elgato's HD60 S, HD60 S+, HD60 X, 4K60 S+, 4K X, Cam Link and similar cards, and many non-Elgato cards. It looks for the card by name and otherwise lets you pick it in Settings > Video > Capture card. Settings > Scene > Capture resolution picks 720p, 1080p, 1440p or 4K when the card offers it. Only the HD60 S has been tested so far. If something looks wrong, attach `%APPDATA%\lumacap\lumacap.log` to an issue.

## Notes
- Updates (Settings > System > Updates): **Off**, **Notify only** (default: a toast when a new release exists), or **Auto-install**. Auto-install downloads `lumacap.exe` from this repo's GitHub release, checks it against the SHA-256 GitHub publishes for that file, then swaps it in; restart Lumacap to use the new version. It never touches a running recording or stream. Only github.com is contacted and nothing about you is sent. Note the checksum comes from the same GitHub release, so this protects against corrupted downloads, not a compromised repo; the exe is unsigned. Leave it on Notify if you'd rather update by hand.
- Output is MP4 only (Media Foundation has no MKV muxer).
- HEVC needs a hardware encoder.

## License
MIT, see [LICENSE](LICENSE). Contributions welcome: see [CONTRIBUTING.md](CONTRIBUTING.md).
