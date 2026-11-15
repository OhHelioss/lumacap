//! Live streaming over RTMP: encodes the capture with the GPU H.264 encoder (software fallback) and the
//! Media Foundation AAC encoder, then hands the packets to `rtmp::Publisher`.
//! Everything here runs on its own thread so the preview and recording never wait for the network.

use super::*;
use windows::core::Interface;
use crate::h264::{aac_asc, annexb_to_avcc, avcc_record, split_nals};
use crate::rtmp::Publisher;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError};

pub struct StreamCtl {
    pub tx_v: SyncSender<Sendable<IMFSample>>,
    pub tx_a: SyncSender<Vec<u8>>,
    pub stop: AtomicBool,
    pub ended: AtomicBool,
    pub live: AtomicBool,
    pub started: Instant,
    pub status: Mutex<String>,
    pub dropped: AtomicU32,
    pub frame_n: AtomicU32,
    pub half: bool,
}

struct Params {
    url: String,
    key: String,
    kbps: u32,
    w: u32,
    h: u32,
    fps: u32,
    audio_rate: Option<u32>,
}

fn set_status(ctl: &StreamCtl, s: &str) {
    if let Ok(mut g) = ctl.status.lock() {
        *g = s.to_string();
    }
}

pub fn status_text() -> Option<String> {
    let st = ui().sh.stream.lock().unwrap().clone()?;
    let s = st.status.lock().map(|g| g.clone()).unwrap_or_default();
    Some(s)
}

pub fn live_secs() -> Option<u64> {
    let st = ui().sh.stream.lock().unwrap().clone()?;
    if st.live.load(Ordering::Relaxed) {
        Some(st.started.elapsed().as_secs())
    } else {
        None
    }
}

pub fn server_url(c: &Settings) -> String {
    match c.stream_server {
        0 => "rtmp://live.twitch.tv/app".to_string(),
        1 => "rtmp://a.rtmp.youtube.com/live2".to_string(),
        _ => c.stream_custom.trim().to_string(),
    }
}

/// Called from the video thread for every frame (cheap when not streaming).
pub fn feed_video(sh: &Shared, sample: &IMFSample) {
    let ctl = match sh.stream.lock() {
        Ok(g) => g.clone(),
        Err(_) => None,
    };
    let Some(ctl) = ctl else { return };
    if !ctl.live.load(Ordering::Relaxed) {
        return;
    }
    let n = ctl.frame_n.fetch_add(1, Ordering::Relaxed);
    if ctl.half && n % 2 == 1 {
        return;
    }
    if ctl.tx_v.try_send(Sendable(sample.clone())).is_err() {
        ctl.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

/// Called from the audio thread with the mixed 16-bit stereo PCM of each chunk.
pub fn feed_audio(sh: &Shared, pcm: &[u8]) {
    let ctl = match sh.stream.lock() {
        Ok(g) => g.clone(),
        Err(_) => None,
    };
    let Some(ctl) = ctl else { return };
    if ctl.live.load(Ordering::Relaxed) {
        let _ = ctl.tx_a.try_send(pcm.to_vec());
    }
}

pub fn stop_stream() {
    let ctl = ui().sh.stream.lock().unwrap().take();
    if let Some(c) = ctl {
        c.stop.store(true, Ordering::Relaxed);
    }
}

/// Once a second: clean up after a stream that ended on its own (network error, wrong key, ...).
pub fn check_ended() {
    let sh = ui().sh.clone();
    let ended = {
        let g = sh.stream.lock().unwrap();
        g.as_ref().map(|c| c.ended.load(Ordering::Relaxed)).unwrap_or(false)
    };
    if ended {
        let ctl = sh.stream.lock().unwrap().take();
        if let Some(c) = ctl {
            let why = c.status.lock().map(|g| g.clone()).unwrap_or_default();
            set_toast(&sh, &format!("Stream ended: {why}"));
        }
    }
}

pub fn go_live() {
    let ui = ui();
    let sh = &ui.sh;
    if sh.stream.lock().unwrap().is_some() {
        return;
    }
    let c = sh.cfg.lock().unwrap().clone();
    let url = server_url(&c);
    if url.is_empty() {
        set_toast(sh, "Set the server address first (Settings > Stream)");
        return;
    }
    if c.stream_key.trim().is_empty() {
        set_toast(sh, "Paste your stream key first (Settings > Stream)");
        return;
    }
    let half = c.stream_fps30 && sh.fmt.fps >= 50;
    let (tx_v, rx_v) = sync_channel(4);
    let (tx_a, rx_a) = sync_channel(64);
    let ctl = Arc::new(StreamCtl {
        tx_v,
        tx_a,
        stop: AtomicBool::new(false),
        ended: AtomicBool::new(false),
        live: AtomicBool::new(false),
        started: Instant::now(),
        status: Mutex::new("Connecting…".to_string()),
        dropped: AtomicU32::new(0),
        frame_n: AtomicU32::new(0),
        half,
    });
    let audio_rate = sh.afmt.as_ref().filter(|a| a.ch == 2 && (a.float || a.bits == 16)).map(|a| a.rate);
    let params = Params {
        url,
        key: c.stream_key.trim().to_string(),
        kbps: c.stream_kbps.clamp(500, 20000),
        w: sh.fmt.w,
        h: sh.fmt.h,
        fps: if half { (sh.fmt.fps / 2).max(1) } else { sh.fmt.fps.max(1) },
        audio_rate,
    };
    *sh.stream.lock().unwrap() = Some(ctl.clone());
    set_toast(sh, "Going live…");
    std::thread::spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let r = run_stream(&ctl, rx_v, rx_a, &params);
        if let Err(e) = r {
            set_status(&ctl, &e);
        } else {
            set_status(&ctl, "stopped");
        }
        ctl.live.store(false, Ordering::Relaxed);
        ctl.ended.store(true, Ordering::Relaxed);
    });
}

// ---------- encoders ----------

struct VEnc {
    mft: IMFTransform,
    gen: Option<IMFMediaEventGenerator>,
    provides: bool,
    out_size: u32,
}

struct AEnc {
    mft: IMFTransform,
    provides: bool,
    out_size: u32,
}

fn err_text(e: windows::core::Error) -> String {
    let m = e.message();
    if m.is_empty() {
        format!("0x{:08X}", e.code().0 as u32)
    } else {
        m
    }
}

unsafe fn set_codec_u32(api: &ICodecAPI, key: &windows::core::GUID, v: u32) {
    let var = windows::core::VARIANT::from(v);
    let _ = api.SetValue(key, &var);
}

unsafe fn configure_video(act: &IMFActivate, w: u32, h: u32, fps: u32, kbps: u32) -> Result<VEnc, String> {
    let mft: IMFTransform = act.ActivateObject().map_err(err_text)?;
    let mut is_async = false;
    if let Ok(attrs) = mft.GetAttributes() {
        if attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0 {
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).map_err(err_text)?;
            is_async = true;
        }
    }
    let out = MFCreateMediaType().map_err(err_text)?;
    out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(err_text)?;
    out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264).map_err(err_text)?;
    out.SetUINT32(&MF_MT_AVG_BITRATE, kbps * 1000).map_err(err_text)?;
    out.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h)).map_err(err_text)?;
    out.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1)).map_err(err_text)?;
    out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1)).map_err(err_text)?;
    out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).map_err(err_text)?;
    let _ = out.SetUINT32(&MF_MT_MPEG2_PROFILE, 100); // High
    mft.SetOutputType(0, &out, 0).map_err(err_text)?;

    let inp = MFCreateMediaType().map_err(err_text)?;
    inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(err_text)?;
    inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).map_err(err_text)?;
    inp.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h)).map_err(err_text)?;
    inp.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1)).map_err(err_text)?;
    inp.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1)).map_err(err_text)?;
    inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).map_err(err_text)?;
    mft.SetInputType(0, &inp, 0).map_err(err_text)?;

    if let Ok(api) = mft.cast::<ICodecAPI>() {
        set_codec_u32(&api, &CODECAPI_AVEncCommonRateControlMode, 0); // CBR
        set_codec_u32(&api, &CODECAPI_AVEncCommonMeanBitRate, kbps * 1000);
        set_codec_u32(&api, &CODECAPI_AVEncMPVGOPSize, fps * 2); // keyframe every 2 s, what Twitch/YouTube expect
        set_codec_u32(&api, &CODECAPI_AVEncMPVDefaultBPictureCount, 0);
        set_codec_u32(&api, &CODECAPI_AVLowLatencyMode, 1);
    }
    let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
    let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
    let info = mft.GetOutputStreamInfo(0).map_err(err_text)?;
    let gen = if is_async { mft.cast::<IMFMediaEventGenerator>().ok() } else { None };
    if is_async && gen.is_none() {
        return Err("encoder gave no event interface".into());
    }
    Ok(VEnc { mft, gen, provides: info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0, out_size: info.cbSize })
}

unsafe fn make_video_encoder(w: u32, h: u32, fps: u32, kbps: u32) -> Result<(VEnc, bool), String> {
    let info = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
    let mut last = String::from("no H.264 encoder found");
    for hw in [true, false] {
        let flags = if hw { MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER } else { MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER };
        let mut arr: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut n = 0u32;
        if MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, flags, None, Some(&info), &mut arr, &mut n).is_err() || n == 0 || arr.is_null() {
            continue;
        }
        let acts: Vec<IMFActivate> = std::slice::from_raw_parts(arr, n as usize).iter().filter_map(|a| a.clone()).collect();
        CoTaskMemFree(Some(arr as *const c_void));
        for a in &acts {
            match configure_video(a, w, h, fps, kbps) {
                Ok(v) => return Ok((v, hw)),
                Err(e) => last = e,
            }
        }
    }
    Err(format!("video encoder: {last}"))
}

unsafe fn make_audio_encoder(rate: u32) -> Result<AEnc, String> {
    let mft: IMFTransform = CoCreateInstance(&AACMFTEncoder, None, CLSCTX_INPROC_SERVER).map_err(err_text)?;
    let out = MFCreateMediaType().map_err(err_text)?;
    out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).map_err(err_text)?;
    out.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC).map_err(err_text)?;
    out.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16).map_err(err_text)?;
    out.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate).map_err(err_text)?;
    out.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2).map_err(err_text)?;
    out.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 20000).map_err(err_text)?; // 160 kbit/s
    out.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0).map_err(err_text)?; // raw AAC, no ADTS
    out.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29).map_err(err_text)?;
    mft.SetOutputType(0, &out, 0).map_err(err_text)?;
    let inp = MFCreateMediaType().map_err(err_text)?;
    inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).map_err(err_text)?;
    inp.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM).map_err(err_text)?;
    inp.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16).map_err(err_text)?;
    inp.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate).map_err(err_text)?;
    inp.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2).map_err(err_text)?;
    inp.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4).map_err(err_text)?;
    inp.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, rate * 4).map_err(err_text)?;
    mft.SetInputType(0, &inp, 0).map_err(err_text)?;
    let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
    let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
    let info = mft.GetOutputStreamInfo(0).map_err(err_text)?;
    Ok(AEnc { mft, provides: info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0, out_size: info.cbSize })
}

/// One ProcessOutput call. Ok(None) means the encoder needs more input first.
unsafe fn pull(mft: &IMFTransform, provides: bool, size: u32) -> Result<Option<IMFSample>, String> {
    let mut data = MFT_OUTPUT_DATA_BUFFER::default();
    if !provides {
        let s = MFCreateSample().map_err(err_text)?;
        let b = MFCreateMemoryBuffer(size.max(1 << 16)).map_err(err_text)?;
        s.AddBuffer(&b).map_err(err_text)?;
        data.pSample = std::mem::ManuallyDrop::new(Some(s));
    }
    let mut status = 0u32;
    let mut bufs = [data];
    let r = mft.ProcessOutput(0, &mut bufs, &mut status);
    let sample = std::mem::ManuallyDrop::take(&mut bufs[0].pSample);
    let _events = std::mem::ManuallyDrop::take(&mut bufs[0].pEvents);
    match r {
        Ok(()) => Ok(sample),
        Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(None),
        Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
            if let Ok(t) = mft.GetOutputAvailableType(0, 0) {
                let _ = mft.SetOutputType(0, &t, 0);
            }
            Ok(None)
        }
        Err(e) => Err(err_text(e)),
    }
}

unsafe fn sample_bytes(s: &IMFSample) -> Option<Vec<u8>> {
    let b = s.ConvertToContiguousBuffer().ok()?;
    let mut p: *mut u8 = std::ptr::null_mut();
    let mut len = 0u32;
    b.Lock(&mut p, None, Some(&mut len)).ok()?;
    let v = if p.is_null() { Vec::new() } else { std::slice::from_raw_parts(p, len as usize).to_vec() };
    let _ = b.Unlock();
    Some(v)
}

struct VState {
    header_sent: bool,
    last_ts: u32,
    sent: u64,
}

unsafe fn handle_video_out(s: &IMFSample, pubr: &Publisher, st: &mut VState) -> Result<(), String> {
    let t = s.GetSampleTime().unwrap_or(0).max(0);
    let ts = ((t / 10_000) as u32).max(st.last_ts);
    st.last_ts = ts;
    let Some(bytes) = sample_bytes(s) else { return Ok(()) };
    let c = annexb_to_avcc(&bytes);
    if !st.header_sent {
        if let (Some(sps), Some(pps)) = (&c.sps, &c.pps) {
            if let Some(rec) = avcc_record(sps, pps) {
                pubr.send_video_header(&rec)?;
                st.header_sent = true;
            }
        }
    }
    if st.header_sent && !c.avcc.is_empty() {
        pubr.send_video(ts, c.keyframe, &c.avcc)?;
        st.sent += 1;
    }
    Ok(())
}

fn run_stream(ctl: &Arc<StreamCtl>, rx_v: Receiver<Sendable<IMFSample>>, rx_a: Receiver<Vec<u8>>, p: &Params) -> Result<(), String> {
    set_status(ctl, "Connecting…");
    let pubr = Publisher::connect(&p.url, &p.key)?;
    let result = unsafe { stream_loop(ctl, &pubr, rx_v, rx_a, p) };
    pubr.close();
    result
}

unsafe fn stream_loop(ctl: &Arc<StreamCtl>, pubr: &Publisher, rx_v: Receiver<Sendable<IMFSample>>, rx_a: Receiver<Vec<u8>>, p: &Params) -> Result<(), String> {
    set_status(ctl, "Starting encoder…");
    let (venc, hw) = make_video_encoder(p.w, p.h, p.fps, p.kbps)?;
    let aenc = match p.audio_rate {
        Some(r) => Some((make_audio_encoder(r)?, r)),
        None => None,
    };
    pubr.send_metadata(p.w, p.h, p.fps as f64, p.kbps as f64, 160.0, p.audio_rate.unwrap_or(0) as f64)?;
    if let Some((_, rate)) = &aenc {
        let asc = aac_asc(*rate, 2).ok_or("unsupported audio sample rate for streaming")?;
        pubr.send_audio_header(&asc)?;
    }

    // SPS/PPS from the encoder's output type when it provides them
    let mut vst = VState { header_sent: false, last_ts: 0, sent: 0 };
    if let Ok(t) = venc.mft.GetOutputCurrentType(0) {
        let mut buf = [0u8; 512];
        let mut n = 0u32;
        if t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buf, Some(&mut n)).is_ok() {
            let nals = split_nals(&buf[..(n as usize).min(buf.len())]);
            let sps = nals.iter().find(|x| x[0] & 0x1f == 7);
            let pps = nals.iter().find(|x| x[0] & 0x1f == 8);
            if let (Some(sps), Some(pps)) = (sps, pps) {
                if let Some(rec) = avcc_record(sps, pps) {
                    pubr.send_video_header(&rec)?;
                    vst.header_sent = true;
                }
            }
        }
    }

    ctl.live.store(true, Ordering::Relaxed);
    set_status(ctl, if hw { "Live (GPU encoder)" } else { "Live (software encoder)" });
    let start = ctl.started;
    let frame_dur = 10_000_000i64 / p.fps.max(1) as i64;
    let mut need_input = 0u32;
    let mut a_base_ms: Option<u32> = None;
    let mut a_frames: u64 = 0; // PCM frames fed
    let mut aac_n: u64 = 0;
    let mut a_us_fed: i64 = 0;

    while !ctl.stop.load(Ordering::Relaxed) {
        if let Some(e) = pubr.is_dead() {
            return Err(e);
        }
        let mut busy = false;

        // ---- audio ----
        if let Some((enc, rate)) = &aenc {
            loop {
                match rx_a.try_recv() {
                    Ok(pcm) => {
                        busy = true;
                        let base = *a_base_ms.get_or_insert(start.elapsed().as_millis() as u32);
                        let _ = base;
                        let frames = (pcm.len() / 4) as u64;
                        let t = a_us_fed * 10;
                        let dur = frames as i64 * 10_000_000 / *rate as i64;
                        if let Some(s) = pcm_sample(&pcm, t, dur) {
                            if enc.mft.ProcessInput(0, &s, 0).is_ok() {
                                a_frames += frames;
                                a_us_fed += dur / 10;
                                loop {
                                    match pull(&enc.mft, enc.provides, enc.out_size)? {
                                        Some(o) => {
                                            if let Some(b) = sample_bytes(&o) {
                                                if !b.is_empty() {
                                                    let ms = a_base_ms.unwrap_or(0) as u64 + aac_n * 1024 * 1000 / *rate as u64;
                                                    pubr.send_audio(ms as u32, &b)?;
                                                    aac_n += 1;
                                                }
                                            }
                                        }
                                        None => break,
                                    }
                                }
                            }
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break,
                }
            }
        }
        let _ = a_frames;

        // ---- video ----
        if let Some(gen) = &venc.gen {
            // asynchronous (hardware) encoder: it tells us when it wants input and when output is ready
            loop {
                match gen.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                    Ok(ev) => {
                        busy = true;
                        let ty = ev.GetType().unwrap_or(0);
                        if ty == METransformNeedInput.0 as u32 {
                            need_input += 1;
                        } else if ty == METransformHaveOutput.0 as u32 {
                            if let Some(o) = pull(&venc.mft, venc.provides, venc.out_size)? {
                                handle_video_out(&o, pubr, &mut vst)?;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            while need_input > 0 {
                match rx_v.try_recv() {
                    Ok(f) => {
                        busy = true;
                        if let Some(c) = dup_sample(&f.0) {
                            let _ = c.SetSampleTime(start.elapsed().as_nanos() as i64 / 100);
                            let _ = c.SetSampleDuration(frame_dur);
                            if venc.mft.ProcessInput(0, &c, 0).is_ok() {
                                need_input -= 1;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        } else {
            // synchronous (software) encoder
            loop {
                match rx_v.try_recv() {
                    Ok(f) => {
                        busy = true;
                        if let Some(c) = dup_sample(&f.0) {
                            let _ = c.SetSampleTime(start.elapsed().as_nanos() as i64 / 100);
                            let _ = c.SetSampleDuration(frame_dur);
                            if venc.mft.ProcessInput(0, &c, 0).is_ok() {
                                while let Some(o) = pull(&venc.mft, venc.provides, venc.out_size)? {
                                    handle_video_out(&o, pubr, &mut vst)?;
                                }
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        }
        if !busy {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    let _ = venc.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
    Ok(())
}
