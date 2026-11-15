//! Scene tweaks applied to every frame before it reaches the preview, recordings, screenshots and the stream:
//! digital zoom with pan, and a webcam picture-in-picture.

use super::*;

pub const ZOOMS: [usize; 5] = [100, 125, 150, 200, 300];
pub const SIZES: [usize; 3] = [15, 22, 30];
pub const CAM_POS: [&str; 4] = ["Top left", "Top right", "Bottom left", "Bottom right"];
pub const SIZE_NAMES: [&str; 3] = ["Small", "Medium", "Large"];

static ZOOM: AtomicU32 = AtomicU32::new(0);
static PAN_X: AtomicU32 = AtomicU32::new(50);
static PAN_Y: AtomicU32 = AtomicU32::new(50);
static POS: AtomicU32 = AtomicU32::new(3);
static SIZE: AtomicU32 = AtomicU32::new(1);
static CAM_ON: AtomicBool = AtomicBool::new(false);
static CAM_GEN: AtomicU32 = AtomicU32::new(0);

struct CamFrame {
    w: usize,
    h: usize,
    data: Vec<u8>, // packed NV12
    at: Instant,
}
static CAM: Mutex<Option<CamFrame>> = Mutex::new(None);

/// Copy the settings into the atomics the video thread reads.
pub fn sync(c: &Settings) {
    ZOOM.store(c.zoom.min(4), Ordering::Relaxed);
    PAN_X.store(c.pan_x.min(100), Ordering::Relaxed);
    PAN_Y.store(c.pan_y.min(100), Ordering::Relaxed);
    POS.store(c.cam_pos.min(3), Ordering::Relaxed);
    SIZE.store(c.cam_size.min(2), Ordering::Relaxed);
}

/// Start (or stop, for an empty name) the webcam capture thread.
pub fn set_cam(name: &str, main_device: &str) {
    let gen = CAM_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    CAM_ON.store(false, Ordering::Relaxed);
    *CAM.lock().unwrap() = None;
    if name.is_empty() || name == main_device {
        return;
    }
    let name = name.to_string();
    std::thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let Ok(devs) = enum_devices(false) else { return };
        let Some(act) = devs.iter().find(|d| friendly_name(d) == name) else { return };
        let Ok((reader, w, h, stride)) = open_cam(act) else {
            set_toast(&ui().sh, "Could not open the webcam (is another app using it?)");
            return;
        };
        CAM_ON.store(true, Ordering::Relaxed);
        let vs = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        while CAM_GEN.load(Ordering::SeqCst) == gen {
            let mut flags = 0u32;
            let mut sample: Option<IMFSample> = None;
            if reader.ReadSample(vs, 0, None, Some(&mut flags), None, Some(&mut sample)).is_err() {
                break;
            }
            if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 | MF_SOURCE_READERF_ERROR.0 as u32) != 0 {
                break;
            }
            let Some(sample) = sample else { continue };
            let Ok(buf) = sample.ConvertToContiguousBuffer() else { continue };
            let mut p: *mut u8 = std::ptr::null_mut();
            let mut len = 0u32;
            if buf.Lock(&mut p, None, Some(&mut len)).is_err() {
                continue;
            }
            let src = std::slice::from_raw_parts(p, len as usize);
            if src.len() >= stride * h * 3 / 2 {
                let mut data = vec![0u8; w * h * 3 / 2];
                for y in 0..h {
                    data[y * w..(y + 1) * w].copy_from_slice(&src[y * stride..y * stride + w]);
                }
                for y in 0..h / 2 {
                    let so = stride * h + y * stride;
                    let d = w * h + y * w;
                    data[d..d + w].copy_from_slice(&src[so..so + w]);
                }
                *CAM.lock().unwrap() = Some(CamFrame { w, h, data, at: Instant::now() });
            }
            let _ = buf.Unlock();
        }
        if CAM_GEN.load(Ordering::SeqCst) == gen {
            CAM_ON.store(false, Ordering::Relaxed);
            *CAM.lock().unwrap() = None;
        }
    });
}

/// Pick a webcam mode around 640x480..1280x720 and ask Windows for NV12 (it decodes MJPEG for us).
unsafe fn open_cam(act: &IMFActivate) -> R<(IMFSourceReader, usize, usize, usize)> {
    let source: IMFMediaSource = act.ActivateObject()?;
    let mut attrs: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut attrs, 2)?;
    let attrs = attrs.unwrap();
    attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?;
    let reader = MFCreateSourceReaderFromMediaSource(&source, &attrs)?;
    let vs = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let mut cands: Vec<(i64, IMFMediaType, u32, u32)> = Vec::new();
    let mut i = 0;
    while let Ok(mt) = reader.GetNativeMediaType(vs, i) {
        i += 1;
        let size = mt.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
        let (w, h) = ((size >> 32) as u32, size as u32);
        if w == 0 || h == 0 || w % 2 != 0 || h % 2 != 0 || w > 1920 {
            continue;
        }
        let rate = mt.GetUINT64(&MF_MT_FRAME_RATE).unwrap_or(0);
        let fps = (rate >> 32) as i64 / ((rate as u32).max(1) as i64);
        let score = -(h as i64 - 540).abs() - (30 - fps).abs() * 20;
        cands.push((score, mt, w, h));
    }
    cands.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, native, w, h) in cands {
        if reader.SetCurrentMediaType(vs, None, &native).is_err() {
            continue;
        }
        let out = MFCreateMediaType()?;
        native.CopyAllItems(&out)?;
        out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        if reader.SetCurrentMediaType(vs, None, &out).is_ok() {
            let stride = reader
                .GetCurrentMediaType(vs)
                .ok()
                .and_then(|t| t.GetUINT32(&MF_MT_DEFAULT_STRIDE).ok())
                .map(|s| (s as i32).unsigned_abs() as usize)
                .filter(|s| *s >= w as usize)
                .unwrap_or(w as usize);
            return Ok((reader, w as usize, h as usize, stride));
        }
    }
    Err(windows::core::Error::from(windows::Win32::Foundation::E_FAIL))
}

pub fn active() -> bool {
    ZOOM.load(Ordering::Relaxed) > 0 || CAM_ON.load(Ordering::Relaxed)
}

/// Returns a new NV12 sample with zoom and overlay applied, or None to use the original frame.
pub unsafe fn process(w: usize, h: usize, sample: &IMFSample) -> Option<IMFSample> {
    if !active() {
        return None;
    }
    let z = ZOOMS[(ZOOM.load(Ordering::Relaxed) as usize).min(4)];
    let cam = CAM.lock().unwrap();
    let cam = cam.as_ref().filter(|c| c.at.elapsed() < Duration::from_secs(1) && c.data.len() >= c.w * c.h * 3 / 2);
    if z == 100 && cam.is_none() {
        return None;
    }
    let buf = sample.ConvertToContiguousBuffer().ok()?;
    let mut p: *mut u8 = std::ptr::null_mut();
    let mut len = 0u32;
    buf.Lock(&mut p, None, Some(&mut len)).ok()?;
    let n = w * h * 3 / 2;
    let mut result = None;
    if len as usize >= n {
        let src = std::slice::from_raw_parts(p, n);
        if let Ok(ob) = MFCreateMemoryBuffer(n as u32) {
            let mut op: *mut u8 = std::ptr::null_mut();
            if ob.Lock(&mut op, None, None).is_ok() {
                let dst = std::slice::from_raw_parts_mut(op, n);
                zoom_into(src, dst, w, h, z);
                if let Some(c) = cam {
                    overlay(dst, w, h, c);
                }
                let _ = ob.Unlock();
                let _ = ob.SetCurrentLength(n as u32);
                if let Ok(os) = MFCreateSample() {
                    if os.AddBuffer(&ob).is_ok() {
                        let _ = os.SetSampleTime(sample.GetSampleTime().unwrap_or(0));
                        let _ = os.SetSampleDuration(sample.GetSampleDuration().unwrap_or(0));
                        result = Some(os);
                    }
                }
            }
        }
    }
    let _ = buf.Unlock();
    result
}

fn zoom_into(src: &[u8], dst: &mut [u8], w: usize, h: usize, z: usize) {
    if z <= 100 {
        dst.copy_from_slice(&src[..dst.len()]);
        return;
    }
    let cw = (w * 100 / z) & !1;
    let ch = (h * 100 / z) & !1;
    let x0 = ((w - cw) * PAN_X.load(Ordering::Relaxed) as usize / 100) & !1;
    let y0 = ((h - ch) * PAN_Y.load(Ordering::Relaxed) as usize / 100) & !1;
    let xm: Vec<usize> = (0..w).map(|x| x0 + x * cw / w).collect();
    for y in 0..h {
        let row = &src[(y0 + y * ch / h) * w..][..w];
        let d = &mut dst[y * w..(y + 1) * w];
        for x in 0..w {
            d[x] = row[xm[x]];
        }
    }
    let (suv, duv) = (&src[w * h..], &mut dst[w * h..]);
    for y in 0..h / 2 {
        let sy = ((y0 + (y * 2) * ch / h) / 2).min(h / 2 - 1);
        let row = &suv[sy * w..][..w];
        let d = &mut duv[y * w..(y + 1) * w];
        for x in (0..w).step_by(2) {
            let sx = xm[x] & !1;
            d[x] = row[sx];
            d[x + 1] = row[sx + 1];
        }
    }
}

fn overlay(dst: &mut [u8], w: usize, h: usize, c: &CamFrame) {
    let ow = (w * SIZES[(SIZE.load(Ordering::Relaxed) as usize).min(2)] / 100) & !1;
    let oh = (ow * c.h / c.w) & !1;
    let m = (w / 40) & !1;
    let b = 4usize;
    if ow < 16 || oh < 16 || ow + 2 * m > w || oh + 2 * m > h || m < b {
        return;
    }
    let (ox, oy) = match POS.load(Ordering::Relaxed) {
        0 => (m, m),
        1 => (w - ow - m, m),
        2 => (m, h - oh - m),
        _ => (w - ow - m, h - oh - m),
    };
    // white frame around the picture
    for y in oy - b..oy + oh + b {
        for x in ox - b..ox + ow + b {
            dst[y * w + x] = 235;
        }
    }
    for y in (oy - b) / 2..(oy + oh + b) / 2 {
        for x in (ox - b..ox + ow + b).step_by(2) {
            dst[w * h + y * w + x] = 128;
            dst[w * h + y * w + x + 1] = 128;
        }
    }
    let (cy, cuv) = c.data.split_at(c.w * c.h);
    for y in 0..oh {
        let sy = y * c.h / oh;
        for x in 0..ow {
            dst[(oy + y) * w + ox + x] = cy[sy * c.w + x * c.w / ow];
        }
    }
    for y in 0..oh / 2 {
        let sy = (y * 2 * c.h / oh) / 2;
        for x in (0..ow).step_by(2) {
            let sx = (x * c.w / ow) & !1;
            let d = w * h + (oy / 2 + y) * w + ox + x;
            dst[d] = cuv[sy * c.w + sx];
            dst[d + 1] = cuv[sy * c.w + sx + 1];
        }
    }
}
