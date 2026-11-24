//! Black bars baked into camera frames.
//!
//! Some cameras put their picture inside a bigger frame with black around
//! it. A DJI Osmo Pocket 3 on a portrait format sends its 16:9 picture in
//! the middle of a 720x1280 frame. The camera thread measures the bars twice
//! a second, `Steady` decides when to trust a measurement, and the
//! compositor crops the bars off so the overlay takes the picture's shape.

use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};

use crate::util::PIX_BGRA;

/// A line belongs to a bar when no sample on it is brighter than this
/// (luma, 0 to 255). Bars are digital black, close to 0.
const DARK: u8 = 24;
/// Samples taken along each line.
const SAMPLES: usize = 64;
/// Bars thinner than this fraction of the frame count as none.
const MIN_BAR: f64 = 0.02;
/// Opposite bars may differ by this fraction of the frame, plus 2 px.
const SYMMETRY: f64 = 0.02;
/// The picture must keep at least this fraction of each side of the frame.
/// A 16:9 picture letterboxed in a 9:16 frame keeps 32% of its height.
const MIN_PICTURE: f64 = 0.25;
/// Pixels cropped off the picture next to each bar, where the edge can be
/// blended with the black.
const INSET: usize = 2;
/// Seconds between measurements.
pub const EVERY: f64 = 0.5;
/// Measurements in a row that must agree before the crop changes. At one
/// every `EVERY` seconds, that spans 1.5 s.
const HOLD: u32 = 4;

/// Rows (top, bottom) and columns (left, right) of black around a frame's
/// picture.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bars {
    pub top: usize,
    pub bottom: usize,
    pub left: usize,
    pub right: usize,
}

impl Bars {
    pub const NONE: Bars = Bars { top: 0, bottom: 0, left: 0, right: 0 };

    pub fn is_none(&self) -> bool {
        *self == Self::NONE
    }

    /// The picture in a `w`x`h` frame as (x, y, width, height), with y
    /// measured up from the bottom edge as CoreImage does.
    pub fn picture(&self, w: f64, h: f64) -> (f64, f64, f64, f64) {
        let (t, b, l, r) = (self.top as f64, self.bottom as f64, self.left as f64, self.right as f64);
        (l, b, w - l - r, h - t - b)
    }
}

/// `n` (at most `SAMPLES`) positions spread evenly over `from..to`.
fn spread(from: usize, to: usize) -> Vec<usize> {
    let len = to - from;
    let n = len.min(SAMPLES);
    (0..n).map(|i| from + (2 * i + 1) * len / (2 * n)).collect()
}

/// The bars of a `w`x`h` frame given each pixel's luma: the dark lines at
/// each edge. None when the whole frame is dark.
pub fn measure(w: usize, h: usize, luma: impl Fn(usize, usize) -> u8) -> Option<Bars> {
    if w == 0 || h == 0 {
        return None;
    }
    let xs = spread(0, w);
    let dark_row = |y: usize| xs.iter().all(|&x| luma(x, y) <= DARK);
    let top = (0..h).find(|&y| !dark_row(y))?;
    let last = (top..h).rev().find(|&y| !dark_row(y))?;
    // Sample columns over the picture's rows only.
    let ys = spread(top, last + 1);
    let dark_col = |x: usize| ys.iter().all(|&y| luma(x, y) <= DARK);
    let left = (0..w).find(|&x| !dark_col(x))?;
    let right = (left..w).rev().find(|&x| !dark_col(x))?;
    Some(Bars { top, bottom: h - 1 - last, left, right: w - 1 - right })
}

/// Measure a BGRA frame (None for other formats or an all-dark frame).
pub fn measure_bgra(pb: &CVPixelBuffer) -> Option<Bars> {
    if CVPixelBufferGetPixelFormatType(pb) != PIX_BGRA {
        return None;
    }
    let (w, h) = (CVPixelBufferGetWidth(pb), CVPixelBufferGetHeight(pb));
    unsafe {
        if CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) != 0 {
            return None;
        }
        let base = CVPixelBufferGetBaseAddress(pb) as *const u8;
        let stride = CVPixelBufferGetBytesPerRow(pb);
        let m = (!base.is_null()).then(|| {
            measure(w, h, |x, y| {
                let p = base.add(y * stride + x * 4);
                let (b, g, r) = (*p as u32, *p.add(1) as u32, *p.add(2) as u32);
                ((19 * b + 183 * g + 54 * r) >> 8) as u8
            })
        });
        CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly);
        m.flatten()
    }
}

/// The crop a measurement of a `w`x`h` frame asks for: no bars when they
/// are thin, the bars (inset) when they look like a letterbox or
/// pillarbox, or None when unsure (lopsided, or too little picture left,
/// as in a dark scene).
fn judge(w: usize, h: usize, m: Bars) -> Option<Bars> {
    let axis = |a: usize, b: usize, len: usize| -> Option<(usize, usize)> {
        let l = len as f64;
        if (a.max(b) as f64) < MIN_BAR * l {
            return Some((0, 0));
        }
        if a.abs_diff(b) as f64 > SYMMETRY * l + 2.0 {
            return None;
        }
        let inset = |v: usize| if v > 0 { v + INSET } else { 0 };
        let (a, b) = (inset(a), inset(b));
        if ((len.saturating_sub(a + b)) as f64) < MIN_PICTURE * l {
            return None;
        }
        Some((a, b))
    };
    let (top, bottom) = axis(m.top, m.bottom, h)?;
    let (left, right) = axis(m.left, m.right, w)?;
    Some(Bars { top, bottom, left, right })
}

/// Two crops of a `w`x`h` frame within a pixel or two of each other.
fn near(a: Bars, b: Bars, w: usize, h: usize) -> bool {
    let (tx, ty) = ((w / 100).max(2), (h / 100).max(2));
    a.top.abs_diff(b.top) <= ty && a.bottom.abs_diff(b.bottom) <= ty && a.left.abs_diff(b.left) <= tx && a.right.abs_diff(b.right) <= tx
}

/// Keeps the crop steady. It switches to a new crop only after `HOLD`
/// measurements in a row agree on it. An unsure measurement (a dark or odd
/// frame) keeps the crop in use and restarts the count.
#[derive(Debug, Default)]
pub struct Steady {
    size: (usize, usize),
    current: Bars,
    pending: Option<(Bars, u32)>,
}

impl Steady {
    /// The crop in use.
    pub fn current(&self) -> Bars {
        self.current
    }

    /// Take a measurement of a `w`x`h` frame (None: dark all over) and
    /// return the crop to use. A new frame size starts over with no crop.
    pub fn update(&mut self, w: usize, h: usize, m: Option<Bars>) -> Bars {
        if self.size != (w, h) {
            *self = Steady { size: (w, h), ..Default::default() };
        }
        match m.and_then(|m| judge(w, h, m)) {
            None => self.pending = None,
            Some(b) if near(b, self.current, w, h) => self.pending = None,
            Some(b) => {
                let n = match self.pending {
                    Some((p, n)) if near(p, b, w, h) => n + 1,
                    _ => 1,
                };
                if n >= HOLD {
                    self.current = b;
                    self.pending = None;
                } else {
                    self.pending = Some((b, n));
                }
            }
        }
        self.current
    }
}

/// One camera's crop: measures its frames every `EVERY` seconds (and at
/// once when the frame size changes) and keeps the result steady.
#[derive(Debug, Default)]
pub struct Tracker {
    steady: Steady,
    last: Option<f64>,
}

impl Tracker {
    pub fn current(&self) -> Bars {
        self.steady.current()
    }

    /// The bars to crop from `pb`, a BGRA camera frame captured at `t`
    /// seconds.
    pub fn frame(&mut self, t: f64, pb: &CVPixelBuffer) -> Bars {
        let size = (CVPixelBufferGetWidth(pb), CVPixelBufferGetHeight(pb));
        let due = self.steady.size != size || self.last.is_none_or(|l| (t - l).abs() >= EVERY);
        if !due {
            return self.steady.current();
        }
        self.last = Some(t);
        self.steady.update(size.0, size.1, measure_bgra(pb))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Luma of a `w`x`h` frame: black outside `pic` (x, y, width, height
    /// from the top-left), `v` inside.
    fn framed(pic: (usize, usize, usize, usize), v: u8) -> impl Fn(usize, usize) -> u8 {
        move |x, y| if x >= pic.0 && x < pic.0 + pic.2 && y >= pic.1 && y < pic.1 + pic.3 { v } else { 0 }
    }

    /// Feed `m` to `s` `n` times; the crop after each.
    fn feed(s: &mut Steady, w: usize, h: usize, m: Option<Bars>, n: usize) -> Vec<Bars> {
        (0..n).map(|_| s.update(w, h, m)).collect()
    }

    const OSMO: Bars = Bars { top: 437, bottom: 438, left: 0, right: 0 };

    #[test]
    fn measures_letterbox_and_pillarbox() {
        // The Osmo's 16:9 picture in a 720x1280 frame.
        assert_eq!(measure(720, 1280, framed((0, 437, 720, 405), 120)), Some(OSMO));
        // A 4:3 picture pillarboxed in 16:9.
        let m = measure(1280, 720, framed((160, 0, 960, 720), 90));
        assert_eq!(m, Some(Bars { top: 0, bottom: 0, left: 160, right: 160 }));
        // No bars; a dark picture still counts if it isn't black.
        assert_eq!(measure(1280, 720, |_, _| 40), Some(Bars::NONE));
        // Black all over.
        assert_eq!(measure(1280, 720, |_, _| 3), None);
        // A dark scene with one bright spot off center is lopsided.
        let spot = measure(1280, 720, framed((900, 100, 40, 40), 200)).unwrap();
        assert_eq!(judge(1280, 720, spot), None);
    }

    #[test]
    fn picture_rect_is_measured_from_the_bottom() {
        let b = Bars { top: 10, bottom: 30, left: 5, right: 15 };
        assert_eq!(b.picture(100.0, 200.0), (5.0, 30.0, 80.0, 160.0));
    }

    #[test]
    fn judges_letterbox_pillarbox_and_unsure_frames() {
        let (w, h) = (720, 1280);
        // Letterbox: cropped, inset 2 px from the picture's edge.
        assert_eq!(judge(w, h, OSMO), Some(Bars { top: 439, bottom: 440, left: 0, right: 0 }));
        // Pillarbox.
        let p = Bars { top: 0, bottom: 0, left: 160, right: 160 };
        assert_eq!(judge(1280, 720, p), Some(Bars { top: 0, bottom: 0, left: 162, right: 162 }));
        // A dark line or two at an edge: no crop.
        assert_eq!(judge(w, h, Bars { top: 3, bottom: 0, left: 1, right: 2 }), Some(Bars::NONE));
        // Lopsided (a dark ceiling above, picture below): unsure.
        assert_eq!(judge(w, h, Bars { top: 300, bottom: 0, left: 0, right: 0 }), None);
        // Too little picture left (a bright spot in a dark room): unsure.
        assert_eq!(judge(w, h, Bars { top: 600, bottom: 600, left: 0, right: 0 }), None);
        assert_eq!(judge(w, h, Bars { top: 0, bottom: 0, left: 300, right: 300 }), None);
    }

    #[test]
    fn letterbox_is_cropped_after_it_holds() {
        let mut s = Steady::default();
        let crops = feed(&mut s, 720, 1280, Some(OSMO), 5);
        let want = judge(720, 1280, OSMO).unwrap();
        assert_eq!(crops, [Bars::NONE, Bars::NONE, Bars::NONE, want, want]);
        // A pixel of jitter in the measurement doesn't move the crop.
        let jitter = Bars { top: 438, bottom: 437, ..OSMO };
        assert_eq!(feed(&mut s, 720, 1280, Some(jitter), 6), [want; 6]);
    }

    #[test]
    fn no_bars_stays_uncropped() {
        let mut s = Steady::default();
        assert!(feed(&mut s, 1280, 720, Some(Bars::NONE), 10).iter().all(Bars::is_none));
        // A dark scene (all black, or a lopsided spot) never adds a crop.
        assert!(feed(&mut s, 1280, 720, None, 10).iter().all(Bars::is_none));
        let spot = Bars { top: 100, bottom: 580, left: 900, right: 340 };
        assert!(feed(&mut s, 1280, 720, Some(spot), 10).iter().all(Bars::is_none));
    }

    #[test]
    fn dark_frames_keep_the_crop() {
        let mut s = Steady::default();
        let want = *feed(&mut s, 720, 1280, Some(OSMO), 4).last().unwrap();
        assert!(!want.is_none());
        // One dark frame, then a long dark scene: the crop stays.
        assert_eq!(s.update(720, 1280, None), want);
        assert_eq!(feed(&mut s, 720, 1280, None, 20), [want; 20]);
        // Unsure, lopsided frames (dark hair against the top bar) too.
        let lopsided = Bars { top: 600, ..OSMO };
        assert_eq!(feed(&mut s, 720, 1280, Some(lopsided), 20), [want; 20]);
    }

    #[test]
    fn one_off_frames_dont_flicker() {
        let mut s = Steady::default();
        let want = *feed(&mut s, 720, 1280, Some(OSMO), 4).last().unwrap();
        // The bars disappear for less than HOLD measurements, now and then.
        for _ in 0..5 {
            assert_eq!(feed(&mut s, 720, 1280, Some(Bars::NONE), 3), [want; 3]);
            assert_eq!(s.update(720, 1280, Some(OSMO)), want);
        }
        // An unsure measurement restarts the count.
        let mut crops = feed(&mut s, 720, 1280, Some(Bars::NONE), 3);
        crops.extend(feed(&mut s, 720, 1280, None, 1));
        crops.extend(feed(&mut s, 720, 1280, Some(Bars::NONE), 3));
        assert_eq!(crops, [want; 7]);
        // When they are gone for good, the crop goes after HOLD.
        assert_eq!(s.update(720, 1280, Some(Bars::NONE)), Bars::NONE);
    }

    #[test]
    fn new_frame_size_starts_over() {
        let mut s = Steady::default();
        feed(&mut s, 720, 1280, Some(OSMO), 4);
        assert!(!s.current().is_none());
        // The camera switched to 1280x720: the old crop means nothing there.
        assert_eq!(s.update(1280, 720, Some(Bars::NONE)), Bars::NONE);
    }
}
