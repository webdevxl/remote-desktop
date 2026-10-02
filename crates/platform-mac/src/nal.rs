//! Minimal parsing of length-prefixed NAL units, just enough to spot keyframes.

use protocol::Codec;

/// Iterates NAL unit payloads in an AVCC/HVCC buffer.
pub fn units(data: &[u8], length_size: usize) -> impl Iterator<Item = &[u8]> {
    let mut rest = data;
    std::iter::from_fn(move || {
        if rest.len() < length_size || length_size == 0 {
            return None;
        }
        let len = rest[..length_size].iter().fold(0usize, |acc, b| (acc << 8) | usize::from(*b));
        let unit = rest.get(length_size..length_size + len)?;
        rest = &rest[length_size + len..];
        Some(unit)
    })
}

/// True if the access unit starts a decodable sequence (IDR / IRAP picture).
pub fn is_keyframe(codec: Codec, data: &[u8], length_size: usize) -> bool {
    units(data, length_size).any(|unit| match (codec, unit.first()) {
        (Codec::H264, Some(b)) => b & 0x1f == 5,
        // HEVC IRAP pictures are NAL types 16..=23 (BLA, IDR, CRA).
        (Codec::Hevc, Some(b)) => (16..=23).contains(&((b >> 1) & 0x3f)),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_hevc_idr() {
        // Two NALs: a TRAIL_R (type 1) then IDR_W_RADL (type 19).
        let data = [0, 0, 0, 2, 0x02, 0x01, 0, 0, 0, 2, 19 << 1, 0x01];
        assert!(is_keyframe(Codec::Hevc, &data, 4));
        assert!(!is_keyframe(Codec::Hevc, &data[..6], 4));
        assert_eq!(units(&data, 4).count(), 2);
    }

    #[test]
    fn finds_h264_idr_and_tolerates_truncation() {
        let data = [0, 0, 0, 1, 0x65, 0, 0, 0, 9, 1];
        assert!(is_keyframe(Codec::H264, &data, 4));
        assert_eq!(units(&data, 4).count(), 1);
    }
}
