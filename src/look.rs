//! Software adjustments to the camera picture: white balance (temperature
//! and tint), exposure, brightness, contrast, saturation, sharpness and
//! noise reduction.
//!
//! They stand in for camera settings a USB camera doesn't take. A DJI Osmo
//! Pocket 3 used as a webcam ignores its own white balance, color,
//! sharpness and noise-reduction settings, and its UVC interface has no
//! white balance, exposure, shutter or ISO control (see `uvc`).
//!
//! `Adjust::steps` turns the settings into a short list of CoreImage
//! filters and `apply` runs them on the GPU. The compositor applies them to
//! the camera's picture after its black bars are cropped and before it is
//! placed, and the same compositor draws the preview and the recording, so
//! the file gets what the preview shows. Values at their neutral point add
//! no filter: a neutral picture costs nothing.

use std::ops::RangeInclusive;

use objc2::Message;
use objc2::rc::Retained;
use objc2_core_image::{CIImage, CIVector};

use crate::util::*;

/// The white balance that leaves the camera's color as it is, in kelvin.
/// The camera has balanced its picture already, so Rec treats it as shot
/// for daylight, the usual studio light. Lower values correct for warmer
/// light and make the picture cooler (bluer), higher ones warmer, as on a
/// camera.
pub const NEUTRAL_K: f64 = 5600.0;
pub const KELVIN: RangeInclusive<f64> = 2500.0..=10000.0;
/// Green (negative) to magenta (positive).
pub const TINT: RangeInclusive<f64> = -100.0..=100.0;
/// Stops.
pub const EXPOSURE: RangeInclusive<f64> = -2.0..=2.0;
/// Brightness, contrast and saturation, in percent.
pub const PERCENT: RangeInclusive<f64> = -100.0..=100.0;
/// Camera style: negative softens, positive sharpens.
pub const SHARPNESS: RangeInclusive<f64> = -2.0..=2.0;
/// 0 is off; Rec can only add noise reduction on top of the camera's own.
pub const NOISE: RangeInclusive<f64> = 0.0..=2.0;

/// Software adjustments to the camera picture. Every field at its default
/// is neutral.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Adjust {
    /// White balance in kelvin (`NEUTRAL_K` leaves the color as it is).
    pub temperature: f64,
    /// -100 (green) to 100 (magenta).
    pub tint: f64,
    /// Stops, -2 to 2.
    pub exposure: f64,
    /// Percent, -100 to 100.
    pub brightness: f64,
    pub contrast: f64,
    pub saturation: f64,
    /// -2 (soft) to 2 (sharp).
    pub sharpness: f64,
    /// 0 (off) to 2.
    pub noise_reduction: f64,
}

impl Default for Adjust {
    fn default() -> Self {
        Adjust::NEUTRAL
    }
}

/// One CoreImage operation on the camera picture.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Step {
    /// Multiply linear red, green and blue (white balance and tint).
    Gains([f64; 3]),
    /// Multiply linear light by 2^EV.
    Exposure(f64),
    /// CIColorControls: a brightness offset, then contrast and saturation
    /// factors (1 is unchanged).
    Color { brightness: f64, contrast: f64, saturation: f64 },
    /// CINoiseReduction's noise level.
    Denoise(f64),
    /// CISharpenLuminance's amount.
    Sharpen(f64),
    /// A Gaussian blur of this sigma in pixels at 1080p (scaled with the
    /// picture's height).
    Soften(f64),
}

impl Adjust {
    pub const NEUTRAL: Adjust = Adjust {
        temperature: NEUTRAL_K,
        tint: 0.0,
        exposure: 0.0,
        brightness: 0.0,
        contrast: 0.0,
        saturation: 0.0,
        sharpness: 0.0,
        noise_reduction: 0.0,
    };

    pub fn is_neutral(&self) -> bool {
        self.steps().is_empty()
    }

    /// Every value inside its range (NaN becomes neutral).
    pub fn clamped(&self) -> Adjust {
        let c = |v: f64, r: RangeInclusive<f64>, n: f64| if v.is_nan() { n } else { v.clamp(*r.start(), *r.end()) };
        Adjust {
            temperature: c(self.temperature, KELVIN, NEUTRAL_K),
            tint: c(self.tint, TINT, 0.0),
            exposure: c(self.exposure, EXPOSURE, 0.0),
            brightness: c(self.brightness, PERCENT, 0.0),
            contrast: c(self.contrast, PERCENT, 0.0),
            saturation: c(self.saturation, PERCENT, 0.0),
            sharpness: c(self.sharpness, SHARPNESS, 0.0),
            noise_reduction: c(self.noise_reduction, NOISE, 0.0),
        }
    }

    /// The filters these settings need, in order: color first, then noise
    /// reduction, then sharpening. Empty when everything is neutral.
    pub fn steps(&self) -> Vec<Step> {
        let a = self.clamped();
        // Closer than this to neutral counts as neutral (slider rounding).
        let off = |v: f64, n: f64, eps: f64| (v - n).abs() < eps;
        let mut steps = vec![];
        if !off(a.temperature, NEUTRAL_K, 0.5) || !off(a.tint, 0.0, 0.05) {
            steps.push(Step::Gains(gains(a.temperature, a.tint)));
        }
        if !off(a.exposure, 0.0, 0.005) {
            steps.push(Step::Exposure(a.exposure));
        }
        if !off(a.brightness, 0.0, 0.05) || !off(a.contrast, 0.0, 0.05) || !off(a.saturation, 0.0, 0.05) {
            steps.push(Step::Color {
                brightness: a.brightness / 100.0 * 0.2,
                contrast: 1.0 + a.contrast / 100.0 * 0.5,
                saturation: 1.0 + a.saturation / 100.0,
            });
        }
        if !off(a.noise_reduction, 0.0, 0.005) {
            steps.push(Step::Denoise(0.02 * a.noise_reduction));
        }
        if a.sharpness >= 0.005 {
            steps.push(Step::Sharpen(0.4 * a.sharpness));
        } else if a.sharpness <= -0.005 {
            steps.push(Step::Soften(0.7 * -a.sharpness));
        }
        steps
    }

    /// The `--cam-adjust` argument for these settings: the values that
    /// aren't neutral as `key=value` pairs (empty when all are).
    pub fn to_arg(&self) -> String {
        let a = self.clamped();
        let n = Adjust::NEUTRAL;
        let mut parts = vec![];
        for (key, v, neutral) in [
            ("temperature", a.temperature, n.temperature),
            ("tint", a.tint, n.tint),
            ("exposure", a.exposure, n.exposure),
            ("brightness", a.brightness, n.brightness),
            ("contrast", a.contrast, n.contrast),
            ("saturation", a.saturation, n.saturation),
            ("sharpness", a.sharpness, n.sharpness),
            ("noise", a.noise_reduction, n.noise_reduction),
        ] {
            if v != neutral {
                parts.push(format!("{key}={v}"));
            }
        }
        parts.join(",")
    }

    /// Parse `--cam-adjust`: comma-separated `key=value` pairs; keys left
    /// out stay neutral and values are clamped to their ranges.
    pub fn parse(s: &str) -> Result<Adjust, String> {
        let mut a = Adjust::NEUTRAL;
        for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let Some((key, value)) = part.split_once('=') else {
                return Err(format!("{part:?} is not KEY=VALUE"));
            };
            let v: f64 = value.trim().trim_end_matches(['k', 'K']).trim().parse().map_err(|_| format!("{part:?}: not a number"))?;
            if !v.is_finite() {
                return Err(format!("{part:?}: not a number"));
            }
            let field = match key.trim().to_lowercase().as_str() {
                "temperature" | "temp" | "wb" => &mut a.temperature,
                "tint" => &mut a.tint,
                "exposure" | "ev" => &mut a.exposure,
                "brightness" => &mut a.brightness,
                "contrast" => &mut a.contrast,
                "saturation" => &mut a.saturation,
                "sharpness" => &mut a.sharpness,
                "noise" | "noise-reduction" | "nr" => &mut a.noise_reduction,
                other => {
                    return Err(format!(
                        "unknown camera adjustment {other:?} (use temperature, tint, exposure, brightness, contrast, saturation, sharpness or noise)"
                    ));
                }
            };
            *field = v;
        }
        Ok(a.clamped())
    }
}

/// CIE 1931 xy chromaticity of a black body at `k` kelvin (Kim et al.'s
/// cubic fit to the Planckian locus, 1667 K to 25000 K).
pub fn planck_xy(k: f64) -> (f64, f64) {
    let t = k.clamp(1667.0, 25000.0);
    let (t2, t3) = (t * t, t * t * t);
    let x = if t <= 4000.0 {
        -0.2661239e9 / t3 - 0.2343589e6 / t2 + 0.8776956e3 / t + 0.179910
    } else {
        -3.0258469e9 / t3 + 2.1070379e6 / t2 + 0.2226347e3 / t + 0.240390
    };
    let (x2, x3) = (x * x, x * x * x);
    let y = if t <= 2222.0 {
        -1.1063814 * x3 - 1.34811020 * x2 + 2.18555832 * x - 0.20219683
    } else if t <= 4000.0 {
        -0.9549476 * x3 - 1.37418593 * x2 + 2.09137015 * x - 0.16748867
    } else {
        3.0817580 * x3 - 5.87338670 * x2 + 3.75112997 * x - 0.37001483
    };
    (x, y)
}

/// The color of a `k`-kelvin light in linear sRGB, at luminance 1.
pub fn white_rgb(k: f64) -> [f64; 3] {
    let (x, y) = planck_xy(k);
    let (cx, cy, cz) = (x / y, 1.0, (1.0 - x - y) / y);
    [
        3.2404542 * cx - 1.5371385 * cy - 0.4985314 * cz,
        -0.9692660 * cx + 1.8760108 * cy + 0.0415560 * cz,
        0.0556434 * cx - 0.2040259 * cy + 1.0572252 * cz,
    ]
}

/// Rec. 709 luma of linear RGB.
fn luma(c: [f64; 3]) -> f64 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// Linear RGB gains for a white balance of `kelvin` and a `tint`: they turn
/// the white of a `kelvin` light into the neutral white (`NEUTRAL_K`), so a
/// lower setting adds blue and a higher one adds red. A positive tint takes
/// green out (magenta), a negative one adds it. Scaled so that gray keeps
/// its brightness.
pub fn gains(kelvin: f64, tint: f64) -> [f64; 3] {
    let n = white_rgb(NEUTRAL_K);
    let w = white_rgb(kelvin.clamp(*KELVIN.start(), *KELVIN.end()));
    let mut g = [n[0] / w[0], n[1] / w[1], n[2] / w[2]];
    g[1] *= 2f64.powf(-tint.clamp(*TINT.start(), *TINT.end()) / 200.0);
    let y = luma(g);
    g.map(|v| v / y)
}

fn vector(x: f64, y: f64, z: f64, w: f64) -> Retained<objc2::runtime::AnyObject> {
    unsafe { obj(&*CIVector::vectorWithX_Y_Z_W(x, y, z, w)) }
}

/// Run `steps` on `img` (the camera picture, which may have its origin off
/// zero after a crop). The result has the same extent. With no steps this
/// is `img` itself.
pub fn apply(img: &CIImage, steps: &[Step]) -> Retained<CIImage> {
    unsafe {
        let extent = img.extent();
        // Filters that sample neighbors read past the edges: clamp first so
        // the edges don't darken, then crop back.
        let local = |img: &CIImage, f: &dyn Fn(&CIImage) -> Retained<CIImage>| f(&img.imageByClampingToExtent()).imageByCroppingToRect(extent);
        let filter = |img: &CIImage, name: &str, params: Vec<(&str, Retained<objc2::runtime::AnyObject>)>| {
            let keys: Vec<Retained<objc2_foundation::NSString>> = params.iter().map(|(k, _)| ns(k)).collect();
            let pairs = keys.iter().zip(params).map(|(k, (_, v))| (&**k, v)).collect();
            img.imageByApplyingFilter_withInputParameters(&ns(name), &dict(pairs))
        };
        let mut out = img.retain();
        for step in steps {
            out = match *step {
                Step::Gains([r, g, b]) => filter(
                    &out,
                    "CIColorMatrix",
                    vec![
                        ("inputRVector", vector(r, 0.0, 0.0, 0.0)),
                        ("inputGVector", vector(0.0, g, 0.0, 0.0)),
                        ("inputBVector", vector(0.0, 0.0, b, 0.0)),
                        ("inputAVector", vector(0.0, 0.0, 0.0, 1.0)),
                        ("inputBiasVector", vector(0.0, 0.0, 0.0, 0.0)),
                    ],
                ),
                Step::Exposure(ev) => filter(&out, "CIExposureAdjust", vec![("inputEV", num_f(ev))]),
                Step::Color { brightness, contrast, saturation } => filter(
                    &out,
                    "CIColorControls",
                    vec![("inputBrightness", num_f(brightness)), ("inputContrast", num_f(contrast)), ("inputSaturation", num_f(saturation))],
                ),
                Step::Denoise(level) => {
                    local(&out, &|i| filter(i, "CINoiseReduction", vec![("inputNoiseLevel", num_f(level)), ("inputSharpness", num_f(0.4))]))
                }
                Step::Sharpen(amount) => local(&out, &|i| filter(i, "CISharpenLuminance", vec![("inputSharpness", num_f(amount))])),
                Step::Soften(sigma) => {
                    let s = sigma * extent.size.height.max(1.0) / 1080.0;
                    local(&out, &|i| i.imageByApplyingGaussianBlurWithSigma(s))
                }
            };
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_adds_no_filter() {
        assert_eq!(Adjust::default(), Adjust::NEUTRAL);
        assert!(Adjust::default().steps().is_empty());
        assert!(Adjust::default().is_neutral());
        // Slider rounding near neutral is still neutral.
        let near = Adjust { temperature: 5600.2, tint: 0.01, exposure: 0.001, sharpness: 0.001, ..Adjust::NEUTRAL };
        assert!(near.is_neutral(), "{:?}", near.steps());
        // Garbage in a hand-edited settings file doesn't turn filters on.
        let nan = Adjust { exposure: f64::NAN, temperature: f64::NAN, ..Adjust::NEUTRAL };
        assert!(nan.is_neutral());
    }

    #[test]
    fn each_setting_adds_its_own_step() {
        let one = |a: Adjust| a.steps();
        assert!(matches!(one(Adjust { temperature: 5000.0, ..Adjust::NEUTRAL })[..], [Step::Gains(_)]));
        assert!(matches!(one(Adjust { tint: 10.0, ..Adjust::NEUTRAL })[..], [Step::Gains(_)]));
        assert_eq!(one(Adjust { exposure: 0.5, ..Adjust::NEUTRAL }), [Step::Exposure(0.5)]);
        assert_eq!(one(Adjust { contrast: 20.0, ..Adjust::NEUTRAL }), [Step::Color { brightness: 0.0, contrast: 1.1, saturation: 1.0 }]);
        assert_eq!(one(Adjust { noise_reduction: 1.0, ..Adjust::NEUTRAL }), [Step::Denoise(0.02)]);
        assert_eq!(one(Adjust { sharpness: 1.0, ..Adjust::NEUTRAL }), [Step::Sharpen(0.4)]);
        assert_eq!(one(Adjust { sharpness: -1.0, ..Adjust::NEUTRAL }), [Step::Soften(0.7)]);
        // Order: color, then noise reduction, then sharpening.
        let all = Adjust { temperature: 3200.0, exposure: 1.0, saturation: -50.0, noise_reduction: 2.0, sharpness: 2.0, ..Adjust::NEUTRAL };
        let kinds: Vec<&str> = all
            .steps()
            .iter()
            .map(|s| match s {
                Step::Gains(_) => "gains",
                Step::Exposure(_) => "exposure",
                Step::Color { .. } => "color",
                Step::Denoise(_) => "denoise",
                Step::Sharpen(_) => "sharpen",
                Step::Soften(_) => "soften",
            })
            .collect();
        assert_eq!(kinds, ["gains", "exposure", "color", "denoise", "sharpen"]);
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn kelvin_follows_the_black_body() {
        // Known points of the Planckian locus.
        let (x, y) = planck_xy(6504.0);
        assert!((x - 0.3135).abs() < 0.002 && (y - 0.3237).abs() < 0.002, "{x} {y}");
        let (x, y) = planck_xy(2856.0);
        assert!((x - 0.4476).abs() < 0.002 && (y - 0.4074).abs() < 0.002, "{x} {y}");
        // A low-kelvin light is warm: more red than blue. A high one is cool.
        let warm = white_rgb(3200.0);
        let cool = white_rgb(8000.0);
        assert!(warm[0] > warm[2], "{warm:?}");
        assert!(cool[2] > cool[0], "{cool:?}");
        for k in [2500.0, 4000.0, 5600.0, 10000.0] {
            assert!(white_rgb(k).iter().all(|&c| c > 0.0), "{k} K is inside sRGB");
        }
    }

    #[test]
    fn white_balance_direction() {
        // Neutral is no change.
        let g = gains(NEUTRAL_K, 0.0);
        assert!(g.iter().all(|&v| close(v, 1.0)), "{g:?}");
        // Set for warm (3200 K) light, the correction adds blue: on footage
        // that was already neutral, a lower setting looks cooler, as on a
        // camera. A higher setting adds red and looks warmer.
        let low = gains(3200.0, 0.0);
        assert!(low[2] > 1.0 && low[0] < 1.0, "{low:?}");
        let high = gains(8000.0, 0.0);
        assert!(high[0] > 1.0 && high[2] < 1.0, "{high:?}");
        // The correction turns the light it's set for into neutral white.
        let warm = white_rgb(3200.0);
        let fixed: Vec<f64> = (0..3).map(|i| warm[i] * low[i]).collect();
        let n = white_rgb(NEUTRAL_K);
        assert!(close(fixed[0] / fixed[1], n[0] / n[1]) && close(fixed[2] / fixed[1], n[2] / n[1]), "{fixed:?}");
        // Monotonic: each step down in kelvin adds blue relative to red.
        let mut last = 0.0;
        for k in (2500..=10000).rev().step_by(100) {
            let g = gains(k as f64, 0.0);
            let ratio = g[2] / g[0];
            assert!(ratio > last, "{k} K");
            last = ratio;
        }
        // Gray keeps its brightness.
        for (k, t) in [(2500.0, 0.0), (10000.0, 0.0), (5000.0, 40.0), (6500.0, -60.0)] {
            assert!(close(luma(gains(k, t)), 1.0), "{k} {t}");
        }
    }

    #[test]
    fn tint_moves_between_green_and_magenta() {
        let magenta = gains(NEUTRAL_K, 30.0);
        assert!(magenta[1] < 1.0 && magenta[0] > 1.0 && magenta[2] > 1.0, "{magenta:?}");
        let green = gains(NEUTRAL_K, -30.0);
        assert!(green[1] > 1.0 && green[0] < 1.0 && green[2] < 1.0, "{green:?}");
        assert!(close(magenta[0], magenta[2]), "tint leaves red against blue alone");
    }

    #[test]
    fn cli_argument_round_trip() {
        assert_eq!(Adjust::NEUTRAL.to_arg(), "");
        assert_eq!(Adjust::parse("").unwrap(), Adjust::NEUTRAL);
        let a = Adjust { temperature: 5000.0, tint: -4.0, exposure: 0.3, sharpness: -1.0, noise_reduction: 0.5, ..Adjust::NEUTRAL };
        assert_eq!(a.to_arg(), "temperature=5000,tint=-4,exposure=0.3,sharpness=-1,noise=0.5");
        assert_eq!(Adjust::parse(&a.to_arg()).unwrap(), a);
        // Aliases, spaces and a "K" suffix; out of range values are clamped.
        let b = Adjust::parse(" temp=5600K , ev = 9, contrast=-150, nr=1 ").unwrap();
        assert_eq!(b, Adjust { exposure: 2.0, contrast: -100.0, noise_reduction: 1.0, ..Adjust::NEUTRAL });
        assert!(Adjust::parse("warmth=3").unwrap_err().contains("unknown"));
        assert!(Adjust::parse("tint").is_err());
        assert!(Adjust::parse("tint=lots").is_err());
        assert!(Adjust::parse("tint=inf").is_err());
    }

    #[test]
    fn settings_file_round_trip() {
        let a = Adjust { temperature: 5000.0, saturation: 12.0, ..Adjust::NEUTRAL };
        let json = serde_json::to_string(&a).unwrap();
        assert_eq!(serde_json::from_str::<Adjust>(&json).unwrap(), a);
        // Missing fields are neutral.
        assert_eq!(serde_json::from_str::<Adjust>(r#"{"exposure": 0.5}"#).unwrap(), Adjust { exposure: 0.5, ..Adjust::NEUTRAL });
    }
}
