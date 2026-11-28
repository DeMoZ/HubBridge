//! VideoToolbox gives H.264 in AVCC form (every NAL unit behind a big-endian length) with the
//! parameter sets apart. The wire format is Annex B (start codes), with SPS and PPS in front of
//! every keyframe.

use hubbridge_proto::h264::START_CODE;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AvccError {
    #[error("unsupported NAL length size {0}")]
    LengthSize(usize),
    #[error("empty sample")]
    Empty,
    #[error("a NAL length field is cut off")]
    TruncatedLength,
    #[error("a NAL unit is longer than the rest of the sample")]
    Overrun,
    #[error("a NAL unit of length 0")]
    EmptyNal,
}

fn push_nal(out: &mut Vec<u8>, nal: &[u8]) {
    out.extend_from_slice(&START_CODE);
    out.extend_from_slice(nal);
}

/// The decoder configuration frame: the parameter sets (SPS, PPS) as Annex B.
pub fn config_frame<S: AsRef<[u8]>>(parameter_sets: &[S]) -> Vec<u8> {
    let mut out = Vec::new();
    for set in parameter_sets {
        push_nal(&mut out, set.as_ref());
    }
    out
}

/// One AVCC sample as an Annex B access unit; a keyframe gets `parameter_sets` in front.
pub fn to_annexb<S: AsRef<[u8]>>(
    sample: &[u8],
    length_size: usize,
    keyframe: bool,
    parameter_sets: &[S],
) -> Result<Vec<u8>, AvccError> {
    if !matches!(length_size, 1 | 2 | 4) {
        return Err(AvccError::LengthSize(length_size));
    }
    if sample.is_empty() {
        return Err(AvccError::Empty);
    }
    let mut out = if keyframe {
        config_frame(parameter_sets)
    } else {
        Vec::new()
    };
    out.reserve(sample.len() + 16);
    let mut rest = sample;
    while !rest.is_empty() {
        let (field, after) = rest.split_at_checked(length_size).ok_or(AvccError::TruncatedLength)?;
        let len = field.iter().fold(0usize, |n, b| (n << 8) | usize::from(*b));
        if len == 0 {
            return Err(AvccError::EmptyNal);
        }
        let (nal, after) = after.split_at_checked(len).ok_or(AvccError::Overrun)?;
        push_nal(&mut out, nal);
        rest = after;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use hubbridge_proto::h264::{self, NAL_IDR, NAL_NON_IDR, NAL_PPS, NAL_SPS};

    use super::*;

    const SETS: [&[u8]; 2] = [&[0x67, 0x64, 0x00], &[0x68, 0xee]];

    fn avcc(nals: &[&[u8]], size: usize) -> Vec<u8> {
        nals.iter()
            .flat_map(|n| {
                let len = (n.len() as u32).to_be_bytes();
                len[4 - size..].iter().chain(n.iter()).copied().collect::<Vec<_>>()
            })
            .collect()
    }

    #[test]
    fn every_length_size() {
        for size in [1, 2, 4] {
            let out = to_annexb(&avcc(&[&[0x41, 1, 2]], size), size, false, &SETS).unwrap();
            assert_eq!(out, [0, 0, 0, 1, 0x41, 1, 2], "length size {size}");
            assert_eq!(h264::validate(&out).unwrap().types, [NAL_NON_IDR]);
        }
    }

    #[test]
    fn a_keyframe_gets_the_parameter_sets_and_keeps_both_nals() {
        // An SEI and an IDR slice.
        let sample = avcc(&[&[0x06, 5, 1], &[0x65, 9, 9, 9]], 4);
        let out = to_annexb(&sample, 4, true, &SETS).unwrap();
        let au = h264::validate(&out).unwrap();
        assert_eq!(au.types, [NAL_SPS, NAL_PPS, 6, NAL_IDR]);
        assert!(au.is_keyframe() && au.has_parameter_sets());
    }

    #[test]
    fn the_config_frame() {
        let config = config_frame(&SETS);
        let au = h264::validate(&config).unwrap();
        assert_eq!(au.types, [NAL_SPS, NAL_PPS]);
        assert!(!au.has_slice());
    }

    #[test]
    fn broken_samples() {
        let go = |s: &[u8]| to_annexb(s, 4, false, &SETS);
        assert_eq!(go(&[]), Err(AvccError::Empty));
        assert_eq!(go(&[0, 0, 0]), Err(AvccError::TruncatedLength));
        assert_eq!(go(&[0, 0, 0, 9, 0x41]), Err(AvccError::Overrun));
        assert_eq!(go(&[0xff, 0xff, 0xff, 0xff, 0x41]), Err(AvccError::Overrun));
        assert_eq!(go(&[0, 0, 0, 0]), Err(AvccError::EmptyNal));
        assert_eq!(go(&[0, 0, 0, 1, 0x41, 0, 0]), Err(AvccError::TruncatedLength));
        assert_eq!(to_annexb(&[1, 0x41], 3, false, &SETS), Err(AvccError::LengthSize(3)));
    }
}
