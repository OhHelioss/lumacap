//! H.264 helpers for streaming: Annex-B byte stream (what Media Foundation encoders produce) to the
//! length-prefixed AVCC layout that FLV/RTMP needs. Pure functions, no panics.

/// Splits an Annex-B buffer into NAL units (without start codes).
pub fn split_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut starts: Vec<(usize, usize)> = Vec::new(); // (index of start code, length of start code)
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let sc = if i > 0 && data[i - 1] == 0 { (i - 1, 4) } else { (i, 3) };
            starts.push(sc);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::new();
    for (k, &(pos, len)) in starts.iter().enumerate() {
        let begin = pos + len;
        let end = starts.get(k + 1).map(|n| n.0).unwrap_or(data.len());
        if begin < end {
            out.push(&data[begin..end]);
        }
    }
    out
}

pub struct Converted {
    pub avcc: Vec<u8>,
    pub keyframe: bool,
    pub sps: Option<Vec<u8>>,
    pub pps: Option<Vec<u8>>,
}

/// Converts one access unit. SPS/PPS/AUD are not copied into the frame data; SPS and PPS are returned separately.
pub fn annexb_to_avcc(data: &[u8]) -> Converted {
    let mut c = Converted { avcc: Vec::with_capacity(data.len() + 16), keyframe: false, sps: None, pps: None };
    for nal in split_nals(data) {
        let t = nal[0] & 0x1f;
        match t {
            7 => c.sps = Some(nal.to_vec()),
            8 => c.pps = Some(nal.to_vec()),
            9 => {}
            _ => {
                if t == 5 {
                    c.keyframe = true;
                }
                c.avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                c.avcc.extend_from_slice(nal);
            }
        }
    }
    c
}

/// AVCDecoderConfigurationRecord from one SPS and one PPS.
pub fn avcc_record(sps: &[u8], pps: &[u8]) -> Option<Vec<u8>> {
    if sps.len() < 4 || pps.is_empty() {
        return None;
    }
    let mut r = vec![1, sps[1], sps[2], sps[3], 0xFF, 0xE1];
    r.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    r.extend_from_slice(sps);
    r.push(1);
    r.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    r.extend_from_slice(pps);
    Some(r)
}

/// AudioSpecificConfig for AAC-LC stereo.
pub fn aac_asc(sample_rate: u32, channels: u32) -> Option<[u8; 2]> {
    let idx = match sample_rate {
        96000 => 0,
        88200 => 1,
        64000 => 2,
        48000 => 3,
        44100 => 4,
        32000 => 5,
        24000 => 6,
        22050 => 7,
        16000 => 8,
        12000 => 9,
        11025 => 10,
        8000 => 11,
        _ => return None,
    };
    let v: u16 = (2 << 11) | (idx << 7) | ((channels as u16 & 0xf) << 3);
    Some(v.to_be_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_annexb() {
        let au = [0, 0, 0, 1, 9, 0xf0, 0, 0, 0, 1, 0x67, 0x64, 0, 0x1f, 0, 0, 1, 0x68, 0xee, 0x3c, 0, 0, 0, 1, 0x65, 1, 2, 3];
        let c = annexb_to_avcc(&au);
        assert!(c.keyframe);
        assert_eq!(c.sps.as_deref(), Some(&[0x67, 0x64, 0, 0x1f][..]));
        assert_eq!(c.pps.as_deref(), Some(&[0x68, 0xee, 0x3c][..]));
        assert_eq!(c.avcc, vec![0, 0, 0, 4, 0x65, 1, 2, 3]);
    }

    #[test]
    fn asc_matches_known_values() {
        assert_eq!(aac_asc(48000, 2), Some([0x11, 0x90]));
        assert_eq!(aac_asc(44100, 2), Some([0x12, 0x10]));
    }

    #[test]
    fn record_layout() {
        let r = avcc_record(&[0x67, 0x64, 0, 0x1f], &[0x68, 0xee]).unwrap();
        assert_eq!(&r[..6], &[1, 0x64, 0, 0x1f, 0xFF, 0xE1]);
    }
}
