//! Wire protocol, mirrors `usb-vr/device/proto.h` (all little-endian).
//!
//! Every message: 8-byte header `'V' 'R' type flags u32 len` + payload.

pub const VR_PORT: u16 = 42420;
pub const VR_VERSION: u32 = 1;
pub const VR_MAX_PAYLOAD: u32 = 4 << 20;
pub const HDR_LEN: usize = 8;

// PC -> goggles
pub const VR_VIDEO: u8 = 0x01;
pub const VR_PING: u8 = 0x02;
pub const VR_CTRL: u8 = 0x03;
pub const VR_STREAM: u8 = 0x04;
// goggles -> PC
pub const VR_POSE: u8 = 0x81;
pub const VR_PONG: u8 = 0x82;
pub const VR_LOG: u8 = 0x83;
pub const VR_FRAME: u8 = 0x84;
pub const VR_INFO: u8 = 0x85;
/// Proposed addition (not sent by the current bridge): "I dropped data and wait for an
/// IDR, send one now". Payload: u32 reason (0 = unspecified). Old PCs ignore unknown types.
pub const VR_KEYREQ: u8 = 0x86;

pub const VR_FLAG_KEY: u8 = 0x01;
pub const VR_CODEC_H264: u8 = 0;
pub const VR_CODEC_HEVC: u8 = 1;
pub const VR_OP_RECENTER: u8 = 1;
pub const VR_OP_QUIT: u8 = 2;

pub fn header(mtype: u8, flags: u8, len: u32) -> [u8; HDR_LEN] {
    let l = len.to_le_bytes();
    [b'V', b'R', mtype, flags, l[0], l[1], l[2], l[3]]
}

/// Parsed header: (type, flags, len). None = bad magic.
pub fn parse_header(h: &[u8; HDR_LEN]) -> Option<(u8, u8, u32)> {
    if h[0] != b'V' || h[1] != b'R' {
        return None;
    }
    Some((h[2], h[3], u32::from_le_bytes([h[4], h[5], h[6], h[7]])))
}

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64le(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
fn f32le(b: &[u8], o: usize) -> f32 {
    f32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

/// vr_video_t: u64 pts_us (PC capture time, PC monotonic clock), u8 codec, 3 reserved.
pub fn video_prefix(pts_us: u64, codec: u8) -> [u8; 12] {
    let mut b = [0u8; 12];
    b[..8].copy_from_slice(&pts_us.to_le_bytes());
    b[8] = codec;
    b
}

/// vr_stream_t: must precede the video (the hardware decoder sizes its buffers from it).
pub fn stream_payload(width: u16, height: u16, codec: u8, fps: u32) -> [u8; 8] {
    let mut b = [0u8; 8];
    b[..2].copy_from_slice(&width.to_le_bytes());
    b[2..4].copy_from_slice(&height.to_le_bytes());
    b[4] = codec;
    b[5] = fps.min(255) as u8;
    b
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Pose {
    pub t_dev_us: u64,
    pub yaw: f32,
    pub pitch: f32,
    pub roll: f32,
    pub active: u8,
    pub source: u8,
}
impl Pose {
    pub fn parse(p: &[u8]) -> Option<Pose> {
        (p.len() >= 24).then(|| Pose {
            t_dev_us: u64le(p, 0),
            yaw: f32le(p, 8),
            pitch: f32le(p, 12),
            roll: f32le(p, 16),
            active: p[20],
            source: p[21],
        })
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FrameReport {
    pub pts_us: u64,
    pub t_recv_us: u64,
    pub t_shown_us: u64,
    pub decode_us: u32,
    pub convert_us: u32,
    pub queue_depth: u32,
    pub dropped: u32,
}
impl FrameReport {
    pub fn parse(p: &[u8]) -> Option<FrameReport> {
        (p.len() >= 40).then(|| FrameReport {
            pts_us: u64le(p, 0),
            t_recv_us: u64le(p, 8),
            t_shown_us: u64le(p, 16),
            decode_us: u32le(p, 24),
            convert_us: u32le(p, 28),
            queue_depth: u32le(p, 32),
            dropped: u32le(p, 36),
        })
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Info {
    pub version: u32,
    pub fb_w: u16,
    pub fb_h: u16,
    pub decoder: u8,
    pub headtrack: u8,
}
impl Info {
    pub fn parse(p: &[u8]) -> Option<Info> {
        (p.len() >= 12).then(|| Info {
            version: u32le(p, 0),
            fb_w: u16le(p, 4),
            fb_h: u16le(p, 6),
            decoder: p[8],
            headtrack: p[9],
        })
    }
    pub fn decoder_name(&self) -> &'static str {
        match self.decoder {
            1 => "software",
            2 => "hardware",
            _ => "none",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn header_roundtrip() {
        let h = header(VR_VIDEO, VR_FLAG_KEY, 1234);
        assert_eq!(parse_header(&h), Some((VR_VIDEO, VR_FLAG_KEY, 1234)));
        assert_eq!(&h[..2], b"VR");
    }
    #[test]
    fn stream_layout() {
        let s = stream_payload(1920, 1080, VR_CODEC_H264, 100);
        assert_eq!(s, [0x80, 0x07, 0x38, 0x04, 0, 100, 0, 0]);
    }
}
