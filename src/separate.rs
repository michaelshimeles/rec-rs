//! The screen and the camera recorded as files of their own (`--separate`),
//! to edit apart later. They come from the same session as the composed
//! video: one clock, one frame grid, the same pauses and length.
//!
//! `NAME.mov` is the composed video, `NAME-screen.mov` the screen and
//! `NAME-camera.mov` the camera's picture with any black bars cropped off.

use std::path::{Path, PathBuf};

use objc2::rc::Retained;
use objc2_core_foundation::{CFRetained, CGAffineTransform, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGColorSpace, kCGColorSpaceSRGB};
use objc2_core_image::{CIColor, CIContext, CIImage};
use objc2_core_video::{CVPixelBuffer, CVPixelBufferGetHeight, CVPixelBufferGetWidth};

use crate::bars::Bars;
use crate::util::*;

/// Which files a recording writes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Files {
    /// One video of the scene (the screen with the camera overlay).
    #[default]
    Composed,
    /// The screen and the camera, each in a file of its own.
    Separate,
    /// The composed video and the two separate files.
    Both,
}

impl Files {
    pub fn from_flags(separate: bool, composed: bool) -> Files {
        match (separate, composed) {
            (false, _) => Files::Composed,
            (true, false) => Files::Separate,
            (true, true) => Files::Both,
        }
    }

    /// The composed video is written.
    pub fn composed(self) -> bool {
        self != Files::Separate
    }

    /// The screen and the camera are written separately.
    pub fn separate(self) -> bool {
        self != Files::Composed
    }
}

/// The screen's and the camera's files for a recording named `base`:
/// `NAME-screen.EXT` and `NAME-camera.EXT` in the same folder.
pub fn paths(base: &Path) -> [PathBuf; 2] {
    let stem = base.file_stem().unwrap_or_default().to_string_lossy();
    let named = |what: &str| {
        let mut name = format!("{stem}-{what}");
        if let Some(ext) = base.extension() {
            name.push('.');
            name.push_str(&ext.to_string_lossy());
        }
        base.with_file_name(name)
    };
    [named("screen"), named("camera")]
}

/// The files a recording named `base` writes, the main one first: the
/// composed video, else the screen's file.
pub fn outputs(base: &Path, files: Files) -> Vec<PathBuf> {
    let mut out = vec![];
    if files.composed() {
        out.push(base.to_path_buf());
    }
    if files.separate() {
        out.extend(paths(base));
    }
    out
}

/// `path` and the other files of its recording that exist (the composed
/// video and the screen's and camera's files), for showing them together.
pub fn recording_files(path: &Path) -> Vec<PathBuf> {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let base_stem = stem.strip_suffix("-screen").or_else(|| stem.strip_suffix("-camera")).unwrap_or(&stem);
    let mut name = base_stem.to_string();
    if let Some(ext) = path.extension() {
        name.push('.');
        name.push_str(&ext.to_string_lossy());
    }
    let base = path.with_file_name(name);
    let mut out: Vec<PathBuf> = [base.clone()].into_iter().chain(paths(&base)).filter(|p| p.exists()).collect();
    if !out.iter().any(|p| p == path) {
        out.insert(0, path.to_path_buf());
    }
    out
}

/// Size of the camera's file: the picture without its black bars, in even
/// pixels, for the newest camera frame (`width`, `height`, bars). Without
/// one yet, 1280x720, the size Rec picks for cameras that have it.
pub fn camera_size(frame: Option<(usize, usize, Bars)>) -> (usize, usize) {
    let Some((w, h, bars)) = frame else { return (1280, 720) };
    let (_, _, pw, ph) = bars.picture(w as f64, h as f64);
    let even = |v: f64| ((v.max(2.0) as usize) & !1).max(2);
    (even(pw), even(ph))
}

/// Draws camera frames for the camera's file: the picture without its
/// black bars, with the camera look (`look`), mirrored when the overlay
/// is, fitted into the file's fixed size (a crop that changes mid-take
/// never changes the file's size). It goes through CoreImage as the
/// compositor does, so the camera looks the same here as in the composed
/// video and the preview. Only the look's adjustments: preview guides
/// never reach a file.
pub struct CamRender {
    ctx: Retained<CIContext>,
    cs: CFRetained<CGColorSpace>,
    /// The camera look's filters; empty when it is neutral.
    adjust: Vec<crate::look::Step>,
    /// The last frame drawn, what it was drawn from, and how.
    last: Option<Drawn>,
}

struct Drawn {
    src: Option<CFRetained<CVPixelBuffer>>,
    bars: Bars,
    mirror: bool,
    out: CFRetained<CVPixelBuffer>,
}

// Used only by the sink, behind its lock.
unsafe impl Send for CamRender {}

fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
    CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
}

impl CamRender {
    pub fn new() -> Self {
        unsafe {
            let opts = dict(vec![(&*ns("kCIContextCacheIntermediates"), num_b(false))]);
            let ctx = CIContext::contextWithOptions(Some(&opts));
            let cs = CGColorSpace::with_name(Some(kCGColorSpaceSRGB)).expect("sRGB colorspace");
            CamRender { ctx, cs, adjust: vec![], last: None }
        }
    }

    /// Change the camera look live (the next frame is drawn with it). A
    /// neutral look adds no filter.
    pub fn set_adjust(&mut self, adjust: &crate::look::Adjust) {
        let steps = adjust.steps();
        if steps != self.adjust {
            self.adjust = steps;
            self.last = None;
        }
    }

    /// The file's frame for `cam` (a camera frame and its bars; None before
    /// the camera's first frame, drawn black), into a buffer from `alloc`.
    /// The camera runs slower than the file's frame rate, so most frames
    /// repeat the one before: those reuse the last buffer drawn.
    pub fn frame(
        &mut self,
        cam: Option<(&CFRetained<CVPixelBuffer>, Bars)>,
        mirror: bool,
        alloc: impl FnOnce() -> Option<CFRetained<CVPixelBuffer>>,
    ) -> Option<CFRetained<CVPixelBuffer>> {
        let same_src = |d: &Drawn| match (&d.src, cam) {
            (None, None) => true,
            (Some(a), Some((b, _))) => CFRetained::as_ptr(a) == CFRetained::as_ptr(b),
            _ => false,
        };
        if let Some(d) = &self.last
            && same_src(d)
            && d.bars == cam.map_or(Bars::NONE, |c| c.1)
            && d.mirror == mirror
        {
            return Some(d.out.clone());
        }
        let out = alloc()?;
        self.draw(cam.map(|(b, bars)| (&**b, bars)), mirror, &out);
        self.last = Some(Drawn { src: cam.map(|c| c.0.clone()), bars: cam.map_or(Bars::NONE, |c| c.1), mirror, out: out.clone() });
        Some(out)
    }

    fn draw(&self, cam: Option<(&CVPixelBuffer, Bars)>, mirror: bool, out: &CVPixelBuffer) {
        let (w, h) = (CVPixelBufferGetWidth(out) as f64, CVPixelBufferGetHeight(out) as f64);
        unsafe {
            let canvas = rect(0.0, 0.0, w, h);
            let mut img = CIImage::imageWithColor(&CIColor::colorWithRed_green_blue(0.0, 0.0, 0.0)).imageByCroppingToRect(canvas);
            if let Some((c, bars)) = cam {
                let mut pic = CIImage::imageWithCVPixelBuffer(c);
                if !bars.is_none() {
                    let e = pic.extent();
                    let (x, y, pw, ph) = bars.picture(e.size.width, e.size.height);
                    pic = pic.imageByCroppingToRect(rect(x, y, pw, ph));
                }
                if !self.adjust.is_empty() {
                    pic = crate::look::apply(&pic, &self.adjust);
                }
                let e = pic.extent();
                if e.size.width > 0.0 && e.size.height > 0.0 {
                    let (sx, sy, tx, ty) = crate::compose::fill(e.size.width, e.size.height, w, h);
                    let (a, tx) = if mirror { (-sx, w - tx) } else { (sx, tx) };
                    let t = CGAffineTransform { a, b: 0.0, c: 0.0, d: sy, tx: tx - a * e.origin.x, ty: ty - sy * e.origin.y };
                    img = pic.imageByApplyingTransform(t).imageByCroppingToRect(canvas).imageByCompositingOverImage(&img);
                }
            }
            self.ctx.render_toCVPixelBuffer_bounds_colorSpace(&img, out, canvas, Some(&self.cs));
        }
    }
}

impl Default for CamRender {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `w`x`h` BGRA frame painted by `f(x, y)`.
    fn paint(w: usize, h: usize, f: impl Fn(usize, usize) -> [u8; 4]) -> CFRetained<CVPixelBuffer> {
        use objc2_core_video::*;
        let pool = make_pool(w, h, PIX_BGRA);
        let mut b = std::ptr::null_mut();
        unsafe {
            CVPixelBufferPool::create_pixel_buffer(None, &pool, std::ptr::NonNull::from(&mut b));
            let b: CFRetained<CVPixelBuffer> = CFRetained::from_raw(std::ptr::NonNull::new(b).unwrap());
            CVPixelBufferLockBaseAddress(&b, CVPixelBufferLockFlags(0));
            let base = CVPixelBufferGetBaseAddress(&b) as *mut u8;
            let stride = CVPixelBufferGetBytesPerRow(&b);
            for y in 0..h {
                for x in 0..w {
                    std::ptr::copy_nonoverlapping(f(x, y).as_ptr(), base.add(y * stride + x * 4), 4);
                }
            }
            CVPixelBufferUnlockBaseAddress(&b, CVPixelBufferLockFlags(0));
            b
        }
    }

    /// An Osmo Pocket 3 on a portrait format: a 720x1280 frame with its
    /// 16:9 picture (720x405) in the middle, black above and below, and an
    /// orange disc in the picture's middle.
    fn letterboxed_cam() -> CFRetained<CVPixelBuffer> {
        let (top, ph) = (437, 405);
        paint(720, 1280, |x, y| {
            if y < top || y >= top + ph {
                return [0, 0, 0, 255];
            }
            let (u, v) = (x as f64 / 720.0, (y - top) as f64 / ph as f64);
            let (dx, dy) = ((u - 0.5) * 16.0 / 9.0, v - 0.5);
            if dx * dx + dy * dy < 0.09 {
                return [60, 150, 240, 255];
            }
            [(210.0 - 120.0 * v) as u8, (130.0 + 90.0 * v) as u8, (60.0 + 80.0 * u) as u8, 255]
        })
    }

    #[test]
    fn files_are_named_after_the_recording() {
        let [s, c] = paths(Path::new("/Users/me/Movies/rec-2026-11-24-183000.mov"));
        assert_eq!(s, Path::new("/Users/me/Movies/rec-2026-11-24-183000-screen.mov"));
        assert_eq!(c, Path::new("/Users/me/Movies/rec-2026-11-24-183000-camera.mov"));
        assert_eq!(paths(Path::new("talk.mp4")), [PathBuf::from("talk-screen.mp4"), PathBuf::from("talk-camera.mp4")]);
        assert_eq!(paths(Path::new("out/take.1.mov"))[1], Path::new("out/take.1-camera.mov"));
        assert_eq!(paths(Path::new("noext"))[0], Path::new("noext-screen"));
    }

    #[test]
    fn outputs_follow_the_files_setting() {
        let base = Path::new("/tmp/a.mov");
        assert_eq!(outputs(base, Files::Composed), [PathBuf::from("/tmp/a.mov")]);
        assert_eq!(outputs(base, Files::Separate), [PathBuf::from("/tmp/a-screen.mov"), PathBuf::from("/tmp/a-camera.mov")]);
        assert_eq!(outputs(base, Files::Both).len(), 3);
        assert_eq!(outputs(base, Files::Both)[0], base);
        assert_eq!(Files::from_flags(false, true), Files::Composed);
        assert_eq!(Files::from_flags(true, false), Files::Separate);
        assert_eq!(Files::from_flags(true, true), Files::Both);
        assert!(Files::Both.composed() && Files::Both.separate());
        assert!(!Files::Separate.composed() && !Files::Composed.separate());
    }

    #[test]
    fn a_recordings_files_are_found_from_any_of_them() {
        let dir = std::env::temp_dir().join(format!("rec-separate-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("rec-1.mov");
        let [screen, cam] = paths(&base);
        for p in [&screen, &cam] {
            std::fs::write(p, b"").unwrap();
        }
        assert_eq!(recording_files(&screen), [screen.clone(), cam.clone()]);
        assert_eq!(recording_files(&cam), [screen.clone(), cam.clone()]);
        std::fs::write(&base, b"").unwrap();
        assert_eq!(recording_files(&base), [base.clone(), screen.clone(), cam.clone()]);
        // A plain recording on its own.
        let other = dir.join("other.mov");
        assert_eq!(recording_files(&other), [other.clone()]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn camera_file_size_is_the_picture_without_bars() {
        assert_eq!(camera_size(None), (1280, 720));
        assert_eq!(camera_size(Some((1280, 720, Bars::NONE))), (1280, 720));
        assert_eq!(camera_size(Some((1920, 1080, Bars::NONE))), (1920, 1080));
        // An Osmo Pocket 3 on a portrait format, after the crop (2 px inset
        // into the picture): 720x401, rounded down to even.
        let osmo = Bars { top: 439, bottom: 440, left: 0, right: 0 };
        assert_eq!(camera_size(Some((720, 1280, osmo))), (720, 400));
        assert_eq!(camera_size(Some((1279, 719, Bars::NONE))), (1278, 718));
    }

    /// [B, G, R] of one pixel.
    fn pixel(b: &CVPixelBuffer, x: usize, y: usize) -> [u8; 3] {
        use objc2_core_video::*;
        unsafe {
            CVPixelBufferLockBaseAddress(b, CVPixelBufferLockFlags::ReadOnly);
            let p = (CVPixelBufferGetBaseAddress(b) as *const u8).add(y * CVPixelBufferGetBytesPerRow(b) + x * 4);
            let v = [*p, *p.add(1), *p.add(2)];
            CVPixelBufferUnlockBaseAddress(b, CVPixelBufferLockFlags::ReadOnly);
            v
        }
    }

    fn lum(v: [u8; 3]) -> u32 {
        (v[0] as u32 * 19 + v[1] as u32 * 183 + v[2] as u32 * 54) / 256
    }

    #[test]
    fn letterboxed_camera_fills_its_file_without_bars() {
        let cam = letterboxed_cam();
        let mut tracker = crate::bars::Tracker::default();
        let bars = (0..60).map(|i| tracker.frame(i as f64 / 30.0, &cam)).last().unwrap();
        let (w, h) = camera_size(Some((720, 1280, bars)));
        assert_eq!((w, h), (720, 400));
        let mut r = CamRender::new();
        let out = r.frame(Some((&cam, bars)), false, || Some(paint(w, h, |_, _| [0, 255, 0, 255]))).unwrap();
        // Picture on every edge, the orange disc in the middle.
        for (x, y) in [(w / 2, 0), (w / 2, h - 1), (0, h / 2), (w - 1, h / 2), (w / 2, h / 2)] {
            assert!(lum(pixel(&out, x, y)) > 60, "black at ({x}, {y}): {:?}", pixel(&out, x, y));
        }
        let mid = pixel(&out, w / 2, h / 2);
        assert!(mid[2] > 200 && (120..180).contains(&mid[1]) && mid[0] < 100, "middle {mid:?}");
    }

    /// Every byte of a BGRA frame.
    fn bytes(b: &CVPixelBuffer) -> Vec<u8> {
        use objc2_core_video::*;
        let (w, h) = (CVPixelBufferGetWidth(b), CVPixelBufferGetHeight(b));
        let mut out = Vec::with_capacity(w * h * 4);
        unsafe {
            CVPixelBufferLockBaseAddress(b, CVPixelBufferLockFlags::ReadOnly);
            let base = CVPixelBufferGetBaseAddress(b) as *const u8;
            for y in 0..h {
                out.extend_from_slice(std::slice::from_raw_parts(base.add(y * CVPixelBufferGetBytesPerRow(b)), w * 4));
            }
            CVPixelBufferUnlockBaseAddress(b, CVPixelBufferLockFlags::ReadOnly);
        }
        out
    }

    /// The camera look reaches the camera's file, a change redraws the
    /// frame on hand, and a neutral look is byte for byte no look at all.
    #[test]
    fn camera_look_is_drawn_and_neutral_is_a_no_op() {
        use crate::look::Adjust;
        let cam = paint(64, 36, |x, y| [(40 + x * 3) as u8, (60 + y * 4) as u8, (200 - x * 2) as u8, 255]);
        let mut r = CamRender::new();
        let draw =
            |r: &mut CamRender| bytes(&r.frame(Some((&cam, Bars::NONE)), false, || Some(paint(64, 36, |_, _| [0, 255, 0, 255]))).unwrap());
        let plain = draw(&mut r);
        r.set_adjust(&Adjust { temperature: 3200.0, ..Adjust::NEUTRAL });
        let cool = draw(&mut r);
        // 3200 K corrects for warm light: the picture turns bluer.
        let blue = |b: &[u8]| b.chunks(4).map(|p| p[0] as i64 - p[2] as i64).sum::<i64>();
        assert!(blue(&cool) > blue(&plain) + 64 * 36 * 10, "3200 K makes the camera file bluer");
        r.set_adjust(&Adjust { sharpness: 2.0, noise_reduction: 1.0, ..Adjust::NEUTRAL });
        assert_ne!(draw(&mut r), plain, "sharpness and noise reduction reach the file");
        r.set_adjust(&Adjust::NEUTRAL);
        assert!(draw(&mut r) == plain, "neutral is the same as no look");
    }

    #[test]
    fn mirrors_repeats_and_keeps_its_size() {
        // Red on the left half, blue on the right.
        let cam = paint(64, 36, |x, _| if x < 32 { [0, 0, 255, 255] } else { [255, 0, 0, 255] });
        let red = |v: [u8; 3]| v[2] > 200 && v[0] < 60;
        let blue = |v: [u8; 3]| v[0] > 200 && v[2] < 60;
        let mut r = CamRender::new();
        let allocs = std::cell::Cell::new(0);
        let alloc = || {
            allocs.set(allocs.get() + 1);
            Some(paint(64, 36, |_, _| [0, 255, 0, 255]))
        };
        let a = r.frame(Some((&cam, Bars::NONE)), false, &alloc).unwrap();
        assert!(red(pixel(&a, 4, 18)) && blue(pixel(&a, 60, 18)));
        // The same camera frame again: the same buffer, not drawn again.
        let b = r.frame(Some((&cam, Bars::NONE)), false, &alloc).unwrap();
        assert_eq!(CFRetained::as_ptr(&a), CFRetained::as_ptr(&b));
        let m = r.frame(Some((&cam, Bars::NONE)), true, &alloc).unwrap();
        assert!(blue(pixel(&m, 4, 18)) && red(pixel(&m, 60, 18)), "mirrored");
        assert_eq!(allocs.get(), 2);
        // A crop that appears mid-take scales into the same size.
        let bars = Bars { top: 0, bottom: 0, left: 16, right: 16 };
        let c = r.frame(Some((&cam, bars)), false, &alloc).unwrap();
        assert_eq!((CVPixelBufferGetWidth(&c), CVPixelBufferGetHeight(&c)), (64, 36));
        // Before the camera's first frame: black.
        let k = r.frame(None, false, &alloc).unwrap();
        assert!(lum(pixel(&k, 32, 18)) < 10);
    }
}
