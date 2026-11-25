//! GPU (CoreImage/Metal) compositing of the webcam over the screen.

use objc2::rc::Retained;
use objc2_core_foundation::{CFRetained, CGAffineTransform, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGColorSpace, kCGColorSpaceSRGB};
use objc2_core_image::{CIColor, CIContext, CIImage};
use objc2_core_video::CVPixelBuffer;

use crate::bars::Bars;
use crate::util::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Corner {
    BottomRight,
    BottomLeft,
    TopRight,
    TopLeft,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Shape {
    /// Rounded rectangle, camera aspect ratio (corner radius from roundness).
    Rounded,
    /// Circle (center square crop).
    Circle,
    /// Square, rounded by roundness (center square crop).
    Square,
    /// Capsule: camera aspect ratio, fully round ends.
    Pill,
    /// Superellipse square (continuous corners); roundness sets the curve.
    Squircle,
    /// Plain rectangle (same as rounded with roundness 0).
    Rect,
}

impl Shape {
    /// The shapes offered in the app.
    pub const PICKER: [Shape; 5] = [Shape::Circle, Shape::Rounded, Shape::Square, Shape::Pill, Shape::Squircle];

    pub fn label(self) -> &'static str {
        match self {
            Shape::Rounded => "Rounded",
            Shape::Circle => "Circle",
            Shape::Square => "Square",
            Shape::Pill => "Pill",
            Shape::Squircle => "Squircle",
            Shape::Rect => "Rectangle",
        }
    }

    /// Crops the camera to a centered square.
    pub fn is_square(self) -> bool {
        matches!(self, Shape::Circle | Shape::Square | Shape::Squircle)
    }

    /// The roundness slider applies.
    pub fn uses_roundness(self) -> bool {
        matches!(self, Shape::Rounded | Shape::Square | Shape::Squircle)
    }
}

/// What the output frame shows. Switchable live while the webcam runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Scene {
    /// The screen only.
    Screen,
    /// The screen with the webcam picture-in-picture.
    #[default]
    ScreenCam,
    /// The webcam full frame.
    Cam,
}

impl Scene {
    pub const ALL: [Scene; 3] = [Scene::Screen, Scene::ScreenCam, Scene::Cam];

    pub fn label(self) -> &'static str {
        match self {
            Scene::Screen => "Screen",
            Scene::ScreenCam => "Screen + Camera",
            Scene::Cam => "Camera",
        }
    }

    pub fn uses_cam(self) -> bool {
        self != Scene::Screen
    }
}

/// Uniform scale and offset that fits (`cover` false) or fills (`cover`
/// true) a `sw`x`sh` source into a `dw`x`dh` canvas, centred.
pub fn fit(sw: f64, sh: f64, dw: f64, dh: f64, cover: bool) -> (f64, f64, f64) {
    let (a, b) = (dw / sw, dh / sh);
    let s = if cover { a.max(b) } else { a.min(b) };
    (s, (dw - sw * s) / 2.0, (dh - sh * s) / 2.0)
}

/// Aspect ratios this close (relative) count as the same: output sizes are
/// rounded to even pixels, so a frame sized from its source differs from it
/// by a fraction of a pixel.
const SAME_ASPECT: f64 = 0.01;

pub fn same_aspect(sw: f64, sh: f64, dw: f64, dh: f64) -> bool {
    let (a, b) = (sw / sh, dw / dh);
    ((a - b) / b).abs() <= SAME_ASPECT
}

/// Scale (x, y) and offset that put a `sw`x`sh` source into a `dw`x`dh`
/// canvas edge to edge when their aspect ratios match up to rounding (a
/// sub-pixel stretch instead of a hairline bar), else fitted and centred.
pub fn fill(sw: f64, sh: f64, dw: f64, dh: f64) -> (f64, f64, f64, f64) {
    if same_aspect(sw, sh, dw, dh) {
        return (dw / sw, dh / sh, 0.0, 0.0);
    }
    let (s, tx, ty) = fit(sw, sh, dw, dh, false);
    (s, s, tx, ty)
}

/// Where the overlay sits: snapped to a corner (stays there when the size
/// changes) or centered at a point given as fractions of the frame from its
/// top-left corner.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Place {
    Corner(Corner),
    At([f64; 2]),
}

#[derive(Clone, Debug, PartialEq)]
pub struct PipSpec {
    /// Overlay width as a fraction of the output width.
    pub size: f64,
    pub place: Place,
    pub shape: Shape,
    /// Corner rounding, 0 (sharp) to 1 (as round as the shape allows).
    pub roundness: f64,
    /// Border width in pixels at 1080p (scaled with the output height).
    pub border: f64,
    pub border_color: [u8; 3],
    pub shadow: bool,
    pub mirror: bool,
}

impl Default for PipSpec {
    fn default() -> Self {
        PipSpec {
            size: 0.22,
            place: Place::Corner(Corner::BottomRight),
            shape: Shape::Rounded,
            roundness: 0.2,
            border: 0.0,
            border_color: [255, 255, 255],
            shadow: false,
            mirror: true,
        }
    }
}

/// Gap between a corner-snapped overlay and the frame edges.
pub fn margin(w: f64) -> f64 {
    (w * 0.02).round()
}

/// Overlay size in output pixels for a `w`x`h` output.
pub fn overlay_size(pip: &PipSpec, w: f64, h: f64, cam_aspect: f64) -> (f64, f64) {
    let aspect = if pip.shape.is_square() { 1.0 } else { cam_aspect.max(0.1) };
    let ow = (w * pip.size).round().clamp(16.0, w);
    let oh = (ow / aspect).round().clamp(16.0, h);
    (ow, oh)
}

/// Overlay rectangle `[x, y, width, height]` in output pixels, measured
/// from the top-left corner, always inside the frame.
pub fn overlay_rect(pip: &PipSpec, w: f64, h: f64, cam_aspect: f64) -> [f64; 4] {
    let (ow, oh) = overlay_size(pip, w, h, cam_aspect);
    let (x, y) = match pip.place {
        Place::Corner(c) => {
            let m = margin(w);
            let x = if matches!(c, Corner::TopLeft | Corner::BottomLeft) { m } else { w - m - ow };
            let y = if matches!(c, Corner::TopLeft | Corner::TopRight) { m } else { h - m - oh };
            (x, y)
        }
        Place::At([cx, cy]) => ((cx * w - ow / 2.0).round(), (cy * h - oh / 2.0).round()),
    };
    [x.clamp(0.0, w - ow), y.clamp(0.0, h - oh), ow, oh]
}

/// The corner whose snapped position is within `radius` (a fraction of
/// the output width) of the overlay's current center, if any.
pub fn snap_corner(pip: &PipSpec, w: f64, h: f64, cam_aspect: f64, radius: f64) -> Option<Corner> {
    let center = |p: &PipSpec| {
        let r = overlay_rect(p, w, h, cam_aspect);
        (r[0] + r[2] / 2.0, r[1] + r[3] / 2.0)
    };
    let c = center(pip);
    [Corner::TopLeft, Corner::TopRight, Corner::BottomLeft, Corner::BottomRight]
        .into_iter()
        .map(|k| {
            let q = center(&PipSpec { place: Place::Corner(k), ..pip.clone() });
            (k, ((q.0 - c.0).powi(2) + (q.1 - c.1).powi(2)).sqrt())
        })
        .filter(|(_, d)| *d <= radius * w)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(k, _)| k)
}

/// Superellipse exponent for a squircle: 2 (a circle) at roundness 1, 4
/// (the classic squircle) at 0.5, 10 (nearly square) at 0.
pub fn squircle_exponent(roundness: f64) -> f64 {
    2.0 + 8.0 * (1.0 - roundness.clamp(0.0, 1.0)).powi(2)
}

/// Anti-aliased coverage (0..1) of the pixel centered at (`px`, `py`) by
/// `shape` filling a `w`x`h` box.
pub fn coverage(shape: Shape, w: f64, h: f64, roundness: f64, px: f64, py: f64) -> f32 {
    let (hw, hh) = (w / 2.0, h / 2.0);
    let (x, y) = ((px - hw).abs(), (py - hh).abs());
    // Approximate signed distance to the edge in pixels (negative inside).
    let d = match shape {
        Shape::Squircle => {
            let n = squircle_exponent(roundness);
            (((x / hw).powf(n) + (y / hh).powf(n)).powf(1.0 / n) - 1.0) * hw.min(hh)
        }
        Shape::Circle => (((x / hw).powi(2) + (y / hh).powi(2)).sqrt() - 1.0) * hw.min(hh),
        _ => {
            let r = match shape {
                Shape::Pill => hw.min(hh),
                Shape::Rect => 0.0,
                _ => roundness.clamp(0.0, 1.0) * hw.min(hh),
            };
            let (qx, qy) = (x - (hw - r), y - (hh - r));
            (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r
        }
    };
    (0.5 - d).clamp(0.0, 1.0) as f32
}

/// Alpha mask (row-major, `w`x`h`) of the shape inset by `inset` pixels.
pub fn mask(shape: Shape, w: usize, h: usize, roundness: f64, inset: f64) -> Vec<u8> {
    let (iw, ih) = (w as f64 - 2.0 * inset, h as f64 - 2.0 * inset);
    let mut out = vec![0u8; w * h];
    if iw <= 0.0 || ih <= 0.0 {
        return out;
    }
    for y in 0..h {
        for x in 0..w {
            let c = coverage(shape, iw, ih, roundness, x as f64 + 0.5 - inset, y as f64 + 0.5 - inset);
            out[y * w + x] = (c * 255.0).round() as u8;
        }
    }
    out
}

/// Soft drop shadow: `alpha` padded by `pad` pixels on every side, blurred
/// with three box passes (close to a Gaussian) and scaled by `opacity`.
pub fn shadow(alpha: &[u8], w: usize, h: usize, pad: usize, opacity: f32) -> Vec<u8> {
    let (sw, sh) = (w + 2 * pad, h + 2 * pad);
    let mut a = vec![0f32; sw * sh];
    for y in 0..h {
        for x in 0..w {
            a[(y + pad) * sw + x + pad] = alpha[y * w + x] as f32 / 255.0;
        }
    }
    let r = (pad / 3).max(1);
    let mut tmp = vec![0f32; sw * sh];
    for _ in 0..3 {
        box_blur(&a, &mut tmp, sw, sh, r, true);
        box_blur(&tmp, &mut a, sw, sh, r, false);
    }
    a.iter().map(|v| (v * opacity * 255.0).round().clamp(0.0, 255.0) as u8).collect()
}

fn box_blur(src: &[f32], dst: &mut [f32], w: usize, h: usize, r: usize, horizontal: bool) {
    let (n, lines) = if horizontal { (w, h) } else { (h, w) };
    let at = |line: usize, i: usize| if horizontal { line * w + i } else { i * w + line };
    let k = (2 * r + 1) as f32;
    for line in 0..lines {
        let mut sum: f32 = (0..=r.min(n - 1)).map(|i| src[at(line, i)]).sum();
        for i in 0..n {
            dst[at(line, i)] = sum / k;
            if i + r + 1 < n {
                sum += src[at(line, i + r + 1)];
            }
            if i >= r {
                sum -= src[at(line, i - r)];
            }
        }
    }
}

/// Premultiplied BGRA image of `color` with per-pixel `alpha`.
pub fn alpha_image(alpha: &[u8], w: usize, h: usize, color: [u8; 3]) -> Retained<CIImage> {
    let mut px = Vec::with_capacity(w * h * 4);
    for &a in alpha {
        let m = |c: u8| ((c as u32 * a as u32 + 127) / 255) as u8;
        px.extend_from_slice(&[m(color[2]), m(color[1]), m(color[0]), a]);
    }
    unsafe {
        let data = objc2_foundation::NSData::with_bytes(&px);
        let cs = CGColorSpace::with_name(Some(kCGColorSpaceSRGB));
        CIImage::imageWithBitmapData_bytesPerRow_size_format_colorSpace(
            &data,
            w * 4,
            CGSize::new(w as f64, h as f64),
            objc2_core_image::kCIFormatBGRA8,
            cs.as_deref(),
        )
    }
}

type DecorKey = (usize, usize, Shape, u64, u64, [u8; 3], bool);

/// Static overlay layers for one size and style. They are built once on
/// the CPU and reused every frame; moving the overlay only changes a
/// transform, so dragging costs nothing.
struct Decor {
    key: DecorKey,
    /// Where the camera shows (inside the border, if any).
    mask: Retained<CIImage>,
    border: Option<Retained<CIImage>>,
    /// Shadow image and its padding in pixels.
    shadow: Option<(Retained<CIImage>, f64)>,
}

impl Decor {
    fn key(pip: &PipSpec, w: usize, h: usize, out_h: f64) -> DecorKey {
        let bw = border_px(pip, out_h);
        (w, h, pip.shape, pip.roundness.to_bits(), bw.to_bits(), pip.border_color, pip.shadow)
    }

    fn build(pip: &PipSpec, w: usize, h: usize, out_h: f64) -> Decor {
        let bw = border_px(pip, out_h);
        let outer = mask(pip.shape, w, h, pip.roundness, 0.0);
        let inner = if bw > 0.0 { mask(pip.shape, w, h, pip.roundness, bw) } else { outer.clone() };
        let shadow = pip.shadow.then(|| {
            let pad = (out_h * 0.03).round().max(3.0) as usize;
            let a = shadow(&outer, w, h, pad, 0.45);
            (alpha_image(&a, w + 2 * pad, h + 2 * pad, [0, 0, 0]), pad as f64)
        });
        Decor {
            key: Self::key(pip, w, h, out_h),
            mask: alpha_image(&inner, w, h, [255, 255, 255]),
            border: (bw > 0.0).then(|| alpha_image(&outer, w, h, pip.border_color)),
            shadow,
        }
    }
}

/// Border width in output pixels.
pub fn border_px(pip: &PipSpec, out_h: f64) -> f64 {
    (pip.border.max(0.0) * out_h / 1080.0).round()
}

/// Fills a window capture's transparent rounded corners. ScreenCaptureKit
/// delivers a single window with its corners (and any other uncovered
/// pixels) transparent, which a video frame shows as black. Behind the
/// window this draws its own edge pixels stretched outward (a clamped inset
/// of the frame), so each corner takes the color of the window chrome next
/// to it: the frame stays edge to edge with no black and nothing cropped.
pub struct CornerFill {
    ctx: Retained<CIContext>,
    cs: CFRetained<CGColorSpace>,
    pool: Option<((usize, usize), CFRetained<objc2_core_video::CVPixelBufferPool>)>,
}

/// How far in from each edge the fill samples: past the largest window
/// corner radius (about 26 points at 2x).
pub fn corner_inset(w: f64, h: f64) -> f64 {
    80.0f64.min(w / 4.0).min(h / 4.0).floor()
}

impl CornerFill {
    pub fn new() -> Self {
        unsafe {
            let opts = dict(vec![(&*ns("kCIContextCacheIntermediates"), num_b(false))]);
            let ctx = CIContext::contextWithOptions(Some(&opts));
            let cs = CGColorSpace::with_name(Some(kCGColorSpaceSRGB)).expect("sRGB colorspace");
            CornerFill { ctx, cs, pool: None }
        }
    }

    /// `pb` (BGRA) with its transparent pixels filled; None if it has no
    /// alpha to fill or a buffer could not be made.
    pub fn fill(&mut self, pb: &CVPixelBuffer) -> Option<CFRetained<CVPixelBuffer>> {
        use objc2_core_video::{CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth, CVPixelBufferPool};
        if CVPixelBufferGetPixelFormatType(pb) != PIX_BGRA {
            return None;
        }
        let (w, h) = (CVPixelBufferGetWidth(pb), CVPixelBufferGetHeight(pb));
        if self.pool.as_ref().is_none_or(|(sz, _)| *sz != (w, h)) {
            self.pool = Some(((w, h), make_pool(w, h, PIX_BGRA)));
        }
        let pool = &self.pool.as_ref()?.1;
        let mut out = std::ptr::null_mut();
        unsafe { CVPixelBufferPool::create_pixel_buffer(None, pool, std::ptr::NonNull::from(&mut out)) };
        let out: CFRetained<CVPixelBuffer> = unsafe { CFRetained::from_raw(std::ptr::NonNull::new(out)?) };
        unsafe {
            let img = CIImage::imageWithCVPixelBuffer(pb);
            let (fw, fh) = (w as f64, h as f64);
            let i = corner_inset(fw, fh);
            let canvas = rect(0.0, 0.0, fw, fh);
            let back = img.imageByCroppingToRect(rect(i, i, fw - 2.0 * i, fh - 2.0 * i)).imageByClampingToExtent().imageByCroppingToRect(canvas);
            let filled = img.imageByCompositingOverImage(&back);
            self.ctx.render_toCVPixelBuffer_bounds_colorSpace(&filled, &out, canvas, Some(&self.cs));
        }
        Some(out)
    }
}

impl Default for CornerFill {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Compositor {
    ctx: Retained<CIContext>,
    cs: CFRetained<CGColorSpace>,
    w: f64,
    h: f64,
    pip: PipSpec,
    decor: Option<Decor>,
    /// Aspect ratio of the last camera frame (16:9 until one arrives).
    cam_aspect: f64,
    /// Software adjustments to the camera picture (`look`); empty when
    /// they are neutral.
    adjust: Vec<crate::look::Step>,
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
    CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
}

fn translate(img: &CIImage, x: f64, y: f64) -> Retained<CIImage> {
    unsafe { img.imageByApplyingTransform(CGAffineTransform { a: 1.0, b: 0.0, c: 0.0, d: 1.0, tx: x, ty: y }) }
}

impl Compositor {
    pub fn new(width: usize, height: usize, pip: PipSpec) -> Self {
        unsafe {
            let opts = dict(vec![(&*ns("kCIContextCacheIntermediates"), num_b(false))]);
            let ctx = CIContext::contextWithOptions(Some(&opts));
            let cs = CGColorSpace::with_name(Some(kCGColorSpaceSRGB)).expect("sRGB colorspace");
            Compositor { ctx, cs, w: width as f64, h: height as f64, pip, decor: None, cam_aspect: 16.0 / 9.0, adjust: vec![] }
        }
    }

    /// Change the overlay live (position, shape, size, style).
    pub fn set_pip(&mut self, pip: PipSpec) {
        self.pip = pip;
    }

    pub fn cam_aspect(&self) -> f64 {
        self.cam_aspect
    }

    /// Change the camera's software adjustments live. Neutral ones remove
    /// every filter.
    pub fn set_adjust(&mut self, adjust: &crate::look::Adjust) {
        self.adjust = adjust.steps();
    }

    fn decor(&mut self, w: usize, h: usize) -> &Decor {
        let key = Decor::key(&self.pip, w, h, self.h);
        if self.decor.as_ref().is_none_or(|d| d.key != key) {
            self.decor = Some(Decor::build(&self.pip, w, h, self.h));
        }
        self.decor.as_ref().unwrap()
    }

    /// Draw one output frame. `cam` comes with the black bars to crop off
    /// it (see `bars`).
    pub fn render(&mut self, scene: Scene, screen: Option<&CVPixelBuffer>, cam: Option<(&CVPixelBuffer, Bars)>, out: &CVPixelBuffer) {
        unsafe {
            let canvas = rect(0.0, 0.0, self.w, self.h);
            let black = CIImage::imageWithColor(&CIColor::colorWithRed_green_blue(0.0, 0.0, 0.0)).imageByCroppingToRect(canvas);
            // The camera's picture without its bars, then adjusted. Its
            // extent's origin is then off zero, and the placements below
            // account for that.
            let cam = cam.map(|(c, bars)| {
                let mut img = CIImage::imageWithCVPixelBuffer(c);
                if !bars.is_none() {
                    let e = img.extent();
                    let (x, y, w, h) = bars.picture(e.size.width, e.size.height);
                    img = img.imageByCroppingToRect(rect(x, y, w, h));
                }
                if self.adjust.is_empty() { img } else { crate::look::apply(&img, &self.adjust) }
            });
            if let Some(c) = &cam {
                let e = c.extent();
                if e.size.height > 0.0 {
                    self.cam_aspect = e.size.width / e.size.height;
                }
            }
            if scene == Scene::Cam {
                let mut img = black;
                if let Some(c) = cam {
                    let e = c.extent();
                    let (s, tx, ty) = fit(e.size.width, e.size.height, self.w, self.h, true);
                    let (a, tx) = if self.pip.mirror { (-s, self.w - tx) } else { (s, tx) };
                    let t = CGAffineTransform { a, b: 0.0, c: 0.0, d: s, tx: tx - a * e.origin.x, ty: ty - s * e.origin.y };
                    img = c.imageByApplyingTransform(t).imageByCroppingToRect(canvas).imageByCompositingOverImage(&img);
                }
                self.ctx.render_toCVPixelBuffer_bounds_colorSpace(&img, out, canvas, Some(&self.cs));
                return;
            }
            let mut img = black;
            if let Some(screen) = screen {
                let sc = CIImage::imageWithCVPixelBuffer(screen);
                let se = sc.extent();
                // Fit the screen into the canvas (a no-op normally: the
                // sink scales native-size captures to the output size).
                let (sx, sy, tx, ty) = fill(se.size.width, se.size.height, self.w, self.h);
                img = sc.imageByApplyingTransform(CGAffineTransform { a: sx, b: 0.0, c: 0.0, d: sy, tx, ty }).imageByCompositingOverImage(&img);
            }
            if scene == Scene::ScreenCam && let Some(c) = cam {
                let [x, top, tw, th] = overlay_rect(&self.pip, self.w, self.h, self.cam_aspect);
                // CoreImage's origin is bottom-left.
                let y = self.h - top - th;
                let e = c.extent();
                // Crop the camera to the overlay's aspect ratio, centered.
                let (s, ox, oy) = fit(e.size.width, e.size.height, tw, th, true);
                let (a, tx) = if self.pip.mirror { (-s, x + tw - ox) } else { (s, x + ox) };
                let t = CGAffineTransform { a, b: 0.0, c: 0.0, d: s, tx: tx - a * e.origin.x, ty: y + oy - s * e.origin.y };
                let target = rect(x, y, tw, th);
                let placed = c.imageByApplyingTransform(t).imageByCroppingToRect(target);
                // Drop the shadow slightly below the overlay.
                let drop = (self.h * 0.006).round();
                let decor = self.decor(tw as usize, th as usize);
                if let Some((sh, pad)) = &decor.shadow {
                    img = translate(sh, x - pad, y - pad - drop).imageByCompositingOverImage(&img);
                }
                if let Some(b) = &decor.border {
                    img = translate(b, x, y).imageByCompositingOverImage(&img);
                }
                let params = dict(vec![(&*ns("inputBackgroundImage"), obj(&*translate(&decor.mask, x, y)))]);
                let overlay = placed.imageByApplyingFilter_withInputParameters(&ns("CISourceInCompositing"), &params);
                img = overlay.imageByCompositingOverImage(&img);
            }
            self.ctx.render_toCVPixelBuffer_bounds_colorSpace(&img, out, canvas, Some(&self.cs));
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn fit_letterboxes_and_cover_crops() {
        // 4:3 camera into a 16:9 canvas.
        let (s, tx, ty) = fit(1280.0, 960.0, 1920.0, 1080.0, false);
        assert_eq!((s, tx, ty), (1.125, 240.0, 0.0));
        let (s, tx, ty) = fit(1280.0, 960.0, 1920.0, 1080.0, true);
        assert_eq!((s, tx, ty), (1.5, 0.0, -180.0));
    }

    #[test]
    fn fill_is_edge_to_edge_for_the_same_aspect() {
        // A 4112x2658 display into its 1670x1080 output: a sub-pixel stretch,
        // no offset, so no hairline bar on any edge.
        let (sx, sy, tx, ty) = fill(4112.0, 2658.0, 1670.0, 1080.0);
        assert_eq!((tx, ty), (0.0, 0.0));
        assert!((4112.0 * sx - 1670.0).abs() < 1e-9 && (2658.0 * sy - 1080.0).abs() < 1e-9);
        // A different shape (a window resized mid-recording) is fitted.
        let (sx, sy, tx, ty) = fill(1280.0, 960.0, 1920.0, 1080.0);
        assert_eq!((sx, sy, tx, ty), (1.125, 1.125, 240.0, 0.0));
        assert_eq!(corner_inset(4086.0, 2464.0), 80.0);
        assert_eq!(corner_inset(200.0, 100.0), 25.0);
    }

    fn spec(shape: Shape, place: Place) -> PipSpec {
        PipSpec { shape, place, ..Default::default() }
    }

    #[test]
    fn corners_and_free_positions() {
        let a = 16.0 / 9.0;
        let p = spec(Shape::Rounded, Place::Corner(Corner::BottomRight));
        // 22% of 1920 = 422 wide, 237 tall, 38 px margin.
        assert_eq!(overlay_rect(&p, 1920.0, 1080.0, a), [1460.0, 805.0, 422.0, 237.0]);
        let p = spec(Shape::Circle, Place::Corner(Corner::TopLeft));
        assert_eq!(overlay_rect(&p, 1920.0, 1080.0, a), [38.0, 38.0, 422.0, 422.0]);
        let p = spec(Shape::Rounded, Place::At([0.5, 0.5]));
        assert_eq!(overlay_rect(&p, 1920.0, 1080.0, a), [749.0, 422.0, 422.0, 237.0]);
        // Dragged past the edge: kept inside the frame.
        let p = spec(Shape::Square, Place::At([1.2, -0.3]));
        assert_eq!(overlay_rect(&p, 1920.0, 1080.0, a), [1498.0, 0.0, 422.0, 422.0]);
    }

    #[test]
    fn corner_placement_survives_resize() {
        let mut p = spec(Shape::Rounded, Place::Corner(Corner::BottomRight));
        p.size = 0.4;
        let r = overlay_rect(&p, 1920.0, 1080.0, 16.0 / 9.0);
        assert_eq!((r[0] + r[2], r[1] + r[3]), (1920.0 - 38.0, 1080.0 - 38.0));
    }

    #[test]
    fn snapping() {
        let a = 16.0 / 9.0;
        let near = spec(Shape::Rounded, Place::At([0.85, 0.82]));
        assert_eq!(snap_corner(&near, 1920.0, 1080.0, a, 0.06), Some(Corner::BottomRight));
        let middle = spec(Shape::Rounded, Place::At([0.5, 0.5]));
        assert_eq!(snap_corner(&middle, 1920.0, 1080.0, a, 0.06), None);
        let top_left = spec(Shape::Circle, Place::At([0.1, 0.18]));
        assert_eq!(snap_corner(&top_left, 1920.0, 1080.0, a, 0.06), Some(Corner::TopLeft));
    }

    #[test]
    fn shape_coverage() {
        let cov = |s, r, x, y| coverage(s, 100.0, 100.0, r, x, y);
        for s in Shape::PICKER {
            assert_eq!(cov(s, 0.5, 50.0, 50.0), 1.0, "{s:?} center");
            assert_eq!(cov(s, 0.5, 50.0, 0.5), 1.0, "{s:?} top edge middle");
        }
        // The corner pixel: covered only by sharp shapes.
        assert_eq!(cov(Shape::Rect, 0.0, 0.5, 0.5), 1.0);
        assert_eq!(cov(Shape::Rounded, 0.0, 0.5, 0.5), 1.0, "roundness 0 is a rectangle");
        assert_eq!(cov(Shape::Rounded, 0.5, 0.5, 0.5), 0.0);
        assert_eq!(cov(Shape::Circle, 0.0, 0.5, 0.5), 0.0);
        assert_eq!(cov(Shape::Squircle, 0.5, 0.5, 0.5), 0.0);
        // The squircle is fuller than a circle at the diagonal (0.8, 0.8).
        assert_eq!(cov(Shape::Circle, 0.5, 90.0, 90.0), 0.0);
        assert_eq!(cov(Shape::Squircle, 0.5, 90.0, 90.0), 1.0);
        // Roundness 1 on a square equals a circle.
        assert_eq!(cov(Shape::Square, 1.0, 90.0, 90.0), 0.0);
        // Anti-aliased edge.
        let edge = cov(Shape::Circle, 0.5, 100.0, 50.0);
        assert!(edge > 0.0 && edge < 1.0, "edge {edge}");
    }

    #[test]
    fn pill_has_round_ends() {
        let cov = |x, y| coverage(Shape::Pill, 200.0, 100.0, 0.0, x, y);
        assert_eq!(cov(5.0, 50.0), 1.0);
        assert_eq!(cov(5.0, 5.0), 0.0);
        assert_eq!(cov(100.0, 1.0), 1.0);
        assert_eq!(cov(195.0, 95.0), 0.0);
    }

    #[test]
    fn border_inset_mask() {
        let outer = mask(Shape::Square, 40, 40, 0.0, 0.0);
        let inner = mask(Shape::Square, 40, 40, 0.0, 4.0);
        assert_eq!((outer[2 * 40 + 20], inner[2 * 40 + 20]), (255, 0), "border band");
        assert_eq!((outer[20 * 40 + 20], inner[20 * 40 + 20]), (255, 255));
    }

    #[test]
    fn shadow_spreads_beyond_the_shape() {
        let a = mask(Shape::Square, 20, 20, 0.0, 0.0);
        let s = shadow(&a, 20, 20, 9, 0.5);
        let w = 38;
        assert!(s[19 * w + 19] > 100, "under the shape");
        assert!(s[19 * w + 4] > 0, "outside the shape");
        assert_eq!(s[0], 0, "far corner");
        assert!(s.iter().all(|&v| v <= 128));
    }

    /// Bake an overlay with CoreImage and read pixels back.
    #[test]
    fn renders_overlay_with_border_into_the_frame() {
        use objc2_core_video::*;
        let solid = |w: usize, h: usize, bgra: [u8; 4]| {
            let pb = crate::util::make_pool(w, h, PIX_BGRA);
            let mut buf = std::ptr::null_mut();
            unsafe {
                CVPixelBufferPool::create_pixel_buffer(None, &pb, std::ptr::NonNull::from(&mut buf));
                let b: CFRetained<CVPixelBuffer> = CFRetained::from_raw(std::ptr::NonNull::new(buf).unwrap());
                CVPixelBufferLockBaseAddress(&b, CVPixelBufferLockFlags(0));
                let base = CVPixelBufferGetBaseAddress(&b) as *mut u8;
                let stride = CVPixelBufferGetBytesPerRow(&b);
                for y in 0..h {
                    for x in 0..w {
                        std::ptr::copy_nonoverlapping(bgra.as_ptr(), base.add(y * stride + x * 4), 4);
                    }
                }
                CVPixelBufferUnlockBaseAddress(&b, CVPixelBufferLockFlags(0));
                (b, pb)
            }
        };
        let (screen, _p1) = solid(640, 360, [0, 0, 0, 255]);
        let (cam, _p2) = solid(320, 180, [0, 0, 255, 255]); // red
        let (out, _p3) = solid(640, 360, [0, 255, 0, 255]);
        let pip = PipSpec {
            size: 0.25,
            place: Place::At([0.5, 0.25]),
            shape: Shape::Square,
            roundness: 0.3,
            border: 12.0,
            border_color: [255, 255, 255],
            ..Default::default()
        };
        let mut c = Compositor::new(640, 360, pip.clone());
        c.render(Scene::ScreenCam, Some(&screen), Some((&cam, Bars::NONE)), &out);
        let [x, y, w, h] = overlay_rect(&pip, 640.0, 360.0, c.cam_aspect());
        assert_eq!([x, y, w, h], [240.0, 10.0, 160.0, 160.0]);
        let px = |px: usize, py: usize| unsafe {
            CVPixelBufferLockBaseAddress(&out, CVPixelBufferLockFlags::ReadOnly);
            let base = CVPixelBufferGetBaseAddress(&out) as *const u8;
            let p = base.add(py * CVPixelBufferGetBytesPerRow(&out) + px * 4);
            let v = [*p, *p.add(1), *p.add(2)];
            CVPixelBufferUnlockBaseAddress(&out, CVPixelBufferLockFlags::ReadOnly);
            v
        };
        let red = |v: [u8; 3]| v[2] > 200 && v[1] < 40 && v[0] < 40;
        let white = |v: [u8; 3]| v.iter().all(|&c| c > 200);
        let black = |v: [u8; 3]| v.iter().all(|&c| c < 30);
        assert!(red(px(320, 90)), "camera at the overlay center: {:?}", px(320, 90));
        // 12 px border at 1080p is 4 px at 360p: just inside the top edge.
        assert!(white(px(320, 11)), "border at the top edge: {:?}", px(320, 11));
        assert!(black(px(241, 11)), "rounded corner shows the screen: {:?}", px(241, 11));
        assert!(black(px(320, 200)), "screen below the overlay: {:?}", px(320, 200));
        // Moving the overlay is live.
        c.set_pip(PipSpec { place: Place::Corner(Corner::BottomLeft), ..pip });
        c.render(Scene::ScreenCam, Some(&screen), Some((&cam, Bars::NONE)), &out);
        assert!(black(px(320, 90)), "old position is screen again");
        assert!(red(px(13 + 80, 360 - 13 - 80)), "new position: {:?}", px(93, 267));
    }

    /// A `w`x`h` BGRA frame painted by `f(x, y)`.
    pub(crate) fn paint(w: usize, h: usize, f: impl Fn(usize, usize) -> [u8; 4]) -> CFRetained<CVPixelBuffer> {
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

    /// With REC_EVIDENCE_DIR set, save `b` there as `<name>.png` (needs ffmpeg).
    pub(crate) fn save_png(b: &CVPixelBuffer, name: &str) {
        use objc2_core_video::*;
        let Some(dir) = std::env::var_os("REC_EVIDENCE_DIR") else { return };
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (w, h) = (CVPixelBufferGetWidth(b), CVPixelBufferGetHeight(b));
        let mut raw = Vec::with_capacity(w * h * 4);
        unsafe {
            CVPixelBufferLockBaseAddress(b, CVPixelBufferLockFlags::ReadOnly);
            let base = CVPixelBufferGetBaseAddress(b) as *const u8;
            for y in 0..h {
                raw.extend_from_slice(std::slice::from_raw_parts(base.add(y * CVPixelBufferGetBytesPerRow(b)), w * 4));
            }
            CVPixelBufferUnlockBaseAddress(b, CVPixelBufferLockFlags::ReadOnly);
        }
        let tmp = dir.join(format!("{name}.bgra"));
        std::fs::write(&tmp, raw).unwrap();
        let ok = std::process::Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-f", "rawvideo", "-pix_fmt", "bgra", "-s", &format!("{w}x{h}"), "-i"])
            .arg(&tmp)
            .arg(dir.join(format!("{name}.png")))
            .status()
            .is_ok_and(|s| s.success());
        std::fs::remove_file(&tmp).unwrap();
        assert!(ok, "ffmpeg could not write {name}.png");
    }

    /// Every BGRA byte of `b`, row by row without padding.
    fn bytes(b: &CVPixelBuffer) -> Vec<u8> {
        use objc2_core_video::*;
        let (w, h) = (CVPixelBufferGetWidth(b), CVPixelBufferGetHeight(b));
        let mut v = Vec::with_capacity(w * h * 4);
        unsafe {
            CVPixelBufferLockBaseAddress(b, CVPixelBufferLockFlags::ReadOnly);
            let base = CVPixelBufferGetBaseAddress(b) as *const u8;
            for y in 0..h {
                v.extend_from_slice(std::slice::from_raw_parts(base.add(y * CVPixelBufferGetBytesPerRow(b)), w * 4));
            }
            CVPixelBufferUnlockBaseAddress(b, CVPixelBufferLockFlags::ReadOnly);
        }
        v
    }

    /// A camera frame with color, detail and edges: a gradient with stripes.
    pub(crate) fn busy_cam(w: usize, h: usize) -> CFRetained<CVPixelBuffer> {
        paint(w, h, |x, y| {
            let stripe = if (x / 3) % 2 == 0 { 30 } else { 0 };
            [(40 + y * 150 / h) as u8, (60 + stripe + x * 100 / w) as u8, (200 - x * 120 / w) as u8, 255]
        })
    }

    /// Neutral adjustments are a true no-op: the frame is byte for byte what
    /// it was before adjustments existed, including after turning them off.
    #[test]
    fn neutral_adjustments_leave_the_frame_untouched() {
        use crate::look::Adjust;
        let (w, h) = (640, 360);
        let screen = paint(w, h, |x, _| [x as u8, 90, 160, 255]);
        let cam = busy_cam(320, 180);
        for scene in [Scene::ScreenCam, Scene::Cam] {
            let plain = paint(w, h, |_, _| [0, 255, 0, 255]);
            Compositor::new(w, h, PipSpec::default()).render(scene, Some(&screen), Some((&cam, Bars::NONE)), &plain);
            let mut c = Compositor::new(w, h, PipSpec::default());
            c.set_adjust(&Adjust { temperature: 3200.0, sharpness: 2.0, noise_reduction: 1.0, ..Adjust::NEUTRAL });
            let adjusted = paint(w, h, |_, _| [0, 255, 0, 255]);
            c.render(scene, Some(&screen), Some((&cam, Bars::NONE)), &adjusted);
            assert_ne!(bytes(&plain), bytes(&adjusted), "{scene:?}: the adjustments do something");
            c.set_adjust(&Adjust::NEUTRAL);
            let back = paint(w, h, |_, _| [0, 255, 0, 255]);
            c.render(scene, Some(&screen), Some((&cam, Bars::NONE)), &back);
            assert!(bytes(&plain) == bytes(&back), "{scene:?}: neutral is identical to no adjustments");
        }
    }

    /// The adjustments reach the composited frame (the one the preview
    /// shows and the file records), in the expected direction.
    #[test]
    fn adjustments_change_the_composited_frame() {
        use crate::look::Adjust;
        let (w, h) = (320, 180);
        let gray = paint(160, 90, |_, _| [128, 128, 128, 255]);
        let center = |a: Adjust, cam: &CVPixelBuffer| {
            let out = paint(w, h, |_, _| [0, 0, 0, 255]);
            let mut c = Compositor::new(w, h, PipSpec::default());
            c.set_adjust(&a);
            c.render(Scene::Cam, None, Some((cam, Bars::NONE)), &out);
            pixel(&out, w / 2, h / 2)
        };
        let n = center(Adjust::NEUTRAL, &gray);
        assert!(n.iter().all(|&v| (125..=131).contains(&v)), "neutral gray: {n:?}");
        // White balance set lower than the light: cooler (blue over red).
        let cool = center(Adjust { temperature: 3200.0, ..Adjust::NEUTRAL }, &gray);
        assert!(cool[0] as i32 - cool[2] as i32 > 40, "3200 K: {cool:?}");
        let warm = center(Adjust { temperature: 9000.0, ..Adjust::NEUTRAL }, &gray);
        assert!(warm[2] as i32 - warm[0] as i32 > 20, "9000 K: {warm:?}");
        let magenta = center(Adjust { tint: 60.0, ..Adjust::NEUTRAL }, &gray);
        assert!(magenta[1] + 10 < magenta[0] && magenta[1] + 10 < magenta[2], "tint +60: {magenta:?}");
        // A stop up is brighter, a stop down darker.
        let up = center(Adjust { exposure: 1.0, ..Adjust::NEUTRAL }, &gray);
        let down = center(Adjust { exposure: -1.0, ..Adjust::NEUTRAL }, &gray);
        assert!(up[1] > 160 && down[1] < 100, "+1 EV {up:?}, -1 EV {down:?}");
        let bright = center(Adjust { brightness: 50.0, ..Adjust::NEUTRAL }, &gray);
        assert!(bright[1] > 140, "brightness +50: {bright:?}");
        // No saturation turns color gray.
        let orange = paint(160, 90, |_, _| [40, 140, 240, 255]);
        let flat = center(Adjust { saturation: -100.0, ..Adjust::NEUTRAL }, &orange);
        let spread = flat.iter().max().unwrap() - flat.iter().min().unwrap();
        assert!(spread <= 3, "saturation -100: {flat:?}");
        let more = center(Adjust { contrast: 100.0, ..Adjust::NEUTRAL }, &paint(160, 90, |_, _| [200, 200, 200, 255]));
        assert!(more[1] > 205, "contrast +100 brightens light gray: {more:?}");
        // With REC_EVIDENCE_DIR: the test card through each setting.
        let card = letterboxed_cam();
        let mut tracker = crate::bars::Tracker::default();
        let bars = (0..60).map(|i| tracker.frame(i as f64 / 30.0, &card)).last().unwrap();
        for (name, a) in [
            ("adjust-neutral", Adjust::NEUTRAL),
            ("adjust-3200k", Adjust { temperature: 3200.0, ..Adjust::NEUTRAL }),
            ("adjust-8000k", Adjust { temperature: 8000.0, ..Adjust::NEUTRAL }),
            ("adjust-osmo-studio-5000k", crate::settings::CameraLook::default().osmo_studio(5000.0).adjust),
            ("adjust-exposure-plus-1", Adjust { exposure: 1.0, ..Adjust::NEUTRAL }),
            ("adjust-saturation-minus-100", Adjust { saturation: -100.0, ..Adjust::NEUTRAL }),
            ("adjust-sharpness-minus-2", Adjust { sharpness: -2.0, ..Adjust::NEUTRAL }),
        ] {
            let out = paint(640, 360, |_, _| [0, 0, 0, 255]);
            let mut c = Compositor::new(640, 360, PipSpec { mirror: false, ..Default::default() });
            c.set_adjust(&a);
            c.render(Scene::Cam, None, Some((&card, bars)), &out);
            save_png(&out, name);
        }
    }

    /// Softening blurs edges and sharpening steepens them, without
    /// darkening the picture's border (the blur is clamped to the picture).
    #[test]
    fn sharpness_both_ways() {
        use crate::look::Adjust;
        // 1:1, at 720p, so the blur isn't hidden by scaling.
        let (w, h) = (1280, 720);
        // Vertical black and white bars 8 px wide.
        let bars_cam = paint(w, h, |x, _| if (x / 8) % 2 == 0 { [30, 30, 30, 255] } else { [220, 220, 220, 255] });
        let row = |a: Adjust| {
            let out = paint(w, h, |_, _| [0, 0, 0, 255]);
            let mut c = Compositor::new(w, h, PipSpec { mirror: false, ..Default::default() });
            c.set_adjust(&a);
            c.render(Scene::Cam, None, Some((&bars_cam, Bars::NONE)), &out);
            (0..w).map(|x| pixel(&out, x, h / 2)[1] as i32).collect::<Vec<i32>>()
        };
        // Sum of steps between neighbors: higher is crisper.
        let edges = |r: &[i32]| r.windows(2).map(|p| (p[1] - p[0]).abs()).max().unwrap();
        let (plain, soft, sharp) =
            (row(Adjust::NEUTRAL), row(Adjust { sharpness: -2.0, ..Adjust::NEUTRAL }), row(Adjust { sharpness: 2.0, ..Adjust::NEUTRAL }));
        assert!(edges(&soft) < edges(&plain), "soft {} vs {}", edges(&soft), edges(&plain));
        // Sharpening overshoots: darker darks or brighter brights near edges.
        let range = |r: &[i32]| r.iter().max().unwrap() - r.iter().min().unwrap();
        assert!(range(&sharp) > range(&plain), "sharp {} vs {}", range(&sharp), range(&plain));
        // The picture's edge columns keep their value when softened.
        assert!(
            (soft[0] - plain[0]).abs() <= 12 && (soft[w - 1] - plain[w - 1]).abs() <= 40,
            "{} {} / {} {}",
            soft[0],
            plain[0],
            soft[w - 1],
            plain[w - 1]
        );
        let denoised = row(Adjust { noise_reduction: 2.0, ..Adjust::NEUTRAL });
        assert!((denoised[w / 2] - plain[w / 2]).abs() <= 40, "noise reduction keeps flat areas");
    }

    /// Zebras and the grid are preview guides. A camera look with both on
    /// gives the compositor (which draws the frame the file, the stream
    /// and the preview all get) only its adjustments, and the frame comes
    /// out byte for byte as with them off; and the recording path's code
    /// doesn't know the guides exist.
    #[test]
    fn preview_guides_never_reach_the_recording() {
        use crate::settings::CameraLook;
        let guides = CameraLook::default().osmo_studio(5600.0);
        assert!(guides.zebras && guides.grid);
        let plain = CameraLook { zebras: false, grid: false, ..guides.clone() };
        // A blown-out camera: zebras would stripe all of it.
        let cam = paint(320, 180, |_, _| [255, 255, 255, 255]);
        let screen = paint(640, 360, |_, _| [250, 250, 250, 255]);
        for scene in [Scene::Cam, Scene::ScreenCam] {
            let render = |look: &CameraLook| {
                let out = paint(640, 360, |_, _| [0, 255, 0, 255]);
                let mut c = Compositor::new(640, 360, PipSpec::default());
                c.set_adjust(&look.adjust);
                c.render(scene, Some(&screen), Some((&cam, Bars::NONE)), &out);
                bytes(&out)
            };
            let with = render(&guides);
            assert!(with == render(&plain), "{scene:?}: the guides change nothing");
            if scene == Scene::Cam {
                assert!(with.chunks(4).all(|p| p[..3].iter().all(|&v| v >= 250)), "no stripes: the frame stays white");
            }
        }
        // The code that draws recorded and streamed frames.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files: Vec<std::path::PathBuf> =
            ["compose.rs", "writer.rs", "record.rs", "look.rs", "scale.rs", "bars.rs", "camera.rs", "separate.rs"]
                .iter()
                .map(|f| dir.join(f))
                .collect();
        files.extend(std::fs::read_dir(dir.join("live")).unwrap().map(|e| e.unwrap().path()));
        for f in files {
            let text = std::fs::read_to_string(&f).unwrap();
            // Up to its tests (this test names the guides).
            let code = text.split("#[cfg(test)]").next().unwrap().to_lowercase();
            for word in ["zebra", "cameralook", ".grid", "set_zebras", "thirds("] {
                assert!(!code.contains(word), "{} mentions {word:?}", f.display());
            }
        }
    }

    /// What the camera adjustments cost the compositor at 1080p (run with
    /// --ignored --nocapture, in release): wall time per frame for 240
    /// frames, neutral against the Osmo studio preset and everything on.
    #[test]
    #[ignore]
    fn adjustment_cost() {
        use crate::look::Adjust;
        fn cpu() -> f64 {
            let mut u: libc::rusage = unsafe { std::mem::zeroed() };
            unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
            let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
            tv(u.ru_utime) + tv(u.ru_stime)
        }
        let (w, h) = (1920, 1080);
        let screen = paint(w, h, |x, y| [(x % 256) as u8, (y % 256) as u8, 128, 255]);
        let cam = busy_cam(1920, 1080);
        let everything = Adjust {
            temperature: 5000.0,
            tint: 5.0,
            exposure: 0.3,
            brightness: 5.0,
            contrast: 10.0,
            saturation: -10.0,
            sharpness: 1.0,
            noise_reduction: 1.0,
        };
        let cases = [
            ("neutral", Adjust::NEUTRAL),
            ("Osmo studio 5600K (soften)", Adjust { sharpness: -1.0, ..Adjust::NEUTRAL }),
            ("color only (WB, EV, contrast, saturation)", Adjust { sharpness: 0.0, noise_reduction: 0.0, ..everything }),
            ("everything (with sharpen and denoise)", everything),
        ];
        let pool = make_pool(w, h, PIX_BGRA);
        for scene in [Scene::Cam, Scene::ScreenCam] {
            for (name, a) in &cases {
                let mut c = Compositor::new(w, h, PipSpec::default());
                c.set_adjust(a);
                let frame = |c: &mut Compositor| {
                    let mut b = std::ptr::null_mut();
                    unsafe { objc2_core_video::CVPixelBufferPool::create_pixel_buffer(None, &pool, std::ptr::NonNull::from(&mut b)) };
                    let b: CFRetained<CVPixelBuffer> = unsafe { CFRetained::from_raw(std::ptr::NonNull::new(b).unwrap()) };
                    objc2::rc::autoreleasepool(|_| c.render(scene, Some(&screen), Some((&cam, Bars::NONE)), &b));
                };
                for _ in 0..20 {
                    frame(&mut c);
                }
                let n = 240;
                let (t, c0) = (std::time::Instant::now(), cpu());
                for _ in 0..n {
                    frame(&mut c);
                }
                let ms = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
                let cpu_ms = (cpu() - c0) * 1000.0 / n as f64;
                println!("{scene:?} 1080p, 1080p camera, {name}: {ms:.2} ms/frame wall, {cpu_ms:.2} ms CPU");
            }
        }
    }

    /// What an Osmo Pocket 3 sends on a portrait format: a 720x1280 frame
    /// with its 16:9 picture (720x405) in the middle and black above and
    /// below.
    pub(crate) fn letterboxed_cam() -> CFRetained<CVPixelBuffer> {
        let (top, ph) = (437, 405);
        paint(720, 1280, |x, y| {
            if y < top || y >= top + ph {
                return [0, 0, 0, 255];
            }
            let (u, v) = (x as f64 / 720.0, (y - top) as f64 / ph as f64);
            // A sky-to-grass gradient with an orange disc in the middle.
            let (dx, dy) = ((u - 0.5) * 16.0 / 9.0, v - 0.5);
            if dx * dx + dy * dy < 0.09 {
                return [60, 150, 240, 255];
            }
            let r = 60.0 + 80.0 * u;
            let g = 130.0 + 90.0 * v;
            let b = 210.0 - 120.0 * v;
            [b as u8, g as u8, r as u8, 255]
        })
    }

    /// A letterboxed camera frame fills the overlay and the Camera scene
    /// with picture, edge to edge.
    #[test]
    fn letterboxed_camera_shows_no_bars() {
        let (w, h) = (1280usize, 720usize);
        let screen = paint(w, h, |x, y| if (x / 80 + y / 80) % 2 == 0 { [70, 64, 60, 255] } else { [86, 80, 76, 255] });
        let cam = letterboxed_cam();
        save_png(&cam, "camera-input");
        let out = paint(w, h, |_, _| [0, 255, 0, 255]);
        let pip = PipSpec { size: 0.3, ..Default::default() };
        let mut c = Compositor::new(w, h, pip.clone());
        let lum = |v: [u8; 3]| (v[0] as u32 * 19 + v[1] as u32 * 183 + v[2] as u32 * 54) / 256;
        let orange = |v: [u8; 3]| v[2] > 200 && (120..180).contains(&v[1]) && v[0] < 100;
        let mut problems = Vec::new();
        // The camera thread measures the bars as frames arrive (30 fps, 2 s).
        let mut tracker = crate::bars::Tracker::default();
        let bars = (0..60).map(|i| tracker.frame(i as f64 / 30.0, &cam)).last().unwrap();
        let cam = Some((&*cam, bars));

        c.render(Scene::ScreenCam, Some(&screen), cam, &out);
        save_png(&out, "screen-and-camera");
        let [x, y, ow, oh] = overlay_rect(&pip, w as f64, h as f64, c.cam_aspect());
        if (ow / oh - 16.0 / 9.0).abs() > 0.02 {
            problems.push(format!("overlay is {ow}x{oh} (aspect {:.3}), not 16:9", ow / oh));
        }
        // Down the middle of the overlay, inside its rounded corners.
        let cx = (x + ow / 2.0) as usize;
        for f in [0.06, 0.25, 0.5, 0.75, 0.94] {
            let py = (y + oh * f) as usize;
            let v = pixel(&out, cx, py);
            if lum(v) < 40 {
                problems.push(format!("overlay is black {:.0}% of the way down (x {cx}, y {py}): {v:?}", f * 100.0));
            }
        }
        let mid = pixel(&out, cx, (y + oh / 2.0) as usize);
        if !orange(mid) {
            problems.push(format!("the picture's middle is not in the overlay's middle: {mid:?}"));
        }

        c.render(Scene::Cam, None, cam, &out);
        save_png(&out, "camera-scene");
        for (px, py) in [(w / 2, 1), (w / 2, h / 2), (w / 2, h - 2), (1, h / 2), (w - 2, h / 2)] {
            let v = pixel(&out, px, py);
            if lum(v) < 40 {
                problems.push(format!("camera scene is black at ({px}, {py}): {v:?}"));
            }
        }
        let mid = pixel(&out, w / 2, h / 2);
        if !orange(mid) {
            problems.push(format!("the picture's middle is not in the camera scene's middle: {mid:?}"));
        }
        assert!(problems.is_empty(), "black bars:\n  {}", problems.join("\n  "));
    }
}

#[cfg(test)]
pub(crate) mod color_probe {
    use super::*;
    use objc2_core_foundation::CFString;
    use objc2_core_video::*;

    pub(crate) fn gray(v: u8, tag: Option<(&CFString, &CFString)>) -> CFRetained<CVPixelBuffer> {
        stripes(64, 36, &[v], tag)
    }

    /// A BGRA frame of vertical gray stripes, optionally color-tagged.
    pub(crate) fn stripes(w: usize, h: usize, vals: &[u8], tag: Option<(&CFString, &CFString)>) -> CFRetained<CVPixelBuffer> {
        let pool = make_pool(w, h, PIX_BGRA);
        let mut b = std::ptr::null_mut();
        unsafe {
            CVPixelBufferPool::create_pixel_buffer(None, &pool, std::ptr::NonNull::from(&mut b));
            let b: CFRetained<CVPixelBuffer> = CFRetained::from_raw(std::ptr::NonNull::new(b).unwrap());
            CVPixelBufferLockBaseAddress(&b, CVPixelBufferLockFlags(0));
            let p = CVPixelBufferGetBaseAddress(&b) as *mut u8;
            let row = CVPixelBufferGetBytesPerRow(&b);
            for y in 0..h {
                for x in 0..w {
                    let v = vals[x * vals.len() / w];
                    for c in 0..4 {
                        *p.add(y * row + x * 4 + c) = if c == 3 { 255 } else { v };
                    }
                }
            }
            CVPixelBufferUnlockBaseAddress(&b, CVPixelBufferLockFlags(0));
            if let Some((prim, tf)) = tag {
                let m = CVAttachmentMode::ShouldPropagate;
                b.set_attachment(kCVImageBufferColorPrimariesKey, prim.as_ref(), m);
                b.set_attachment(kCVImageBufferTransferFunctionKey, tf.as_ref(), m);
            }
            b
        }
    }

    fn center(b: &CVPixelBuffer) -> u8 {
        unsafe {
            CVPixelBufferLockBaseAddress(b, CVPixelBufferLockFlags(1));
            let p = CVPixelBufferGetBaseAddress(b) as *const u8;
            let v = *p.add(CVPixelBufferGetBytesPerRow(b) * 18 + 32 * 4 + 1);
            CVPixelBufferUnlockBaseAddress(b, CVPixelBufferLockFlags(1));
            v
        }
    }

    /// Camera-style frames through the compositor (run with --ignored).
    #[test]
    #[ignore]
    fn camera_tags() {
        let out = gray(0, None);
        let mut c = Compositor::new(64, 36, PipSpec::default());
        unsafe {
            for (name, tag) in [
                ("untagged", None),
                ("709/709", Some((kCVImageBufferColorPrimaries_ITU_R_709_2, kCVImageBufferTransferFunction_ITU_R_709_2))),
                ("709/sRGB", Some((kCVImageBufferColorPrimaries_ITU_R_709_2, kCVImageBufferTransferFunction_sRGB))),
            ] {
                let vals: Vec<u8> = [32u8, 64, 128, 192].iter().map(|&v| {
                    let cam = gray(v, tag);
                    c.render(Scene::Cam, None, Some((&cam, Bars::NONE)), &out);
                    center(&out)
                }).collect();
                println!("{name}: in [32, 64, 128, 192] -> {vals:?}");
            }
        }
    }
}
