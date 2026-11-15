//! Minimal RTMP publisher (plain `rtmp://`, no TLS): handshake, connect, publish, then FLV-style audio/video messages.
//! Written to never panic: every failure is returned as an error string.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

type Res<T> = Result<T, String>;

const CHUNK_OUT: usize = 4096;
const CSID_CTRL: u32 = 2;
const CSID_CMD: u32 = 3;
const CSID_AUDIO: u32 = 4;
const CSID_VIDEO: u32 = 6;

fn io<T>(r: std::io::Result<T>) -> Res<T> {
    r.map_err(|e| e.to_string())
}

// ---------- AMF0 ----------

fn amf_str(out: &mut Vec<u8>, s: &str) {
    out.push(0x02);
    out.extend_from_slice(&(s.len() as u16).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
}
fn amf_num(out: &mut Vec<u8>, n: f64) {
    out.push(0x00);
    out.extend_from_slice(&n.to_be_bytes());
}
fn amf_key(out: &mut Vec<u8>, k: &str) {
    out.extend_from_slice(&(k.len() as u16).to_be_bytes());
    out.extend_from_slice(k.as_bytes());
}
fn amf_obj_end(out: &mut Vec<u8>) {
    out.extend_from_slice(&[0, 0, 9]);
}

/// Very small AMF0 reader: returns the first string in the payload (command name) and the first number after it.
fn amf_first_string(p: &[u8]) -> Option<String> {
    if p.len() < 3 || p[0] != 0x02 {
        return None;
    }
    let n = u16::from_be_bytes([p[1], p[2]]) as usize;
    let b = p.get(3..3 + n)?;
    Some(String::from_utf8_lossy(b).to_string())
}

/// Collects every string value found in the payload (cheap way to look for `NetStream.Publish.Start` and friends).
fn amf_all_strings(p: &[u8]) -> Vec<String> {
    let mut v = Vec::new();
    let mut i = 0;
    while i + 3 <= p.len() {
        if p[i] == 0x02 {
            let n = u16::from_be_bytes([p[i + 1], p[i + 2]]) as usize;
            if let Some(b) = p.get(i + 3..i + 3 + n) {
                v.push(String::from_utf8_lossy(b).to_string());
                i += 3 + n;
                continue;
            }
        }
        i += 1;
    }
    v
}

/// Returns the number following the command name and transaction id: the stream id in a `_result` to createStream.
fn amf_numbers(p: &[u8]) -> Vec<f64> {
    let mut v = Vec::new();
    let mut i = 0;
    // skip over the command string
    if p.first() == Some(&0x02) && p.len() >= 3 {
        i = 3 + u16::from_be_bytes([p[1], p[2]]) as usize;
    }
    while i + 9 <= p.len() {
        if p[i] == 0x00 {
            let mut b = [0u8; 8];
            b.copy_from_slice(&p[i + 1..i + 9]);
            v.push(f64::from_be_bytes(b));
            i += 9;
        } else if p[i] == 0x05 {
            i += 1; // null
        } else {
            break;
        }
    }
    v
}

// ---------- chunk reader ----------

struct Msg {
    kind: u8,
    payload: Vec<u8>,
}

struct ChunkState {
    ts: u32,
    len: usize,
    kind: u8,
    sid: u32,
    buf: Vec<u8>,
}

struct Reader {
    s: TcpStream,
    chunk: usize,
    states: Vec<(u32, ChunkState)>,
    bytes_in: Arc<AtomicU64>,
}

impl Reader {
    fn read_exact(&mut self, n: usize) -> Res<Vec<u8>> {
        let mut v = vec![0u8; n];
        io(self.s.read_exact(&mut v))?;
        self.bytes_in.fetch_add(n as u64, Ordering::Relaxed);
        Ok(v)
    }

    fn st(&mut self, csid: u32) -> usize {
        if let Some(i) = self.states.iter().position(|(c, _)| *c == csid) {
            return i;
        }
        self.states.push((csid, ChunkState { ts: 0, len: 0, kind: 0, sid: 0, buf: Vec::new() }));
        self.states.len() - 1
    }

    /// Read chunks until one whole message is available.
    fn next_msg(&mut self) -> Res<Msg> {
        loop {
            let b0 = self.read_exact(1)?[0];
            let fmt = b0 >> 6;
            let mut csid = (b0 & 0x3f) as u32;
            if csid == 0 {
                csid = 64 + self.read_exact(1)?[0] as u32;
            } else if csid == 1 {
                let b = self.read_exact(2)?;
                csid = 64 + b[0] as u32 + (b[1] as u32) * 256;
            }
            let idx = self.st(csid);
            let mut ts_field = None;
            if fmt <= 2 {
                let h = self.read_exact(3)?;
                ts_field = Some(((h[0] as u32) << 16) | ((h[1] as u32) << 8) | h[2] as u32);
            }
            if fmt <= 1 {
                let h = self.read_exact(4)?;
                let len = ((h[0] as usize) << 16) | ((h[1] as usize) << 8) | h[2] as usize;
                self.states[idx].1.len = len;
                self.states[idx].1.kind = h[3];
            }
            if fmt == 0 {
                let h = self.read_exact(4)?;
                self.states[idx].1.sid = u32::from_le_bytes([h[0], h[1], h[2], h[3]]);
            }
            if let Some(t) = ts_field {
                let t = if t == 0xFFFFFF {
                    let e = self.read_exact(4)?;
                    u32::from_be_bytes([e[0], e[1], e[2], e[3]])
                } else {
                    t
                };
                self.states[idx].1.ts = if fmt == 0 { t } else { self.states[idx].1.ts.wrapping_add(t) };
            }
            let (len, have) = (self.states[idx].1.len, self.states[idx].1.buf.len());
            if len > 4 * 1024 * 1024 {
                return Err("server sent an oversized message".into());
            }
            let want = len.saturating_sub(have).min(self.chunk.max(1));
            let data = self.read_exact(want)?;
            self.states[idx].1.buf.extend_from_slice(&data);
            if self.states[idx].1.buf.len() >= len {
                let payload = std::mem::take(&mut self.states[idx].1.buf);
                return Ok(Msg { kind: self.states[idx].1.kind, payload });
            }
        }
    }
}

// ---------- publisher ----------

struct Writer {
    s: TcpStream,
    sent: u64,
}

impl Writer {
    fn send_msg(&mut self, csid: u32, ts: u32, kind: u8, sid: u32, payload: &[u8]) -> Res<()> {
        let mut out = Vec::with_capacity(payload.len() + payload.len() / CHUNK_OUT * 5 + 24);
        let ext = ts >= 0xFFFFFF;
        let tsf = if ext { 0xFFFFFF } else { ts };
        let basic = |fmt: u8| -> Vec<u8> {
            if csid < 64 {
                vec![(fmt << 6) | csid as u8]
            } else {
                let c = csid - 64;
                vec![(fmt << 6) | 1, (c & 0xff) as u8, (c >> 8) as u8]
            }
        };
        out.extend_from_slice(&basic(0));
        out.extend_from_slice(&tsf.to_be_bytes()[1..]);
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes()[1..]);
        out.push(kind);
        out.extend_from_slice(&sid.to_le_bytes());
        if ext {
            out.extend_from_slice(&ts.to_be_bytes());
        }
        let mut off = 0;
        loop {
            let n = (payload.len() - off).min(CHUNK_OUT);
            out.extend_from_slice(&payload[off..off + n]);
            off += n;
            if off >= payload.len() {
                break;
            }
            out.extend_from_slice(&basic(3));
            if ext {
                out.extend_from_slice(&ts.to_be_bytes());
            }
        }
        io(self.s.write_all(&out))?;
        self.sent += out.len() as u64;
        Ok(())
    }
}

pub struct Publisher {
    w: Arc<Mutex<Writer>>,
    stream_id: u32,
    dead: Arc<AtomicBool>,
    err: Arc<Mutex<String>>,
    bytes_out: Arc<AtomicU64>,
}

fn lock(w: &Arc<Mutex<Writer>>) -> Res<std::sync::MutexGuard<'_, Writer>> {
    w.lock().map_err(|_| "internal error".to_string())
}

fn parse_url(url: &str) -> Res<(String, u16, String)> {
    let rest = url.strip_prefix("rtmp://").ok_or_else(|| {
        if url.starts_with("rtmps://") {
            "RTMPS (encrypted) servers are not supported yet. Use an rtmp:// server".to_string()
        } else {
            "the server address must start with rtmp://".to_string()
        }
    })?;
    let (hostport, path) = rest.split_once('/').ok_or("the server address needs an application, e.g. rtmp://host/app")?;
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().map_err(|_| "bad port in server address".to_string())?),
        None => (hostport.to_string(), 1935),
    };
    if host.is_empty() || path.is_empty() {
        return Err("incomplete server address".into());
    }
    Ok((host, port, path.trim_end_matches('/').to_string()))
}

impl Publisher {
    /// Connect and start publishing `key` to `url` (e.g. `rtmp://live.twitch.tv/app`).
    pub fn connect(url: &str, key: &str) -> Res<Publisher> {
        let (host, port, app) = parse_url(url)?;
        let addr = io((host.as_str(), port).to_socket_addrs())?.next().ok_or("could not resolve the server")?;
        let mut s = io(TcpStream::connect_timeout(&addr, Duration::from_secs(8)))?;
        let _ = s.set_nodelay(true);
        let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
        let _ = s.set_write_timeout(Some(Duration::from_secs(10)));

        // handshake
        let mut c1 = vec![0u8; 1537];
        c1[0] = 3;
        // c1[1..9] = time + zero; the rest is random-ish filler
        let mut seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1) | 1;
        for b in c1[9..].iter_mut() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            *b = (seed >> 24) as u8;
        }
        io(s.write_all(&c1))?;
        let mut s0s1 = vec![0u8; 1 + 1536];
        io(s.read_exact(&mut s0s1))?;
        if s0s1[0] != 3 {
            return Err("server is not speaking RTMP".into());
        }
        io(s.write_all(&s0s1[1..]))?; // C2 echoes S1
        let mut s2 = vec![0u8; 1536];
        io(s.read_exact(&mut s2))?;

        let bytes_in = Arc::new(AtomicU64::new(0));
        let rs = io(s.try_clone())?;
        let mut reader = Reader { s: rs, chunk: 128, states: Vec::new(), bytes_in: bytes_in.clone() };
        let w = Arc::new(Mutex::new(Writer { s, sent: 0 }));

        // our chunk size
        lock(&w)?.send_msg(CSID_CTRL, 0, 1, 0, &(CHUNK_OUT as u32).to_be_bytes())?;

        // connect
        let tc = format!("rtmp://{}:{}/{}", host, port, app);
        let mut p = Vec::new();
        amf_str(&mut p, "connect");
        amf_num(&mut p, 1.0);
        p.push(0x03);
        amf_key(&mut p, "app");
        amf_str(&mut p, &app);
        amf_key(&mut p, "type");
        amf_str(&mut p, "nonprivate");
        amf_key(&mut p, "flashVer");
        amf_str(&mut p, "FMLE/3.0 (compatible; Lumacap)");
        amf_key(&mut p, "tcUrl");
        amf_str(&mut p, &tc);
        amf_obj_end(&mut p);
        lock(&w)?.send_msg(CSID_CMD, 0, 20, 0, &p)?;
        wait_for(&mut reader, &w, |m| m.kind == 20 && amf_first_string(&m.payload).as_deref() == Some("_result"), "connect")?;

        // releaseStream / FCPublish / createStream
        let mut p = Vec::new();
        amf_str(&mut p, "releaseStream");
        amf_num(&mut p, 2.0);
        p.push(0x05);
        amf_str(&mut p, key);
        lock(&w)?.send_msg(CSID_CMD, 0, 20, 0, &p)?;
        let mut p = Vec::new();
        amf_str(&mut p, "FCPublish");
        amf_num(&mut p, 3.0);
        p.push(0x05);
        amf_str(&mut p, key);
        lock(&w)?.send_msg(CSID_CMD, 0, 20, 0, &p)?;
        let mut p = Vec::new();
        amf_str(&mut p, "createStream");
        amf_num(&mut p, 4.0);
        p.push(0x05);
        lock(&w)?.send_msg(CSID_CMD, 0, 20, 0, &p)?;
        let reply = wait_for(&mut reader, &w, |m| m.kind == 20 && amf_first_string(&m.payload).as_deref() == Some("_result") && amf_numbers(&m.payload).first() == Some(&4.0), "createStream")?;
        let stream_id = amf_numbers(&reply.payload).get(1).copied().unwrap_or(1.0) as u32;

        // publish
        let mut p = Vec::new();
        amf_str(&mut p, "publish");
        amf_num(&mut p, 5.0);
        p.push(0x05);
        amf_str(&mut p, key);
        amf_str(&mut p, "live");
        lock(&w)?.send_msg(CSID_CMD, 0, 20, stream_id, &p)?;
        let st = wait_for(&mut reader, &w, |m| m.kind == 20 && amf_first_string(&m.payload).as_deref() == Some("onStatus"), "publish")?;
        let codes = amf_all_strings(&st.payload);
        if !codes.iter().any(|c| c == "NetStream.Publish.Start") {
            let why = codes.iter().find(|c| c.starts_with("NetStream.")).cloned().unwrap_or_else(|| "the server refused the stream".into());
            return Err(format!("publish failed: {why} (check the stream key)"));
        }

        // from now on a background thread drains what the server sends and answers acknowledgements
        let dead = Arc::new(AtomicBool::new(false));
        let err = Arc::new(Mutex::new(String::new()));
        {
            let (w2, dead2, err2) = (w.clone(), dead.clone(), err.clone());
            let _ = reader.s.set_read_timeout(None);
            std::thread::spawn(move || {
                let mut window: u64 = 2_500_000;
                let mut acked: u64 = 0;
                loop {
                    match reader.next_msg() {
                        Ok(m) => {
                            if m.kind == 1 && m.payload.len() >= 4 {
                                reader.chunk = (u32::from_be_bytes([m.payload[0], m.payload[1], m.payload[2], m.payload[3]]) as usize).clamp(1, 1 << 20);
                            }
                            if m.kind == 5 && m.payload.len() >= 4 {
                                window = u32::from_be_bytes([m.payload[0], m.payload[1], m.payload[2], m.payload[3]]) as u64;
                            }
                            let got = reader.bytes_in.load(Ordering::Relaxed);
                            if got - acked >= window / 2 {
                                acked = got;
                                if let Ok(mut g) = w2.lock() {
                                    let _ = g.send_msg(CSID_CTRL, 0, 3, 0, &(got as u32).to_be_bytes());
                                }
                            }
                            if m.kind == 20 {
                                let strs = amf_all_strings(&m.payload);
                                if let Some(bad) = strs.iter().find(|c| c.contains("Failed") || c.contains("Closed") || c.contains("Unpublish")) {
                                    if let Ok(mut e) = err2.lock() {
                                        *e = bad.clone();
                                    }
                                    dead2.store(true, Ordering::Relaxed);
                                }
                            }
                        }
                        Err(e) => {
                            if let Ok(mut g) = err2.lock() {
                                if g.is_empty() {
                                    *g = format!("connection lost ({e})");
                                }
                            }
                            dead2.store(true, Ordering::Relaxed);
                            return;
                        }
                    }
                }
            });
        }
        Ok(Publisher { w, stream_id, dead, err, bytes_out: Arc::new(AtomicU64::new(0)) })
    }

    pub fn is_dead(&self) -> Option<String> {
        if self.dead.load(Ordering::Relaxed) {
            Some(self.err.lock().map(|e| e.clone()).unwrap_or_default())
        } else {
            None
        }
    }

    pub fn bytes_sent(&self) -> u64 {
        self.bytes_out.load(Ordering::Relaxed)
    }

    fn send(&self, csid: u32, ts: u32, kind: u8, payload: &[u8]) -> Res<()> {
        if let Some(e) = self.is_dead() {
            return Err(e);
        }
        let mut g = self.w.lock().map_err(|_| "internal error".to_string())?;
        let r = g.send_msg(csid, ts, kind, self.stream_id, payload);
        self.bytes_out.store(g.sent, Ordering::Relaxed);
        if let Err(e) = &r {
            self.dead.store(true, Ordering::Relaxed);
            if let Ok(mut x) = self.err.lock() {
                *x = format!("connection lost ({e})");
            }
        }
        r
    }

    /// `@setDataFrame onMetaData`
    pub fn send_metadata(&self, width: u32, height: u32, fps: f64, video_kbps: f64, audio_kbps: f64, sample_rate: f64) -> Res<()> {
        let mut p = Vec::new();
        amf_str(&mut p, "@setDataFrame");
        amf_str(&mut p, "onMetaData");
        p.push(0x08); // ECMA array
        p.extend_from_slice(&9u32.to_be_bytes());
        for (k, v) in [("width", width as f64), ("height", height as f64), ("framerate", fps), ("videodatarate", video_kbps), ("videocodecid", 7.0), ("audiodatarate", audio_kbps), ("audiosamplerate", sample_rate), ("audiocodecid", 10.0), ("stereo", 1.0)] {
            amf_key(&mut p, k);
            amf_num(&mut p, v);
        }
        amf_obj_end(&mut p);
        self.send(CSID_CMD, 0, 18, &p)
    }

    /// AVCDecoderConfigurationRecord
    pub fn send_video_header(&self, avcc: &[u8]) -> Res<()> {
        let mut p = vec![0x17, 0, 0, 0, 0];
        p.extend_from_slice(avcc);
        self.send(CSID_VIDEO, 0, 9, &p)
    }

    /// One access unit in AVCC format (4-byte length prefixed NAL units).
    pub fn send_video(&self, ts_ms: u32, keyframe: bool, avcc_data: &[u8]) -> Res<()> {
        let mut p = Vec::with_capacity(avcc_data.len() + 5);
        p.push(if keyframe { 0x17 } else { 0x27 });
        p.extend_from_slice(&[1, 0, 0, 0]);
        p.extend_from_slice(avcc_data);
        self.send(CSID_VIDEO, ts_ms, 9, &p)
    }

    /// AudioSpecificConfig (2 bytes for AAC-LC)
    pub fn send_audio_header(&self, asc: &[u8]) -> Res<()> {
        let mut p = vec![0xAF, 0];
        p.extend_from_slice(asc);
        self.send(CSID_AUDIO, 0, 8, &p)
    }

    /// One raw AAC frame (no ADTS header).
    pub fn send_audio(&self, ts_ms: u32, aac: &[u8]) -> Res<()> {
        let mut p = Vec::with_capacity(aac.len() + 2);
        p.push(0xAF);
        p.push(1);
        p.extend_from_slice(aac);
        self.send(CSID_AUDIO, ts_ms, 8, &p)
    }

    pub fn close(&self) {
        if let Ok(g) = self.w.lock() {
            let _ = g.s.shutdown(std::net::Shutdown::Both);
        }
        self.dead.store(true, Ordering::Relaxed);
    }
}

/// Read messages (answering protocol-control ones) until `done` matches. Gives up on a server `_error`.
fn wait_for(r: &mut Reader, w: &Arc<Mutex<Writer>>, done: impl Fn(&Msg) -> bool, what: &str) -> Res<Msg> {
    for _ in 0..200 {
        let m = r.next_msg().map_err(|e| format!("{what}: {e}"))?;
        match m.kind {
            1 if m.payload.len() >= 4 => {
                r.chunk = (u32::from_be_bytes([m.payload[0], m.payload[1], m.payload[2], m.payload[3]]) as usize).clamp(1, 1 << 20);
            }
            6 => {
                // peer bandwidth: acknowledge with a window size of our own
                if let Ok(mut g) = w.lock() {
                    let _ = g.send_msg(CSID_CTRL, 0, 5, 0, &2_500_000u32.to_be_bytes());
                }
            }
            _ => {}
        }
        if m.kind == 20 && amf_first_string(&m.payload).as_deref() == Some("_error") {
            let why = amf_all_strings(&m.payload).into_iter().find(|s| s.starts_with("NetConnection") || s.starts_with("NetStream")).unwrap_or_else(|| "the server returned an error".into());
            return Err(format!("{what}: {why}"));
        }
        if done(&m) {
            return Ok(m);
        }
    }
    Err(format!("{what}: no answer from the server"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_parsing() {
        assert_eq!(parse_url("rtmp://live.twitch.tv/app").unwrap(), ("live.twitch.tv".into(), 1935, "app".into()));
        assert_eq!(parse_url("rtmp://127.0.0.1:19350/live").unwrap(), ("127.0.0.1".into(), 19350, "live".into()));
        assert!(parse_url("rtmps://x/y").unwrap_err().contains("RTMPS"));
        assert!(parse_url("http://x/y").is_err());
        assert!(parse_url("rtmp://host").is_err());
    }
}
