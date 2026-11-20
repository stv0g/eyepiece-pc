//! Annex-B helpers: the bridge wants start-code delimited access units with SPS/PPS
//! (VPS/SPS/PPS) in front of every keyframe.

pub const START: [u8; 4] = [0, 0, 0, 1];

pub fn is_annexb(d: &[u8]) -> bool {
    d.starts_with(&[0, 0, 1]) || d.starts_with(&START)
}

/// NAL unit types in an Annex-B buffer.
pub fn nal_types(d: &[u8], hevc: bool) -> Vec<u8> {
    let mut v = vec![];
    let mut i = 0;
    while i + 3 <= d.len() {
        if d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1 {
            if i + 3 < d.len() {
                let b = d[i + 3];
                v.push(if hevc { (b >> 1) & 0x3f } else { b & 0x1f });
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    v
}

pub fn has_param_sets(d: &[u8], hevc: bool) -> bool {
    let t = nal_types(d, hevc);
    if hevc {
        t.contains(&33) // SPS
    } else {
        t.contains(&7)
    }
}

/// Length-prefixed NAL units → Annex-B.
pub fn lp_to_annexb(d: &[u8], len_size: usize, out: &mut Vec<u8>) {
    let mut p = 0;
    while p + len_size <= d.len() {
        let mut n = 0usize;
        for k in 0..len_size {
            n = n << 8 | d[p + k] as usize;
        }
        p += len_size;
        let e = (p + n).min(d.len());
        out.extend_from_slice(&START);
        out.extend_from_slice(&d[p..e]);
        p = e;
    }
}

/// Encoder extradata (Annex-B already, or avcC / hvcC) → Annex-B parameter sets.
pub fn param_sets_from_extradata(ed: &[u8], hevc: bool) -> Vec<u8> {
    if is_annexb(ed) {
        return ed.to_vec();
    }
    let mut out = vec![];
    let rd16 = |p: usize| -> usize { if p + 2 <= ed.len() { (ed[p] as usize) << 8 | ed[p + 1] as usize } else { 0 } };
    if !hevc && ed.len() > 6 && ed[0] == 1 {
        // avcC
        let mut p = 5;
        for pass in 0..2 {
            if p >= ed.len() {
                break;
            }
            let n = if pass == 0 { (ed[p] & 0x1f) as usize } else { ed[p] as usize };
            p += 1;
            for _ in 0..n {
                let l = rd16(p);
                p += 2;
                if p + l > ed.len() {
                    return out;
                }
                out.extend_from_slice(&START);
                out.extend_from_slice(&ed[p..p + l]);
                p += l;
            }
        }
    } else if hevc && ed.len() > 23 {
        // hvcC
        let arrays = ed[22] as usize;
        let mut p = 23;
        for _ in 0..arrays {
            let n = rd16(p + 1);
            p += 3;
            for _ in 0..n {
                let l = rd16(p);
                p += 2;
                if p + l > ed.len() {
                    return out;
                }
                out.extend_from_slice(&START);
                out.extend_from_slice(&ed[p..p + l]);
                p += l;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn types() {
        let d = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9];
        assert_eq!(nal_types(&d, false), vec![7, 8, 5]);
        assert!(has_param_sets(&d, false));
    }
    #[test]
    fn avcc() {
        let ed = [1, 0x64, 0, 0x28, 0xff, 0xe1, 0, 2, 0x67, 0xaa, 1, 0, 1, 0x68];
        assert_eq!(param_sets_from_extradata(&ed, false), vec![0, 0, 0, 1, 0x67, 0xaa, 0, 0, 0, 1, 0x68]);
    }
}
