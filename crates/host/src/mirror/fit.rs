//! The stream size: the source scaled to the largest size that fits the phone display and
//! 1920×1080, keeping the aspect ratio, with even width and height. The phone centers it
//! between black bars.

/// The largest stream, whatever the display can show.
pub const MAX_WIDTH: u32 = 1920;
pub const MAX_HEIGHT: u32 = 1080;

/// Bitrate bounds of the presets, kbit/s.
const MIN_BITRATE_KBPS: u64 = 2_000;
const MAX_BITRATE_KBPS: u64 = 20_000;

/// The stream size for a `src_w`×`src_h` source on a `dst_w`×`dst_h` display. Integer
/// arithmetic, rounded down to even numbers; (0, 0) when any size is 0.
pub fn fit(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> (u32, u32) {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return (0, 0);
    }
    let (sw, sh) = (u64::from(src_w), u64::from(src_h));
    let (bw, bh) = (u64::from(dst_w.min(MAX_WIDTH)), u64::from(dst_h.min(MAX_HEIGHT)));
    // Which side limits the scale: bw/sw <= bh/sh, without the division.
    let (w, h) = if bw * sh <= bh * sw {
        (bw, sh * bw / sw)
    } else {
        (sw * bh / sh, bh)
    };
    // Both are at most 1920.
    ((w & !1) as u32, (h & !1) as u32)
}

/// The preset bitrate: 0.25 bit per pixel per frame, between 2 and 20 Mbit/s.
pub fn preset_bitrate_kbps(width: u32, height: u32, fps: u32) -> u32 {
    let kbps = u64::from(width) * u64::from(height) * u64::from(fps) / 4 / 1000;
    kbps.clamp(MIN_BITRATE_KBPS, MAX_BITRATE_KBPS) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_examples_of_the_design() {
        assert_eq!(fit(3024, 1964, 1280, 720), (1108, 720));
        assert_eq!(fit(2560, 1440, 1280, 720), (1280, 720));
        assert_eq!(fit(1024, 768, 1280, 720), (960, 720));
        assert_eq!(fit(5120, 2880, 3840, 2160), (1920, 1080));
    }

    #[test]
    fn sixteen_by_ten_and_the_measured_monitor() {
        assert_eq!(fit(1280, 800, 1280, 720), (1152, 720));
        // 2560×1600 without DeX: the 1080p cap decides.
        assert_eq!(fit(3024, 1964, 2560, 1600), (1662, 1080));
    }

    #[test]
    fn a_small_source_is_scaled_up_but_never_above_1080p() {
        assert_eq!(fit(640, 360, 1280, 720), (1280, 720));
        assert_eq!(fit(640, 360, 3840, 2160), (1920, 1080));
        assert_eq!(fit(100, 100, 4000, 4000), (1080, 1080));
    }

    #[test]
    fn always_even_and_inside_the_display() {
        for (sw, sh) in [
            (1365, 767),
            (1727, 1117),
            (3, 5),
            (1081, 1921),
            (7680, 1080),
            (1000, 1001),
        ] {
            for (dw, dh) in [(1280, 720), (719, 479), (640, 480), (2560, 1600), (1081, 1923)] {
                let (w, h) = fit(sw, sh, dw, dh);
                assert!(w % 2 == 0 && h % 2 == 0, "{sw}x{sh} on {dw}x{dh}: {w}x{h}");
                assert!(
                    w <= dw.min(MAX_WIDTH) && h <= dh.min(MAX_HEIGHT),
                    "{sw}x{sh} on {dw}x{dh}: {w}x{h}"
                );
            }
        }
        assert_eq!(fit(0, 10, 1280, 720), (0, 0));
        assert_eq!(fit(10, 10, 1280, 0), (0, 0));
    }

    #[test]
    fn preset_bitrates() {
        assert_eq!(preset_bitrate_kbps(1108, 720, 30), 5983);
        assert_eq!(preset_bitrate_kbps(640, 360, 30), 2000);
        assert_eq!(preset_bitrate_kbps(1920, 1080, 60), 20000);
    }
}
