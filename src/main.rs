//! Lumacap - a lightweight, open-source capture-card recorder for Windows (built and tested with the Elgato HD60 S).
//!
//! Media Foundation capture -> hardware H.264 + AAC -> MP4, live preview with
//! audio monitoring, screenshots, instant replay, settings panel.
//!
//! Hotkeys (global): F8 save replay, F9 record, F10 screenshot. F11 = fullscreen.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod h264;
mod rtmp;
mod scene;
mod stream;
mod update;

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicIsize, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use windows::core::{w, GUID, PCWSTR, PROPVARIANT, PWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::GdiPlus::{
    FillModeAlternate, GdipAddPathArc, GdipClosePathFigure, GdipCreateFromHDC, GdipCreatePath, GdipCreatePen1,
    GdipCreateSolidFill, GdipDeleteBrush, GdipDeleteGraphics, GdipDeletePath, GdipDeletePen, GdipDrawEllipse,
    GdipDrawPath, GdipFillEllipse, GdipFillPath, GdipFillRectangle, GdipSetSmoothingMode, GdiplusStartup,
    GdiplusStartupInput, GpBrush, GpGraphics, GpPath, GpPen, GpSolidFill, SmoothingModeAntiAlias, Status, UnitWorld,
};
use windows::Win32::Media::Audio::{
    waveOutClose, waveOutOpen, waveOutPrepareHeader, waveOutReset, waveOutUnprepareHeader, waveOutWrite, CALLBACK_NULL,
    HWAVEOUT, WAVEFORMATEX, WAVEHDR, WAVE_FORMAT_PCM, WAVE_MAPPER, WHDR_INQUEUE,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::{GetCurrentProcess, GetGuiResources, GR_GDIOBJECTS, GR_USEROBJECTS, GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST, THREAD_PRIORITY_ABOVE_NORMAL};
use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
use windows::Win32::System::Diagnostics::Debug::{Beep, SetUnhandledExceptionFilter, EXCEPTION_POINTERS};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::{GetLocalTime, GetTickCount64};
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::Registry::{RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ};
use windows::Win32::UI::Shell::{
    SHBrowseForFolderW, SHGetPathFromIDListW, Shell_NotifyIconW, ShellExecuteW, BIF_RETURNONLYFSDIRS, BROWSEINFOW, NIF_ICON,
    NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

type R<T> = windows::core::Result<T>;

const TIMER_ID: usize = 1;
const NO_BASE: i64 = i64::MIN;
const SEG_SECS: u64 = 5; // replay buffer segment length

// ---------- small helpers ----------

/// COM objects used across threads. The Media Foundation objects here are free-threaded (MTA).
struct Sendable<T>(T);
unsafe impl<T> Send for Sendable<T> {}
unsafe impl<T> Sync for Sendable<T> {}

fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn timestamp_name() -> String {
    unsafe {
        let t = GetLocalTime();
        format!("{:04}-{:02}-{:02}_{:02}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
    }
}

unsafe fn friendly_name(a: &IMFActivate) -> String {
    let mut p = PWSTR::null();
    let mut len = 0u32;
    if a.GetAllocatedString(&MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &mut p, &mut len).is_ok() {
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const c_void));
        s
    } else {
        String::from("?")
    }
}

unsafe fn enum_devices(audio: bool) -> R<Vec<IMFActivate>> {
    let mut attrs: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut attrs, 1)?;
    let attrs = attrs.unwrap();
    attrs.SetGUID(
        &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
        if audio {
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_AUDCAP_GUID
        } else {
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID
        },
    )?;
    let mut arr: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut n = 0u32;
    MFEnumDeviceSources(&attrs, &mut arr, &mut n)?;
    let mut out = Vec::new();
    for i in 0..n as usize {
        if let Some(a) = arr.add(i).read() {
            out.push(a);
        }
    }
    if !arr.is_null() {
        CoTaskMemFree(Some(arr as *const c_void));
    }
    Ok(out)
}

/// Append a line to %APPDATA%\lumacap\lumacap.log (and print it). Attach this file to bug reports.
pub(crate) fn log_line(s: &str) {
    use std::io::Write;
    static START: std::sync::Once = std::sync::Once::new();
    let dir = std::env::var("APPDATA").map(|a| format!("{a}\\lumacap")).unwrap_or_else(|_| ".".into());
    let _ = std::fs::create_dir_all(&dir);
    let path = format!("{dir}\\lumacap.log");
    START.call_once(|| {
        if std::fs::metadata(&path).map(|m| m.len() > 400_000).unwrap_or(false) {
            let _ = std::fs::remove_file(&path);
        }
    });
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{}s] {}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0), s);
    }
}

macro_rules! logln {
    ($($a:tt)*) => {{
        let s = format!($($a)*);
        println!("{}", s);
        log_line(&s);
    }};
}

macro_rules! elogln {
    ($($a:tt)*) => {{
        let s = format!($($a)*);
        eprintln!("{}", s);
        log_line(&s);
    }};
}

fn pick(devs: &[IMFActivate], want: Option<usize>, needles: &[&str]) -> Option<usize> {
    if let Some(i) = want {
        return (i < devs.len()).then_some(i);
    }
    for (i, d) in devs.iter().enumerate() {
        let name = unsafe { friendly_name(d) }.to_lowercase();
        if name.contains("facecam") || name.contains("wave") {
            continue; // Elgato webcams and microphones are not capture cards
        }
        if needles.iter().any(|n| name.contains(n)) {
            return Some(i);
        }
    }
    None
}

// ---------- settings (saved to %APPDATA%\lumacap\settings.ini) ----------

#[derive(Clone)]
struct Settings {
    bitrate: u32, // Mbps
    out_dir: String,
    shot_dir: String,
    volume: u32, // 0..100
    muted: bool,
    replay_on: bool,
    replay_secs: u32,
    topmost: bool,
    win_w: i32,
    win_h: i32,
    hevc: bool,
    fps30: bool,
    fmp4: bool,
    game_gain: u32, // percent, recording mix
    mic_gain: u32,  // percent
    mic_name: String,
    mic_separate: bool,
    clip_beep: bool,
    light: bool,
    accent: u32,
    hk_rec: u32,
    hk_shot: u32,
    hk_replay: u32,
    hk_pause: u32,
    split_min: u32,
    tray: bool,
    autostart: bool,
    hk_mic: u32,
    video_name: String,
    update_mode: u8, // 0 off, 1 notify, 2 auto-install
    stream_server: u32, // 0 Twitch, 1 YouTube, 2 custom, 3 Facebook
    stream_custom: String,
    stream_key: String,
    stream_kbps: u32,
    stream_fps30: bool,
    res: u32, // capture resolution: 0 auto (1080p), 1 720p, 2 1440p, 3 2160p
    zoom: u32, // index into scene::ZOOMS
    pan_x: u32,
    pan_y: u32,
    cam_name: String,
    cam_pos: u32,
    cam_size: u32,
}

fn settings_path() -> String {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    format!("{base}\\lumacap\\settings.ini")
}

impl Settings {
    fn default_new() -> Self {
        let profile = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into());
        Settings {
            bitrate: 20,
            out_dir: format!("{profile}\\Videos\\Lumacap"),
            shot_dir: format!("{profile}\\Pictures\\Lumacap"),
            volume: 60,
            muted: false,
            replay_on: false,
            replay_secs: 30,
            topmost: false,
            win_w: 0,
            win_h: 0,
            hevc: false,
            fps30: false,
            fmp4: false,
            game_gain: 100,
            mic_gain: 100,
            mic_name: String::new(),
            mic_separate: false,
            clip_beep: true,
            light: false,
            accent: 0,
            hk_rec: 0x78,
            hk_shot: 0x79,
            hk_replay: 0x77,
            hk_pause: 0x76,
            split_min: 0,
            tray: false,
            autostart: false,
            hk_mic: 0x7B,
            video_name: String::new(),
            update_mode: 1,
            stream_server: 0,
            stream_custom: String::new(),
            stream_key: String::new(),
            stream_kbps: 6000,
            stream_fps30: false,
            res: 0,
            zoom: 0,
            pan_x: 50,
            pan_y: 50,
            cam_name: String::new(),
            cam_pos: 3,
            cam_size: 1,
        }
    }

    fn load() -> Self {
        let mut s = Self::default_new();
        let old_path = settings_path().replace("\\lumacap\\", "\\hd60rec\\");
        if let Ok(txt) = std::fs::read_to_string(settings_path()).or_else(|_| std::fs::read_to_string(&old_path)) {
            for line in txt.lines() {
                let Some((k, v)) = line.split_once('=') else { continue };
                let v = v.trim();
                match k.trim() {
                    "bitrate" => s.bitrate = v.parse().unwrap_or(s.bitrate).clamp(2, 100),
                    "out_dir" if !v.is_empty() => s.out_dir = v.to_string(),
                    "shot_dir" if !v.is_empty() => s.shot_dir = v.to_string(),
                    "volume" => s.volume = v.parse().unwrap_or(s.volume).min(100),
                    "muted" => s.muted = v == "1",
                    "replay_on" => s.replay_on = v == "1",
                    "replay_secs" => s.replay_secs = v.parse().unwrap_or(s.replay_secs).clamp(10, 300),
                    "topmost" => s.topmost = v == "1",
                    "win_w" => s.win_w = v.parse().unwrap_or(0),
                    "win_h" => s.win_h = v.parse().unwrap_or(0),
                    "hevc" => s.hevc = v == "1",
                    "fps30" => s.fps30 = v == "1",
                    "fmp4" => s.fmp4 = v == "1",
                    "game_gain" => s.game_gain = v.parse().unwrap_or(100).min(200),
                    "mic_gain" => s.mic_gain = v.parse().unwrap_or(100).min(300),
                    "mic_name" => s.mic_name = v.to_string(),
                    "mic_separate" => s.mic_separate = v == "1",
                    "clip_beep" => s.clip_beep = v != "0",
                    "light" => s.light = v == "1",
                    "accent" => s.accent = v.parse().unwrap_or(0).min(5),
                    "hk_rec" => s.hk_rec = v.parse().unwrap_or(s.hk_rec),
                    "hk_shot" => s.hk_shot = v.parse().unwrap_or(s.hk_shot),
                    "hk_replay" => s.hk_replay = v.parse().unwrap_or(s.hk_replay),
                    "hk_pause" => s.hk_pause = v.parse().unwrap_or(s.hk_pause),
                    "split_min" => s.split_min = v.parse().unwrap_or(0),
                    "tray" => s.tray = v == "1",
                    "autostart" => s.autostart = v == "1",
                    "hk_mic" => s.hk_mic = v.parse().unwrap_or(s.hk_mic),
                    "video_name" => s.video_name = v.to_string(),
                    "update_check" => s.update_mode = if v == "0" { 0 } else { 1 },
                    "update_mode" => s.update_mode = if v.parse::<u8>().unwrap_or(1) == 0 { 0 } else { 1 },
                    "stream_server" => s.stream_server = v.parse().unwrap_or(0).min(3),
                    "stream_custom" => s.stream_custom = v.to_string(),
                    "stream_key" => s.stream_key = v.to_string(),
                    "stream_kbps" => s.stream_kbps = v.parse().unwrap_or(6000).clamp(1000, 12000),
                    "stream_fps30" => s.stream_fps30 = v == "1",
                    "res" => s.res = v.parse().unwrap_or(0).min(3),
                    "zoom" => s.zoom = v.parse().unwrap_or(0).min(4),
                    "pan_x" => s.pan_x = v.parse().unwrap_or(50).min(100),
                    "pan_y" => s.pan_y = v.parse().unwrap_or(50).min(100),
                    "cam_name" => s.cam_name = v.to_string(),
                    "cam_pos" => s.cam_pos = v.parse().unwrap_or(3).min(3),
                    "cam_size" => s.cam_size = v.parse().unwrap_or(1).min(2),
                    _ => {}
                }
            }
        }
        s
    }

    fn save(&self) {
        let p = settings_path();
        if let Some(dir) = std::path::Path::new(&p).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let txt = format!(
            "bitrate={}\nout_dir={}\nshot_dir={}\nvolume={}\nmuted={}\nreplay_on={}\nreplay_secs={}\ntopmost={}\nwin_w={}\nwin_h={}\nhevc={}\nfps30={}\nfmp4={}\ngame_gain={}\nmic_gain={}\nmic_name={}\nmic_separate={}\nclip_beep={}\nlight={}\naccent={}\nhk_rec={}\nhk_shot={}\nhk_replay={}\nhk_pause={}\nsplit_min={}\ntray={}\nautostart={}\nhk_mic={}\nvideo_name={}\nupdate_mode={}\nstream_server={}\nstream_custom={}\nstream_key={}\nstream_kbps={}\nstream_fps30={}\nres={}\nzoom={}\npan_x={}\npan_y={}\ncam_name={}\ncam_pos={}\ncam_size={}\n",
            self.bitrate,
            self.out_dir,
            self.shot_dir,
            self.volume,
            self.muted as u8,
            self.replay_on as u8,
            self.replay_secs,
            self.topmost as u8,
            self.win_w,
            self.win_h,
            self.hevc as u8,
            self.fps30 as u8,
            self.fmp4 as u8,
            self.game_gain,
            self.mic_gain,
            self.mic_name,
            self.mic_separate as u8,
            self.clip_beep as u8,
            self.light as u8,
            self.accent,
            self.hk_rec,
            self.hk_shot,
            self.hk_replay,
            self.hk_pause,
            self.split_min,
            self.tray as u8,
            self.autostart as u8,
            self.hk_mic,
            self.video_name,
            self.update_mode,
            self.stream_server,
            self.stream_custom,
            self.stream_key,
            self.stream_kbps,
            self.stream_fps30 as u8,
            self.res,
            self.zoom,
            self.pan_x,
            self.pan_y,
            self.cam_name,
            self.cam_pos,
            self.cam_size
        );
        let _ = std::fs::write(p, txt);
    }
}

// ---------- capture setup ----------

#[derive(Clone, Copy)]
struct VideoFmt {
    w: u32,
    h: u32,
    fps: u32,
}

/// Open the video device, choose a mode close to 1080p/wanted fps, output NV12 (own converter if needed).
static MODE: Mutex<String> = Mutex::new(String::new());
static LIGHT: AtomicBool = AtomicBool::new(false);
static ACCENT_IDX: AtomicU32 = AtomicU32::new(0);
static MOVING: AtomicBool = AtomicBool::new(false);
static LAST_PAINT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
const ACCENTS: [(&str, (u8, u8, u8)); 6] = [
    ("Blue", (92, 124, 255)),
    ("Violet", (150, 104, 255)),
    ("Teal", (20, 184, 166)),
    ("Green", (52, 199, 120)),
    ("Orange", (255, 149, 64)),
    ("Pink", (240, 98, 160)),
];

fn accent_col() -> COLORREF {
    let (r, g, b) = ACCENTS[(ACCENT_IDX.load(Ordering::Relaxed) as usize).min(5)].1;
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}

/// Light theme: maps the dark palette (neutral colours only) onto a light one; saturated colours pass through.
fn th(c: COLORREF) -> COLORREF {
    if !LIGHT.load(Ordering::Relaxed) {
        return c;
    }
    let (r, g, b) = (c.0 & 0xFF, (c.0 >> 8) & 0xFF, (c.0 >> 16) & 0xFF);
    let (mx, mn) = (r.max(g).max(b), r.min(g).min(b));
    if mx - mn > 45 || mx >= 248 {
        return c;
    }
    let v = ((r + g + b) / 3) as f32;
    const PTS: [(f32, f32); 12] = [(0.0, 190.0), (10.0, 190.0), (14.0, 238.0), (26.0, 255.0), (38.0, 236.0), (52.0, 222.0), (62.0, 214.0), (80.0, 176.0), (106.0, 150.0), (139.0, 110.0), (236.0, 24.0), (248.0, 24.0)];
    let mut t = 24.0f32;
    for w in PTS.windows(2) {
        if v <= w[1].0 {
            let k = (v - w[0].0) / (w[1].0 - w[0].0).max(0.001);
            t = w[0].1 + (w[1].1 - w[0].1) * k;
            break;
        }
    }
    let t = t.clamp(0.0, 255.0);
    COLORREF((t - 2.0).max(0.0) as u32 | ((t - 1.0).max(0.0) as u32) << 8 | ((t + 1.0).min(255.0) as u32) << 16)
}

fn paint_allowed() -> bool {
    if !MOVING.load(Ordering::Relaxed) {
        return true;
    }
    let now = unsafe { GetTickCount64() };
    if now.saturating_sub(LAST_PAINT.load(Ordering::Relaxed)) >= 80 {
        LAST_PAINT.store(now, Ordering::Relaxed);
        true
    } else {
        false
    }
}

unsafe fn apply_titlebar(hwnd: HWND, light: bool) {
    let dark: i32 = if light { 0 } else { 1 };
    let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const i32 as *const c_void, 4);
}

static MON_PLAYING: AtomicBool = AtomicBool::new(false);
static PRESENTS: AtomicU32 = AtomicU32::new(0);
static PRESENT_US: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static PAINTS: AtomicU32 = AtomicU32::new(0);
static T_DRAW: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static T_CHIPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static T_BLT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static PV_LOGGED: AtomicU32 = AtomicU32::new(0);
static LAST_FPS_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static HK_CAPTURE: AtomicI32 = AtomicI32::new(-1);
static QUITTING: AtomicBool = AtomicBool::new(false);
static HIDDEN: AtomicBool = AtomicBool::new(false);
static LOW_WARN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
const WM_TRAY: u32 = WM_APP + 2;

/// A hotkey is stored as one number: low 8 bits = virtual key, bit 8 = Alt, bit 9 = Ctrl, bit 10 = Shift.
/// Old settings files hold a bare virtual key, which decodes as "no modifiers".
const HK_ALT: u32 = 1 << 8;
const HK_CTRL: u32 = 1 << 9;
const HK_SHIFT: u32 = 1 << 10;

fn vk_base_name(vk: u32) -> String {
    match vk {
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        0x60..=0x69 => format!("Numpad {}", vk - 0x60),
        0x6A => "Numpad *".into(),
        0x6B => "Numpad +".into(),
        0x6D => "Numpad -".into(),
        0x6E => "Numpad .".into(),
        0x6F => "Numpad /".into(),
        0x30..=0x39 | 0x41..=0x5A => ((vk as u8) as char).to_string(),
        0x2D => "Insert".into(),
        0x2E => "Delete".into(),
        0x24 => "Home".into(),
        0x23 => "End".into(),
        0x21 => "Page Up".into(),
        0x22 => "Page Down".into(),
        0x25 => "Left".into(),
        0x26 => "Up".into(),
        0x27 => "Right".into(),
        0x28 => "Down".into(),
        0x20 => "Space".into(),
        0x09 => "Tab".into(),
        0x0D => "Enter".into(),
        0x08 => "Backspace".into(),
        0x13 => "Pause".into(),
        0x91 => "Scroll Lock".into(),
        0xBA => ";".into(),
        0xBB => "=".into(),
        0xBC => ",".into(),
        0xBD => "-".into(),
        0xBE => ".".into(),
        0xBF => "/".into(),
        0xC0 => "`".into(),
        0xDB => "[".into(),
        0xDC => "\\".into(),
        0xDD => "]".into(),
        0xDE => "'".into(),
        _ => format!("Key {vk}"),
    }
}

fn vk_name(code: u32) -> String {
    let mut s = String::new();
    if code & HK_CTRL != 0 {
        s.push_str("Ctrl+");
    }
    if code & HK_ALT != 0 {
        s.push_str("Alt+");
    }
    if code & HK_SHIFT != 0 {
        s.push_str("Shift+");
    }
    s.push_str(&vk_base_name(code & 0xFF));
    s
}

/// Keys that exist on the keyboard and make sense as a hotkey.
fn vk_known(vk: u32) -> bool {
    matches!(vk, 0x70..=0x87 | 0x60..=0x6F | 0x30..=0x39 | 0x41..=0x5A | 0x2D | 0x2E | 0x24 | 0x23 | 0x21 | 0x22
        | 0x25..=0x28 | 0x20 | 0x09 | 0x0D | 0x08 | 0x13 | 0x91 | 0xBA..=0xC0 | 0xDB..=0xDE)
}

/// Checks a key plus modifiers. Err carries the message shown to the user.
fn hotkey_check(vk: u32, mods: u32) -> Result<(), &'static str> {
    if !vk_known(vk) {
        return Err("That key can't be used as a hotkey");
    }
    let alt = mods & HK_ALT != 0;
    let ctrl = mods & HK_CTRL != 0;
    let shift = mods & HK_SHIFT != 0;
    // keys that are safe on their own (they don't type anything)
    let bare_ok = matches!(vk, 0x70..=0x79 | 0x7B..=0x87 | 0x60..=0x6F | 0x2D | 0x24 | 0x23 | 0x21 | 0x22 | 0x13 | 0x91);
    if mods == 0 && !bare_ok {
        return Err("Add Ctrl or Alt for this key (a bare key would stop it working in other apps)");
    }
    if mods == HK_SHIFT && !bare_ok {
        return Err("Add Ctrl or Alt: Shift alone would break typing capitals");
    }
    if vk == 0x7A && mods == 0 {
        return Err("F11 is fullscreen in Lumacap; add Ctrl, Alt or Shift or pick another key");
    }
    if alt && matches!(vk, 0x73 | 0x09 | 0x20 | 0x1B) {
        return Err("Windows uses that combo (Alt+F4, Alt+Tab, Alt+Space)");
    }
    if ctrl && alt && vk == 0x2E {
        return Err("Ctrl+Alt+Delete belongs to Windows");
    }
    if ctrl && !alt && !shift && matches!(vk, 0x43 | 0x56 | 0x58 | 0x41 | 0x5A | 0x59) {
        return Err("Ctrl+C/V/X/A/Z/Y are copy, paste and undo everywhere; pick another combo");
    }
    Ok(())
}

fn hk_register(hwnd: HWND, id: i32, code: u32) -> bool {
    let m = HOT_KEY_MODIFIERS(((code >> 8) & 7) | MOD_NOREPEAT.0);
    unsafe { RegisterHotKey(hwnd, id, m, code & 0xFF).is_ok() }
}

unsafe fn register_hotkeys(hwnd: HWND, c: &Settings) {
    for (id, vk) in [(1, c.hk_rec), (2, c.hk_shot), (3, c.hk_replay), (4, c.hk_pause), (5, c.hk_mic)] {
        let _ = UnregisterHotKey(hwnd, id);
        if !hk_register(hwnd, id, vk) {
            elogln!("{} is taken by another app; use the on-screen buttons.", vk_name(vk));
        }
    }
}

unsafe fn set_hotkey(hwnd: HWND, idx: i32, vk: u32) {
    let sh = ui().sh.clone();
    HK_CAPTURE.store(-1, Ordering::Relaxed);
    let mut c = sh.cfg.lock().unwrap();
    let cur = [c.hk_rec, c.hk_shot, c.hk_replay, c.hk_pause, c.hk_mic];
    if cur.iter().enumerate().any(|(i, k)| i as i32 != idx && *k == vk) {
        drop(c);
        set_toast(&sh, "That key is already used by another action");
        return;
    }
    let old = cur[idx as usize];
    let set = |c: &mut Settings, v: u32| match idx {
        0 => c.hk_rec = v,
        1 => c.hk_shot = v,
        2 => c.hk_replay = v,
        3 => c.hk_pause = v,
        _ => c.hk_mic = v,
    };
    set(&mut c, vk);
    let id = idx + 1;
    let _ = UnregisterHotKey(hwnd, id);
    if !hk_register(hwnd, id, vk) {
        set(&mut c, old);
        let _ = hk_register(hwnd, id, old);
        drop(c);
        set_toast(&sh, "That key is taken by another app");
        return;
    }
    c.save();
}

/// Latest release tag from GitHub (e.g. "v0.1.3"), or None when offline.

fn version_tuple(v: &str) -> Vec<u32> {
    v.trim_start_matches('v').split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

fn free_bytes(dir: &str) -> Option<u64> {
    let wd = wide(dir);
    let mut avail = 0u64;
    unsafe {
        GetDiskFreeSpaceExW(PCWSTR(wd.as_ptr()), Some(&mut avail), None, None).ok()?;
    }
    Some(avail)
}

fn stop_recording() -> bool {
    let sh = ui().sh.clone();
    let rec = sh.rec.lock().unwrap().take();
    match rec {
        Some(r) => {
            finalize_when_free(r);
            true
        }
        None => false,
    }
}

/// Runs about once a second: disk-space guard and automatic file splitting.
fn maintenance() {
    stream::check_ended();
    let sh = ui().sh.clone();
    let rec = sh.rec.lock().unwrap().clone();
    let Some(r) = rec else { return };
    let c = sh.cfg.lock().unwrap().clone();
    if let Some(free) = free_bytes(&c.out_dir) {
        if free < 300 << 20 {
            stop_recording();
            set_toast(&sh, "Recording stopped: the disk is almost full");
            return;
        }
        if free < 2 << 30 {
            let now = unsafe { GetTickCount64() };
            if now.saturating_sub(LOW_WARN.load(Ordering::Relaxed)) > 30_000 {
                LOW_WARN.store(now, Ordering::Relaxed);
                set_toast(&sh, &format!("Low disk space: {:.1} GB free", free as f64 / 1_073_741_824.0));
            }
        }
    }
    if c.split_min > 0 && !r.paused.load(Ordering::SeqCst) && rec_elapsed_secs(&r) >= c.split_min as u64 * 60 {
        if stop_recording() {
            start_recording();
            set_toast(&sh, "Started a new recording part");
        }
    }
}

unsafe fn tray_nid(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW::default();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    let hinst: HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();
    nid.hIcon = LoadIconW(hinst, PCWSTR(1usize as *const u16)).unwrap_or_default();
    for (i, c) in "Lumacap".encode_utf16().enumerate().take(127) {
        nid.szTip[i] = c;
    }
    nid
}

unsafe fn tray_add(hwnd: HWND) {
    let nid = tray_nid(hwnd);
    let _ = Shell_NotifyIconW(NIM_ADD, &nid);
}

unsafe fn tray_remove(hwnd: HWND) {
    let nid = tray_nid(hwnd);
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
}

unsafe fn hide_to_tray(hwnd: HWND) {
    let _ = ShowWindow(hwnd, SW_HIDE);
    HIDDEN.store(true, Ordering::Relaxed);
}

unsafe fn show_from_tray(hwnd: HWND) {
    HIDDEN.store(false, Ordering::Relaxed);
    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = ShowWindow(hwnd, SW_RESTORE);
    let _ = SetForegroundWindow(hwnd);
}

unsafe fn tray_menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else { return };
    let recording = ui().sh.rec.lock().unwrap().is_some();
    let _ = AppendMenuW(menu, MF_STRING, 1, w!("Show window"));
    let _ = AppendMenuW(menu, MF_STRING, 2, if recording { w!("Stop recording") } else { w!("Start recording") });
    let _ = AppendMenuW(menu, MF_STRING, 3, w!("Save replay"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, 4, w!("Quit"));
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, None);
    let _ = DestroyMenu(menu);
    match cmd.0 {
        1 => show_from_tray(hwnd),
        2 => toggle_record(),
        3 => save_replay(ui().sh.clone()),
        4 => {
            QUITTING.store(true, Ordering::Relaxed);
            let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        _ => {}
    }
}

unsafe fn set_autostart(on: bool) {
    let mut key = HKEY::default();
    let sub = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    if RegCreateKeyExW(HKEY_CURRENT_USER, sub, 0, PCWSTR::null(), REG_OPTION_NON_VOLATILE, KEY_WRITE, None, &mut key, None).0 != 0 {
        return;
    }
    if on {
        if let Ok(exe) = std::env::current_exe() {
            let val = wide(&format!("\"{}\" --minimized", exe.display()));
            let bytes = std::slice::from_raw_parts(val.as_ptr() as *const u8, val.len() * 2);
            let _ = RegSetValueExW(key, w!("Lumacap"), 0, REG_SZ, Some(bytes));
        }
    } else {
        let _ = RegDeleteValueW(key, w!("Lumacap"));
    }
    let _ = RegCloseKey(key);
}

static LAST_CLICK: AtomicI32 = AtomicI32::new(0);
static LAST_MSG: AtomicU32 = AtomicU32::new(0);
static HWND_RAW: AtomicIsize = AtomicIsize::new(0);
static FRAME_PENDING: AtomicBool = AtomicBool::new(false);
const WM_FRAME: u32 = WM_APP + 1;

fn post_frame_msg() {
    let hw = HWND_RAW.load(Ordering::Relaxed);
    if hw != 0 && !FRAME_PENDING.swap(true, Ordering::Relaxed) {
        unsafe {
            let _ = PostMessageW(HWND(hw as *mut c_void), WM_FRAME, WPARAM(0), LPARAM(0));
        }
    }
}

unsafe fn open_video(act: &IMFActivate, want_fps: u32, target_h: i64) -> R<(IMFSourceReader, VideoFmt, bool)> {
    let source: IMFMediaSource = act.ActivateObject()?;
    let mut attrs: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut attrs, 3)?;
    let attrs = attrs.unwrap();
    attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?;
    let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
    let reader = MFCreateSourceReaderFromMediaSource(&source, &attrs)?;
    let vs = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

    let mut cands: Vec<(i64, IMFMediaType, VideoFmt, String, bool)> = Vec::new();
    let mut i = 0;
    while let Ok(mt) = reader.GetNativeMediaType(vs, i) {
        i += 1;
        let size = mt.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
        let rate = mt.GetUINT64(&MF_MT_FRAME_RATE).unwrap_or(0);
        let (w, h) = ((size >> 32) as u32, size as u32);
        let (rn, rd) = ((rate >> 32) as u32, (rate as u32).max(1));
        let fps = if rn / rd >= 2 { rn / rd } else { want_fps }; // card often reports no frame rate
        let sub = mt.GetGUID(&MF_MT_SUBTYPE).unwrap_or_default();
        let cc: String =
            sub.data1.to_le_bytes().iter().map(|&b| if b.is_ascii_graphic() { b as char } else { '?' }).collect();
        logln!("  native mode {i}: {w}x{h} @ {fps} fps  {cc}");
        if w == 0 || h == 0 {
            continue;
        }
        let mut score = 0i64;
        score -= (target_h - h as i64).abs() * 10;
        score -= (want_fps as i64 - fps as i64).abs() * 50;
        if sub == MFVideoFormat_NV12 {
            score += 40;
        } else if sub.data1 == 0x32595559 {
            score += 30; // YUY2
        } else {
            score -= 3000; // compressed modes decode badly when USB drops a packet
        }
        cands.push((score, mt, VideoFmt { w, h, fps }, cc, sub.data1 == 0x32595559));
    }
    cands.sort_by(|a, b| b.0.cmp(&a.0));
    let mut fmt = None;
    let mut cpu = false;
    for (_, native, f, cc, is_yuy2) in cands {
        if let Err(e) = reader.SetCurrentMediaType(vs, None, &native) {
            logln!("  skip {}x{}@{} {cc}: select failed: {e}", f.w, f.h, f.fps);
            continue;
        }
        let out = MFCreateMediaType()?;
        native.CopyAllItems(&out)?;
        out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        out.SetUINT64(&MF_MT_FRAME_SIZE, pack(f.w, f.h))?;
        out.SetUINT64(&MF_MT_FRAME_RATE, pack(f.fps, 1))?;
        match reader.SetCurrentMediaType(vs, None, &out) {
            Ok(()) => {
                logln!("  using {}x{}@{} {cc} -> NV12 (Windows converter)", f.w, f.h, f.fps);
                *MODE.lock().unwrap() = format!("{}x{} @ {} fps, {} via Windows converter", f.w, f.h, f.fps, cc);
                fmt = Some(f);
                break;
            }
            Err(e) => {
                logln!("  {}x{}@{} {cc}: Windows NV12 convert failed: {e}", f.w, f.h, f.fps);
                if is_yuy2 {
                    reader.SetCurrentMediaType(vs, None, &native)?;
                    logln!("  using {}x{}@{} YUY2 -> NV12 (own CPU converter)", f.w, f.h, f.fps);
                    *MODE.lock().unwrap() = format!("{}x{} @ {} fps, YUY2 via own converter", f.w, f.h, f.fps);
                    fmt = Some(f);
                    cpu = true;
                    break;
                }
            }
        }
    }
    let fmt = fmt.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?;
    Ok((reader, fmt, cpu))
}

#[derive(Clone, Copy, Debug)]
struct AudioFmt {
    float: bool,
    rate: u32,
    ch: u32,
    bits: u32,
}

unsafe fn pcm_type(f: &AudioFmt) -> R<IMFMediaType> {
    let mt = MFCreateMediaType()?;
    mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
    mt.SetGUID(&MF_MT_SUBTYPE, if f.float { &MFAudioFormat_Float } else { &MFAudioFormat_PCM })?;
    mt.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, f.ch)?;
    mt.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, f.rate)?;
    mt.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, f.bits)?;
    mt.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, f.ch * f.bits / 8)?;
    mt.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, f.rate * f.ch * f.bits / 8)?;
    Ok(mt)
}

/// Try PCM16 / float at 48k then 44.1k, finally the device's own format.
unsafe fn open_audio(act: &IMFActivate) -> R<(IMFSourceReader, AudioFmt)> {
    let source: IMFMediaSource = act.ActivateObject()?;
    let mut attrs: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut attrs, 1)?;
    let attrs = attrs.unwrap();
    let reader = MFCreateSourceReaderFromMediaSource(&source, &attrs)?;
    let a = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
    let tries = [
        AudioFmt { float: false, rate: 48000, ch: 2, bits: 16 },
        AudioFmt { float: true, rate: 48000, ch: 2, bits: 32 },
        AudioFmt { float: false, rate: 44100, ch: 2, bits: 16 },
        AudioFmt { float: true, rate: 44100, ch: 2, bits: 32 },
    ];
    for f in tries {
        if reader.SetCurrentMediaType(a, None, &pcm_type(&f)?).is_ok() {
            logln!("audio format: {:?}", f);
            return Ok((reader, f));
        }
    }
    // last resort: whatever the device natively delivers
    let native = reader.GetNativeMediaType(a, 0)?;
    let f = AudioFmt {
        float: native.GetGUID(&MF_MT_SUBTYPE).unwrap_or_default() == MFAudioFormat_Float,
        rate: native.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND).unwrap_or(48000),
        ch: native.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).unwrap_or(2),
        bits: native.GetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE).unwrap_or(16),
    };
    reader.SetCurrentMediaType(a, None, &native)?;
    logln!("audio format (native): {:?}", f);
    Ok((reader, f))
}

// ---------- recorder (sink writer) ----------

struct Recorder {
    writer: Sendable<IMFSinkWriter>,
    video_idx: u32,
    audio_idx: Option<u32>,
    mic: Option<(Sendable<IMFSinkWriter>, u32)>, // optional separate mic-only file
    base: AtomicI64,   // first video timestamp (device clock)
    v_wall: AtomicI64, // wall clock when first video frame arrived
    a_base: AtomicI64, // first audio timestamp (device clock)
    a_off: AtomicI64,  // audio start offset relative to video start
    m_base: AtomicI64,
    // pause / resume bookkeeping
    paused: AtomicBool,
    pause_start: Mutex<Option<Instant>>,
    paused_ms: AtomicI64,
    v_resume: AtomicBool,
    a_resume: AtomicBool,
    m_resume: AtomicBool,
    v_last_end: AtomicI64,
    a_last_end: AtomicI64,
    m_last_end: AtomicI64,
    v_skip: AtomicI64,
    a_skip: AtomicI64,
    m_skip: AtomicI64,
    half: bool, // 30 fps output from a 60 fps capture
    fcount: AtomicU32,
    // diagnostics for lumacap.log
    v_dur: i64,           // nominal output frame duration (100 ns)
    v_next: AtomicI64,    // timestamp the next frame should get (-1 = not started)
    d_early: AtomicU32,   // frames dropped because they arrived early
    d_nonmono: AtomicU32, // source timestamps that went backwards
    d_mingap_us: AtomicI64,
    d_frames: AtomicU32,
    d_errs: AtomicU32,
    d_gaps: AtomicU32,    // frame gaps over 50 ms
    d_maxgap_us: AtomicI64,
    d_maxwrite_us: AtomicI64,
    d_prev_ts: AtomicI64,
    d_last_t: AtomicI64,
    d_first_err: Mutex<String>,
    path: String,
    started: Instant,
}

#[derive(Clone, Default)]
struct RecOpts {
    hevc: bool,
    fmp4: bool,
    half: bool,
    mic_path: Option<String>,
    mic_fmt: Option<AudioFmt>,
}

fn rec_elapsed_secs(r: &Recorder) -> u64 {
    let total = r.started.elapsed().as_millis() as i64;
    let mut paused = r.paused_ms.load(Ordering::Relaxed);
    if let Some(t) = *r.pause_start.lock().unwrap() {
        paused += t.elapsed().as_millis() as i64;
    }
    ((total - paused).max(0) / 1000) as u64
}

unsafe fn aac_stream(writer: &IMFSinkWriter, af: &AudioFmt) -> R<u32> {
    let aout = MFCreateMediaType()?;
    aout.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
    aout.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
    aout.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
    aout.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, af.rate)?;
    aout.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, if af.ch == 1 { 1 } else { 2 })?;
    aout.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 24000)?; // 192 kbps
    aout.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?;
    let idx = writer.AddStream(&aout)?;
    writer.SetInputMediaType(idx, &pcm_type(af)?, None)?;
    Ok(idx)
}

unsafe fn new_writer(path: &str, container: &windows::core::GUID, hw: bool) -> R<IMFSinkWriter> {
    let mut attrs: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut attrs, 4)?;
    let attrs = attrs.unwrap();
    if hw {
        attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
    }
    attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
    attrs.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, container)?;
    let wp = wide(path);
    MFCreateSinkWriterFromURL(PCWSTR(wp.as_ptr()), None, &attrs)
}

unsafe fn start_recorder(path: &str, f: &VideoFmt, bitrate_bps: u32, afmt: Option<&AudioFmt>, o: &RecOpts) -> R<Recorder> {
    let container = if o.fmp4 { &MFTranscodeContainerType_FMPEG4 } else { &MFTranscodeContainerType_MPEG4 };
    let writer = new_writer(path, container, true)?;
    let fps = if o.half { (f.fps / 2).max(1) } else { f.fps };

    let vout = MFCreateMediaType()?;
    vout.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    vout.SetGUID(&MF_MT_SUBTYPE, if o.hevc { &MFVideoFormat_HEVC } else { &MFVideoFormat_H264 })?;
    vout.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps)?;
    vout.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    vout.SetUINT64(&MF_MT_FRAME_SIZE, pack(f.w, f.h))?;
    vout.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
    vout.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
    if !o.hevc {
        vout.SetUINT32(&MF_MT_MPEG2_PROFILE, 100)?;
    }
    let video_idx = writer.AddStream(&vout)?;

    let vin = MFCreateMediaType()?;
    vin.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    vin.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
    vin.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    vin.SetUINT64(&MF_MT_FRAME_SIZE, pack(f.w, f.h))?;
    vin.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
    vin.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
    writer.SetInputMediaType(video_idx, &vin, None)?;

    let audio_idx = match afmt {
        Some(af) => Some(aac_stream(&writer, af)?),
        None => None,
    };
    writer.BeginWriting()?;

    // optional separate mic-only file
    let mic = match (&o.mic_path, &o.mic_fmt) {
        (Some(mp), Some(mf)) => {
            let mw = new_writer(mp, &MFTranscodeContainerType_MPEG4, false)?;
            let mi = aac_stream(&mw, mf)?;
            mw.BeginWriting()?;
            Some((Sendable(mw), mi))
        }
        _ => None,
    };

    Ok(Recorder {
        writer: Sendable(writer),
        video_idx,
        audio_idx,
        mic,
        base: AtomicI64::new(NO_BASE),
        v_wall: AtomicI64::new(NO_BASE),
        a_base: AtomicI64::new(NO_BASE),
        a_off: AtomicI64::new(0),
        m_base: AtomicI64::new(NO_BASE),
        paused: AtomicBool::new(false),
        pause_start: Mutex::new(None),
        paused_ms: AtomicI64::new(0),
        v_resume: AtomicBool::new(false),
        a_resume: AtomicBool::new(false),
        m_resume: AtomicBool::new(false),
        v_last_end: AtomicI64::new(0),
        a_last_end: AtomicI64::new(0),
        m_last_end: AtomicI64::new(0),
        v_skip: AtomicI64::new(0),
        a_skip: AtomicI64::new(0),
        m_skip: AtomicI64::new(0),
        half: o.half,
        fcount: AtomicU32::new(0),
        v_dur: 10_000_000 / fps.max(1) as i64,
        v_next: AtomicI64::new(-1),
        d_early: AtomicU32::new(0),
        d_nonmono: AtomicU32::new(0),
        d_mingap_us: AtomicI64::new(i64::MAX),
        d_frames: AtomicU32::new(0),
        d_errs: AtomicU32::new(0),
        d_gaps: AtomicU32::new(0),
        d_maxgap_us: AtomicI64::new(0),
        d_maxwrite_us: AtomicI64::new(0),
        d_prev_ts: AtomicI64::new(-1),
        d_last_t: AtomicI64::new(0),
        d_first_err: Mutex::new(String::new()),
        path: path.to_string(),
        started: Instant::now(),
    })
}

/// Wait until nobody else is writing to this recorder, then finalize the file(s).
fn finalize_when_free(rec: Arc<Recorder>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        for _ in 0..100 {
            if Arc::strong_count(&rec) <= 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        unsafe {
            let fin = rec.writer.0.Finalize();
            let wall = rec.started.elapsed().as_secs_f64().max(0.001);
            let fr = rec.d_frames.load(Ordering::Relaxed);
            if !rec.path.contains("\\seg_") { logln!(
                "recording stats: {} frames in {:.1}s wall ({:.1} fps), video ends at {:.1}s, capture gap min {} us / max {} ms, gaps over 50 ms: {}, backwards timestamps: {}, dropped early: {}, slowest WriteSample {} ms, write errors: {}{}, finalize: {:?}",
                fr,
                wall,
                fr as f64 / wall,
                rec.d_last_t.load(Ordering::Relaxed) as f64 / 1e7,
                { let m = rec.d_mingap_us.load(Ordering::Relaxed); if m == i64::MAX { 0 } else { m } },
                rec.d_maxgap_us.load(Ordering::Relaxed) / 1000,
                rec.d_gaps.load(Ordering::Relaxed),
                rec.d_nonmono.load(Ordering::Relaxed),
                rec.d_early.load(Ordering::Relaxed),
                rec.d_maxwrite_us.load(Ordering::Relaxed) / 1000,
                rec.d_errs.load(Ordering::Relaxed),
                { let e = rec.d_first_err.lock().unwrap(); if e.is_empty() { String::new() } else { format!(" (first: {})", *e) } },
                fin.is_ok()
            ); }
            if let Some((mw, _)) = &rec.mic {
                let _ = mw.0.Finalize();
            }
        }
    })
}

// ---------- shared state ----------

struct Preview {
    w: usize,
    h: usize,
    data: Vec<u32>, // BGRA
}

struct ReplayBuf {
    cur: Option<Arc<Recorder>>,
    cur_started: Instant,
    next: Arc<Mutex<Option<Recorder>>>,
    preparing: Arc<AtomicBool>,
    done: VecDeque<String>,
}

struct Shared {
    stream: Mutex<Option<Arc<stream::StreamCtl>>>,
    rec: Mutex<Option<Arc<Recorder>>>,
    replay: Mutex<Option<ReplayBuf>>,
    cfg: Mutex<Settings>,
    preview: Mutex<Preview>,
    pv_full: AtomicBool, // true: preview at full capture resolution (big window)
    running: AtomicBool,
    frames: AtomicU32,
    fmt: VideoFmt,
    yuy2: bool,
    afmt: Option<AudioFmt>,
    audio_note: Mutex<String>,
    shot_req: AtomicBool,
    toast: Mutex<(String, Instant)>,
    fps_meas: AtomicU32,
    last_frames: AtomicU32,
    last_flags: AtomicU32,
    peak: AtomicU32, // latest audio chunk peak, 0..32768
    pv_slot: Mutex<Option<(Sendable<IMFSample>, bool)>>,
    pv_cv: Condvar,
    glitches: AtomicU32,
    mic_peak: AtomicU32,
    mic_buf: Mutex<VecDeque<i16>>, // stereo i16 samples waiting to be mixed
    mic_gen: AtomicU32,
    mic_idx: AtomicI32, // -1 = off
    mic_fmt: Mutex<Option<AudioFmt>>,
    mic_err: Mutex<String>,
    mic_names: Vec<String>,
    video_names: Vec<String>,
    mic_muted: AtomicBool,
    adevs: Vec<Sendable<IMFActivate>>,
    game_gain: AtomicU32, // percent
    mic_gain: AtomicU32,  // percent
    vol: AtomicU32,  // 0..100
    muted: AtomicBool,
}

fn set_toast(sh: &Shared, msg: &str) {
    *sh.toast.lock().unwrap() = (msg.to_string(), Instant::now());
}

/// A second IMFSample that shares the same buffer, so two writers can have their own timestamps.
unsafe fn dup_sample(src: &IMFSample) -> Option<IMFSample> {
    let buf = src.GetBufferByIndex(0).ok()?;
    let s = MFCreateSample().ok()?;
    s.AddBuffer(&buf).ok()?;
    let _ = s.SetSampleTime(src.GetSampleTime().unwrap_or(0));
    let _ = s.SetSampleDuration(src.GetSampleDuration().unwrap_or(0));
    Some(s)
}

fn write_video(rec: &Recorder, sample: &IMFSample, ts: i64) {
    unsafe {
        if rec.paused.load(Ordering::Relaxed) {
            return;
        }
        if rec.half && rec.fcount.fetch_add(1, Ordering::Relaxed) % 2 == 1 {
            return;
        }
        let dur = sample.GetSampleDuration().unwrap_or(166_666);
        if rec.base.load(Ordering::SeqCst) == NO_BASE {
            rec.base.store(ts, Ordering::SeqCst);
            rec.v_wall.store(MFGetSystemTime(), Ordering::SeqCst);
        }
        if rec.v_resume.swap(false, Ordering::SeqCst) {
            let gap = (ts - rec.v_last_end.load(Ordering::SeqCst)).max(0);
            rec.v_skip.fetch_add(gap, Ordering::SeqCst);
        }
        let dev_t = ts - rec.base.load(Ordering::SeqCst) - rec.v_skip.load(Ordering::SeqCst);
        if dev_t < 0 {
            return;
        }
        // Capture cards deliver frames in bursts, so the device timestamps jitter (an HD60 X recording showed 33 to
        // 127 fps "variable" timing). Players then drop or freeze frames. Snap the timestamps to an even grid and only
        // follow the device clock when it has really moved (dropped frames) or a frame arrives far too early.
        let vd = rec.v_dur.max(1);
        let exp = rec.v_next.load(Ordering::SeqCst);
        let t = if exp < 0 {
            dev_t
        } else if dev_t + vd < exp {
            rec.d_early.fetch_add(1, Ordering::Relaxed);
            return;
        } else if dev_t < exp + vd {
            exp
        } else {
            dev_t
        };
        rec.v_next.store(t + vd, Ordering::SeqCst);
        rec.v_last_end.store(ts + dur, Ordering::SeqCst);
        let _ = sample.SetSampleTime(t);
        let _ = sample.SetSampleDuration(vd);
        let t0 = Instant::now();
        let res = rec.writer.0.WriteSample(rec.video_idx, sample);
        let wus = t0.elapsed().as_micros() as i64;
        if wus > rec.d_maxwrite_us.load(Ordering::Relaxed) {
            rec.d_maxwrite_us.store(wus, Ordering::Relaxed);
        }
        let prev = rec.d_prev_ts.swap(ts, Ordering::Relaxed);
        if prev >= 0 {
            let gap = (ts - prev) / 10; // 100 ns -> us
            if gap <= 0 {
                rec.d_nonmono.fetch_add(1, Ordering::Relaxed);
            } else if gap < rec.d_mingap_us.load(Ordering::Relaxed) {
                rec.d_mingap_us.store(gap, Ordering::Relaxed);
            }
            if gap > rec.d_maxgap_us.load(Ordering::Relaxed) {
                rec.d_maxgap_us.store(gap, Ordering::Relaxed);
            }
            if gap > 50_000 {
                rec.d_gaps.fetch_add(1, Ordering::Relaxed);
            }
        }
        rec.d_last_t.store(t, Ordering::Relaxed);
        match res {
            Ok(()) => {
                rec.d_frames.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                if rec.d_errs.fetch_add(1, Ordering::Relaxed) == 0 {
                    *rec.d_first_err.lock().unwrap() = e.to_string();
                }
            }
        }
    }
}

/// Audio and video come from different device clocks, so audio is timed from its own first
/// sample and offset by how long after the first video frame it arrived.
fn write_audio(rec: &Recorder, sample: &IMFSample, ts: i64) {
    unsafe {
        let Some(idx) = rec.audio_idx else { return };
        if rec.paused.load(Ordering::Relaxed) {
            return;
        }
        let vw = rec.v_wall.load(Ordering::SeqCst);
        if vw == NO_BASE {
            return; // wait for the first video frame
        }
        let dur = sample.GetSampleDuration().unwrap_or(0);
        let mut ab = rec.a_base.load(Ordering::SeqCst);
        if ab == NO_BASE {
            ab = ts;
            rec.a_base.store(ts, Ordering::SeqCst);
            rec.a_off.store((MFGetSystemTime() - vw).max(0), Ordering::SeqCst);
        }
        if rec.a_resume.swap(false, Ordering::SeqCst) {
            let gap = (ts - rec.a_last_end.load(Ordering::SeqCst)).max(0);
            rec.a_skip.fetch_add(gap, Ordering::SeqCst);
        }
        let t = ts - ab + rec.a_off.load(Ordering::SeqCst) - rec.a_skip.load(Ordering::SeqCst);
        if t < 0 {
            return;
        }
        rec.a_last_end.store(ts + dur, Ordering::SeqCst);
        let _ = sample.SetSampleTime(t);
        let _ = rec.writer.0.WriteSample(idx, sample);
    }
}

fn write_mic(rec: &Recorder, sample: &IMFSample, ts: i64) {
    unsafe {
        let Some((mw, idx)) = &rec.mic else { return };
        if rec.paused.load(Ordering::Relaxed) {
            return;
        }
        let dur = sample.GetSampleDuration().unwrap_or(0);
        let mut mb = rec.m_base.load(Ordering::SeqCst);
        if mb == NO_BASE {
            mb = ts;
            rec.m_base.store(ts, Ordering::SeqCst);
        }
        if rec.m_resume.swap(false, Ordering::SeqCst) {
            let gap = (ts - rec.m_last_end.load(Ordering::SeqCst)).max(0);
            rec.m_skip.fetch_add(gap, Ordering::SeqCst);
        }
        let t = ts - mb - rec.m_skip.load(Ordering::SeqCst);
        if t < 0 {
            return;
        }
        rec.m_last_end.store(ts + dur, Ordering::SeqCst);
        let _ = sample.SetSampleTime(t);
        let _ = mw.0.WriteSample(*idx, sample);
    }
}

/// Build an IMFSample holding PCM bytes.
unsafe fn pcm_sample(bytes: &[u8], ts: i64, dur: i64) -> Option<IMFSample> {
    let buf = MFCreateMemoryBuffer(bytes.len() as u32).ok()?;
    let mut p: *mut u8 = std::ptr::null_mut();
    buf.Lock(&mut p, None, None).ok()?;
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
    let _ = buf.Unlock();
    let _ = buf.SetCurrentLength(bytes.len() as u32);
    let s = MFCreateSample().ok()?;
    s.AddBuffer(&buf).ok()?;
    let _ = s.SetSampleTime(ts);
    let _ = s.SetSampleDuration(dur);
    Some(s)
}

// ---------- video thread ----------

/// YUY2 (packed 4:2:2) -> NV12 sample, copying timing from the source sample.
fn yuy2_band(src: &[u8], w: usize, y0: usize, y1: usize, h: usize, yp: &mut [u8], uvp: &mut [u8]) {
    for y in y0..y1 {
        let row = &src[y * w * 2..(y + 1) * w * 2];
        let ly = y - y0;
        for (px, o) in row.chunks_exact(4).zip(yp[ly * w..(ly + 1) * w].chunks_exact_mut(2)) {
            o[0] = px[0];
            o[1] = px[2];
        }
        if y % 2 == 0 {
            let r2 = if y + 1 < h { &src[(y + 1) * w * 2..(y + 2) * w * 2] } else { row };
            let uy = ly / 2;
            for ((a, b), o) in row.chunks_exact(4).zip(r2.chunks_exact(4)).zip(uvp[uy * w..(uy + 1) * w].chunks_exact_mut(2)) {
                o[0] = ((a[1] as u16 + b[1] as u16) >> 1) as u8;
                o[1] = ((a[3] as u16 + b[3] as u16) >> 1) as u8;
            }
        }
    }
}

unsafe fn yuy2_sample_to_nv12(src: &IMFSample, w: usize, h: usize) -> Option<IMFSample> {
    let buf = src.ConvertToContiguousBuffer().ok()?;
    let mut p: *mut u8 = std::ptr::null_mut();
    let mut len = 0u32;
    buf.Lock(&mut p, None, Some(&mut len)).ok()?;
    let mut result = None;
    if len as usize >= w * h * 2 {
        let sdat = std::slice::from_raw_parts(p, w * h * 2);
        let out_len = w * h * 3 / 2;
        if let Ok(ob) = MFCreateMemoryBuffer(out_len as u32) {
            let mut op: *mut u8 = std::ptr::null_mut();
            if ob.Lock(&mut op, None, None).is_ok() {
                let d = std::slice::from_raw_parts_mut(op, out_len);
                let (yp, uvp) = d.split_at_mut(w * h);
                let rows_per = ((h / 4) + 1) & !1;
                std::thread::scope(|sc| {
                    let (mut yrest, mut uvrest) = (yp, uvp);
                    let mut y0 = 0;
                    while y0 < h {
                        let y1 = (y0 + rows_per).min(h);
                        let (ya, yb) = yrest.split_at_mut((y1 - y0) * w);
                        let (ua, ub) = uvrest.split_at_mut((y1 - y0).div_ceil(2) * w);
                        yrest = yb;
                        uvrest = ub;
                        let sd = &*sdat;
                        sc.spawn(move || yuy2_band(sd, w, y0, y1, h, ya, ua));
                        y0 = y1;
                    }
                });
                let _ = ob.Unlock();
                let _ = ob.SetCurrentLength(out_len as u32);
                if let Ok(os) = MFCreateSample() {
                    if os.AddBuffer(&ob).is_ok() {
                        let _ = os.SetSampleTime(src.GetSampleTime().unwrap_or(0));
                        let _ = os.SetSampleDuration(src.GetSampleDuration().unwrap_or(0));
                        result = Some(os);
                    }
                }
            }
        }
    }
    let _ = buf.Unlock();
    result
}

fn video_thread(reader: Sendable<IMFSourceReader>, sh: Arc<Shared>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
        let vs = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let (w, h) = (sh.fmt.w as usize, sh.fmt.h as usize);
        while sh.running.load(Ordering::Relaxed) {
            let mut flags = 0u32;
            let mut ts = 0i64;
            let mut sample: Option<IMFSample> = None;
            if reader.0.ReadSample(vs, 0, None, Some(&mut flags), Some(&mut ts), Some(&mut sample)).is_err() {
                break;
            }
            if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 | MF_SOURCE_READERF_ERROR.0 as u32) != 0 {
                break;
            }
            sh.last_flags.store(flags, Ordering::Relaxed);
            let Some(sample) = sample else { continue };
            if sample.GetUINT32(&MFSampleExtension_Discontinuity).unwrap_or(0) != 0 {
                sh.glitches.fetch_add(1, Ordering::Relaxed);
            }
            let sample = if sh.yuy2 {
                match yuy2_sample_to_nv12(&sample, w, h) {
                    Some(s) => s,
                    None => continue,
                }
            } else {
                sample
            };
            let sample = match scene::process(w, h, &sample) {
                Some(s) => s,
                None => sample,
            };
            sh.frames.fetch_add(1, Ordering::Relaxed);

            // hand the newest frame to the preview thread (latest wins)
            let want_shot = sh.shot_req.swap(false, Ordering::SeqCst);
            {
                let mut slot = sh.pv_slot.lock().unwrap();
                let keep = want_shot || slot.as_ref().map(|x| x.1).unwrap_or(false);
                *slot = Some((Sendable(sample.clone()), keep));
            }
            sh.pv_cv.notify_one();

            stream::feed_video(&sh, &sample);
            replay_tick(&sh);
            let rec = sh.rec.lock().unwrap().clone();
            let rp = sh.replay.lock().unwrap().as_ref().and_then(|r| r.cur.clone());
            match (rec, rp) {
                (Some(a), Some(b)) => {
                    if let Some(d) = dup_sample(&sample) {
                        write_video(&b, &d, ts);
                    }
                    write_video(&a, &sample, ts);
                }
                (Some(a), None) | (None, Some(a)) => write_video(&a, &sample, ts),
                (None, None) => {}
            }

        }
    }
}

fn preview_thread(sh: Arc<Shared>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }
    let (w, h) = (sh.fmt.w as usize, sh.fmt.h as usize);
    let mut spare: Vec<u32> = Vec::new();
    while sh.running.load(Ordering::Relaxed) {
        let item = {
            let mut g = sh.pv_slot.lock().unwrap();
            while g.is_none() {
                if !sh.running.load(Ordering::Relaxed) {
                    return;
                }
                g = sh.pv_cv.wait_timeout(g, Duration::from_millis(100)).unwrap().0;
            }
            g.take()
        };
        let Some((sample, want_shot)) = item else { continue };
        if HIDDEN.load(Ordering::Relaxed) && !want_shot {
            continue;
        }
        unsafe {
            let Ok(buf) = sample.0.ConvertToContiguousBuffer() else { continue };
            let mut p: *mut u8 = std::ptr::null_mut();
            let mut len = 0u32;
            if buf.Lock(&mut p, None, Some(&mut len)).is_err() {
                continue;
            }
            if len as usize >= w * h * 3 / 2 {
                let src = std::slice::from_raw_parts(p, w * h * 3 / 2);
                let full = sh.pv_full.load(Ordering::Relaxed);
                let (tw, th) = (PV_TW.load(Ordering::Relaxed).max(0) as usize, PV_TH.load(Ordering::Relaxed).max(0) as usize);
                // on-screen size between half and full capture size: scale once, here, with a smooth filter
                let smooth = tw * 2 > w && tw < w && th * 2 > h && th < h && tw >= 16 && th >= 16;
                let (pw, ph) = if smooth { (tw, th) } else if full { (w, h) } else { (w / 2, h / 2) };
                spare.resize(pw * ph, 0);
                if smooth {
                    nv12_scale_to_bgra(src, w, h, &mut spare, tw, th);
                } else {
                    nv12_to_bgra(src, w, h, &mut spare, full);
                }
                {
                    let mut pv = sh.preview.lock().unwrap();
                    pv.w = pw;
                    pv.h = ph;
                    std::mem::swap(&mut pv.data, &mut spare);
                }
                post_frame_msg();
                if want_shot {
                    let copy = src.to_vec();
                    let sh2 = sh.clone();
                    std::thread::spawn(move || save_screenshot(copy, w, h, &sh2));
                }
            } else {
                sh.glitches.fetch_add(1, Ordering::Relaxed);
            }
            let _ = buf.Unlock();
        }
    }
}

/// NV12 -> BGRA. `full`: 1:1 conversion. Otherwise 2x2 box-filtered half size (much sharper than skipping pixels).
static PV_TW: AtomicI32 = AtomicI32::new(0);
static PV_TH: AtomicI32 = AtomicI32::new(0);

/// Bilinear NV12 -> BGRA scaler to exactly the on-screen size, so the final blit is 1:1 and text stays smooth.
fn nv12_scale_to_bgra(src: &[u8], w: usize, h: usize, dst: &mut [u32], tw: usize, th: usize) {
    let uv = &src[w * h..];
    let px = |yv: i32, u: i32, v: i32| -> u32 {
        let c = 298 * (yv - 16);
        let r = ((c + 459 * v + 128) >> 8).clamp(0, 255) as u32;
        let g = ((c - 55 * u - 136 * v + 128) >> 8).clamp(0, 255) as u32;
        let b = ((c + 541 * u + 128) >> 8).clamp(0, 255) as u32;
        (r << 16) | (g << 8) | b
    };
    // per-column source index (x0, x1), weight (0..256) and chroma pair offset
    let mut cols: Vec<(u32, u32, u32, u32)> = Vec::with_capacity(tw);
    for x in 0..tw {
        let fx = ((x as f32 + 0.5) * w as f32 / tw as f32 - 0.5).clamp(0.0, (w - 1) as f32);
        let x0 = fx as usize;
        let x1 = (x0 + 1).min(w - 1);
        let wt = ((fx - x0 as f32) * 256.0) as u32;
        let cx = (fx + 0.5) as usize;
        cols.push((x0 as u32, x1 as u32, wt, (cx.min(w - 1) & !1) as u32));
    }
    for y in 0..th {
        let fy = ((y as f32 + 0.5) * h as f32 / th as f32 - 0.5).clamp(0.0, (h - 1) as f32);
        let y0 = fy as usize;
        let y1 = (y0 + 1).min(h - 1);
        let wy = ((fy - y0 as f32) * 256.0) as u32;
        let cy = ((fy + 0.5) as usize).min(h - 1) / 2;
        let (r0, r1) = (&src[y0 * w..(y0 + 1) * w], &src[y1 * w..(y1 + 1) * w]);
        let uvrow = &uv[cy * w..(cy + 1) * w];
        let out = &mut dst[y * tw..(y + 1) * tw];
        for (o, &(x0, x1, wx, cx)) in out.iter_mut().zip(cols.iter()) {
            let (x0, x1, cx) = (x0 as usize, x1 as usize, cx as usize);
            let top = r0[x0] as u32 * (256 - wx) + r0[x1] as u32 * wx;
            let bot = r1[x0] as u32 * (256 - wx) + r1[x1] as u32 * wx;
            let l = ((top * (256 - wy) + bot * wy + 32768) >> 16) as i32;
            *o = px(l, uvrow[cx] as i32 - 128, uvrow[cx + 1] as i32 - 128);
        }
    }
}

fn nv12_to_bgra(src: &[u8], w: usize, h: usize, dst: &mut [u32], full: bool) {
    let uv = &src[w * h..];
    let px = |yv: i32, u: i32, v: i32| -> u32 {
        let c = 298 * (yv - 16);
        let r = ((c + 459 * v + 128) >> 8).clamp(0, 255) as u32;
        let g = ((c - 55 * u - 136 * v + 128) >> 8).clamp(0, 255) as u32;
        let b = ((c + 541 * u + 128) >> 8).clamp(0, 255) as u32;
        (r << 16) | (g << 8) | b
    };
    if full {
        for y in 0..h {
            let uvrow = &uv[(y / 2) * w..(y / 2 + 1) * w];
            for x in 0..w {
                let ui = x & !1;
                dst[y * w + x] = px(src[y * w + x] as i32, uvrow[ui] as i32 - 128, uvrow[ui + 1] as i32 - 128);
            }
        }
    } else {
        let (pw, ph) = (w / 2, h / 2);
        for y in 0..ph {
            let (r0, r1) = (&src[2 * y * w..(2 * y + 1) * w], &src[(2 * y + 1) * w..(2 * y + 2) * w]);
            let uvrow = &uv[y * w..(y + 1) * w];
            for x in 0..pw {
                let l = (r0[2 * x] as i32 + r0[2 * x + 1] as i32 + r1[2 * x] as i32 + r1[2 * x + 1] as i32 + 2) / 4;
                dst[y * pw + x] = px(l, uvrow[2 * x] as i32 - 128, uvrow[2 * x + 1] as i32 - 128);
            }
        }
    }
}

fn save_screenshot(nv: Vec<u8>, w: usize, h: usize, sh: &Shared) {
    let mut rgb = vec![0u8; w * h * 3];
    let uv = &nv[w * h..];
    for y in 0..h {
        for x in 0..w {
            let yy = nv[y * w + x] as i32 - 16;
            let ui = (y / 2) * w + (x & !1);
            let u = uv[ui] as i32 - 128;
            let v = uv[ui + 1] as i32 - 128;
            let c = 298 * yy;
            let o = (y * w + x) * 3;
            rgb[o] = ((c + 459 * v + 128) >> 8).clamp(0, 255) as u8;
            rgb[o + 1] = ((c - 55 * u - 136 * v + 128) >> 8).clamp(0, 255) as u8;
            rgb[o + 2] = ((c + 541 * u + 128) >> 8).clamp(0, 255) as u8;
        }
    }
    let dir = sh.cfg.lock().unwrap().shot_dir.clone();
    let _ = std::fs::create_dir_all(&dir);
    let path = format!("{}\\Lumacap_{}.png", dir, timestamp_name());
    let res = (|| -> Result<(), String> {
        let f = std::fs::File::create(&path).map_err(|e| e.to_string())?;
        let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w as u32, h as u32);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut wr = enc.write_header().map_err(|e| e.to_string())?;
        wr.write_image_data(&rgb).map_err(|e| e.to_string())
    })();
    match res {
        Ok(()) => {
            logln!("screenshot: {path}");
            set_toast(sh, "Screenshot saved");
        }
        Err(e) => {
            elogln!("screenshot failed: {e}");
            set_toast(sh, "Screenshot failed");
        }
    }
}

// ---------- audio: monitor + thread ----------

/// Plays the captured audio through the default output (so you can hear your game).
struct Monitor {
    hwo: HWAVEOUT,
    bufs: Vec<Box<(WAVEHDR, Vec<u8>)>>,
    next: usize,
}

impl Monitor {
    unsafe fn new(rate: u32) -> Option<Monitor> {
        let mut wf = WAVEFORMATEX::default();
        wf.wFormatTag = WAVE_FORMAT_PCM as u16;
        wf.nChannels = 2;
        wf.nSamplesPerSec = rate;
        wf.wBitsPerSample = 16;
        wf.nBlockAlign = 4;
        wf.nAvgBytesPerSec = rate * 4;
        let mut hwo = HWAVEOUT::default();
        if waveOutOpen(Some(&mut hwo), WAVE_MAPPER, &wf, 0, 0, CALLBACK_NULL) != 0 {
            return None;
        }
        let bufs = (0..12).map(|_| Box::new((WAVEHDR::default(), Vec::new()))).collect();
        Some(Monitor { hwo, bufs, next: 0 })
    }

    unsafe fn push(&mut self, pcm: &[u8]) {
        let n = self.bufs.len();
        for k in 0..n {
            let i = (self.next + k) % n;
            let b = &mut self.bufs[i];
            if b.0.dwFlags & WHDR_INQUEUE != 0 {
                continue;
            }
            if b.0.dwFlags != 0 {
                waveOutUnprepareHeader(self.hwo, &mut b.0, std::mem::size_of::<WAVEHDR>() as u32);
            }
            b.1.clear();
            b.1.extend_from_slice(pcm);
            b.0 = WAVEHDR::default();
            b.0.lpData = windows::core::PSTR(b.1.as_mut_ptr());
            b.0.dwBufferLength = b.1.len() as u32;
            waveOutPrepareHeader(self.hwo, &mut b.0, std::mem::size_of::<WAVEHDR>() as u32);
            waveOutWrite(self.hwo, &mut b.0, std::mem::size_of::<WAVEHDR>() as u32);
            self.next = (i + 1) % n;
            return;
        }
        // all buffers busy: drop this chunk (keeps latency bounded)
    }

    /// Drop everything that is queued so a volume change or mute is heard immediately.
    unsafe fn flush(&mut self) {
        waveOutReset(self.hwo);
    }

    unsafe fn close(&mut self) {
        waveOutReset(self.hwo);
        for b in self.bufs.iter_mut() {
            if b.0.dwFlags != 0 {
                waveOutUnprepareHeader(self.hwo, &mut b.0, std::mem::size_of::<WAVEHDR>() as u32);
            }
        }
        waveOutClose(self.hwo);
    }
}

fn audio_thread(reader: Sendable<IMFSourceReader>, sh: Arc<Shared>, af: AudioFmt) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let a = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
        let mut monitor = if af.ch == 2 { Monitor::new(af.rate) } else { None };
        let can_mix = af.ch == 2 && (af.float || af.bits == 16);
        let mut game: Vec<i16> = Vec::new();
        let mut pcm: Vec<u8> = Vec::new();
        let mut mixb: Vec<u8> = Vec::new();
        while sh.running.load(Ordering::Relaxed) {
            let mut flags = 0u32;
            let mut ts = 0i64;
            let mut sample: Option<IMFSample> = None;
            if reader.0.ReadSample(a, 0, None, Some(&mut flags), Some(&mut ts), Some(&mut sample)).is_err() {
                break;
            }
            if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 | MF_SOURCE_READERF_ERROR.0 as u32) != 0 {
                break;
            }
            let Some(sample) = sample else { continue };

            // decode to i16 for the meter, the monitor and the mixer
            game.clear();
            if can_mix {
                if let Ok(buf) = sample.ConvertToContiguousBuffer() {
                    let mut p: *mut u8 = std::ptr::null_mut();
                    let mut len = 0u32;
                    if buf.Lock(&mut p, None, Some(&mut len)).is_ok() {
                        let bytes = std::slice::from_raw_parts(p, len as usize);
                        if af.float {
                            for c in bytes.chunks_exact(4) {
                                let f = f32::from_le_bytes([c[0], c[1], c[2], c[3]]);
                                game.push((f * 32767.0).clamp(-32768.0, 32767.0) as i16);
                            }
                        } else {
                            for c in bytes.chunks_exact(2) {
                                game.push(i16::from_le_bytes([c[0], c[1]]));
                            }
                        }
                        let _ = buf.Unlock();
                    }
                }
                let peak = game.iter().map(|&x| (x as i32).abs()).max().unwrap_or(0);
                sh.peak.store(peak as u32, Ordering::Relaxed);

                // live monitor: main volume slider x the game volume from settings (0 = silent)
                let gain = if sh.muted.load(Ordering::Relaxed) {
                    0.0
                } else {
                    let v = sh.vol.load(Ordering::Relaxed) as f32 / 100.0;
                    v * v * (sh.game_gain.load(Ordering::Relaxed) as f32 / 100.0)
                };
                if gain <= 0.001 {
                    if MON_PLAYING.swap(false, Ordering::Relaxed) {
                        if let Some(m) = monitor.as_mut() {
                            m.flush(); // clear what is already queued
                        }
                    }
                } else {
                    MON_PLAYING.store(true, Ordering::Relaxed);
                }
                if gain > 0.001 && !game.is_empty() {
                    pcm.clear();
                    for &x in &game {
                        pcm.extend_from_slice(&((x as f32 * gain).clamp(-32768.0, 32767.0) as i16).to_le_bytes());
                    }
                    if let Some(m) = monitor.as_mut() {
                        m.push(&pcm);
                    }
                }
            }

            // mix game + mic (with per-source gain) into the recorded track
            let out = if can_mix && !game.is_empty() {
                let gg = sh.game_gain.load(Ordering::Relaxed) as f32 / 100.0;
                let mg = if sh.mic_muted.load(Ordering::Relaxed) { 0.0 } else { sh.mic_gain.load(Ordering::Relaxed) as f32 / 100.0 };
                mixb.clear();
                {
                    let mut mb = sh.mic_buf.lock().unwrap();
                    let cap = af.rate as usize * 2 * 4 / 10; // keep at most ~400 ms of mic backlog
                    if mb.len() > cap {
                        let extra = (mb.len() - cap) & !1;
                        mb.drain(..extra);
                    }
                    for &g in &game {
                        let m = mb.pop_front().unwrap_or(0);
                        let v = g as f32 * gg + m as f32 * mg;
                        mixb.extend_from_slice(&(v.clamp(-32768.0, 32767.0) as i16).to_le_bytes());
                    }
                }
                let dur = (game.len() / 2) as i64 * 10_000_000 / af.rate as i64;
                stream::feed_audio(&sh, &mixb);
                pcm_sample(&mixb, ts, dur).unwrap_or_else(|| sample.clone())
            } else {
                sample.clone()
            };

            let rec = sh.rec.lock().unwrap().clone();
            let rp = sh.replay.lock().unwrap().as_ref().and_then(|r| r.cur.clone());
            match (rec, rp) {
                (Some(x), Some(y)) => {
                    if let Some(d) = dup_sample(&out) {
                        write_audio(&y, &d, ts);
                    }
                    write_audio(&x, &out, ts);
                }
                (Some(x), None) | (None, Some(x)) => write_audio(&x, &out, ts),
                (None, None) => {}
            }
        }
        if let Some(m) = monitor.as_mut() {
            m.close();
        }
    }
}

// ---------- microphone ----------

unsafe fn open_mic(act: &IMFActivate, rate: u32) -> R<(IMFSourceReader, AudioFmt)> {
    let source: IMFMediaSource = act.ActivateObject()?;
    let mut attrs: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut attrs, 1)?;
    let attrs = attrs.unwrap();
    let reader = MFCreateSourceReaderFromMediaSource(&source, &attrs)?;
    let a = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
    for f in [
        AudioFmt { float: false, rate, ch: 2, bits: 16 },
        AudioFmt { float: false, rate, ch: 1, bits: 16 },
    ] {
        if reader.SetCurrentMediaType(a, None, &pcm_type(&f)?).is_ok() {
            return Ok((reader, f));
        }
    }
    Err(windows::core::Error::from(windows::Win32::Foundation::E_FAIL))
}

fn mic_thread(sh: Arc<Shared>, act: Sendable<IMFActivate>, gen: u32) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let rate = sh.afmt.map(|a| a.rate).unwrap_or(48000);
        let (reader, mf) = match open_mic(&act.0, rate) {
            Ok(x) => x,
            Err(e) => {
                *sh.mic_err.lock().unwrap() = format!("can't open mic: {}", e.message());
                sh.mic_idx.store(-1, Ordering::Relaxed);
                return;
            }
        };
        if sh.mic_gen.load(Ordering::SeqCst) != gen {
            return; // user switched again while opening
        }
        *sh.mic_fmt.lock().unwrap() = Some(mf);
        let a = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
        while sh.running.load(Ordering::Relaxed) && sh.mic_gen.load(Ordering::SeqCst) == gen {
            let mut flags = 0u32;
            let mut ts = 0i64;
            let mut sample: Option<IMFSample> = None;
            if reader.ReadSample(a, 0, None, Some(&mut flags), Some(&mut ts), Some(&mut sample)).is_err() {
                break;
            }
            if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 | MF_SOURCE_READERF_ERROR.0 as u32) != 0 {
                break;
            }
            let Some(sample) = sample else { continue };
            if let Ok(buf) = sample.ConvertToContiguousBuffer() {
                let mut p: *mut u8 = std::ptr::null_mut();
                let mut len = 0u32;
                if buf.Lock(&mut p, None, Some(&mut len)).is_ok() {
                    let bytes = std::slice::from_raw_parts(p, len as usize);
                    let mut peak = 0i32;
                    let mut mb = sh.mic_buf.lock().unwrap();
                    for c in bytes.chunks_exact(2) {
                        let v = i16::from_le_bytes([c[0], c[1]]);
                        peak = peak.max((v as i32).abs());
                        mb.push_back(v);
                        if mf.ch == 1 {
                            mb.push_back(v); // mono -> stereo
                        }
                    }
                    drop(mb);
                    sh.mic_peak.store(peak as u32, Ordering::Relaxed);
                    let _ = buf.Unlock();
                }
            }
            let rec = sh.rec.lock().unwrap().clone();
            if let Some(r) = rec {
                if r.mic.is_some() {
                    write_mic(&r, &sample, ts);
                }
            }
        }
    }
}

/// Switch the microphone (empty name = off).
fn start_mic(sh: &Arc<Shared>, name: &str) {
    let gen = sh.mic_gen.fetch_add(1, Ordering::SeqCst) + 1;
    sh.mic_buf.lock().unwrap().clear();
    *sh.mic_fmt.lock().unwrap() = None;
    sh.mic_err.lock().unwrap().clear();
    sh.mic_peak.store(0, Ordering::Relaxed);
    match sh.mic_names.iter().position(|n| n == name) {
        Some(i) if !name.is_empty() => {
            sh.mic_idx.store(i as i32, Ordering::Relaxed);
            let act = Sendable(sh.adevs[i].0.clone());
            let sh2 = sh.clone();
            std::thread::spawn(move || mic_thread(sh2, act, gen));
        }
        _ => sh.mic_idx.store(-1, Ordering::Relaxed),
    }
}

// ---------- instant replay (rolling segments, remuxed on demand) ----------

static SEG_ID: AtomicU32 = AtomicU32::new(0);

fn replay_dir() -> String {
    format!("{}\\lumacap_replay", std::env::temp_dir().display())
}

fn next_seg_path() -> String {
    format!("{}\\seg_{}.mp4", replay_dir(), SEG_ID.fetch_add(1, Ordering::SeqCst))
}

fn keep_segments(secs: u32) -> usize {
    (secs as u64).div_ceil(SEG_SECS) as usize + 2
}

fn start_replay(sh: &Arc<Shared>) -> Result<(), String> {
    let dir = replay_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let bitrate = sh.cfg.lock().unwrap().bitrate * 1_000_000;
    let rec = unsafe { start_recorder(&next_seg_path(), &sh.fmt, bitrate, sh.afmt.as_ref(), &RecOpts::default()) }.map_err(|e| e.to_string())?;
    *sh.replay.lock().unwrap() = Some(ReplayBuf {
        cur: Some(Arc::new(rec)),
        cur_started: Instant::now(),
        next: Arc::new(Mutex::new(None)),
        preparing: Arc::new(AtomicBool::new(false)),
        done: VecDeque::new(),
    });
    Ok(())
}

fn stop_replay(sh: &Arc<Shared>) {
    let rb = sh.replay.lock().unwrap().take();
    if let Some(mut rb) = rb {
        if let Some(c) = rb.cur.take() {
            let h = finalize_when_free(c);
            std::thread::spawn(move || {
                let _ = h.join();
                let _ = std::fs::remove_dir_all(replay_dir());
            });
        }
    }
}

/// Swap in a freshly started segment; returns the finalize handle of the old one.
fn swap_segment(rb: &mut ReplayBuf, new: Recorder, secs: u32) -> Option<std::thread::JoinHandle<()>> {
    let old = rb.cur.replace(Arc::new(new));
    rb.cur_started = Instant::now();
    let handle = old.map(|o| {
        rb.done.push_back(o.path.clone());
        finalize_when_free(o)
    });
    let keep = keep_segments(secs);
    while rb.done.len() > keep {
        if let Some(f) = rb.done.pop_front() {
            let _ = std::fs::remove_file(f);
        }
    }
    handle
}

/// Called from the video thread each frame: prepares and rotates segments.
fn replay_tick(sh: &Arc<Shared>) {
    let mut g = sh.replay.lock().unwrap();
    let Some(rb) = g.as_mut() else { return };
    let el = rb.cur_started.elapsed();
    let (secs, bitrate) = {
        let c = sh.cfg.lock().unwrap();
        (c.replay_secs, c.bitrate * 1_000_000)
    };
    if el >= Duration::from_millis(SEG_SECS * 1000 - 1500)
        && !rb.preparing.load(Ordering::SeqCst)
        && rb.next.lock().unwrap().is_none()
    {
        rb.preparing.store(true, Ordering::SeqCst);
        let (next, preparing) = (rb.next.clone(), rb.preparing.clone());
        let (fmt, afmt) = (sh.fmt, sh.afmt);
        std::thread::spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                if let Ok(r) = start_recorder(&next_seg_path(), &fmt, bitrate, afmt.as_ref(), &RecOpts::default()) {
                    *next.lock().unwrap() = Some(r);
                }
            }
            preparing.store(false, Ordering::SeqCst);
        });
    }
    if el >= Duration::from_secs(SEG_SECS) {
        let ready = rb.next.lock().unwrap().take();
        if let Some(new) = ready {
            let _ = swap_segment(rb, new, secs);
        }
    }
}

/// Save the last N seconds. Runs on its own thread.
fn save_replay(sh: Arc<Shared>) {
    std::thread::spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        if sh.replay.lock().unwrap().is_none() {
            set_toast(&sh, "Turn on Instant replay in settings first");
            return;
        }
        set_toast(&sh, "Saving replay…");
        let (secs, bitrate, out_dir) = {
            let c = sh.cfg.lock().unwrap();
            (c.replay_secs, c.bitrate * 1_000_000, c.out_dir.clone())
        };
        // close the current segment now so the newest moments are included
        let prepared = {
            let g = sh.replay.lock().unwrap();
            g.as_ref().and_then(|rb| rb.next.lock().unwrap().take())
        };
        let new = match prepared {
            Some(r) => r,
            None => match unsafe { start_recorder(&next_seg_path(), &sh.fmt, bitrate, sh.afmt.as_ref(), &RecOpts::default()) } {
                Ok(r) => r,
                Err(e) => {
                    set_toast(&sh, "Replay failed");
                    elogln!("replay: {e}");
                    return;
                }
            },
        };
        let (handle, paths) = {
            let mut g = sh.replay.lock().unwrap();
            let Some(rb) = g.as_mut() else { return };
            let h = swap_segment(rb, new, secs);
            let k = (secs as u64).div_ceil(SEG_SECS) as usize;
            let n = rb.done.len();
            (h, rb.done.iter().skip(n.saturating_sub(k)).cloned().collect::<Vec<_>>())
        };
        if let Some(h) = handle {
            let _ = h.join();
        }
        let _ = std::fs::create_dir_all(&out_dir);
        let out = format!("{}\\Lumacap_replay_{}.mp4", out_dir, timestamp_name());
        match unsafe { remux(&paths, &out) } {
            Ok(()) => {
                logln!("replay saved: {out}");
                set_toast(&sh, "Replay saved");
                if sh.cfg.lock().unwrap().clip_beep {
                    std::thread::spawn(|| {
                        let _ = unsafe { Beep(1175, 90) };
                        let _ = unsafe { Beep(1568, 140) };
                    });
                }
            }
            Err(e) => {
                elogln!("replay remux failed: {e}");
                set_toast(&sh, "Replay failed");
            }
        }
    });
}

/// Join finished MP4 segments into one file without re-encoding.
unsafe fn remux(paths: &[String], out: &str) -> R<()> {
    if paths.is_empty() {
        return Err(windows::core::Error::from(windows::Win32::Foundation::E_FAIL));
    }
    let mut writer: Option<IMFSinkWriter> = None;
    let (mut vi, mut ai) = (0u32, None::<u32>);
    let mut offset = 0i64;
    for p in paths {
        let wp = wide(p);
        let reader = MFCreateSourceReaderFromURL(PCWSTR(wp.as_ptr()), None::<&IMFAttributes>)?;
        let vs = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let aud = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
        let vt = reader.GetNativeMediaType(vs, 0)?;
        let at = reader.GetNativeMediaType(aud, 0).ok();
        if writer.is_none() {
            let mut attrs: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut attrs, 2)?;
            let attrs = attrs.unwrap();
            attrs.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
            let wo = wide(out);
            let wr = MFCreateSinkWriterFromURL(PCWSTR(wo.as_ptr()), None, &attrs)?;
            vi = wr.AddStream(&vt)?;
            wr.SetInputMediaType(vi, &vt, None)?;
            if let Some(at) = &at {
                let i = wr.AddStream(at)?;
                wr.SetInputMediaType(i, at, None)?;
                ai = Some(i);
            }
            wr.BeginWriting()?;
            writer = Some(wr);
        }
        let wr = writer.as_ref().unwrap();
        let expected = 1 + at.is_some() as u32;
        let mut ended = 0u32;
        let mut seg_end = 0i64;
        while ended < expected {
            let mut actual = 0u32;
            let mut flags = 0u32;
            let mut ts = 0i64;
            let mut sample: Option<IMFSample> = None;
            reader.ReadSample(
                MF_SOURCE_READER_ANY_STREAM.0 as u32,
                0,
                Some(&mut actual),
                Some(&mut flags),
                Some(&mut ts),
                Some(&mut sample),
            )?;
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                ended += 1;
                continue;
            }
            let Some(s) = sample else { continue };
            let dur = s.GetSampleDuration().unwrap_or(0);
            let _ = s.SetSampleTime(ts + offset);
            if actual == vs || actual == 0 {
                seg_end = seg_end.max(ts + dur);
                wr.WriteSample(vi, &s)?;
            } else if let Some(i) = ai {
                wr.WriteSample(i, &s)?;
            }
        }
        offset += seg_end;
    }
    if let Some(wr) = writer {
        wr.Finalize()?;
    }
    Ok(())
}

// ---------- library: thumbnails, duration, trim ----------

struct Thumb {
    w: usize,
    h: usize,
    data: Vec<u32>, // BGRA
}

type ThumbInfo = (u64, Option<i64>, Option<Arc<Thumb>>);
static THUMBS: Mutex<Option<HashMap<String, ThumbInfo>>> = Mutex::new(None);
static LIB_WORKER: AtomicBool = AtomicBool::new(false);

fn with_thumbs<R>(f: impl FnOnce(&mut HashMap<String, ThumbInfo>) -> R) -> R {
    let mut g = THUMBS.lock().unwrap();
    f(g.get_or_insert_with(HashMap::new))
}

struct FrameReader {
    reader: IMFSourceReader,
    w: usize,
    h: usize,
    stride: i32,
}

unsafe fn open_frame_reader(path: &str) -> Option<FrameReader> {
    let wp = wide(path);
    let mut attrs: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut attrs, 1).ok()?;
    let attrs = attrs?;
    attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1).ok()?;
    let reader = MFCreateSourceReaderFromURL(PCWSTR(wp.as_ptr()), &attrs).ok()?;
    let vs = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let _ = reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false);
    let _ = reader.SetStreamSelection(vs, true);
    let out = MFCreateMediaType().ok()?;
    out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ok()?;
    out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32).ok()?;
    reader.SetCurrentMediaType(vs, None, &out).ok()?;
    let cur = reader.GetCurrentMediaType(vs).ok()?;
    let size = cur.GetUINT64(&MF_MT_FRAME_SIZE).ok()?;
    let (w, h) = ((size >> 32) as usize, (size as u32) as usize);
    if w == 0 || h == 0 {
        return None;
    }
    let stride = cur.GetUINT32(&MF_MT_DEFAULT_STRIDE).map(|v| v as i32).unwrap_or(-(w as i32 * 4));
    Some(FrameReader { reader, w, h, stride })
}

/// Seek, decode forward to `pos` (100 ns units) and return a box-filtered frame at most `max_w` wide.
unsafe fn grab_frame(fr: &FrameReader, pos: i64, max_w: usize) -> Option<Thumb> {
    let vs = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let pv = PROPVARIANT::from(pos.max(0));
    fr.reader.SetCurrentPosition(&GUID::zeroed(), &pv).ok()?;
    let mut last: Option<IMFSample> = None;
    for _ in 0..240 {
        let mut flags = 0u32;
        let mut ts = 0i64;
        let mut sample: Option<IMFSample> = None;
        fr.reader.ReadSample(vs, 0, None, Some(&mut flags), Some(&mut ts), Some(&mut sample)).ok()?;
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            break;
        }
        if let Some(sm) = sample {
            last = Some(sm);
            if ts >= pos {
                break;
            }
        }
    }
    let sample = last?;
    let buf = sample.ConvertToContiguousBuffer().ok()?;
    let mut p: *mut u8 = std::ptr::null_mut();
    let mut len = 0u32;
    buf.Lock(&mut p, None, Some(&mut len)).ok()?;
    let (w, h) = (fr.w, fr.h);
    let rowb = fr.stride.unsigned_abs() as usize;
    let mut out = None;
    if len as usize >= rowb * h && rowb >= w * 4 {
        let src = std::slice::from_raw_parts(p, rowb * h);
        let tw = w.min(max_w).max(1);
        let th = (h * tw / w).max(1);
        let mut data = vec![0u32; tw * th];
        for y in 0..th {
            let (sy0, sy1) = (y * h / th, ((y + 1) * h / th).max(y * h / th + 1).min(h));
            let mem_y = |yy: usize| if fr.stride < 0 { h - 1 - yy } else { yy };
            for x in 0..tw {
                let (sx0, sx1) = (x * w / tw, ((x + 1) * w / tw).max(x * w / tw + 1).min(w));
                let (mut b, mut g, mut r, mut n) = (0u32, 0u32, 0u32, 0u32);
                for yy in sy0..sy1 {
                    let row = &src[mem_y(yy) * rowb..];
                    for xx in sx0..sx1 {
                        b += row[xx * 4] as u32;
                        g += row[xx * 4 + 1] as u32;
                        r += row[xx * 4 + 2] as u32;
                        n += 1;
                    }
                }
                data[y * tw + x] = ((r / n) << 16) | ((g / n) << 8) | (b / n);
            }
        }
        out = Some(Thumb { w: tw, h: th, data });
    }
    let _ = buf.Unlock();
    out
}

unsafe fn media_duration(path: &str) -> Option<i64> {
    let wp = wide(path);
    let reader = MFCreateSourceReaderFromURL(PCWSTR(wp.as_ptr()), None::<&IMFAttributes>).ok()?;
    let pv = reader.GetPresentationAttribute(0xFFFF_FFFF, &MF_PD_DURATION).ok()?;
    u64::try_from(&pv).ok().map(|v| v as i64)
}

fn lib_worker_start() {
    if LIB_WORKER.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let mut tried: HashSet<(String, u64)> = HashSet::new();
        loop {
            let todo = LIB.lock().unwrap().iter().find(|it| {
                !tried.contains(&(it.path.clone(), it.size))
                    && with_thumbs(|m| m.get(&it.path).map(|e| e.0 != it.size).unwrap_or(true))
            }).cloned();
            let Some(it) = todo else { break };
            tried.insert((it.path.clone(), it.size));
            let recording_now = ui().sh.rec.lock().unwrap().as_ref().map(|r| r.path == it.path).unwrap_or(false);
            if recording_now {
                continue;
            }
            unsafe {
                let dur = media_duration(&it.path);
                let mut th = None;
                if !it.mic {
                    if let (Some(fr), Some(d)) = (open_frame_reader(&it.path), dur) {
                        th = grab_frame(&fr, (d / 4).min(50_000_000), 192).map(Arc::new);
                    }
                }
                with_thumbs(|m| m.insert(it.path.clone(), (it.size, dur, th)));
            }
            post_frame_msg();
        }
        LIB_WORKER.store(false, Ordering::SeqCst);
    });
}

struct TrimState {
    path: String,
    name: String,
    dur: i64,
    start: i64,
    end: i64,
    pos: i64,
    frame: Option<Arc<Thumb>>,
    status: String,
}

static TRIM: Mutex<Option<TrimState>> = Mutex::new(None);
static TRIM_OPEN: AtomicBool = AtomicBool::new(false);
static TRIM_REQ: AtomicI64 = AtomicI64::new(-1);
static TRIM_GEN: AtomicU32 = AtomicU32::new(0);
static TL_DRAG: AtomicBool = AtomicBool::new(false);
static EXPORTING: AtomicBool = AtomicBool::new(false);
const SEC: i64 = 10_000_000;

fn fmt_time(t: i64) -> String {
    let t = t.max(0);
    let (m, sec, tenth) = (t / SEC / 60, (t / SEC) % 60, (t / (SEC / 10)) % 10);
    format!("{}:{:02}.{}", m, sec, tenth)
}

fn fmt_dur(t: i64) -> String {
    let t = (t.max(0) + SEC / 2) / SEC;
    if t >= 3600 {
        format!("{}:{:02}:{:02}", t / 3600, (t / 60) % 60, t % 60)
    } else {
        format!("{}:{:02}", t / 60, t % 60)
    }
}

fn trim_seek(pos: i64) {
    if let Some(t) = TRIM.lock().unwrap().as_mut() {
        t.pos = pos.clamp(0, t.dur);
        TRIM_REQ.store(t.pos, Ordering::SeqCst);
    }
}

fn trim_open(it: &LibItem) {
    let dur = with_thumbs(|m| m.get(&it.path).and_then(|e| e.1)).or_else(|| unsafe { media_duration(&it.path) });
    let Some(dur) = dur.filter(|d| *d > SEC / 2) else {
        set_toast(&ui().sh, "Can't read this file");
        return;
    };
    *TRIM.lock().unwrap() = Some(TrimState { path: it.path.clone(), name: it.name.clone(), dur, start: 0, end: dur, pos: 0, frame: None, status: String::new() });
    let gen = TRIM_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    TRIM_REQ.store(0, Ordering::SeqCst);
    let path = it.path.clone();
    std::thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let Some(fr) = open_frame_reader(&path) else { return };
        while TRIM_GEN.load(Ordering::SeqCst) == gen {
            let want = TRIM_REQ.swap(-1, Ordering::SeqCst);
            if want >= 0 {
                if let Some(f) = grab_frame(&fr, want, 640) {
                    if let Some(t) = TRIM.lock().unwrap().as_mut() {
                        t.frame = Some(Arc::new(f));
                    }
                    post_frame_msg();
                }
            } else {
                std::thread::sleep(Duration::from_millis(12));
            }
        }
    });
    TRIM_OPEN.store(true, Ordering::Relaxed);
}

fn trim_close() {
    TRIM_OPEN.store(false, Ordering::Relaxed);
    TRIM_GEN.fetch_add(1, Ordering::SeqCst);
    *TRIM.lock().unwrap() = None;
}

/// Passthrough (no re-encode) copy of [start, end]. Starts on the keyframe at or before `start`.
unsafe fn trim_export(src: &str, start: i64, end: i64, out: &str) -> R<()> {
    let e_fail = || windows::core::Error::from(windows::Win32::Foundation::E_FAIL);
    let wp = wide(src);
    let reader = MFCreateSourceReaderFromURL(PCWSTR(wp.as_ptr()), None::<&IMFAttributes>)?;
    let vs = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let aud = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
    let vt = reader.GetNativeMediaType(vs, 0)?;
    let at = reader.GetNativeMediaType(aud, 0).ok();
    // which real stream index is the video one?
    let mut vidx = 0u32;
    for i in 0..8u32 {
        match reader.GetNativeMediaType(i, 0) {
            Ok(t) => {
                if t.GetGUID(&MF_MT_MAJOR_TYPE).map(|g| g == MFMediaType_Video).unwrap_or(false) {
                    vidx = i;
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let mut attrs: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut attrs, 2)?;
    let attrs = attrs.ok_or_else(e_fail)?;
    attrs.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
    let wo = wide(out);
    let wr = MFCreateSinkWriterFromURL(PCWSTR(wo.as_ptr()), None, &attrs)?;
    let vi = wr.AddStream(&vt)?;
    wr.SetInputMediaType(vi, &vt, None)?;
    let mut ai = None;
    if let Some(at) = &at {
        let i = wr.AddStream(at)?;
        wr.SetInputMediaType(i, at, None)?;
        ai = Some(i);
    }
    wr.BeginWriting()?;
    reader.SetCurrentPosition(&GUID::zeroed(), &PROPVARIANT::from(start.max(0)))?;
    let mut base: Option<i64> = None;
    let (mut v_done, mut a_done) = (false, ai.is_none());
    let mut wrote = 0u32;
    while !(v_done && a_done) {
        let mut actual = 0u32;
        let mut flags = 0u32;
        let mut ts = 0i64;
        let mut sample: Option<IMFSample> = None;
        reader.ReadSample(MF_SOURCE_READER_ANY_STREAM.0 as u32, 0, Some(&mut actual), Some(&mut flags), Some(&mut ts), Some(&mut sample))?;
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            break;
        }
        let Some(sm) = sample else { continue };
        let is_video = actual == vidx;
        if is_video {
            if ts >= end && base.is_some() {
                v_done = true;
                continue;
            }
            if base.is_none() {
                base = Some(ts);
            }
        } else {
            let Some(b) = base else { continue }; // wait for the first video frame
            if ts < b {
                continue;
            }
            if ts >= end {
                a_done = true;
                continue;
            }
        }
        let b = base.unwrap_or(0);
        let _ = sm.SetSampleTime(ts - b);
        if is_video {
            wr.WriteSample(vi, &sm)?;
            wrote += 1;
        } else if let Some(i) = ai {
            wr.WriteSample(i, &sm)?;
        }
    }
    wr.Finalize()?;
    if wrote == 0 {
        return Err(e_fail());
    }
    Ok(())
}

fn trim_status(msg: &str) {
    if let Some(t) = TRIM.lock().unwrap().as_mut() {
        t.status = msg.to_string();
    }
    post_frame_msg();
}

fn trim_export_start() {
    if EXPORTING.swap(true, Ordering::SeqCst) {
        return;
    }
    let uih = ui();
    let Some((path, a, b)) = TRIM.lock().unwrap().as_ref().map(|t| (t.path.clone(), t.start, t.end)) else {
        EXPORTING.store(false, Ordering::SeqCst);
        return;
    };
    if b - a < SEC / 2 {
        set_toast(&uih.sh, "Pick a longer range");
        trim_status("Pick a longer range (at least half a second)");
        EXPORTING.store(false, Ordering::SeqCst);
        return;
    }
    set_toast(&uih.sh, "Exporting clip…");
    trim_status("Exporting…");
    std::thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let stem = path.trim_end_matches(".mp4").to_string();
        let mut n = 1;
        let out = loop {
            let c = format!("{}_clip{}.mp4", stem, n);
            if !std::path::Path::new(&c).exists() {
                break c;
            }
            n += 1;
        };
        let res = trim_export(&path, a, b, &out);
        let ui = ui();
        match res {
            Ok(()) => {
                set_toast(&ui.sh, "Clip saved");
                let nm = std::path::Path::new(&out).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                trim_status(&format!("Saved {nm} in your recordings folder"));
            }
            Err(e) => {
                elogln!("trim: {e}");
                let _ = std::fs::remove_file(&out);
                set_toast(&ui.sh, "Clip export failed");
                trim_status(&format!("Export failed: {}", e.message()));
            }
        }
        refresh_library();
        post_frame_msg();
        EXPORTING.store(false, Ordering::SeqCst);
    });
}

// ---------- UI state ----------

struct Ui {
    sh: Arc<Shared>,
    device: String,
    #[allow(dead_code)]
    audio_name: String,
}

static UI: Mutex<Option<Sendable<Arc<Ui>>>> = Mutex::new(None);
static SCALE: AtomicU32 = AtomicU32::new(100);
static HOVER: AtomicI32 = AtomicI32::new(0);
static TICKS: AtomicU32 = AtomicU32::new(0);
static PANEL: AtomicBool = AtomicBool::new(false);
static FULL: AtomicBool = AtomicBool::new(false);
static DRAG: AtomicBool = AtomicBool::new(false);
static SAVED_PLACEMENT: Mutex<Option<Sendable<WINDOWPLACEMENT>>> = Mutex::new(None);
static DISP_PEAK: AtomicU32 = AtomicU32::new(0);

const BAR_H: i32 = 104;

// control ids
const C_REC: i32 = 1;
const C_SHOT: i32 = 2;
const C_REPLAY: i32 = 3;
const C_MUTE: i32 = 4;
const C_VOL: i32 = 5;
const C_FOLDER: i32 = 6;
const C_GEAR: i32 = 7;
const C_PAUSE: i32 = 8;
const C_LIB: i32 = 9;
const L_CLOSE: i32 = 60;
const L_FOLDER: i32 = 61;
const L_ROW0: i32 = 100; // visible row i: 100+4i play, +1 trim, +2 show, +3 delete
const L_VIS: i32 = 6;
const T_CLOSE: i32 = 70;
const T_TL: i32 = 71;
const T_SETIN: i32 = 72;
const T_SETOUT: i32 = 73;
const T_M1: i32 = 74;
const T_P1: i32 = 75;
const T_EXPORT: i32 = 76;
const P_CLOSE: i32 = 20;
const P_TAB0: i32 = 30; // 30..32
const P_ROW0: i32 = 200; // row i: 200+3i (left/main), 201+3i (right); keep clear of L_*/T_* ids

static TAB: AtomicI32 = AtomicI32::new(0);
static LIB_OPEN: AtomicBool = AtomicBool::new(false);
static LIB_SCROLL: AtomicI32 = AtomicI32::new(0);
static LIB: Mutex<Vec<LibItem>> = Mutex::new(Vec::new());

#[derive(Clone)]
struct LibItem {
    path: String,
    name: String,
    size: u64,
    age_secs: u64,
    mic: bool,
}

fn refresh_library() {
    let ui = ui();
    let dir = ui.sh.cfg.lock().unwrap().out_dir.clone();
    let mut v: Vec<(std::time::SystemTime, LibItem)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let path = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            let low = name.to_lowercase();
            if !(low.ends_with(".mp4") || low.ends_with(".mic.m4a")) {
                continue;
            }
            if let Ok(md) = e.metadata() {
                let t = md.modified().unwrap_or(std::time::UNIX_EPOCH);
                let age = t.elapsed().map(|d| d.as_secs()).unwrap_or(0);
                v.push((t, LibItem { path: path.to_string_lossy().to_string(), name, size: md.len(), age_secs: age, mic: low.ends_with(".m4a") }));
            }
        }
    }
    v.sort_by(|a, b| b.0.cmp(&a.0));
    *LIB.lock().unwrap() = v.into_iter().map(|x| x.1).collect();
    lib_worker_start();
}

fn age_text(sec: u64) -> String {
    match sec {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", sec / 60),
        3600..=86399 => format!("{} h ago", sec / 3600),
        _ => format!("{} d ago", sec / 86400),
    }
}

fn lib_panel_rect(rc: &RECT) -> RECT {
    let (pw, ph) = (s(820).min(rc.right - 20), s(530).min(rc.bottom - 20));
    let (x, y) = ((rc.right - pw) / 2, (rc.bottom - ph) / 2);
    RECT { left: x, top: y, right: x + pw, bottom: y + ph }
}

fn lib_card_rect(pr: &RECT, i: i32) -> RECT {
    let gap = s(16);
    let cwid = ((pr.right - pr.left) - 2 * s(24) - 2 * gap) / 3;
    let (col, row) = (i % 3, i / 3);
    let l = pr.left + s(24) + col * (cwid + gap);
    let t = pr.top + s(76) + row * s(206);
    RECT { left: l, top: t, right: l + cwid, bottom: t + s(190) }
}

fn lib_thumb_rect(c: &RECT) -> RECT {
    RECT { left: c.left, top: c.top, right: c.right, bottom: c.top + (c.right - c.left) * 9 / 16 }
}

#[allow(dead_code)]
fn lib_row_y(pr: &RECT, i: i32) -> i32 {
    pr.top + s(82) + i * s(66)
}

fn trim_panel_rect(rc: &RECT) -> RECT {
    let (pw, ph) = (s(800).min(rc.right - 20), s(560).min(rc.bottom - 20));
    let (x, y) = ((rc.right - pw) / 2, (rc.bottom - ph) / 2);
    RECT { left: x, top: y, right: x + pw, bottom: y + ph }
}

fn trim_frame_rect(pr: &RECT) -> RECT {
    let (fw, fh) = (s(560), s(315));
    let x = pr.left + (pr.right - pr.left - fw) / 2;
    RECT { left: x, top: pr.top + s(74), right: x + fw, bottom: pr.top + s(74) + fh }
}

fn lib_shell(verb: PCWSTR, file: &str, params: &str) {
    let wf = wide(file);
    let wp = wide(params);
    unsafe {
        ShellExecuteW(None, verb, PCWSTR(wf.as_ptr()), if params.is_empty() { PCWSTR::null() } else { PCWSTR(wp.as_ptr()) }, PCWSTR::null(), SW_SHOWNORMAL);
    }
}
static DISP_MIC: AtomicU32 = AtomicU32::new(0);

#[derive(Clone, Copy, PartialEq)]
enum K {
    Step,
    Toggle,
    Cycle,
    Change,
    Pick,
}

/// Inline text editing in the Stream tab: (0 = server address, 1 = stream key, text so far).
static EDIT: Mutex<Option<(u8, String)>> = Mutex::new(None);

fn edit_commit(sh: &Shared) {
    let Some((field, text)) = EDIT.lock().unwrap().take() else { return };
    // pasted text often carries spaces or line breaks from the web page: drop them
    let text: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if field == 0 {
        let low = text.to_ascii_lowercase();
        if low.starts_with("rtmp://") || low.starts_with("rtmps://") {
            sh.cfg.lock().unwrap().stream_custom = text;
            set_toast(sh, "Server address saved");
        } else {
            set_toast(sh, "The address must start with rtmp:// or rtmps://");
        }
    } else if !text.is_empty() {
        sh.cfg.lock().unwrap().stream_key = text;
        set_toast(sh, "Stream key saved");
    }
}

fn clipboard_text() -> Option<String> {
    use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
    use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    unsafe {
        OpenClipboard(None).ok()?;
        let out = (|| {
            let h = GetClipboardData(13).ok()?; // CF_UNICODETEXT
            let hg = windows::Win32::Foundation::HGLOBAL(h.0);
            let p = GlobalLock(hg) as *const u16;
            if p.is_null() {
                return None;
            }
            let mut n = 0usize;
            while n < 2048 && *p.add(n) != 0 {
                n += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
            let _ = GlobalUnlock(hg);
            Some(s)
        })();
        let _ = CloseClipboard();
        out
    }
}

fn tab_rows(tab: i32) -> &'static [K] {
    match tab {
        0 => &[K::Step, K::Cycle, K::Cycle, K::Cycle, K::Cycle, K::Cycle, K::Pick, K::Change],
        1 => &[K::Step, K::Pick, K::Step, K::Toggle, K::Toggle],
        2 => &[K::Toggle, K::Step, K::Toggle, K::Toggle, K::Cycle, K::Pick, K::Change, K::Change],
        4 => &[K::Cycle, K::Change, K::Change, K::Step, K::Cycle, K::Toggle, K::Change],
        5 => &[K::Pick, K::Step, K::Step, K::Pick, K::Cycle, K::Cycle, K::Cycle],
        _ => &[K::Cycle, K::Cycle, K::Cycle, K::Cycle, K::Cycle, K::Toggle, K::Toggle, K::Toggle],
    }
}

fn s(v: i32) -> i32 {
    v * SCALE.load(Ordering::Relaxed) as i32 / 100
}

fn ui() -> Arc<Ui> {
    UI.lock().unwrap().as_ref().unwrap().0.clone()
}

fn save_cfg(sh: &Shared) {
    sh.cfg.lock().unwrap().save();
}

fn toggle_record() {
    let sh = ui().sh.clone();
    if stop_recording() {
        set_toast(&sh, "Recording saved");
    } else {
        start_recording();
    }
}

fn start_recording() {
    let ui = ui();
    let sh = &ui.sh;
    let c = sh.cfg.lock().unwrap().clone();
    let _ = std::fs::create_dir_all(&c.out_dir);
    let stamp = timestamp_name();
    let path = format!("{}\\Lumacap_{}.mp4", c.out_dir, stamp);
    let mic_fmt = *sh.mic_fmt.lock().unwrap();
    let mic_on = sh.mic_idx.load(Ordering::Relaxed) >= 0 && mic_fmt.is_some();
    let opts = RecOpts {
        hevc: c.hevc,
        fmp4: c.fmp4,
        half: c.fps30 && sh.fmt.fps >= 50,
        mic_path: if c.mic_separate && mic_on { Some(format!("{}\\Lumacap_{}.mic.m4a", c.out_dir, stamp)) } else { None },
        mic_fmt,
    };
    match unsafe { start_recorder(&path, &sh.fmt, c.bitrate * 1_000_000, sh.afmt.as_ref(), &opts) } {
        Ok(rec) => {
            logln!("recording: {} ({} {}x{} @ {} fps{}, {} Mbps requested, {})", path, if c.hevc { "HEVC" } else { "H.264" }, sh.fmt.w, sh.fmt.h, if opts.half { sh.fmt.fps / 2 } else { sh.fmt.fps }, if opts.half { " (30 fps mode)" } else { "" }, c.bitrate, if c.fmp4 { "safe MP4" } else { "MP4" });
            *sh.rec.lock().unwrap() = Some(Arc::new(rec));
        }
        Err(e) => {
            elogln!("could not start recording: {e}");
            set_toast(sh, if c.hevc { "Could not start (HEVC needs a hardware encoder)" } else { "Could not start recording" });
        }
    }
}

fn toggle_mic_mute() {
    let sh = ui().sh.clone();
    let m = !sh.mic_muted.load(Ordering::Relaxed);
    sh.mic_muted.store(m, Ordering::Relaxed);
    set_toast(&sh, if m { "Microphone muted" } else { "Microphone on" });
}

fn toggle_pause() {
    let ui = ui();
    let sh = &ui.sh;
    let rec = sh.rec.lock().unwrap().clone();
    let Some(r) = rec else {
        set_toast(sh, "Not recording");
        return;
    };
    if r.paused.load(Ordering::SeqCst) {
        if let Some(t) = r.pause_start.lock().unwrap().take() {
            r.paused_ms.fetch_add(t.elapsed().as_millis() as i64, Ordering::SeqCst);
        }
        r.v_resume.store(true, Ordering::SeqCst);
        r.a_resume.store(true, Ordering::SeqCst);
        r.m_resume.store(true, Ordering::SeqCst);
        r.paused.store(false, Ordering::SeqCst);
        set_toast(sh, "Recording resumed");
    } else {
        *r.pause_start.lock().unwrap() = Some(Instant::now());
        r.paused.store(true, Ordering::SeqCst);
        set_toast(sh, "Recording paused");
    }
}

fn take_screenshot() {
    let ui = ui();
    if ui.sh.frames.load(Ordering::Relaxed) == 0 {
        set_toast(&ui.sh, "No video to capture yet");
    } else {
        ui.sh.shot_req.store(true, Ordering::SeqCst);
    }
}

fn open_folder() {
    let ui = ui();
    let dir = ui.sh.cfg.lock().unwrap().out_dir.clone();
    let _ = std::fs::create_dir_all(&dir);
    let wd = wide(&dir);
    unsafe {
        ShellExecuteW(None, w!("open"), PCWSTR(wd.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

fn choose_folder(hwnd: HWND, title: PCWSTR) -> Option<String> {
    unsafe {
        let bi = BROWSEINFOW { hwndOwner: hwnd, lpszTitle: title, ulFlags: BIF_RETURNONLYFSDIRS, ..Default::default() };
        let pidl = SHBrowseForFolderW(&bi);
        if pidl.is_null() {
            return None;
        }
        let mut buf = [0u16; 260];
        let ok = SHGetPathFromIDListW(pidl, &mut buf).as_bool();
        CoTaskMemFree(Some(pidl as *const c_void));
        if !ok {
            return None;
        }
        let n = buf.iter().position(|&c| c == 0).unwrap_or(260);
        Some(String::from_utf16_lossy(&buf[..n]))
    }
}

unsafe fn toggle_fullscreen(hwnd: HWND) {
    if FULL.load(Ordering::Relaxed) {
        SetWindowLongPtrW(hwnd, GWL_STYLE, (WS_OVERLAPPEDWINDOW | WS_VISIBLE).0 as isize);
        if let Some(p) = SAVED_PLACEMENT.lock().unwrap().as_ref() {
            let _ = SetWindowPlacement(hwnd, &p.0);
        }
        let _ = SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED);
        FULL.store(false, Ordering::Relaxed);
    } else {
        let mut wp = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
        let _ = GetWindowPlacement(hwnd, &mut wp);
        *SAVED_PLACEMENT.lock().unwrap() = Some(Sendable(wp));
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(mon, &mut mi);
        SetWindowLongPtrW(hwnd, GWL_STYLE, WS_VISIBLE.0 as isize);
        let r = mi.rcMonitor;
        let _ = SetWindowPos(hwnd, HWND_TOP, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_FRAMECHANGED);
        FULL.store(true, Ordering::Relaxed);
        set_toast(&ui().sh, "Press Esc or F11 to exit full screen");
    }
    let _ = InvalidateRect(hwnd, None, false);
}

unsafe fn apply_topmost(hwnd: HWND, on: bool) {
    let _ = SetWindowPos(
        hwnd,
        if on { HWND_TOPMOST } else { HWND_NOTOPMOST },
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );
}

// ---------- controls / layout ----------

fn panel_rect(rc: &RECT) -> RECT {
    let (pw, ph) = (s(620).min(rc.right - 20), s(540).min(rc.bottom - 20));
    let (x, y) = ((rc.right - pw) / 2, (rc.bottom - ph) / 2);
    RECT { left: x, top: y, right: x + pw, bottom: y + ph }
}

fn panel_row(pr: &RECT, i: i32) -> i32 {
    pr.top + s(124) + i * s(52)
}

fn controls(rc: &RECT) -> Vec<(i32, RECT)> {
    let mut v = Vec::new();
    let (w, h) = (rc.right, rc.bottom);
    let sq = |cx: i32, cy: i32, r: i32| RECT { left: cx - r, top: cy - r, right: cx + r, bottom: cy + r };
    let btn = |x: i32, y: i32, ww: i32, hh: i32| RECT { left: x, top: y, right: x + ww, bottom: y + hh };
    if TRIM_OPEN.load(Ordering::Relaxed) {
        let pr = trim_panel_rect(rc);
        v.push((T_CLOSE, sq(pr.right - s(36), pr.top + s(36), s(18))));
        v.push((T_TL, btn(pr.left + s(28), pr.top + s(412), pr.right - pr.left - s(56), s(36))));
        let y = pr.top + s(496);
        let x0 = pr.left + s(28);
        v.push((T_SETIN, btn(x0, y, s(120), s(40))));
        v.push((T_SETOUT, btn(x0 + s(130), y, s(120), s(40))));
        v.push((T_M1, btn(x0 + s(270), y, s(72), s(40))));
        v.push((T_P1, btn(x0 + s(350), y, s(72), s(40))));
        v.push((T_EXPORT, btn(pr.right - s(28) - s(170), y, s(170), s(40))));
        return v;
    }
    if LIB_OPEN.load(Ordering::Relaxed) {
        let pr = lib_panel_rect(rc);
        v.push((L_CLOSE, sq(pr.right - s(36), pr.top + s(36), s(18))));
        v.push((L_FOLDER, btn(pr.right - s(170), pr.top + s(20), s(114), s(32))));
        let n = LIB.lock().unwrap().len() as i32;
        let off = LIB_SCROLL.load(Ordering::Relaxed);
        for i in 0..L_VIS {
            if off + i >= n {
                break;
            }
            let tr = lib_thumb_rect(&lib_card_rect(&pr, i));
            for k in 0..4 {
                v.push((L_ROW0 + 4 * i + k, btn(tr.right - s(8) - (4 - k) * s(32), tr.bottom - s(36), s(28), s(28))));
            }
        }
        return v;
    }
    if PANEL.load(Ordering::Relaxed) {
        let pr = panel_rect(rc);
        v.push((P_CLOSE, sq(pr.right - s(36), pr.top + s(36), s(18))));
        for t in 0..6 {
            v.push((P_TAB0 + t, btn(pr.left + s(28) + t * s(80), pr.top + s(66), s(76), s(34))));
        }
        for (i, k) in tab_rows(TAB.load(Ordering::Relaxed)).iter().enumerate() {
            let i = i as i32;
            let (a, b) = (P_ROW0 + 3 * i, P_ROW0 + 3 * i + 1);
            let y = panel_row(&pr, i);
            match k {
                K::Step => {
                    v.push((a, btn(pr.right - s(230), y, s(40), s(36))));
                    v.push((b, btn(pr.right - s(70), y, s(40), s(36))));
                }
                K::Toggle => v.push((a, btn(pr.right - s(110), y + s(1), s(70), s(34)))),
                K::Cycle => v.push((a, btn(pr.right - s(210), y, s(170), s(36)))),
                K::Change => v.push((a, btn(pr.right - s(150), y, s(110), s(36)))),
                K::Pick => {
                    v.push((a, btn(pr.right - s(350), y, s(40), s(36))));
                    v.push((b, btn(pr.right - s(70), y, s(40), s(36))));
                }
            }
        }
        return v;
    }
    if FULL.load(Ordering::Relaxed) {
        return v;
    }
    let bt = h - s(BAR_H);
    let cy = bt + s(42);
    let cx = w / 2;
    v.push((C_PAUSE, sq(cx - s(165), cy, s(28))));
    v.push((C_REC, sq(cx - s(55), cy, s(28))));
    v.push((C_SHOT, sq(cx + s(55), cy, s(28))));
    v.push((C_REPLAY, sq(cx + s(165), cy, s(28))));
    v.push((C_GEAR, sq(w - s(40), cy, s(20))));
    v.push((C_FOLDER, sq(w - s(88), cy, s(20))));
    v.push((C_LIB, sq(w - s(136), cy, s(20))));
    v.push((C_MUTE, sq(w - s(348), cy - s(6), s(18))));
    v.push((C_VOL, RECT { left: w - s(320), top: cy - s(22), right: w - s(188), bottom: cy + s(10) }));
    v
}

fn hit_test(rc: &RECT, x: i32, y: i32) -> i32 {
    for (id, r) in controls(rc) {
        if x >= r.left && x < r.right && y >= r.top && y < r.bottom {
            return id;
        }
    }
    0
}

fn trim_set_from_x(rc: &RECT, x: i32) {
    if let Some((_, r)) = controls(rc).into_iter().find(|c| c.0 == T_TL) {
        let dur = TRIM.lock().unwrap().as_ref().map(|t| t.dur).unwrap_or(0);
        let f = ((x - r.left) as f64 / (r.right - r.left).max(1) as f64).clamp(0.0, 1.0);
        trim_seek((dur as f64 * f) as i64);
    }
}

fn slider_track(rc: &RECT) -> (i32, i32, i32) {
    let cy = rc.bottom - s(BAR_H) + s(42);
    (rc.right - s(320) + s(8), rc.right - s(188) - s(8), cy - s(6))
}

fn set_volume_from_x(rc: &RECT, x: i32) {
    let (x0, x1, _) = slider_track(rc);
    let v = ((x - x0) * 100 / (x1 - x0).max(1)).clamp(0, 100) as u32;
    let sh = &ui().sh;
    sh.vol.store(v, Ordering::Relaxed);
    sh.muted.store(false, Ordering::Relaxed);
}

fn sync_volume_cfg() {
    let sh = &ui().sh;
    {
        let mut c = sh.cfg.lock().unwrap();
        c.volume = sh.vol.load(Ordering::Relaxed);
        c.muted = sh.muted.load(Ordering::Relaxed);
    }
    save_cfg(sh);
}

unsafe fn click(hwnd: HWND, id: i32, x: i32) {
    if id != P_ROW0 + 3 * 1 && id != P_ROW0 + 3 * 2 {
        *EDIT.lock().unwrap() = None;
    }
    LAST_CLICK.store(id, Ordering::Relaxed);
    let ui = ui();
    let sh = &ui.sh;
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let step_u32 = |v: u32, which: i32, d: u32, lo: u32, hi: u32| -> u32 {
        if which == 0 {
            v.saturating_sub(d).max(lo)
        } else {
            (v + d).min(hi)
        }
    };
    match id {
        C_REC => toggle_record(),
        C_PAUSE => toggle_pause(),
        C_SHOT => take_screenshot(),
        C_REPLAY => save_replay(sh.clone()),
        C_MUTE => {
            let m = !sh.muted.load(Ordering::Relaxed);
            sh.muted.store(m, Ordering::Relaxed);
            sync_volume_cfg();
        }
        C_VOL => {
            set_volume_from_x(&rc, x);
            DRAG.store(true, Ordering::Relaxed);
            SetCapture(hwnd);
        }
        C_FOLDER => open_folder(),
        C_GEAR => PANEL.store(true, Ordering::Relaxed),
        C_LIB => {
            refresh_library();
            LIB_SCROLL.store(0, Ordering::Relaxed);
            LIB_OPEN.store(true, Ordering::Relaxed);
        }
        L_CLOSE => LIB_OPEN.store(false, Ordering::Relaxed),
        L_FOLDER => {
            let dir = sh.cfg.lock().unwrap().out_dir.clone();
            let _ = std::fs::create_dir_all(&dir);
            lib_shell(w!("open"), &dir, "");
        }
        T_CLOSE => trim_close(),
        T_TL => {
            trim_set_from_x(&rc, x);
            TL_DRAG.store(true, Ordering::Relaxed);
            SetCapture(hwnd);
        }
        T_SETIN => {
            if let Some(t) = TRIM.lock().unwrap().as_mut() {
                t.start = t.pos.min(t.end - SEC / 2).max(0);
            }
        }
        T_SETOUT => {
            if let Some(t) = TRIM.lock().unwrap().as_mut() {
                t.end = t.pos.max(t.start + SEC / 2).min(t.dur);
            }
        }
        T_M1 | T_P1 => {
            let p = TRIM.lock().unwrap().as_ref().map(|t| t.pos).unwrap_or(0);
            trim_seek(p + if id == T_P1 { SEC } else { -SEC });
        }
        T_EXPORT => trim_export_start(),
        r if (L_ROW0..L_ROW0 + 4 * L_VIS).contains(&r) => {
            let (i, which) = ((r - L_ROW0) / 4, (r - L_ROW0) % 4);
            let item = LIB.lock().unwrap().get((LIB_SCROLL.load(Ordering::Relaxed) + i) as usize).cloned();
            if let Some(it) = item {
                match which {
                    0 => lib_shell(w!("open"), &it.path, ""),
                    1 => {
                        if it.mic {
                            set_toast(sh, "Trim works on video recordings");
                        } else {
                            trim_open(&it);
                        }
                    }
                    2 => lib_shell(w!("open"), "explorer.exe", &format!("/select,\"{}\"", it.path)),
                    _ => {
                        let msg = wide(&format!("Delete this recording permanently?\n\n{}", it.name));
                        let ans = MessageBoxW(hwnd, PCWSTR(msg.as_ptr()), w!("Lumacap"), MB_YESNO | MB_ICONQUESTION);
                        if ans == IDYES {
                            let _ = std::fs::remove_file(&it.path);
                            if !it.mic {
                                let mic = it.path.trim_end_matches(".mp4").to_string() + ".mic.m4a";
                                let _ = std::fs::remove_file(mic);
                            }
                            refresh_library();
                            let n = LIB.lock().unwrap().len() as i32;
                            let off = LIB_SCROLL.load(Ordering::Relaxed).min(((n + 2) / 3 * 3 - L_VIS).max(0));
                            LIB_SCROLL.store(off, Ordering::Relaxed);
                        }
                    }
                }
            }
        }
        P_CLOSE => {
            *EDIT.lock().unwrap() = None;
            PANEL.store(false, Ordering::Relaxed)
        }
        t if (P_TAB0..P_TAB0 + 6).contains(&t) => TAB.store(t - P_TAB0, Ordering::Relaxed),
        r if r >= P_ROW0 => {
            let (i, which) = ((r - P_ROW0) / 3, (r - P_ROW0) % 3);
            let tab = TAB.load(Ordering::Relaxed);
            match (tab, i) {
                (0, 0) => {
                    let mut c = sh.cfg.lock().unwrap();
                    c.bitrate = step_u32(c.bitrate, which, 5, 5, 80);
                }
                (0, 1) => {
                    let mut c = sh.cfg.lock().unwrap();
                    c.hevc = !c.hevc;
                }
                (0, 2) => {
                    let mut c = sh.cfg.lock().unwrap();
                    c.fps30 = !c.fps30;
                }
                (0, 3) => {
                    let mut c = sh.cfg.lock().unwrap();
                    c.fmp4 = !c.fmp4;
                }
                (1, 0) => {
                    let v = {
                        let mut c = sh.cfg.lock().unwrap();
                        c.game_gain = step_u32(c.game_gain, which, 10, 0, 200);
                        c.game_gain
                    };
                    sh.game_gain.store(v, Ordering::Relaxed);
                }
                (1, 1) => {
                    let mut opts = vec![String::new()];
                    opts.extend(sh.mic_names.iter().cloned());
                    let cur = {
                        let c = sh.cfg.lock().unwrap();
                        opts.iter().position(|n| *n == c.mic_name).unwrap_or(0)
                    };
                    let n = opts.len();
                    let next = if which == 0 { (cur + n - 1) % n } else { (cur + 1) % n };
                    sh.cfg.lock().unwrap().mic_name = opts[next].clone();
                    start_mic(sh, &opts[next]);
                }
                (1, 2) => {
                    let v = {
                        let mut c = sh.cfg.lock().unwrap();
                        c.mic_gain = step_u32(c.mic_gain, which, 10, 0, 300);
                        c.mic_gain
                    };
                    sh.mic_gain.store(v, Ordering::Relaxed);
                }
                (1, 3) => {
                    let mut c = sh.cfg.lock().unwrap();
                    c.mic_separate = !c.mic_separate;
                }
                (2, 0) => {
                    let on = sh.replay.lock().unwrap().is_some();
                    if on {
                        stop_replay(sh);
                        sh.cfg.lock().unwrap().replay_on = false;
                    } else {
                        match start_replay(sh) {
                            Ok(()) => sh.cfg.lock().unwrap().replay_on = true,
                            Err(e) => {
                                elogln!("replay: {e}");
                                set_toast(sh, "Could not start instant replay");
                            }
                        }
                    }
                }
                (2, 1) => {
                    let mut c = sh.cfg.lock().unwrap();
                    c.replay_secs = step_u32(c.replay_secs, which, 10, 10, 120);
                }
                (2, 2) => {
                    let mut c = sh.cfg.lock().unwrap();
                    c.clip_beep = !c.clip_beep;
                }
                (2, 3) => {
                    let on = {
                        let mut c = sh.cfg.lock().unwrap();
                        c.topmost = !c.topmost;
                        c.topmost
                    };
                    apply_topmost(hwnd, on);
                }
                (2, 4) => {
                    let l = {
                        let mut c = sh.cfg.lock().unwrap();
                        c.light = !c.light;
                        c.light
                    };
                    LIGHT.store(l, Ordering::Relaxed);
                    apply_titlebar(hwnd, l);
                }
                (2, 5) => {
                    let n = ACCENTS.len() as u32;
                    let cur = ACCENT_IDX.load(Ordering::Relaxed);
                    let nx = if which == 0 { (cur + n - 1) % n } else { (cur + 1) % n };
                    ACCENT_IDX.store(nx, Ordering::Relaxed);
                    sh.cfg.lock().unwrap().accent = nx;
                }
                (2, 6) => {
                    if let Some(p) = choose_folder(hwnd, w!("Choose the folder for recordings")) {
                        sh.cfg.lock().unwrap().out_dir = p;
                    }
                }
                (2, 7) => {
                    if let Some(p) = choose_folder(hwnd, w!("Choose the folder for screenshots")) {
                        sh.cfg.lock().unwrap().shot_dir = p;
                    }
                }
                (3, 7) => {
                    let mut c = sh.cfg.lock().unwrap();
                    c.update_mode = if c.update_mode == 0 { 1 } else { 0 };
                }
                (4, 0) => {
                    let mut c = sh.cfg.lock().unwrap();
                    c.stream_server = stream::next_service(c.stream_server, which != 0);
                }
                (4, 1) => {
                    let custom = stream::is_custom(&sh.cfg.lock().unwrap());
                    if custom {
                        *EDIT.lock().unwrap() = Some((0, String::new()));
                    } else {
                        set_toast(sh, "Set Service to Custom server to type your own address");
                    }
                }
                (4, 2) => {
                    *EDIT.lock().unwrap() = Some((1, String::new()));
                }
                (4, 3) => {
                    let mut c = sh.cfg.lock().unwrap();
                    c.stream_kbps = step_u32(c.stream_kbps, which, 500, 1000, 12000);
                }
                (4, 4) => {
                    let mut c = sh.cfg.lock().unwrap();
                    c.stream_fps30 = !c.stream_fps30;
                }
                (4, 6) => stream::test_connection(),
                (5, 0) => {
                    let mut c = sh.cfg.lock().unwrap();
                    c.zoom = if which == 0 { c.zoom.saturating_sub(1) } else { (c.zoom + 1).min(4) };
                    scene::sync(&c);
                }
                (5, 1) => {
                    let mut c = sh.cfg.lock().unwrap();
                    c.pan_x = step_u32(c.pan_x, which, 10, 0, 100);
                    scene::sync(&c);
                }
                (5, 2) => {
                    let mut c = sh.cfg.lock().unwrap();
                    c.pan_y = step_u32(c.pan_y, which, 10, 0, 100);
                    scene::sync(&c);
                }
                (5, 3) => {
                    let names: Vec<String> = sh.video_names.iter().filter(|n| **n != ui.device).cloned().collect();
                    let cur = sh.cfg.lock().unwrap().cam_name.clone();
                    // list: Off, then every other video device
                    let pos = if cur.is_empty() { 0 } else { names.iter().position(|n| *n == cur).map(|i| i + 1).unwrap_or(0) };
                    let n = names.len() + 1;
                    let next = if which == 0 { (pos + n - 1) % n } else { (pos + 1) % n };
                    let name = if next == 0 { String::new() } else { names[next - 1].clone() };
                    sh.cfg.lock().unwrap().cam_name = name.clone();
                    scene::set_cam(&name, &ui.device);
                    if names.is_empty() {
                        set_toast(sh, "No webcam found");
                    }
                }
                (5, 4) => {
                    let mut c = sh.cfg.lock().unwrap();
                    c.cam_pos = (c.cam_pos + 1) % 4;
                    scene::sync(&c);
                }
                (5, 5) => {
                    let mut c = sh.cfg.lock().unwrap();
                    c.cam_size = (c.cam_size + 1) % 3;
                    scene::sync(&c);
                }
                (5, 6) => {
                    let mut c = sh.cfg.lock().unwrap();
                    c.res = (c.res + 1) % 4;
                    drop(c);
                    set_toast(sh, "Restart Lumacap to apply the new resolution");
                }
                (4, 5) => {
                    if sh.stream.lock().unwrap().is_some() {
                        stream::stop_stream();
                        set_toast(sh, "Stream stopped");
                    } else {
                        save_cfg(sh);
                        stream::go_live();
                    }
                }
                (1, 4) => {
                    let m = !sh.mic_muted.load(Ordering::Relaxed);
                    sh.mic_muted.store(m, Ordering::Relaxed);
                }
                (0, 5) => {
                    let presets = [10u32, 20, 40, 60];
                    let mut c = sh.cfg.lock().unwrap();
                    let cur = presets.iter().position(|&p| p >= c.bitrate).unwrap_or(presets.len() - 1);
                    let next = if which == 0 { (cur + presets.len() - 1) % presets.len() } else { (cur + 1) % presets.len() };
                    c.bitrate = presets[next];
                }
                (0, 7) => {
                    lib_shell(w!("open"), "https://help.elgato.com/hc/en-us/articles/360027961152", "");
                }
                (0, 6) => {
                    let n = sh.video_names.len();
                    if n > 1 {
                        let name = {
                            let mut c = sh.cfg.lock().unwrap();
                            let cur = sh.video_names.iter().position(|v| *v == c.video_name).unwrap_or(0);
                            let next = if which == 0 { (cur + n - 1) % n } else { (cur + 1) % n };
                            c.video_name = sh.video_names[next].clone();
                            c.video_name.clone()
                        };
                        let _ = name;
                        set_toast(sh, "Restart Lumacap to switch capture card");
                    }
                }
                (0, 4) => {
                    let mut c = sh.cfg.lock().unwrap();
                    c.split_min = match c.split_min {
                        0 => 15,
                        15 => 30,
                        30 => 60,
                        _ => 0,
                    };
                }
                (3, k) if k < 5 => {
                    let cur = HK_CAPTURE.load(Ordering::Relaxed);
                    HK_CAPTURE.store(if cur == k { -1 } else { k }, Ordering::Relaxed);
                }
                (3, 5) => {
                    let on = {
                        let mut c = sh.cfg.lock().unwrap();
                        c.tray = !c.tray;
                        if !c.tray && c.autostart {
                            c.autostart = false;
                            set_autostart(false);
                        }
                        c.tray
                    };
                    if on {
                        tray_add(hwnd);
                    } else {
                        tray_remove(hwnd);
                    }
                }
                (3, 6) => {
                    let (on, need_tray) = {
                        let mut c = sh.cfg.lock().unwrap();
                        c.autostart = !c.autostart;
                        let need = c.autostart && !c.tray;
                        if need {
                            c.tray = true;
                        }
                        (c.autostart, need)
                    };
                    set_autostart(on);
                    if need_tray {
                        tray_add(hwnd);
                    }
                }
                _ => {}
            }
            save_cfg(sh);
        }
        _ => {}
    }
    let _ = InvalidateRect(hwnd, None, false);
}

// ---------- drawing helpers ----------

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}

fn argb(c: COLORREF) -> u32 {
    0xFF00_0000 | ((c.0 & 0xFF) << 16) | (c.0 & 0xFF00) | ((c.0 >> 16) & 0xFF)
}

unsafe fn fill_rect(hdc: HDC, r: &RECT, col: COLORREF) {
    let b = CreateSolidBrush(th(col));
    FillRect(hdc, r, b);
    let _ = DeleteObject(b);
}

unsafe fn aa(hdc: HDC, f: impl FnOnce(*mut GpGraphics)) {
    let mut g: *mut GpGraphics = std::ptr::null_mut();
    if GdipCreateFromHDC(hdc, &mut g) == Status(0) && !g.is_null() {
        GdipSetSmoothingMode(g, SmoothingModeAntiAlias);
        f(g);
        GdipDeleteGraphics(g);
    }
}

unsafe fn fill_ring(
    g: *mut GpGraphics,
    fill_argb: u32,
    ring: Option<(COLORREF, i32)>,
    draw_fill: impl Fn(*mut GpGraphics, *mut GpBrush),
    draw_ring: impl Fn(*mut GpGraphics, *mut GpPen),
) {
    let mut br: *mut GpSolidFill = std::ptr::null_mut();
    GdipCreateSolidFill(fill_argb, &mut br);
    draw_fill(g, br as *mut GpBrush);
    GdipDeleteBrush(br as *mut GpBrush);
    if let Some((c, wd)) = ring {
        let mut pen: *mut GpPen = std::ptr::null_mut();
        GdipCreatePen1(argb(th(c)), wd as f32, UnitWorld, &mut pen);
        draw_ring(g, pen);
        GdipDeletePen(pen);
    }
}

unsafe fn circle(hdc: HDC, c: (i32, i32), r: i32, fill: COLORREF, ring: Option<(COLORREF, i32)>) {
    aa(hdc, |g| {
        let (x, y, d) = ((c.0 - r) as f32, (c.1 - r) as f32, (2 * r) as f32);
        let inset = ring.map_or(0.0, |(_, w)| w as f32 / 2.0);
        fill_ring(
            g,
            argb(th(fill)),
            ring,
            |g, b| {
                GdipFillEllipse(g, b, x, y, d, d);
            },
            |g, pen| {
                GdipDrawEllipse(g, pen, x + inset, y + inset, d - 2.0 * inset, d - 2.0 * inset);
            },
        );
    });
}

unsafe fn rr_path(r: &RECT, rad: i32, inset: f32) -> *mut GpPath {
    let mut path: *mut GpPath = std::ptr::null_mut();
    GdipCreatePath(FillModeAlternate, &mut path);
    let (x, y) = (r.left as f32 + inset, r.top as f32 + inset);
    let (w, h) = ((r.right - r.left) as f32 - 2.0 * inset, (r.bottom - r.top) as f32 - 2.0 * inset);
    let d = (rad as f32).min(w).min(h).max(1.0);
    GdipAddPathArc(path, x, y, d, d, 180.0, 90.0);
    GdipAddPathArc(path, x + w - d, y, d, d, 270.0, 90.0);
    GdipAddPathArc(path, x + w - d, y + h - d, d, d, 0.0, 90.0);
    GdipAddPathArc(path, x, y + h - d, d, d, 90.0, 90.0);
    GdipClosePathFigure(path);
    path
}

unsafe fn round_rect_argb(hdc: HDC, r: &RECT, rad: i32, fill_argb: u32, ring: Option<(COLORREF, i32)>) {
    aa(hdc, |g| {
        let inset = ring.map_or(0.0, |(_, w)| w as f32 / 2.0);
        let p_fill = rr_path(r, rad, 0.0);
        let p_ring = rr_path(r, rad, inset);
        fill_ring(
            g,
            fill_argb,
            ring,
            |g, b| {
                GdipFillPath(g, b, p_fill);
            },
            |g, pen| {
                GdipDrawPath(g, pen, p_ring);
            },
        );
        GdipDeletePath(p_fill);
        GdipDeletePath(p_ring);
    });
}

unsafe fn round_rect(hdc: HDC, r: &RECT, rad: i32, fill: COLORREF, ring: Option<(COLORREF, i32)>) {
    round_rect_argb(hdc, r, rad, argb(th(fill)), ring);
}

unsafe fn dim(hdc: HDC, r: &RECT, alpha: u8) {
    aa(hdc, |g| {
        let mut br: *mut GpSolidFill = std::ptr::null_mut();
        GdipCreateSolidFill((alpha as u32) << 24, &mut br);
        GdipFillRectangle(g, br as *mut GpBrush, r.left as f32, r.top as f32, (r.right - r.left) as f32, (r.bottom - r.top) as f32);
        GdipDeleteBrush(br as *mut GpBrush);
    });
}

unsafe fn text(hdc: HDC, t: &str, r: &mut RECT, col: COLORREF, flags: DRAW_TEXT_FORMAT) {
    if t.is_empty() {
        return; // DrawText with an empty buffer + ellipsis flags dereferences a dangling pointer
    }
    SetTextColor(hdc, th(col));
    let mut wv: Vec<u16> = t.encode_utf16().collect();
    wv.push(0);
    wv.pop();
    wv.reserve(8);
    DrawTextW(hdc, &mut wv, r, flags);
}

unsafe fn glyph(hdc: HDC, code: u32, r: &RECT, col: COLORREF) {
    if let Some(ch) = char::from_u32(code) {
        let mut rr = *r;
        let mut b = [0u16; 2];
        let enc = ch.encode_utf16(&mut b);
        SetTextColor(hdc, th(col));
        DrawTextW(hdc, enc, &mut rr, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    }
}

unsafe fn font(size: i32, weight: i32, face: PCWSTR) -> HFONT {
    CreateFontW(
        -s(size),
        0,
        0,
        0,
        weight,
        0,
        0,
        0,
        DEFAULT_CHARSET.0 as u32,
        OUT_DEFAULT_PRECIS.0 as u32,
        CLIP_DEFAULT_PRECIS.0 as u32,
        CLEARTYPE_QUALITY.0 as u32,
        0,
        face,
    )
}

fn center(r: &RECT) -> (i32, i32) {
    ((r.left + r.right) / 2, (r.top + r.bottom) / 2)
}

unsafe fn draw_thumb(hdc: HDC, t: &Thumb, dest: &RECT) {
    fill_rect(hdc, dest, rgb(13, 14, 17));
    let (dw, dh) = (dest.right - dest.left, dest.bottom - dest.top);
    let (fw, fh) = if dw as i64 * t.h as i64 > dh as i64 * t.w as i64 {
        ((dh as i64 * t.w as i64 / t.h as i64) as i32, dh)
    } else {
        (dw, (dw as i64 * t.h as i64 / t.w as i64) as i32)
    };
    let (x, y) = (dest.left + (dw - fw) / 2, dest.top + (dh - fh) / 2);
    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = t.w as i32;
    bmi.bmiHeader.biHeight = -(t.h as i32);
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = BI_RGB.0;
    SetStretchBltMode(hdc, HALFTONE);
    let _ = SetBrushOrgEx(hdc, 0, 0, None);
    StretchDIBits(hdc, x, y, fw, fh, 0, 0, t.w as i32, t.h as i32, Some(t.data.as_ptr() as *const c_void), &bmi, DIB_RGB_COLORS, SRCCOPY);
}

fn mixc(a: COLORREF, b: COLORREF, t: f32) -> COLORREF {
    let ch = |sh: u32| {
        let (x, y) = (((a.0 >> sh) & 0xFF) as f32, ((b.0 >> sh) & 0xFF) as f32);
        (x + (y - x) * t.clamp(0.0, 1.0)) as u8
    };
    rgb(ch(0), ch(8), ch(16))
}

static ANIM: Mutex<[i32; 128]> = Mutex::new([0; 128]);

/// Soft drop shadow made of stacked translucent rounded rects.
unsafe fn shadow(hdc: HDC, r: &RECT, rad: i32, spread: i32) {
    let layers = 4;
    for i in (1..=layers).rev() {
        let g = i * spread / layers;
        let rr = RECT { left: r.left - g, top: r.top - g + s(5), right: r.right + g, bottom: r.bottom + g + s(5) };
        round_rect_argb(hdc, &rr, rad + g, 0x0F000000, None);
    }
}

unsafe fn toggle_switch(hdc: HDC, r: &RECT, on: bool, hover: bool) {
    let track = if on { accent_col() } else { rgb(49, 54, 65) };
    round_rect(hdc, r, r.bottom - r.top, if hover { track_light(track.0) } else { track }, None);
    let d = r.bottom - r.top - s(8);
    let kx = if on { r.right - s(4) - d / 2 } else { r.left + s(4) + d / 2 };
    circle(hdc, (kx, (r.top + r.bottom) / 2), d / 2, rgb(255, 255, 255), None);
}

fn track_light(c: u32) -> COLORREF {
    let (r, g, b) = ((c & 0xFF) as u8, ((c >> 8) & 0xFF) as u8, ((c >> 16) & 0xFF) as u8);
    rgb(r.saturating_add(20), g.saturating_add(20), b.saturating_add(20))
}

// ---------- paint ----------

/// The last fully painted frame, kept so new video frames can be blitted without repainting the whole UI.
struct Fast {
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    w: i32,
    h: i32,
    dx: i32,
    dy: i32,
    dw: i32,
    dh: i32,
    prad: i32,
    full: bool,
    rec: Option<(u64, bool)>,
    replay: Option<String>,
    fps: Option<String>,
}
unsafe impl Send for Fast {}
static FAST: Mutex<Option<Fast>> = Mutex::new(None);
static NEXT_PRESENT_US: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn now_us() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_micros() as u64
}

unsafe fn free_fast(f: Fast) {
    SelectObject(f.dc, f.old);
    let _ = DeleteObject(f.bmp);
    let _ = DeleteDC(f.dc);
}

unsafe fn draw_preview(hdc: HDC, dx: i32, dy: i32, dw: i32, dh: i32, data: &[u32], pvw: i32, pvh: i32) {
    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = pvw;
    bmi.bmiHeader.biHeight = -pvh;
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = BI_RGB.0;
    // HALFTONE was far too slow for live video at large window sizes
    SetStretchBltMode(hdc, COLORONCOLOR);
    let _ = SetBrushOrgEx(hdc, 0, 0, None);
    StretchDIBits(hdc, dx, dy, dw, dh, 0, 0, pvw, pvh, Some(data.as_ptr() as *const c_void), &bmi, DIB_RGB_COLORS, SRCCOPY);
}

unsafe fn draw_chips(hdc: HDC, dx: i32, dy: i32, dw: i32, rec: &Option<(u64, bool)>, replay: &Option<String>, fps: &Option<String>) {
    let white = rgb(236, 238, 242);
    let red = rgb(240, 68, 76);
    let c_border = rgb(44, 48, 58);
    let chip = |hdc: HDC, r: &RECT| round_rect_argb(hdc, r, (r.bottom - r.top) / 2, 0xC8101216, Some((c_border, 1)));
    let f_bold = font(16, 700, w!("Segoe UI"));
    let f_small = font(13, 400, w!("Segoe UI"));
    if let Some((secs, paused)) = rec {
        SelectObject(hdc, f_bold);
        let pill = RECT { left: dx + s(16), top: dy + s(16), right: dx + s(176), bottom: dy + s(50) };
        chip(hdc, &pill);
        circle(hdc, (pill.left + s(20), (pill.top + pill.bottom) / 2), s(6), if *paused { rgb(240, 190, 40) } else { red }, None);
        let mut tr = RECT { left: pill.left + s(38), top: pill.top + s(5), right: pill.right, bottom: pill.bottom };
        text(hdc, &format!("{} {:02}:{:02}", if *paused { "PAUSED" } else { "REC" }, secs / 60, secs % 60), &mut tr, white, DT_LEFT | DT_SINGLELINE);
    } else if let Some(label) = replay {
        SelectObject(hdc, f_small);
        let pill = RECT { left: dx + s(16), top: dy + s(16), right: dx + s(160), bottom: dy + s(46) };
        chip(hdc, &pill);
        circle(hdc, (pill.left + s(18), (pill.top + pill.bottom) / 2), s(4), rgb(52, 199, 120), None);
        let mut tr = RECT { left: pill.left + s(32), top: pill.top + s(5), right: pill.right, bottom: pill.bottom };
        text(hdc, label, &mut tr, white, DT_LEFT | DT_SINGLELINE);
    }
    if let Some(label_t) = fps {
        SelectObject(hdc, f_small);
        let wpx = s(40) + label_t.chars().count() as i32 * s(7);
        let pill = RECT { left: dx + dw - s(16) - wpx, top: dy + s(16), right: dx + dw - s(16), bottom: dy + s(46) };
        chip(hdc, &pill);
        let mut tr = RECT { left: pill.left, top: pill.top + s(5), right: pill.right, bottom: pill.bottom };
        text(hdc, label_t, &mut tr, white, DT_CENTER | DT_SINGLELINE);
    }
    if let Some((label_t, warn)) = stream::chip_text() {
        SelectObject(hdc, f_bold);
        let top = dy + if rec.is_some() { s(58) } else if replay.is_some() { s(54) } else { s(16) };
        let wpx = s(56) + label_t.chars().count() as i32 * s(9);
        let pill = RECT { left: dx + s(16), top, right: dx + s(16) + wpx, bottom: top + s(34) };
        chip(hdc, &pill);
        circle(hdc, (pill.left + s(20), (pill.top + pill.bottom) / 2), s(6), if warn { rgb(240, 190, 40) } else { rgb(145, 80, 255) }, None);
        let mut tr = RECT { left: pill.left + s(38), top: pill.top + s(5), right: pill.right, bottom: pill.bottom };
        text(hdc, &label_t, &mut tr, white, DT_LEFT | DT_SINGLELINE);
    }
    SelectObject(hdc, GetStockObject(SYSTEM_FONT));
    let _ = DeleteObject(f_bold);
    let _ = DeleteObject(f_small);
}

/// Push the newest video frame to the screen without repainting the controls. Returns false if a full repaint is needed.
unsafe fn fast_present(hwnd: HWND) -> bool {
    let g = FAST.lock().unwrap();
    let Some(f) = g.as_ref() else { return false };
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    if rc.right != f.w || rc.bottom != f.h {
        return false;
    }
    let sh = &ui().sh;
    {
        let t = sh.toast.lock().unwrap();
        if t.1.elapsed().as_millis() < 2800 && !t.0.is_empty() {
            return false;
        }
    }
    let fw = sh.fmt.w as i64;
    sh.pv_full.store(f.dw as i64 * 10 > fw * 6, Ordering::Relaxed);
    PV_TW.store(f.dw, Ordering::Relaxed);
    PV_TH.store(f.dh, Ordering::Relaxed);
    let hdc = f.dc;
    let rgn = if f.prad > 0 {
        let r = CreateRoundRectRgn(f.dx, f.dy, f.dx + f.dw + 1, f.dy + f.dh + 1, f.prad * 2, f.prad * 2);
        SelectClipRgn(hdc, r);
        Some(r)
    } else {
        None
    };
    {
        let buf = sh.preview.lock().unwrap();
        if buf.data.is_empty() {
            if let Some(r) = rgn {
                SelectClipRgn(hdc, None);
                let _ = DeleteObject(r);
            }
            return false;
        }
        let td = now_us();
        draw_preview(hdc, f.dx, f.dy, f.dw, f.dh, &buf.data, buf.w as i32, buf.h as i32);
        T_DRAW.fetch_add(now_us().saturating_sub(td), Ordering::Relaxed);
    }
    if let Some(r) = rgn {
        SelectClipRgn(hdc, None);
        let _ = DeleteObject(r);
    }
    let tc = now_us();
    let prev_rect = RECT { left: f.dx, top: f.dy, right: f.dx + f.dw, bottom: f.dy + f.dh };
    if !f.full {
        round_rect_argb(hdc, &prev_rect, f.prad, 0x00000000, Some((rgb(44, 48, 58), 1)));
    }
    draw_chips(hdc, f.dx, f.dy, f.dw, &f.rec, &f.replay, &f.fps);
    T_CHIPS.fetch_add(now_us().saturating_sub(tc), Ordering::Relaxed);
    let tb = now_us();
    let wdc = GetDC(hwnd);
    let (l, t) = ((f.dx - 2).max(0), (f.dy - 2).max(0));
    let (r, b) = ((f.dx + f.dw + 2).min(f.w), (f.dy + f.dh + 2).min(f.h));
    let _ = BitBlt(wdc, l, t, r - l, b - t, hdc, l, t, SRCCOPY);
    ReleaseDC(hwnd, wdc);
    T_BLT.fetch_add(now_us().saturating_sub(tb), Ordering::Relaxed);
    true
}

unsafe fn paint(hwnd: HWND) {
    PAINTS.fetch_add(1, Ordering::Relaxed);
    let mut ps = PAINTSTRUCT::default();
    let hdc0 = BeginPaint(hwnd, &mut ps);
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let (cw, ch) = (rc.right.max(1), rc.bottom.max(1));
    let hdc = CreateCompatibleDC(hdc0);
    let bmp = CreateCompatibleBitmap(hdc0, cw, ch);
    let old_bmp = SelectObject(hdc, bmp);
    SetBkMode(hdc, TRANSPARENT);

    let ui = ui();
    let sh = &ui.sh;
    let full = FULL.load(Ordering::Relaxed);
    let panel = PANEL.load(Ordering::Relaxed);
    let ctl = controls(&rc);
    let rect_of = |id: i32| ctl.iter().find(|c| c.0 == id).map(|c| c.1).unwrap_or_default();
    let recording = sh.rec.lock().unwrap().as_ref().map(|r| (rec_elapsed_secs(r), r.path.clone(), r.paused.load(Ordering::Relaxed)));
    let replay_on = sh.replay.lock().unwrap().is_some();
    let frames = sh.frames.load(Ordering::Relaxed);
    let fps_in = sh.fps_meas.load(Ordering::Relaxed);
    let hover = if panel && DRAG.load(Ordering::Relaxed) { 0 } else { HOVER.load(Ordering::Relaxed) };
    let muted = sh.muted.load(Ordering::Relaxed);
    let vol = sh.vol.load(Ordering::Relaxed);
    let cfg = sh.cfg.lock().unwrap().clone();
    let anim = {
        let mut a = ANIM.lock().unwrap();
        for id in 0..128usize {
            let tgt = if hover == id as i32 { 1000 } else { 0 };
            a[id] += (tgt - a[id]).clamp(-240, 240);
        }
        *a
    };
    let hv_t = |id: i32| anim[(id.max(0) as usize).min(127)] as f32 / 1000.0;

    let f_small = font(13, 400, w!("Segoe UI"));
    let f_norm = font(15, 400, w!("Segoe UI"));
    let f_bold = font(16, 700, w!("Segoe UI"));
    let f_big = font(20, 600, w!("Segoe UI"));
    let f_icon = font(20, 400, w!("Segoe MDL2 Assets"));
    let white = rgb(236, 238, 242);
    let muted_c = rgb(139, 146, 160);
    let red = rgb(240, 68, 76);
    let accent = accent_col();
    let c_surface = rgb(24, 26, 32);
    let c_surface2 = rgb(38, 42, 51);
    let c_hot = rgb(52, 57, 70);
    let c_border = rgb(44, 48, 58);

    fill_rect(hdc, &rc, rgb(13, 14, 17));

    // ---- preview ----
    let bh = if full { 0 } else { s(BAR_H) };
    let pad = if full { 0 } else { s(12) };
    let (pw, ph) = (cw, ch - bh);
    let (aw, ah) = ((pw - 2 * pad).max(1), (ph - 2 * pad).max(1));
    let (fw, fh) = (sh.fmt.w as i64, sh.fmt.h as i64);
    let (dw, dh) = if aw as i64 * fh > ah as i64 * fw { ((ah as i64 * fw / fh) as i32, ah) } else { (aw, (aw as i64 * fh / fw) as i32) };
    let (dx, dy) = ((pw - dw) / 2, (ph - dh) / 2);

    let prev_rect = RECT { left: dx, top: dy, right: dx + dw, bottom: dy + dh };
    let prad = if full { 0 } else { s(16) };
    if !full {
        shadow(hdc, &prev_rect, prad, s(20));
    }
    let clip_rgn = if prad > 0 {
        let r = CreateRoundRectRgn(dx, dy, dx + dw + 1, dy + dh + 1, prad * 2, prad * 2);
        SelectClipRgn(hdc, r);
        Some(r)
    } else {
        None
    };
    if frames > 0 && fps_in > 0 {
        sh.pv_full.store(dw as i64 * 10 > fw * 6, Ordering::Relaxed);
        PV_TW.store(dw, Ordering::Relaxed);
        PV_TH.store(dh, Ordering::Relaxed);
        let buf = sh.preview.lock().unwrap();
        draw_preview(hdc, dx, dy, dw, dh, &buf.data, buf.w as i32, buf.h as i32);
    } else {
        let box_r = RECT { left: dx, top: dy, right: dx + dw, bottom: dy + dh };
        fill_rect(hdc, &box_r, rgb(13, 14, 17));
        let old = SelectObject(hdc, f_big);
        let mut r1 = RECT { left: dx, top: dy + dh / 2 - s(40), right: dx + dw, bottom: dy + dh / 2 - s(8) };
        text(hdc, if frames == 0 { "Waiting for video…" } else { "No signal" }, &mut r1, white, DT_CENTER | DT_SINGLELINE);
        SelectObject(hdc, f_norm);
        let mut r2 = RECT { left: dx + s(20), top: dy + dh / 2 + s(4), right: dx + dw - s(20), bottom: dy + dh };
        let msg = format!("{}\nCheck the HDMI cable and source, and that no other app is using the card.", ui.device);
        text(hdc, &msg, &mut r2, muted_c, DT_CENTER | DT_WORDBREAK);
        SelectObject(hdc, old);
    }
    if let Some(r) = clip_rgn {
        SelectClipRgn(hdc, None);
        let _ = DeleteObject(r);
    }
    if !full {
        round_rect_argb(hdc, &prev_rect, prad, 0x00000000, Some((c_border, 1)));
    }

    // ---- overlays (glass chips) ----
    let chip_rec: Option<(u64, bool)> = recording.as_ref().map(|(secs, _, paused)| (*secs as u64, *paused));
    let chip_replay: Option<String> = if recording.is_none() && replay_on { Some(format!("Replay {}s", cfg.replay_secs)) } else { None };
    let chip_fps: Option<String> = if frames > 0 && fps_in > 0 { Some(format!("{}p  ·  {} fps", sh.fmt.h, fps_in)) } else { None };
    draw_chips(hdc, dx, dy, dw, &chip_rec, &chip_replay, &chip_fps);

    // ---- bottom bar (floating dock) ----
    if !full {
        let bar = RECT { left: 0, top: ch - bh, right: cw, bottom: ch };
        let dock = RECT { left: s(14), top: bar.top + s(4), right: cw - s(14), bottom: ch - s(10) };
        shadow(hdc, &dock, s(24), s(16));
        round_rect(hdc, &dock, s(24), c_surface, Some((c_border, 1)));
        let btn_fill = |id: i32| mixc(c_surface2, c_hot, hv_t(id));

        SelectObject(hdc, f_bold);
        let mut r = RECT { left: s(34), top: bar.top + s(24), right: cw / 2 - s(235), bottom: bar.top + s(46) };
        text(hdc, &ui.device, &mut r, white, DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS);
        SelectObject(hdc, f_small);
        let note = sh.audio_note.lock().unwrap().clone();
        let status = if frames > 0 && fps_in > 0 {
            let mut st = String::new();
            if let Some((_, path, _)) = &recording {
                let mb = std::fs::metadata(path).map(|m| m.len() as f64 / 1_048_576.0).unwrap_or(0.0);
                st += &format!("recording · {:.1} MB  ·  ", mb);
            }
            st += &format!("{}x{} · {} fps", sh.fmt.w, sh.fmt.h, fps_in);
            if sh.afmt.is_some() {
                st += "  ·  audio on";
            } else if !note.is_empty() {
                st += &format!("  ·  {note}");
            }
            st
        } else {
            String::from("no signal")
        };
        let mut r = RECT { left: s(34), top: bar.top + s(48), right: cw / 2 - s(235), bottom: bar.top + s(70) };
        text(hdc, &status, &mut r, muted_c, DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS);

        let is_rec = recording.is_some();
        let is_paused = matches!(&recording, Some((_, _, true)));

        // record: solid red, white centre; pulses while recording
        let rr = rect_of(C_REC);
        let rcn = center(&rr);
        let pulse = if is_rec && !is_paused { ((TICKS.load(Ordering::Relaxed) % 60) as f32 / 60.0 * std::f32::consts::TAU).sin() * 0.5 + 0.5 } else { 0.0 };
        let rec_fill = mixc(mixc(red, rgb(255, 110, 116), hv_t(C_REC)), rgb(170, 36, 44), pulse * 0.55);
        circle(hdc, rcn, s(29), rec_fill, None);
        if is_rec {
            round_rect(hdc, &RECT { left: rcn.0 - s(9), top: rcn.1 - s(9), right: rcn.0 + s(9), bottom: rcn.1 + s(9) }, s(5), rgb(255, 255, 255), None);
        } else {
            circle(hdc, rcn, s(9), rgb(255, 255, 255), None);
        }
        // pause
        let ur = rect_of(C_PAUSE);
        let ucn = center(&ur);
        circle(hdc, ucn, s(25), if is_rec { btn_fill(C_PAUSE) } else { c_surface2 }, if is_paused { Some((rgb(240, 190, 40), s(2))) } else { None });
        SelectObject(hdc, f_icon);
        glyph(hdc, if is_paused { 0xE768 } else { 0xE769 }, &ur, if is_rec { white } else { rgb(78, 85, 100) });
        // screenshot
        let sr = rect_of(C_SHOT);
        let scn = center(&sr);
        circle(hdc, scn, s(25), btn_fill(C_SHOT), None);
        SelectObject(hdc, f_icon);
        glyph(hdc, 0xE722, &sr, white);
        // replay
        let pr = rect_of(C_REPLAY);
        let pcn = center(&pr);
        circle(hdc, pcn, s(25), btn_fill(C_REPLAY), if replay_on { Some((rgb(52, 199, 120), s(2))) } else { None });
        SelectObject(hdc, f_icon);
        glyph(hdc, 0xE81C, &pr, if replay_on { white } else { muted_c });

        SelectObject(hdc, f_small);
        let lbl = |hdc: HDC, cx: i32, t: &str| {
            let mut l = RECT { left: cx - s(54), top: bar.top + s(76), right: cx + s(54), bottom: ch };
            text(hdc, t, &mut l, muted_c, DT_CENTER | DT_SINGLELINE);
        };
        let (kr, ks, kp, ku) = (vk_name(cfg.hk_rec), vk_name(cfg.hk_shot), vk_name(cfg.hk_replay), vk_name(cfg.hk_pause));
        lbl(hdc, ucn.0, &if is_paused { format!("Resume ({ku})") } else { format!("Pause ({ku})") });
        lbl(hdc, rcn.0, &if is_rec { format!("Stop ({kr})") } else { format!("Record ({kr})") });
        lbl(hdc, scn.0, &format!("Screenshot ({ks})"));
        lbl(hdc, pcn.0, &format!("Save replay ({kp})"));

        // volume: mute icon, slider, meter
        let mr = rect_of(C_MUTE);
        if hv_t(C_MUTE) > 0.01 {
            circle(hdc, center(&mr), s(18), mixc(c_surface, c_surface2, hv_t(C_MUTE)), None);
        }
        SelectObject(hdc, f_icon);
        let ic = if muted || vol == 0 { 0xE74F } else if vol < 34 { 0xE993 } else if vol < 67 { 0xE994 } else { 0xE995 };
        glyph(hdc, ic, &mr, if muted { muted_c } else { white });
        let (x0, x1, ty) = slider_track(&rc);
        let eff = if muted { 0 } else { vol as i32 };
        let kx = x0 + (x1 - x0) * eff / 100;
        round_rect(hdc, &RECT { left: x0, top: ty - s(2), right: x1, bottom: ty + s(2) }, s(4), c_surface2, None);
        if kx > x0 {
            round_rect(hdc, &RECT { left: x0, top: ty - s(2), right: kx, bottom: ty + s(2) }, s(4), accent, None);
        }
        circle(hdc, (kx, ty + 1), s(9), rgb(10, 11, 14), None);
        circle(hdc, (kx, ty), s(8), rgb(255, 255, 255), None);
        // level meter (smoothed)
        let raw = sh.peak.load(Ordering::Relaxed);
        let prev = DISP_PEAK.load(Ordering::Relaxed);
        let disp = raw.max(prev * 88 / 100);
        DISP_PEAK.store(disp, Ordering::Relaxed);
        let level = ((disp as f32 / 32768.0).sqrt().clamp(0.0, 1.0) * (x1 - x0) as f32) as i32;
        let my = ty + s(20);
        round_rect(hdc, &RECT { left: x0, top: my - s(3), right: x1, bottom: my + s(3) }, s(6), c_surface2, None);
        if level > 2 {
            let frac = level as f32 / (x1 - x0) as f32;
            let col = if frac > 0.9 { red } else if frac > 0.7 { rgb(240, 190, 40) } else { rgb(52, 199, 120) };
            round_rect(hdc, &RECT { left: x0, top: my - s(3), right: x0 + level, bottom: my + s(3) }, s(6), col, None);
        }

        // folder + library + settings
        for (id, code) in [(C_FOLDER, 0xE8B7u32), (C_LIB, 0xE8F1u32), (C_GEAR, 0xE713u32)] {
            let r = rect_of(id);
            if hv_t(id) > 0.01 {
                circle(hdc, center(&r), s(21), mixc(c_surface, c_surface2, hv_t(id)), None);
            }
            SelectObject(hdc, f_icon);
            glyph(hdc, code, &r, mixc(muted_c, white, hv_t(id)));
        }
    }

    // ---- recording library ----
    if LIB_OPEN.load(Ordering::Relaxed) && !TRIM_OPEN.load(Ordering::Relaxed) {
        dim(hdc, &rc, 150);
        let pr = lib_panel_rect(&rc);
        shadow(hdc, &pr, s(18), s(26));
        round_rect(hdc, &pr, s(18), rgb(31, 34, 41), Some((rgb(49, 54, 65), 1)));
        SelectObject(hdc, f_big);
        let mut t = RECT { left: pr.left + s(28), top: pr.top + s(18), right: pr.right, bottom: pr.top + s(58) };
        text(hdc, "Recordings", &mut t, white, DT_LEFT | DT_SINGLELINE);
        let cb = rect_of(L_CLOSE);
        if hover == L_CLOSE {
            circle(hdc, center(&cb), s(17), rgb(38, 42, 51), None);
        }
        SelectObject(hdc, f_icon);
        glyph(hdc, 0xE711, &cb, white);
        {
            let fr = rect_of(L_FOLDER);
            round_rect(hdc, &fr, s(10), if hover == L_FOLDER { rgb(49, 54, 65) } else { rgb(38, 42, 51) }, None);
            SelectObject(hdc, f_norm);
            let mut tr = fr;
            tr.top += s(5);
            text(hdc, "Open folder", &mut tr, white, DT_CENTER | DT_SINGLELINE);
        }
        let items = LIB.lock().unwrap().clone();
        let off = LIB_SCROLL.load(Ordering::Relaxed);
        if items.is_empty() {
            SelectObject(hdc, f_norm);
            let mut r = RECT { left: pr.left, top: pr.top + s(150), right: pr.right, bottom: pr.top + s(190) };
            text(hdc, "No recordings yet. Press F9 to record.", &mut r, muted_c, DT_CENTER | DT_SINGLELINE);
        }
        for i in 0..L_VIS {
            let idx = (off + i) as usize;
            let Some(it) = items.get(idx) else { break };
            let card = lib_card_rect(&pr, i);
            let thr = lib_thumb_rect(&card);
            let info = with_thumbs(|m| m.get(&it.path).filter(|e| e.0 == it.size).cloned());
            // thumbnail with rounded corners
            let rgn = CreateRoundRectRgn(thr.left, thr.top, thr.right + 1, thr.bottom + 1, s(24), s(24));
            SelectClipRgn(hdc, rgn);
            match info.as_ref().and_then(|e| e.2.clone()) {
                Some(t) => draw_thumb(hdc, &t, &thr),
                None => {
                    fill_rect(hdc, &thr, rgb(31, 34, 41));
                    SelectObject(hdc, f_icon);
                    glyph(hdc, if it.mic { 0xE720 } else { 0xE714 }, &thr, rgb(78, 85, 100));
                }
            }
            SelectClipRgn(hdc, None);
            let _ = DeleteObject(rgn);
            round_rect_argb(hdc, &thr, s(12), 0x00000000, Some((c_border, 1)));
            // duration badge
            let dur = info.as_ref().and_then(|e| e.1).map(fmt_dur).unwrap_or_else(|| "…".into());
            SelectObject(hdc, f_small);
            let badge = RECT { left: thr.left + s(8), top: thr.bottom - s(32), right: thr.left + s(8) + s(24) + dur.chars().count() as i32 * s(7), bottom: thr.bottom - s(8) };
            round_rect_argb(hdc, &badge, s(12), 0xC8101216, None);
            let mut br = RECT { left: badge.left, top: badge.top + s(3), right: badge.right, bottom: badge.bottom };
            text(hdc, &dur, &mut br, rgb(255, 255, 255), DT_CENTER | DT_SINGLELINE);
            // action buttons on the thumbnail
            for (k, code) in [0xE768u32, 0xE8C6, 0xE838, 0xE74D].iter().enumerate() {
                let id = L_ROW0 + 4 * i + k as i32;
                let r = rect_of(id);
                let off_btn = k == 1 && it.mic;
                let hot = hover == id && !off_btn;
                let col = if hot { if k == 3 { 0xF0F0444C } else { (argb(accent) & 0x00FFFFFF) | 0xF0000000 } } else { 0xC8101216 };
                round_rect_argb(hdc, &r, s(14), col, None);
                SelectObject(hdc, f_icon);
                glyph(hdc, *code, &r, if off_btn { rgb(120, 126, 140) } else { rgb(255, 255, 255) });
            }
            // title + details
            SelectObject(hdc, f_norm);
            let mut a = RECT { left: card.left + s(2), top: thr.bottom + s(8), right: card.right, bottom: thr.bottom + s(30) };
            text(hdc, &it.name, &mut a, white, DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS);
            SelectObject(hdc, f_small);
            let mut b = RECT { left: card.left + s(2), top: thr.bottom + s(30), right: card.right, bottom: thr.bottom + s(50) };
            let kind = if it.mic { "  ·  mic track" } else { "" };
            text(hdc, &format!("{:.1} MB  ·  {}{}", it.size as f64 / 1_048_576.0, age_text(it.age_secs), kind), &mut b, muted_c, DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS);
        }
        if items.len() as i32 > L_VIS {
            SelectObject(hdc, f_small);
            let mut r = RECT { left: pr.left, top: pr.bottom - s(32), right: pr.right, bottom: pr.bottom - s(10) };
            text(hdc, &format!("{}–{} of {}  ·  scroll for more", off + 1, (off + L_VIS).min(items.len() as i32), items.len()), &mut r, muted_c, DT_CENTER | DT_SINGLELINE);
        }
    }

    // ---- trim view ----
    if TRIM_OPEN.load(Ordering::Relaxed) {
        dim(hdc, &rc, 170);
        let pr = trim_panel_rect(&rc);
        shadow(hdc, &pr, s(18), s(26));
        round_rect(hdc, &pr, s(18), rgb(31, 34, 41), Some((rgb(49, 54, 65), 1)));
        let (name, dur, start, end, pos, frame) = match TRIM.lock().unwrap().as_ref() {
            Some(t) => (t.name.clone(), t.dur, t.start, t.end, t.pos, t.frame.clone()),
            None => (String::new(), 1, 0, 1, 0, None),
        };
        SelectObject(hdc, f_big);
        let mut t = RECT { left: pr.left + s(28), top: pr.top + s(18), right: pr.right - s(60), bottom: pr.top + s(58) };
        text(hdc, "Trim clip", &mut t, white, DT_LEFT | DT_SINGLELINE);
        SelectObject(hdc, f_small);
        let mut t2 = RECT { left: pr.left + s(140), top: pr.top + s(26), right: pr.right - s(60), bottom: pr.top + s(50) };
        text(hdc, &name, &mut t2, muted_c, DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS);
        let cb = rect_of(T_CLOSE);
        if hover == T_CLOSE {
            circle(hdc, center(&cb), s(17), rgb(38, 42, 51), None);
        }
        SelectObject(hdc, f_icon);
        glyph(hdc, 0xE711, &cb, white);

        let fr = trim_frame_rect(&pr);
        match frame {
            Some(f) => draw_thumb(hdc, &f, &fr),
            None => {
                fill_rect(hdc, &fr, rgb(13, 14, 17));
                SelectObject(hdc, f_norm);
                let mut r = fr;
                r.top += (fr.bottom - fr.top) / 2 - s(10);
                text(hdc, "Loading…", &mut r, muted_c, DT_CENTER | DT_SINGLELINE);
            }
        }

        // timeline
        let tl = rect_of(T_TL);
        let (tx0, tx1) = (tl.left, tl.right);
        let xs = |v: i64| tx0 + ((tx1 - tx0) as f64 * (v as f64 / dur.max(1) as f64)) as i32;
        let mid = (tl.top + tl.bottom) / 2;
        round_rect(hdc, &RECT { left: tx0, top: mid - s(7), right: tx1, bottom: mid + s(7) }, s(14), rgb(38, 42, 51), None);
        round_rect(hdc, &RECT { left: xs(start), top: mid - s(7), right: xs(end).max(xs(start) + s(4)), bottom: mid + s(7) }, s(14), accent, None);
        for hx in [xs(start), xs(end)] {
            round_rect(hdc, &RECT { left: hx - s(3), top: mid - s(14), right: hx + s(3), bottom: mid + s(14) }, s(4), rgb(236, 238, 242), None);
        }
        let px = xs(pos);
        round_rect(hdc, &RECT { left: px - s(2), top: tl.top, right: px + s(2), bottom: tl.bottom }, s(3), rgb(240, 190, 40), None);

        SelectObject(hdc, f_norm);
        let mut st = RECT { left: pr.left + s(28), top: pr.top + s(456), right: pr.right - s(28), bottom: pr.top + s(484) };
        text(hdc, &format!("Start {}     End {}     Clip length {}     Playhead {}", fmt_time(start), fmt_time(end), fmt_dur(end - start), fmt_time(pos)), &mut st, white, DT_LEFT | DT_SINGLELINE);

        let exporting = EXPORTING.load(Ordering::Relaxed);
        for (id, label_t) in [(T_SETIN, "Set start"), (T_SETOUT, "Set end"), (T_M1, "‹ 1 s"), (T_P1, "1 s ›"), (T_EXPORT, if exporting { "Exporting…" } else { "Export clip" })] {
            let r = rect_of(id);
            let fill = if id == T_EXPORT { if hover == id { mixc(accent, rgb(255, 255, 255), 0.2) } else { accent } } else if hover == id { rgb(49, 54, 65) } else { rgb(38, 42, 51) };
            round_rect(hdc, &r, s(12), fill, None);
            SelectObject(hdc, f_bold);
            let mut rr = r;
            rr.top += s(9);
            text(hdc, label_t, &mut rr, if id == T_EXPORT { rgb(255, 255, 255) } else { white }, DT_CENTER | DT_SINGLELINE);
        }
        SelectObject(hdc, f_small);
        let mut hint = RECT { left: pr.left + s(28), top: pr.top + s(540) - s(2), right: pr.right - s(28), bottom: pr.bottom };
        hint.top = pr.bottom - s(18);
        let status = TRIM.lock().unwrap().as_ref().map(|t| t.status.clone()).unwrap_or_default();
        if status.is_empty() {
            text(hdc, "Drag the bar or use ← → (Shift = 5 s). Cuts without re-encoding, so the clip starts at the nearest keyframe before Start.", &mut hint, muted_c, DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS);
        } else {
            text(hdc, &status, &mut hint, if status.starts_with("Export failed") { red } else { accent }, DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS);
        }
    }

    // ---- settings panel ----
    if panel {
        dim(hdc, &rc, 150);
        let pr = panel_rect(&rc);
        shadow(hdc, &pr, s(18), s(26));
        round_rect(hdc, &pr, s(18), rgb(31, 34, 41), Some((rgb(49, 54, 65), 1)));
        SelectObject(hdc, f_big);
        let mut t = RECT { left: pr.left + s(28), top: pr.top + s(18), right: pr.right, bottom: pr.top + s(58) };
        text(hdc, "Settings", &mut t, white, DT_LEFT | DT_SINGLELINE);
        let cb = rect_of(P_CLOSE);
        if hover == P_CLOSE {
            circle(hdc, center(&cb), s(17), rgb(38, 42, 51), None);
        }
        SelectObject(hdc, f_icon);
        glyph(hdc, 0xE711, &cb, white);

        let tab = TAB.load(Ordering::Relaxed);
        for (i, name) in ["Video", "Audio", "General", "System", "Stream", "Scene"].iter().enumerate() {
            let r = rect_of(P_TAB0 + i as i32);
            let sel = tab == i as i32;
            let fill = if sel { accent } else if hover == P_TAB0 + i as i32 { rgb(49, 54, 65) } else { rgb(38, 42, 51) };
            round_rect(hdc, &r, s(17), fill, None);
            SelectObject(hdc, f_norm);
            let mut rr = r;
            rr.top += s(6);
            text(hdc, name, &mut rr, if sel { rgb(255, 255, 255) } else { white }, DT_CENTER | DT_SINGLELINE);
        }

        let label = |hdc: HDC, row: i32, name: &str, sub: &str| {
            let y = panel_row(&pr, row);
            SelectObject(hdc, f_norm);
            let mut a = RECT { left: pr.left + s(28), top: y, right: pr.right - s(240), bottom: y + s(20) };
            text(hdc, name, &mut a, white, DT_LEFT | DT_SINGLELINE);
            SelectObject(hdc, f_small);
            let mut b = RECT { left: pr.left + s(28), top: y + s(20), right: pr.right - s(240), bottom: y + s(40) };
            text(hdc, sub, &mut b, muted_c, DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS);
        };
        let small_btn = |hdc: HDC, id: i32, t: &str| {
            let r = rect_of(id);
            round_rect(hdc, &r, s(10), if hover == id { rgb(49, 54, 65) } else { rgb(38, 42, 51) }, None);
            SelectObject(hdc, f_bold);
            let mut rr = r;
            rr.top += s(6);
            text(hdc, t, &mut rr, white, DT_CENTER | DT_SINGLELINE);
        };
        let between = |hdc: HDC, a: i32, b: i32, t: &str| {
            let (ra, rb) = (rect_of(a), rect_of(b));
            SelectObject(hdc, f_norm);
            let mut r = RECT { left: ra.right, top: ra.top + s(6), right: rb.left, bottom: ra.bottom };
            text(hdc, t, &mut r, white, DT_CENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
        };
        let id_a = |i: i32| P_ROW0 + 3 * i;
        let step = |hdc: HDC, i: i32, v: &str| {
            small_btn(hdc, id_a(i), "−");
            small_btn(hdc, id_a(i) + 1, "+");
            between(hdc, id_a(i), id_a(i) + 1, v);
        };
        let pick = |hdc: HDC, i: i32, v: &str| {
            small_btn(hdc, id_a(i), "‹");
            small_btn(hdc, id_a(i) + 1, "›");
            between(hdc, id_a(i), id_a(i) + 1, v);
        };
        let cycle = |hdc: HDC, i: i32, v: &str| small_btn(hdc, id_a(i), v);
        let toggle = |hdc: HDC, i: i32, on: bool| toggle_switch(hdc, &rect_of(id_a(i)), on, hover == id_a(i));

        match tab {
            0 => {
                label(hdc, 0, "Video quality", "Bitrate of new recordings");
                step(hdc, 0, &format!("{} Mbps", cfg.bitrate));
                label(hdc, 1, "Codec", "HEVC = smaller files, needs a hardware encoder");
                cycle(hdc, 1, if cfg.hevc { "HEVC (H.265)" } else { "H.264" });
                label(hdc, 2, "Frame rate", "30 fps halves the file size");
                cycle(hdc, 2, if cfg.fps30 { "30 fps" } else { "60 fps" });
                label(hdc, 3, "File format", "Crash-safe keeps the video if the app or PC dies");
                cycle(hdc, 3, if cfg.fmp4 { "Crash-safe MP4" } else { "MP4" });
                label(hdc, 4, "Split long recordings", "Starts a new file automatically; keeps files small and safe");
                cycle(hdc, 4, match cfg.split_min {
                    0 => "Off",
                    15 => "Every 15 min",
                    30 => "Every 30 min",
                    _ => "Every 60 min",
                });
                label(hdc, 5, "Quality", "Preset for the bitrate above");
                cycle(hdc, 5, match cfg.bitrate {
                    10 => "Small",
                    20 => "Balanced",
                    40 => "High",
                    60 => "Max",
                    _ => "Custom",
                });
                label(hdc, 6, "Capture card", &format!("Restart to apply  ·  {}  ·  {} glitched frames", MODE.lock().unwrap(), sh.glitches.load(Ordering::Relaxed)));
                pick(hdc, 6, if cfg.video_name.is_empty() { &ui.device } else { &cfg.video_name });
                label(hdc, 7, "Elgato drivers", "Opens Elgato's official driver page");
                cycle(hdc, 7, "Open page");
            }
            1 => {
                label(hdc, 0, "Game volume", "What you hear and what is recorded (0 = silent)");
                step(hdc, 0, &format!("{}%", cfg.game_gain));
                let err = sh.mic_err.lock().unwrap().clone();
                label(hdc, 1, "Microphone", if err.is_empty() { "Mixed into the recording" } else { &err });
                pick(hdc, 1, if cfg.mic_name.is_empty() { "Off" } else { &cfg.mic_name });
                label(hdc, 2, "Mic volume", "");
                step(hdc, 2, &format!("{}%", cfg.mic_gain));
                // live mic meter under the label
                let y = panel_row(&pr, 2) + s(26);
                let (mx0, mx1) = (pr.left + s(28), pr.left + s(268));
                round_rect(hdc, &RECT { left: mx0, top: y, right: mx1, bottom: y + s(7) }, s(7), rgb(38, 42, 51), None);
                let raw = sh.mic_peak.load(Ordering::Relaxed);
                let disp = raw.max(DISP_MIC.load(Ordering::Relaxed) * 88 / 100);
                DISP_MIC.store(disp, Ordering::Relaxed);
                let lvl = ((disp as f32 / 32768.0).sqrt().clamp(0.0, 1.0) * (mx1 - mx0) as f32) as i32;
                if lvl > 2 {
                    let frac = lvl as f32 / (mx1 - mx0) as f32;
                    let col = if frac > 0.9 { red } else if frac > 0.7 { rgb(240, 190, 40) } else { rgb(60, 190, 90) };
                    round_rect(hdc, &RECT { left: mx0, top: y, right: mx0 + lvl, bottom: y + s(7) }, s(7), col, None);
                }
                label(hdc, 3, "Separate mic file", "Also saves your mic alone as a .m4a next to the video");
                toggle(hdc, 3, cfg.mic_separate);
                label(hdc, 4, "Mute microphone", &format!("Also on the {} key", vk_name(cfg.hk_mic)));
                toggle(hdc, 4, sh.mic_muted.load(Ordering::Relaxed));
                SelectObject(hdc, f_small);
                let y = panel_row(&pr, 5) + s(6);
                let playing = MON_PLAYING.load(Ordering::Relaxed);
                let mut m1 = RECT { left: pr.left + s(28), top: y, right: pr.right - s(28), bottom: y + s(20) };
                text(hdc, if playing { "Lumacap is playing the game sound through your speakers." } else { "Lumacap is not playing any game sound (volume at 0 or muted)." }, &mut m1, muted_c, DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS);
                let mut m2 = RECT { left: pr.left + s(28), top: y + s(22), right: pr.right - s(28), bottom: y + s(64) };
                text(hdc, "Still hear it at 0? Then it comes from outside Lumacap: the card's HDMI pass-through to your TV or monitor, or Windows 'Listen to this device' on the capture card.", &mut m2, muted_c, DT_LEFT | DT_WORDBREAK);
            }
            2 => {
                label(hdc, 0, "Instant replay", "Starts with the app; F8 saves the last moments");
                toggle(hdc, 0, replay_on);
                label(hdc, 1, "Replay length", "How far back F8 saves");
                step(hdc, 1, &format!("{} s", cfg.replay_secs));
                label(hdc, 2, "Clip sound", "Beep when a replay is saved");
                toggle(hdc, 2, cfg.clip_beep);
                label(hdc, 3, "Always on top", "Keep this window above other windows");
                toggle(hdc, 3, cfg.topmost);
                label(hdc, 4, "Theme", "Dark or light interface");
                cycle(hdc, 4, if cfg.light { "Light" } else { "Dark" });
                label(hdc, 5, "Accent colour", "Buttons, toggles and highlights");
                {
                    let ai = (cfg.accent as usize).min(5);
                    small_btn(hdc, id_a(5), "‹");
                    small_btn(hdc, id_a(5) + 1, "›");
                    let (ra, rb) = (rect_of(id_a(5)), rect_of(id_a(5) + 1));
                    let cxm = (ra.right + rb.left) / 2;
                    let cym = (ra.top + ra.bottom) / 2;
                    let (cr, cg, cb2) = ACCENTS[ai].1;
                    circle(hdc, (cxm - s(40), cym), s(9), rgb(cr, cg, cb2), None);
                    SelectObject(hdc, f_norm);
                    let mut tr = RECT { left: cxm - s(22), top: ra.top + s(6), right: rb.left, bottom: ra.bottom };
                    text(hdc, ACCENTS[ai].0, &mut tr, white, DT_LEFT | DT_SINGLELINE);
                }
                label(hdc, 6, "Recordings folder", &cfg.out_dir);
                cycle(hdc, 6, "Change");
                label(hdc, 7, "Screenshots folder", &cfg.shot_dir);
                cycle(hdc, 7, "Change");
            }
            5 => {
                label(hdc, 0, "Zoom", "Crops the picture and scales it back up");
                pick(hdc, 0, &format!("{}%", scene::ZOOMS[(cfg.zoom as usize).min(4)]));
                label(hdc, 1, "Pan left / right", "Where the zoomed area sits");
                step(hdc, 1, &format!("{}%", cfg.pan_x));
                label(hdc, 2, "Pan up / down", "Where the zoomed area sits");
                step(hdc, 2, &format!("{}%", cfg.pan_y));
                label(hdc, 3, "Webcam overlay", "Shown in the preview, recordings, screenshots and streams");
                pick(hdc, 3, if cfg.cam_name.is_empty() { "Off" } else { &cfg.cam_name });
                label(hdc, 4, "Webcam position", "Corner of the picture");
                cycle(hdc, 4, scene::CAM_POS[(cfg.cam_pos as usize).min(3)]);
                label(hdc, 5, "Webcam size", "Width relative to the picture");
                cycle(hdc, 5, scene::SIZE_NAMES[(cfg.cam_size as usize).min(2)]);
                label(hdc, 6, "Capture resolution", "Closest mode your card offers. Restart to apply");
                cycle(hdc, 6, ["Auto (1080p)", "720p", "1440p", "4K (2160p)"][(cfg.res as usize).min(3)]);
                SelectObject(hdc, f_small);
                let y = panel_row(&pr, 7) + s(2);
                let mut m1 = RECT { left: pr.left + s(28), top: y, right: pr.right - s(28), bottom: y + s(40) };
                text(hdc, "Zoom and webcam are experimental. 4K needs a card that offers it and a fast PC.", &mut m1, muted_c, DT_LEFT | DT_WORDBREAK);
            }
            4 => {
                let live = sh.stream.lock().unwrap().is_some();
                label(hdc, 0, "Service", "For Kick or others, choose Custom and paste your server address");
                cycle(hdc, 0, stream::service_name(cfg.stream_server));
                let ed = EDIT.lock().unwrap().clone();
                let caret = if (GetTickCount64() / 500) % 2 == 0 { "|" } else { " " };
                label(hdc, 1, "Server address", &if let Some((0, tx)) = &ed { format!("Type or Ctrl+V, Enter to save:  {tx}{caret}") } else if stream::is_custom(&cfg) { if cfg.stream_custom.is_empty() { "Not set. Click Edit and type your rtmp:// or rtmps:// address".to_string() } else { cfg.stream_custom.clone() } } else { stream::server_url(&cfg) });
                cycle(hdc, 1, if matches!(&ed, Some((0, _))) { "Editing…" } else if stream::is_custom(&cfg) { "Edit" } else { "Preset" });
                let k = cfg.stream_key.trim();
                label(hdc, 2, "Stream key", &if let Some((1, tx)) = &ed { format!("Type or Ctrl+V, Enter to save:  {}{caret}  ({} chars)", "•".repeat(tx.chars().count().min(40)), tx.chars().count()) } else if k.is_empty() { "Not set. Click Edit, then paste your key".to_string() } else { format!("Saved: ••••••{}", k.chars().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect::<String>()) });
                cycle(hdc, 2, if matches!(&ed, Some((1, _))) { "Editing…" } else { "Edit" });
                label(hdc, 3, "Stream bitrate", stream::bitrate_hint(&cfg));
                step(hdc, 3, &format!("{} kbps", cfg.stream_kbps));
                label(hdc, 4, "Frame rate", "30 fps uses less upload bandwidth");
                cycle(hdc, 4, if cfg.stream_fps30 { "30 fps" } else { "Source" });
                label(hdc, 5, "Go live", &if live { stream::status_text().unwrap_or_default() } else { "Streams the capture with the GPU encoder".to_string() });
                toggle(hdc, 5, live);
                SelectObject(hdc, f_small);
                label(hdc, 6, "Test connection", "Connects and disconnects without sending video");
                cycle(hdc, 6, "Test");
                let y = panel_row(&pr, 7) + s(2);
                let mut m1 = RECT { left: pr.left + s(28), top: y, right: pr.right - s(28), bottom: y + s(40) };
                text(hdc, "Your key is stored on this PC only (settings.ini) and is sent to nothing but the server above. Keep it private.", &mut m1, muted_c, DT_LEFT | DT_WORDBREAK);
            }
            _ => {
                let names = ["Record / stop", "Screenshot", "Save replay", "Pause / resume", "Mute microphone"];
                let keys = [cfg.hk_rec, cfg.hk_shot, cfg.hk_replay, cfg.hk_pause, cfg.hk_mic];
                let cap = HK_CAPTURE.load(Ordering::Relaxed);
                for i in 0..5usize {
                    label(hdc, i as i32, names[i], if cap == i as i32 { "Press the new key (Esc to cancel)" } else { "Works while a game is focused. Click, then press any key or combo (Esc cancels)" });
                    cycle(hdc, i as i32, &if cap == i as i32 { "Press a key…".to_string() } else { vk_name(keys[i]) });
                }
                label(hdc, 5, "Minimize to tray", "Closing the window keeps recording and replay running");
                toggle(hdc, 5, cfg.tray);
                label(hdc, 6, "Start with Windows", "Starts hidden in the tray, ready for instant replay");
                toggle(hdc, 6, cfg.autostart);
                label(hdc, 7, "Notify about updates", "Asks GitHub for the latest version at startup; nothing else is sent");
                toggle(hdc, 7, cfg.update_mode > 0);
            }
        }
    }

    {
        let t = sh.toast.lock().unwrap();
        if t.1.elapsed().as_millis() < 2800 && !t.0.is_empty() {
            SelectObject(hdc, f_norm);
            let tw = s(48) + t.0.chars().count() as i32 * s(8);
            let pill = RECT { left: pw / 2 - tw / 2, top: ph - s(66), right: pw / 2 + tw / 2, bottom: ph - s(32) };
            round_rect(hdc, &pill, s(17), rgb(31, 34, 41), Some((rgb(49, 54, 65), 1)));
            let mut tr = pill;
            tr.top += s(6);
            text(hdc, &t.0, &mut tr, white, DT_CENTER | DT_SINGLELINE);
        }
    }

    let _ = BitBlt(hdc0, 0, 0, cw, ch, hdc, 0, 0, SRCCOPY);
    // a font that is still selected cannot be deleted (that leaked one GDI object per repaint)
    SelectObject(hdc, GetStockObject(SYSTEM_FONT));
    for f in [f_small, f_norm, f_bold, f_big, f_icon] {
        let _ = DeleteObject(f);
    }
    let overlay_open = PANEL.load(Ordering::Relaxed) || LIB_OPEN.load(Ordering::Relaxed) || TRIM_OPEN.load(Ordering::Relaxed);
    let toast_on = {
        let t = sh.toast.lock().unwrap();
        t.1.elapsed().as_millis() < 2800 && !t.0.is_empty()
    };
    let keep = !overlay_open && !toast_on && frames > 0 && fps_in > 0;
    let newfast = Fast { dc: hdc, bmp, old: old_bmp, w: cw, h: ch, dx, dy, dw, dh, prad, full, rec: chip_rec, replay: chip_replay, fps: chip_fps };
    let prev_fast = FAST.lock().unwrap().take();
    if let Some(pf) = prev_fast {
        free_fast(pf);
    }
    if keep {
        *FAST.lock().unwrap() = Some(newfast);
    } else {
        free_fast(newfast);
    }
    let _ = EndPaint(hwnd, &ps);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let xy = |lp: LPARAM| ((lp.0 & 0xffff) as i16 as i32, ((lp.0 >> 16) & 0xffff) as i16 as i32);
    if msg != WM_TIMER && msg != WM_MOUSEMOVE && msg != WM_SETCURSOR && msg != WM_NCHITTEST {
        LAST_MSG.store(msg, Ordering::Relaxed);
    }
    match msg {
        WM_HOTKEY => {
            let cap = HK_CAPTURE.load(Ordering::Relaxed);
            if cap >= 0 {
                // the pressed key is one we have registered globally: treat it as the new binding
                let c = ui().sh.cfg.lock().unwrap().clone();
                let vk = match wp.0 as i32 {
                    1 => c.hk_rec,
                    2 => c.hk_shot,
                    3 => c.hk_replay,
                    4 => c.hk_pause,
                    _ => c.hk_mic,
                };
                set_hotkey(hwnd, cap, vk);
                let _ = InvalidateRect(hwnd, None, false);
                return LRESULT(0);
            }
            match wp.0 as i32 {
                1 => toggle_record(),
                2 => take_screenshot(),
                3 => save_replay(ui().sh.clone()),
                4 => toggle_pause(),
                5 => toggle_mic_mute(),
                _ => {}
            }
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN if EDIT.lock().unwrap().is_some() => {
            if wp.0 as u16 == VK_ESCAPE.0 {
                *EDIT.lock().unwrap() = None;
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_CHAR if EDIT.lock().unwrap().is_some() => {
            let ch = wp.0 as u32;
            match ch {
                13 => edit_commit(&ui().sh),
                8 => {
                    if let Some(e) = EDIT.lock().unwrap().as_mut() {
                        e.1.pop();
                    }
                }
                22 => {
                    if let Some(c) = clipboard_text() {
                        let c: String = c.chars().filter(|x| !x.is_control()).collect();
                        if let Some(e) = EDIT.lock().unwrap().as_mut() {
                            e.1.push_str(c.trim());
                            e.1.truncate(300);
                        }
                    }
                }
                32..=0xFFFF => {
                    if let Some(c) = char::from_u32(ch) {
                        if let Some(e) = EDIT.lock().unwrap().as_mut() {
                            if e.1.len() < 300 {
                                e.1.push(c);
                            }
                        }
                    }
                }
                _ => {}
            }
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN if HK_CAPTURE.load(Ordering::Relaxed) >= 0 => {
            let k = wp.0 as u32;
            let cap = HK_CAPTURE.load(Ordering::Relaxed);
            let down = |v: VIRTUAL_KEY| GetKeyState(v.0 as i32) < 0;
            if k == VK_ESCAPE.0 as u32 && !down(VK_CONTROL) && !down(VK_MENU) && !down(VK_SHIFT) {
                HK_CAPTURE.store(-1, Ordering::Relaxed);
            } else if matches!(k, 0x10 | 0x11 | 0x12 | 0xA0..=0xA5 | 0x5B | 0x5C) {
                // a modifier on its own: keep waiting for the actual key
            } else if down(VK_LWIN) || down(VK_RWIN) {
                set_toast(&ui().sh, "Windows-key combos are reserved by Windows");
            } else {
                let mods = (if down(VK_MENU) { HK_ALT } else { 0 })
                    | (if down(VK_CONTROL) { HK_CTRL } else { 0 })
                    | (if down(VK_SHIFT) { HK_SHIFT } else { 0 });
                match hotkey_check(k, mods) {
                    Ok(()) => set_hotkey(hwnd, cap, k | mods),
                    Err(m) => set_toast(&ui().sh, m),
                }
            }
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let k = wp.0 as u16;
            if k == VK_F11.0 {
                toggle_fullscreen(hwnd);
            } else if TRIM_OPEN.load(Ordering::Relaxed) && (k == VK_LEFT.0 || k == VK_RIGHT.0) {
                let step = if GetKeyState(VK_SHIFT.0 as i32) < 0 { 5 * SEC } else { SEC };
                let p = TRIM.lock().unwrap().as_ref().map(|t| t.pos).unwrap_or(0);
                trim_seek(p + if k == VK_RIGHT.0 { step } else { -step });
                let _ = InvalidateRect(hwnd, None, false);
            } else if k == VK_ESCAPE.0 {
                if TRIM_OPEN.load(Ordering::Relaxed) {
                    trim_close();
                } else if LIB_OPEN.load(Ordering::Relaxed) {
                    LIB_OPEN.store(false, Ordering::Relaxed);
                } else if PANEL.load(Ordering::Relaxed) {
                    PANEL.store(false, Ordering::Relaxed);
                } else if FULL.load(Ordering::Relaxed) {
                    toggle_fullscreen(hwnd);
                }
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
            let (x, y) = xy(lp);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let id = hit_test(&rc, x, y);
            if id != 0 {
                click(hwnd, id, x);
            } else if TRIM_OPEN.load(Ordering::Relaxed) {
                // clicks outside the trim panel do nothing
            } else if LIB_OPEN.load(Ordering::Relaxed) {
                let pr = lib_panel_rect(&rc);
                if x < pr.left || x > pr.right || y < pr.top || y > pr.bottom {
                    LIB_OPEN.store(false, Ordering::Relaxed);
                    let _ = InvalidateRect(hwnd, None, false);
                }
            } else if msg == WM_LBUTTONDBLCLK && !PANEL.load(Ordering::Relaxed) {
                toggle_fullscreen(hwnd);
            } else if PANEL.load(Ordering::Relaxed) {
                let pr = panel_rect(&rc);
                if x < pr.left || x > pr.right || y < pr.top || y > pr.bottom {
                    PANEL.store(false, Ordering::Relaxed);
                    let _ = InvalidateRect(hwnd, None, false);
                }
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            if LIB_OPEN.load(Ordering::Relaxed) && !TRIM_OPEN.load(Ordering::Relaxed) {
                let delta = (wp.0 >> 16) as i16 as i32;
                let n = LIB.lock().unwrap().len() as i32;
                let max = ((n + 2) / 3 * 3 - L_VIS).max(0);
                let off = (LIB_SCROLL.load(Ordering::Relaxed) - delta.signum() * 3).clamp(0, max);
                LIB_SCROLL.store(off, Ordering::Relaxed);
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if TL_DRAG.swap(false, Ordering::Relaxed) {
                let _ = ReleaseCapture();
            }
            if DRAG.swap(false, Ordering::Relaxed) {
                let _ = ReleaseCapture();
                sync_volume_cfg();
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let (x, y) = xy(lp);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            if DRAG.load(Ordering::Relaxed) {
                set_volume_from_x(&rc, x);
            }
            if TL_DRAG.load(Ordering::Relaxed) {
                trim_set_from_x(&rc, x);
            }
            let hv = hit_test(&rc, x, y);
            if HOVER.swap(hv, Ordering::Relaxed) != hv {
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_SETCURSOR => {
            if (lp.0 & 0xffff) as u32 == 1 && HOVER.load(Ordering::Relaxed) != 0 {
                if let Ok(c) = LoadCursorW(None, IDC_HAND) {
                    SetCursor(c);
                    return LRESULT(1);
                }
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_GETMINMAXINFO => {
            let mmi = lp.0 as *mut MINMAXINFO;
            (*mmi).ptMinTrackSize = POINT { x: s(1140), y: s(620) };
            LRESULT(0)
        }
        WM_ENTERSIZEMOVE => {
            MOVING.store(true, Ordering::Relaxed);
            LRESULT(0)
        }
        WM_EXITSIZEMOVE => {
            MOVING.store(false, Ordering::Relaxed);
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        WM_FRAME => {
            FRAME_PENDING.store(false, Ordering::Relaxed);
            // While a panel is open the 30 Hz timer does the redraws; frame-driven repaints would starve input.
            let overlay = PANEL.load(Ordering::Relaxed) || LIB_OPEN.load(Ordering::Relaxed) || TRIM_OPEN.load(Ordering::Relaxed);
            if !overlay && !HIDDEN.load(Ordering::Relaxed) {
                // WM_FRAME is a posted message and posted messages beat mouse/keyboard input. If drawing a frame is
                // slower than the frame interval the queue never empties and the window looks frozen, so after a slow
                // present we skip frames for twice as long as it took, keeping the message queue free for input.
                let t0 = now_us();
                if t0 >= NEXT_PRESENT_US.load(Ordering::Relaxed) {
                    let fast_ok = !MOVING.load(Ordering::Relaxed) && fast_present(hwnd);
                    if fast_ok {
                        let took = now_us().saturating_sub(t0);
                        PRESENTS.fetch_add(1, Ordering::Relaxed);
                        PRESENT_US.fetch_add(took, Ordering::Relaxed);
                        // Only back off when a present is really slow (input could starve); a normal 3-8 ms draw
                        // must not cost frames at 60 fps.
                        if took > 25_000 {
                            NEXT_PRESENT_US.store(now_us() + took.min(50_000), Ordering::Relaxed);
                        }
                    } else if paint_allowed() {
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
            }
            LRESULT(0)
        }
        WM_TIMER => {
            let t = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
            // measure with the real clock: WM_TIMER is low priority and can be delayed, so counting ticks over-reports fps
            let now_ms = GetTickCount64();
            let since = now_ms.saturating_sub(LAST_FPS_MS.load(Ordering::Relaxed));
            if since >= 1000 {
                LAST_FPS_MS.store(now_ms, Ordering::Relaxed);
                let sh = &ui().sh;
                let f = sh.frames.load(Ordering::Relaxed);
                let last = sh.last_frames.swap(f, Ordering::Relaxed);
                let fps = (f.wrapping_sub(last) as u64 * 1000 + since / 2) / since;
                sh.fps_meas.store(fps.min(u32::MAX as u64) as u32, Ordering::Relaxed);
                let pres = PRESENTS.swap(0, Ordering::Relaxed) as u64;
                let pus = PRESENT_US.swap(0, Ordering::Relaxed);
                let pnt = PAINTS.swap(0, Ordering::Relaxed);
                let (t_d, t_c, t_b) = (T_DRAW.swap(0, Ordering::Relaxed), T_CHIPS.swap(0, Ordering::Relaxed), T_BLT.swap(0, Ordering::Relaxed));
                let shown = (pres + pnt as u64) * 1000 / since.max(1);
                if fps >= 25 && shown * 100 < fps * 85 && !HIDDEN.load(Ordering::Relaxed) && PV_LOGGED.load(Ordering::Relaxed) < 30 {
                    PV_LOGGED.fetch_add(1, Ordering::Relaxed);
                    logln!(
                        "preview: capture {} fps, drawn {} /s (fast {} avg {} us [stretch {} us, chips {} us, blit {} us], full repaints {}), panel={} full_preview={}",
                        fps,
                        shown,
                        pres,
                        if pres > 0 { pus / pres } else { 0 },
                        if pres > 0 { t_d / pres } else { 0 },
                        if pres > 0 { t_c / pres } else { 0 },
                        if pres > 0 { t_b / pres } else { 0 },
                        pnt,
                        PANEL.load(Ordering::Relaxed),
                        sh.pv_full.load(Ordering::Relaxed)
                    );
                }
                maintenance();
            }
            // With the fast video path the controls only need ~10 repaints a second unless something is animating.
            let busy = PANEL.load(Ordering::Relaxed)
                || LIB_OPEN.load(Ordering::Relaxed)
                || TRIM_OPEN.load(Ordering::Relaxed)
                || FAST.lock().unwrap().is_none()
                || ANIM.lock().unwrap().iter().any(|&a| a != 0);
            if (busy || t % 10 == 0) && paint_allowed() && !HIDDEN.load(Ordering::Relaxed) {
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_TRAY => {
            let ev = (lp.0 & 0xFFFF) as u32;
            if ev == WM_LBUTTONUP {
                if IsWindowVisible(hwnd).as_bool() {
                    hide_to_tray(hwnd);
                } else {
                    show_from_tray(hwnd);
                }
            } else if ev == WM_RBUTTONUP {
                tray_menu(hwnd);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let tray = ui().sh.cfg.lock().unwrap().tray;
            if tray && !QUITTING.load(Ordering::Relaxed) {
                hide_to_tray(hwnd);
                LRESULT(0)
            } else {
                DefWindowProcW(hwnd, msg, wp, lp)
            }
        }
        WM_DESTROY => {
            tray_remove(hwnd);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

// ---------- main ----------

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn msgbox(t: &str) {
    let wv = wide(t);
    unsafe {
        MessageBoxW(None, PCWSTR(wv.as_ptr()), w!("Lumacap"), MB_OK | MB_ICONERROR);
    }
}

fn diag_dir() -> String {
    let dir = std::env::var("APPDATA").map(|a| format!("{a}\\lumacap")).unwrap_or_else(|_| ".".into());
    let _ = std::fs::create_dir_all(&dir);
    dir
}

unsafe extern "system" fn crash_filter(ep: *const EXCEPTION_POINTERS) -> i32 {
    let (code, addr) = if !ep.is_null() && !(*ep).ExceptionRecord.is_null() {
        ((*(*ep).ExceptionRecord).ExceptionCode.0, (*(*ep).ExceptionRecord).ExceptionAddress as usize)
    } else {
        (0, 0)
    };
    let msg = format!(
        "{} NATIVE CRASH code=0x{:08x} addr=0x{:x} last_click={} last_msg=0x{:x}\n",
        timestamp_name(),
        code as u32,
        addr,
        LAST_CLICK.load(Ordering::Relaxed),
        LAST_MSG.load(Ordering::Relaxed)
    );
    let _ = std::fs::OpenOptions::new().create(true).append(true).open(format!("{}\\crash.txt", diag_dir())).and_then(|mut f| std::io::Write::write_all(&mut f, msg.as_bytes()));
    0 // EXCEPTION_CONTINUE_SEARCH
}

/// Writes diag.txt once a second; if the UI stops ticking it records that it hung.
fn start_watchdog(sh: Arc<Shared>) {
    std::thread::spawn(move || {
        let t0 = Instant::now();
        let (mut last_ticks, mut stuck) = (0u32, 0u32);
        loop {
            let t = TICKS.load(Ordering::Relaxed);
            if t == last_ticks {
                stuck += 1;
            } else {
                stuck = 0;
                last_ticks = t;
            }
            let (gdi, user) = unsafe {
                let p = GetCurrentProcess();
                (GetGuiResources(p, GR_GDIOBJECTS), GetGuiResources(p, GR_USEROBJECTS))
            };
            let txt = format!(
                "uptime_s={}\nui_stuck_s={}\ngdi_objects={}\nuser_objects={}\nframes={}\nglitches={}\nlast_click={}\nlast_msg=0x{:x}\npanel={} lib={} trim={}\n",
                t0.elapsed().as_secs(),
                stuck,
                gdi,
                user,
                sh.frames.load(Ordering::Relaxed),
                sh.glitches.load(Ordering::Relaxed),
                LAST_CLICK.load(Ordering::Relaxed),
                LAST_MSG.load(Ordering::Relaxed),
                PANEL.load(Ordering::Relaxed),
                LIB_OPEN.load(Ordering::Relaxed),
                TRIM_OPEN.load(Ordering::Relaxed)
            );
            let _ = std::fs::write(format!("{}\\diag.txt", diag_dir()), txt);
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

fn install_crash_log() {
    unsafe {
        SetUnhandledExceptionFilter(Some(crash_filter));
    }
    std::panic::set_hook(Box::new(|info| {
        let dir = std::env::var("APPDATA").map(|a| format!("{a}\\lumacap")).unwrap_or_else(|_| ".".into());
        let _ = std::fs::create_dir_all(&dir);
        let msg = format!("{}\n{}\n", timestamp_name(), info);
        let _ = std::fs::OpenOptions::new().create(true).append(true).open(format!("{dir}\\crash.txt")).and_then(|mut f| std::io::Write::write_all(&mut f, msg.as_bytes()));
    }));
}

fn main() {
    install_crash_log();
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        let _ = SetProcessDPIAware();
    }
    if let Err(e) = run() {
        msgbox(&format!("Could not start:\n\n{e}\n\nClose Elgato software / OBS (only one app can use the card) and try again."));
        std::process::exit(1);
    }
}

fn run() -> R<()> {
    let args: Vec<String> = std::env::args().collect();
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
        SCALE.store((GetDpiForSystem() * 100 / 96).clamp(100, 300), Ordering::Relaxed);

        let vdevs = enum_devices(false)?;
        let adevs = enum_devices(true)?;

        if args.iter().any(|a| a == "--list") {
            logln!("Video devices:");
            for (i, d) in vdevs.iter().enumerate() {
                logln!("  [{i}] {}", friendly_name(d));
            }
            logln!("Audio devices:");
            for (i, d) in adevs.iter().enumerate() {
                logln!("  [{i}] {}", friendly_name(d));
            }
            return Ok(());
        }

        let saved_v = {
            let n = Settings::load().video_name;
            if n.is_empty() { None } else { vdevs.iter().position(|d| friendly_name(d) == n) }
        };
        let want_v = arg(&args, "--video").and_then(|s| s.parse().ok()).or(saved_v);
        let vi = pick(&vdevs, want_v, &["hd60", "4k60", "4k x", "4k s", "game capture", "cam link"]).or_else(|| pick(&vdevs, None, &["elgato"])).or(if vdevs.is_empty() { None } else { Some(0) });
        let Some(vi) = vi else {
            msgbox("No video capture device found. Is the HD60 S plugged into a USB 3.0 port?");
            return Ok(());
        };
        let device = friendly_name(&vdevs[vi]);
        logln!("Lumacap {} starting", env!("CARGO_PKG_VERSION"));
        logln!("video devices: {}", vdevs.iter().map(|d| friendly_name(d)).collect::<Vec<_>>().join(" | "));
        logln!("audio devices: {}", adevs.iter().map(|d| friendly_name(d)).collect::<Vec<_>>().join(" | "));
        logln!("video: {device}");

        let (vreader, fmt, yuy2) = open_video(&vdevs[vi], arg(&args, "--fps").and_then(|s| s.parse().ok()).unwrap_or(60), [1080i64, 720, 1440, 2160][Settings::load().res.min(3) as usize])?;
        logln!("mode: {}x{} @ {} fps", fmt.w, fmt.h, fmt.fps);

        let no_audio = arg(&args, "--audio").as_deref() == Some("none");
        let want_a = arg(&args, "--audio").and_then(|s| s.parse().ok());
        let ai = if no_audio { None } else { pick(&adevs, want_a, &["hd60", "4k60", "4k x", "4k s", "game capture", "cam link", "digital audio interface"]).or_else(|| pick(&adevs, None, &["elgato"])) };
        let mut audio_name = String::new();
        let mut audio_note = String::new();
        let mut audio: Option<(IMFSourceReader, AudioFmt)> = None;
        match ai {
            Some(i) => {
                audio_name = friendly_name(&adevs[i]);
                logln!("audio: {audio_name}");
                match open_audio(&adevs[i]) {
                    Ok(r) => audio = Some(r),
                    Err(e) => {
                        audio_note = format!("audio error: {}", e.message());
                        elogln!("{audio_note}");
                    }
                }
            }
            None => {
                audio_note = String::from("no audio device found");
                logln!("audio: none found (use --audio N, see --list)");
            }
        }

        let mut cfg = Settings::load();
        if let Some(b) = arg(&args, "--bitrate").and_then(|s| s.parse().ok()) {
            cfg.bitrate = b;
        }
        if let Some(o) = arg(&args, "--out") {
            cfg.out_dir = o;
        }
        if let Some(o) = arg(&args, "--shots") {
            cfg.shot_dir = o;
        }

        let can_mix = audio.as_ref().map(|a| (a.1.float || a.1.bits == 16) && a.1.ch >= 1).unwrap_or(false);
        let rec_afmt = audio.as_ref().map(|a| if can_mix { AudioFmt { float: false, rate: a.1.rate, ch: 2, bits: 16 } } else { a.1 });
        let mic_names: Vec<String> = adevs.iter().map(|d| friendly_name(d)).collect();
        let video_names: Vec<String> = vdevs.iter().map(|d| friendly_name(d)).collect();
        LIGHT.store(cfg.light, Ordering::Relaxed);
        ACCENT_IDX.store(cfg.accent.min(5), Ordering::Relaxed);
        let sh = Arc::new(Shared {
            stream: Mutex::new(None),
            rec: Mutex::new(None),
            replay: Mutex::new(None),
            preview: Mutex::new(Preview { w: 2, h: 2, data: vec![0u32; 4] }),
            pv_full: AtomicBool::new(false),
            running: AtomicBool::new(true),
            frames: AtomicU32::new(0),
            fmt,
            yuy2,
            afmt: rec_afmt,
            audio_note: Mutex::new(audio_note),
            shot_req: AtomicBool::new(false),
            toast: Mutex::new((String::new(), Instant::now())),
            fps_meas: AtomicU32::new(0),
            last_frames: AtomicU32::new(0),
            last_flags: AtomicU32::new(0),
            peak: AtomicU32::new(0),
            vol: AtomicU32::new(cfg.volume),
            muted: AtomicBool::new(cfg.muted),
            pv_slot: Mutex::new(None),
            pv_cv: Condvar::new(),
            glitches: AtomicU32::new(0),
            mic_peak: AtomicU32::new(0),
            mic_buf: Mutex::new(VecDeque::new()),
            mic_gen: AtomicU32::new(0),
            mic_idx: AtomicI32::new(-1),
            mic_fmt: Mutex::new(None),
            mic_err: Mutex::new(String::new()),
            mic_names,
            video_names,
            mic_muted: AtomicBool::new(false),
            adevs: adevs.into_iter().map(Sendable).collect(),
            game_gain: AtomicU32::new(cfg.game_gain),
            mic_gain: AtomicU32::new(cfg.mic_gain),
            cfg: Mutex::new(cfg.clone()),
        });
        if !cfg.mic_name.is_empty() {
            start_mic(&sh, &cfg.mic_name);
        }
        *UI.lock().unwrap() = Some(Sendable(Arc::new(Ui { sh: sh.clone(), device, audio_name })));

        let sh_v = sh.clone();
        let vr = Sendable(vreader);
        let vt = std::thread::spawn(move || video_thread(vr, sh_v));
        let sh_p = sh.clone();
        std::thread::spawn(move || preview_thread(sh_p));
        scene::sync(&cfg);
        if !cfg.cam_name.is_empty() {
            let dev = ui().device.clone();
            scene::set_cam(&cfg.cam_name, &dev);
        }
        if cfg.update_mode > 0 {
            let sh_u = sh.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(6));
                loop {
                    if sh_u.cfg.lock().unwrap().update_mode > 0 {
                        if let Some(rel) = update::fetch_latest() {
                            if version_tuple(&rel.tag) > version_tuple(env!("CARGO_PKG_VERSION")) {
                                set_toast(&sh_u, &format!("Update available: {} (github.com/OhHelioss/lumacap/releases)", rel.tag));
                                return;
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_secs(12 * 3600));
                }
            });
        }
        let at = audio.map(|(r, af)| {
            let sh_a = sh.clone();
            let r = Sendable(r);
            std::thread::spawn(move || audio_thread(r, sh_a, af))
        });

        let mut gp_token = 0usize;
        let gp_in = GdiplusStartupInput { GdiplusVersion: 1, ..Default::default() };
        GdiplusStartup(&mut gp_token, &gp_in, std::ptr::null_mut());

        // window
        let hinst: HINSTANCE = GetModuleHandleW(None)?.into();
        let class = w!("hd60rec_window");
        let icon = LoadIconW(hinst, PCWSTR(1usize as *const u16)).unwrap_or_default();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hIcon: icon,
            style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
            ..Default::default()
        };
        RegisterClassW(&wc);
        let (ww, wh) = if cfg.win_w > 300 && cfg.win_h > 300 { (cfg.win_w, cfg.win_h) } else { (s(1180), s(740)) };
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("Lumacap"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            ww,
            wh,
            None,
            None,
            hinst,
            None,
        )?;
        HWND_RAW.store(hwnd.0 as isize, Ordering::Relaxed);
        apply_titlebar(hwnd, cfg.light);
        if cfg.topmost {
            apply_topmost(hwnd, true);
        }
        register_hotkeys(hwnd, &cfg);
        if cfg.tray {
            tray_add(hwnd);
        }
        if args.iter().any(|a| a == "--minimized") {
            if cfg.tray {
                hide_to_tray(hwnd);
            } else {
                let _ = ShowWindow(hwnd, SW_SHOWMINNOACTIVE);
            }
        }
        SetTimer(hwnd, TIMER_ID, 33, None);
        start_watchdog(sh.clone());
        if cfg.replay_on {
            if let Err(e) = start_replay(&sh) {
                elogln!("instant replay: {e}");
                sh.cfg.lock().unwrap().replay_on = false;
            }
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // shutdown: save settings + window size, finalize anything active
        if !FULL.load(Ordering::Relaxed) {
            let mut r = RECT::default();
            if GetWindowRect(hwnd, &mut r).is_ok() {
                let mut c = sh.cfg.lock().unwrap();
                c.win_w = r.right - r.left;
                c.win_h = r.bottom - r.top;
            }
        }
        save_cfg(&sh);
        sh.running.store(false, Ordering::Relaxed);
        let rec = sh.rec.lock().unwrap().take();
        if let Some(rec) = rec {
            let _ = finalize_when_free(rec).join();
        }
        stop_replay(&sh);
        let _ = UnregisterHotKey(hwnd, 1);
        let _ = UnregisterHotKey(hwnd, 2);
        let _ = UnregisterHotKey(hwnd, 3);
        let _ = UnregisterHotKey(hwnd, 4);
        std::thread::sleep(Duration::from_millis(300)); // let temp-segment cleanup start
        drop(vt);
        drop(at);
        std::process::exit(0);
    }
}
