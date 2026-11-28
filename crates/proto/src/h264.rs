//! The little of H.264 Annex B that both ends need: splitting into NAL units and checking an
//! access unit. No I/O, no decoding.

/// `nal_unit_type` values used here.
pub const NAL_NON_IDR: u8 = 1;
pub const NAL_IDR: u8 = 5;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;

/// The four-byte start code.
pub const START_CODE: [u8; 4] = [0, 0, 0, 1];

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum H264Error {
    #[error("empty data")]
    Empty,
    #[error("no start code at the beginning")]
    NoStartCode,
    #[error("empty NAL unit")]
    EmptyNal,
    #[error("the forbidden bit of a NAL unit header is set")]
    ForbiddenBit,
}

/// Position of the next `00 00 01` at or after `from`.
fn find_start(data: &[u8], from: usize) -> Option<usize> {
    data.get(from..)?
        .windows(3)
        .position(|w| w == [0, 0, 1])
        .map(|at| from + at)
}

/// NAL units of an Annex B stream (3- and 4-byte start codes), without start codes and
/// without the zero bytes that may trail them. Bytes before the first start code are skipped.
/// Emulation prevention bytes (`00 00 03`) stay as they are.
pub fn split_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut next = find_start(data, 0);
    while let Some(at) = next {
        let begin = at + 3;
        next = find_start(data, begin);
        let mut end = next.unwrap_or(data.len());
        // The leading zero of a 4-byte start code, and trailing zeros.
        while end > begin && data[end - 1] == 0 {
            end -= 1;
        }
        nals.push(&data[begin..end]);
    }
    nals
}

/// The type of a NAL unit (without start code); 0 for an empty one.
pub fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| b & 0x1f)
}

/// Whether the data holds an IDR slice.
pub fn is_keyframe(data: &[u8]) -> bool {
    split_nals(data).iter().any(|n| nal_type(n) == NAL_IDR)
}

/// Whether the data holds both an SPS and a PPS.
pub fn has_parameter_sets(data: &[u8]) -> bool {
    let nals = split_nals(data);
    [NAL_SPS, NAL_PPS]
        .iter()
        .all(|t| nals.iter().any(|n| nal_type(n) == *t))
}

/// What a valid access unit (or config frame) holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessUnit {
    /// NAL unit types in order.
    pub types: Vec<u8>,
}

impl AccessUnit {
    pub fn is_keyframe(&self) -> bool {
        self.types.contains(&NAL_IDR)
    }

    pub fn has_parameter_sets(&self) -> bool {
        self.types.contains(&NAL_SPS) && self.types.contains(&NAL_PPS)
    }

    pub fn has_slice(&self) -> bool {
        self.types.iter().any(|t| (NAL_NON_IDR..=NAL_IDR).contains(t))
    }
}

/// Checks that `data` is a well-formed Annex B access unit: it starts with a start code and
/// every NAL unit has a header.
pub fn validate(data: &[u8]) -> Result<AccessUnit, H264Error> {
    if data.is_empty() {
        return Err(H264Error::Empty);
    }
    if !(data.starts_with(&START_CODE) || data.starts_with(&START_CODE[1..])) {
        return Err(H264Error::NoStartCode);
    }
    let mut types = Vec::new();
    for nal in split_nals(data) {
        let Some(&header) = nal.first() else {
            return Err(H264Error::EmptyNal);
        };
        if header & 0x80 != 0 {
            return Err(H264Error::ForbiddenBit);
        }
        types.push(nal_type(nal));
    }
    Ok(AccessUnit { types })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn join(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter()
            .flat_map(|n| START_CODE.iter().chain(n.iter()).copied())
            .collect()
    }

    #[test]
    fn sps_pps_idr() {
        let data = join(&[&[0x67, 1, 2], &[0x68, 3], &[0x65, 4, 5]]);
        let au = validate(&data).unwrap();
        assert_eq!(au.types, [NAL_SPS, NAL_PPS, NAL_IDR]);
        assert!(au.is_keyframe() && au.has_parameter_sets() && au.has_slice());
        assert!(is_keyframe(&data) && has_parameter_sets(&data));
        assert_eq!(split_nals(&data)[1], [0x68, 3]);
    }

    #[test]
    fn non_idr_only_with_a_three_byte_start_code() {
        let data = [0, 0, 1, 0x41, 9, 9];
        let au = validate(&data).unwrap();
        assert_eq!(au.types, [NAL_NON_IDR]);
        assert!(!au.is_keyframe() && !au.has_parameter_sets() && au.has_slice());
    }

    #[test]
    fn mixed_start_codes_and_trailing_zeros() {
        let data = [0, 0, 0, 1, 0x67, 5, 0, 0, 1, 0x68, 6, 0, 0, 0, 0, 1, 0x65, 7, 0];
        assert_eq!(split_nals(&data), [&[0x67u8, 5][..], &[0x68, 6][..], &[0x65, 7][..]]);
    }

    #[test]
    fn emulation_prevention_bytes_stay() {
        let nal: &[u8] = &[0x65, 0, 0, 3, 1, 0, 0, 3, 0, 7];
        let data = join(&[nal]);
        assert_eq!(split_nals(&data), [nal]);
    }

    #[test]
    fn bad_input() {
        assert_eq!(validate(&[]), Err(H264Error::Empty));
        assert_eq!(validate(&[0x65, 1, 2]), Err(H264Error::NoStartCode));
        assert_eq!(validate(&[9, 0, 0, 1, 0x65]), Err(H264Error::NoStartCode));
        assert_eq!(validate(&[0, 0, 0, 1]), Err(H264Error::EmptyNal));
        assert_eq!(validate(&[0, 0, 1, 0, 0, 1, 0x65]), Err(H264Error::EmptyNal));
        assert_eq!(validate(&[0, 0, 1, 0xe5, 1]), Err(H264Error::ForbiddenBit));
        assert!(split_nals(&[1, 2, 3]).is_empty());
    }
}
