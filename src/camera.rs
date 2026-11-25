//! Webcam capture (AVCaptureSession) and device enumeration.

use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_av_foundation::*;
use objc2_core_media::CMSampleBuffer;
use objc2_core_video::kCVPixelBufferPixelFormatTypeKey;
use objc2_foundation::{NSArray, NSObject, NSObjectProtocol};
use dispatch2::DispatchQueue;

use crate::record::Shared;
use crate::util::*;

pub struct Device {
    pub id: String,
    pub name: String,
}

/// The camera picture size Rec asks a camera for. The camera's own format
/// closest to it is used (see `pick_format`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize)]
pub enum CamResolution {
    #[default]
    #[value(name = "720p")]
    #[serde(rename = "720p")]
    Hd720,
    #[value(name = "1080p")]
    #[serde(rename = "1080p")]
    Hd1080,
}

impl CamResolution {
    pub const ALL: [CamResolution; 2] = [CamResolution::Hd720, CamResolution::Hd1080];

    /// Width and height in pixels.
    pub fn size(self) -> (u32, u32) {
        match self {
            CamResolution::Hd720 => (1280, 720),
            CamResolution::Hd1080 => (1920, 1080),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            CamResolution::Hd720 => "720p",
            CamResolution::Hd1080 => "1080p",
        }
    }
}

fn discover(types: &[&AVCaptureDeviceType], media: &AVMediaType) -> Vec<Retained<AVCaptureDevice>> {
    unsafe {
        let arr = NSArray::from_slice(types);
        let s = AVCaptureDeviceDiscoverySession::discoverySessionWithDeviceTypes_mediaType_position(
            &arr,
            Some(media),
            AVCaptureDevicePosition::Unspecified,
        );
        s.devices().iter().collect()
    }
}

fn cameras_raw() -> Vec<Retained<AVCaptureDevice>> {
    unsafe {
        discover(
            &[
                AVCaptureDeviceTypeBuiltInWideAngleCamera,
                AVCaptureDeviceTypeExternal,
                AVCaptureDeviceTypeContinuityCamera,
            ],
            AVMediaTypeVideo.unwrap(),
        )
    }
}

fn describe(d: &AVCaptureDevice) -> Device {
    unsafe { Device { id: d.uniqueID().to_string(), name: d.localizedName().to_string() } }
}

pub fn cameras() -> Vec<Device> {
    cameras_raw().iter().map(|d| describe(d)).collect()
}

pub fn microphones() -> Vec<Device> {
    unsafe {
        discover(&[AVCaptureDeviceTypeMicrophone], AVMediaTypeAudio.unwrap())
            .iter()
            .map(|d| describe(d))
            .collect()
    }
}

/// A saved or requested mic resolved against the connected devices.
pub struct MicPick {
    /// Unique ID to open; None = the system default mic.
    pub id: Option<String>,
    /// Why the requested mic wasn't used (it is not connected).
    pub warning: Option<String>,
}

/// Resolve a mic selector without failing: by unique ID, then by name, then
/// the system default with a warning. Audio never blocks a session.
pub fn pick_mic(sel: &str, name: Option<&str>) -> MicPick {
    let mics = microphones();
    match match_mic(&mics, sel, name) {
        Some(id) => MicPick { id: Some(id), warning: None },
        None => {
            let label = name.filter(|n| !n.is_empty()).unwrap_or(sel);
            MicPick { id: None, warning: Some(format!("Microphone \u{201c}{label}\u{201d} not connected; using the default mic")) }
        }
    }
}

fn match_mic(mics: &[Device], sel: &str, name: Option<&str>) -> Option<String> {
    // Exact unique ID.
    if let Some(m) = mics.iter().find(|m| m.id == sel) {
        return Some(m.id.clone());
    }
    // USB unique IDs end in a port/instance index (`…:SERIAL:4`) that can
    // change between plugs; match on the rest.
    let stem = |id: &str| id.rsplit_once(':').filter(|(_, n)| n.chars().all(|c| c.is_ascii_digit())).map(|(s, _)| s.to_string());
    if let Some(st) = stem(sel)
        && let Some(m) = mics.iter().find(|m| stem(&m.id).as_deref() == Some(st.as_str()))
    {
        return Some(m.id.clone());
    }
    // Saved display name (exact, then substring), then the selector as a name.
    for n in name.into_iter().chain([sel]).filter(|n| !n.is_empty()) {
        let lower = n.to_lowercase();
        if let Some(m) = mics.iter().find(|m| m.name.to_lowercase() == lower) {
            return Some(m.id.clone());
        }
        if let Some(m) = mics.iter().find(|m| m.name.to_lowercase().contains(&lower)) {
            return Some(m.id.clone());
        }
    }
    None
}

/// Call `f` on the main queue whenever a capture device is connected or
/// disconnected. The observers live for the process.
pub fn on_device_change(f: impl Fn() + Clone + 'static) {
    use objc2_foundation::{NSNotification, NSNotificationCenter, NSOperationQueue};
    unsafe {
        let center = NSNotificationCenter::defaultCenter();
        let queue = NSOperationQueue::mainQueue();
        for name in [AVCaptureDeviceWasConnectedNotification, AVCaptureDeviceWasDisconnectedNotification] {
            let f = f.clone();
            let block = block2::RcBlock::new(move |_n: std::ptr::NonNull<NSNotification>| f());
            let token = center.addObserverForName_object_queue_usingBlock(Some(name), None, Some(&queue), &block);
            std::mem::forget(token);
        }
    }
}

pub struct CamIvars {
    /// Where frames go; switched when the camera moves to another session.
    shared: std::sync::Mutex<Arc<Shared>>,
    /// Black bars in the camera's frames. Kept with the camera, so a
    /// session that takes it over crops from its first frame.
    bars: std::sync::Mutex<crate::bars::Tracker>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "RecCameraOutput"]
    #[ivars = CamIvars]
    pub struct CameraOutput;

    unsafe impl NSObjectProtocol for CameraOutput {}

    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for CameraOutput {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        unsafe fn did_output(&self, _o: &AVCaptureOutput, sb: &CMSampleBuffer, _c: &AVCaptureConnection) {
            unsafe {
                if let Some(pb) = sb.image_buffer() {
                    // REC_DEBUG: the camera's frame size and color tags (once).
                    static LOGGED: std::sync::Once = std::sync::Once::new();
                    let debug = std::env::var_os("REC_DEBUG").is_some();
                    if debug {
                        LOGGED.call_once(|| {
                            let a = pb.attachments(objc2_core_video::CVAttachmentMode::ShouldPropagate);
                            let (w, h) = (objc2_core_video::CVPixelBufferGetWidth(&pb), objc2_core_video::CVPixelBufferGetHeight(&pb));
                            eprintln!("rec: debug: camera frame {w}x{h}, attachments: {a:?}");
                        });
                    }
                    let pts = sb.presentation_time_stamp();
                    let bars = {
                        let mut t = self.ivars().bars.lock().unwrap();
                        let before = t.current();
                        let bars = t.frame(secs(pts), &pb);
                        if debug && bars != before {
                            eprintln!("rec: debug: camera black bars to crop: {bars:?}");
                        }
                        bars
                    };
                    let shared = self.ivars().shared.lock().unwrap().clone();
                    shared.with(|s| s.push_cam(pts, pb, bars));
                }
            }
        }
    }
);

pub struct Camera {
    session: Retained<AVCaptureSession>,
    out: Retained<CameraOutput>,
    _queue: dispatch2::DispatchRetained<DispatchQueue>,
    pub name: String,
    /// The device's unique ID.
    pub id: String,
    /// The picture size it was started for.
    pub resolution: CamResolution,
    /// When it started: auto-exposure and white balance settle over the
    /// first second or two, so a fresh camera looks dark at first.
    pub started: std::time::Instant,
}

unsafe impl Send for Camera {}

impl Camera {
    /// The device `sel` names: None = the system's preferred camera, else a
    /// unique ID or name substring.
    fn device(sel: Option<&str>) -> Result<Retained<AVCaptureDevice>> {
        let cams = cameras_raw();
        match sel {
            // System-preferred camera (what FaceTime would use), else the
            // built-in camera, else anything.
            None => unsafe { AVCaptureDevice::systemPreferredCamera() }
                .or_else(|| {
                    cams.iter()
                        .find(|d| unsafe { d.deviceType().to_string() == AVCaptureDeviceTypeBuiltInWideAngleCamera.to_string() })
                        .cloned()
                })
                .or_else(|| cams.into_iter().next()),
            Some(s) => {
                let lower = s.to_lowercase();
                cams.into_iter().find(|d| unsafe {
                    d.uniqueID().to_string() == s || d.localizedName().to_string().to_lowercase().contains(&lower)
                })
            }
        }
        .ok_or_else(|| anyhow!("no camera found (see `rec-rs devices`)"))
    }

    /// The unique ID of the camera `sel` names (see `device`).
    pub fn resolve(sel: Option<&str>) -> Result<String> {
        Ok(unsafe { Self::device(sel)?.uniqueID().to_string() })
    }

    /// The unique ID and name of the camera `sel` names (see `device`).
    pub fn find(sel: Option<&str>) -> Result<Device> {
        Ok(describe(&*Self::device(sel)?))
    }

    /// Send frames to another session's sink (the camera keeps running,
    /// already exposed, instead of restarting).
    pub fn retarget(&self, shared: Arc<Shared>) {
        *self.out.ivars().shared.lock().unwrap() = shared;
    }

    /// `sel`: None = first camera, else unique ID or name substring.
    /// `res`: the picture size to ask for.
    pub fn start(sel: Option<&str>, res: CamResolution, shared: Arc<Shared>) -> Result<Self> {
        let dev = Self::device(sel)?;
        unsafe {
            let name = dev.localizedName().to_string();
            let session = AVCaptureSession::new();
            session.beginConfiguration();
            // Used only if the device can't be configured below.
            let preset = match res {
                CamResolution::Hd720 => AVCaptureSessionPreset1280x720,
                CamResolution::Hd1080 => AVCaptureSessionPreset1920x1080,
            };
            if session.canSetSessionPreset(preset) {
                session.setSessionPreset(preset);
            } else {
                session.setSessionPreset(AVCaptureSessionPresetHigh);
            }
            let input = AVCaptureDeviceInput::deviceInputWithDevice_error(&dev)
                .map_err(|e| anyhow!("camera {name}: {}", e.localizedDescription()))?;
            if !session.canAddInput(&input) {
                bail!("cannot use camera {name}");
            }
            session.addInput(&input);
            // Pick the format here. A preset takes the first format of its
            // size, and on some cameras (a DJI Osmo Pocket 3) that can be a
            // portrait one with the landscape picture letterboxed inside.
            let (picked, lock) = use_best_format(&dev, res).unzip();
            let output = AVCaptureVideoDataOutput::new();
            output.setVideoSettings(Some(&dict(vec![(
                cf_ns(kCVPixelBufferPixelFormatTypeKey),
                num_i(PIX_BGRA as i64),
            )])));
            output.setAlwaysDiscardsLateVideoFrames(true);
            let delegate = Self::make_delegate(shared);
            let queue = DispatchQueue::new("rec.camera", None);
            output.setSampleBufferDelegate_queue(Some(ProtocolObject::from_ref(&*delegate)), Some(&queue));
            if !session.canAddOutput(&output) {
                bail!("cannot add camera output");
            }
            session.addOutput(&output);
            session.commitConfiguration();
            session.startRunning();
            // Running on our format now: the session won't change it.
            drop(lock);
            if !session.isRunning() {
                bail!("camera {name} failed to start");
            }
            if std::env::var_os("REC_DEBUG").is_some() {
                let f = format_info(&dev.activeFormat());
                let now = format!("{}x{} at {:.2} fps", f.width, f.height, 1.0 / dev.activeVideoMinFrameDuration().seconds());
                eprintln!("rec: debug: camera {name}: picked {picked:?}; running {now}, preset {}", session.sessionPreset());
            }
            let id = dev.uniqueID().to_string();
            Ok(Camera { session, out: delegate, _queue: queue, name, id, resolution: res, started: std::time::Instant::now() })
        }
    }

    fn make_delegate(shared: Arc<Shared>) -> Retained<CameraOutput> {
        let this = CameraOutput::alloc().set_ivars(CamIvars { shared: std::sync::Mutex::new(shared), bars: Default::default() });
        unsafe { msg_send![super(this), init] }
    }

    pub fn stop(&self) {
        unsafe { self.session.stopRunning() };
    }
}

/// What `pick_format` weighs about a camera format.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FormatInfo {
    pub width: u32,
    pub height: u32,
    /// The highest frame rate it supports.
    pub max_fps: f64,
}

/// The format to capture from, by what matters most: landscape, at least
/// 29.9 fps, no bigger than 1080p, 16:9, then the size closest to `target`
/// (bigger up to 1080p first, then smaller). Ties go to the device's own
/// order. None for an empty list.
pub fn pick_format(formats: &[FormatInfo], target: CamResolution) -> Option<usize> {
    const FULL_HD: u64 = 1920 * 1080;
    let (tw, th) = target.size();
    let want = tw as u64 * th as u64;
    let rank = |f: &FormatInfo| {
        let (w, h) = (f.width as u64, f.height as u64);
        let px = w * h;
        let empty = px == 0;
        let portrait = w < h;
        let slow = f.max_fps.is_nan() || f.max_fps < 29.9;
        let huge = px > FULL_HD.max(want);
        // More than 1% off 16:9.
        let not_wide = (w * 9).abs_diff(h * 16) * 100 > h * 16;
        let (smaller, distance) = if px >= want { (false, px - want) } else { (true, want - px) };
        (empty, portrait, slow, huge, not_wide, smaller, distance)
    };
    formats.iter().enumerate().min_by_key(|(i, f)| (rank(f), *i)).map(|(i, _)| i)
}

unsafe fn format_info(f: &AVCaptureDeviceFormat) -> FormatInfo {
    unsafe {
        let d = objc2_core_media::CMVideoFormatDescriptionGetDimensions(&f.formatDescription());
        let max_fps = f.videoSupportedFrameRateRanges().iter().map(|r| r.maxFrameRate()).fold(0.0, f64::max);
        FormatInfo { width: d.width.max(0) as u32, height: d.height.max(0) as u32, max_fps }
    }
}

/// The frame duration that runs `f` at 30 fps, and whether its range is a
/// single rate (then the longest duration is set too, so it can't drop to
/// another rate). Cameras report "30" as 30.00003, so a range topping out
/// near 30 uses its own shortest duration rather than exactly 1/30 s,
/// which would be out of range and throw.
unsafe fn thirty_fps(f: &AVCaptureDeviceFormat) -> Option<(objc2_core_media::CMTime, bool)> {
    unsafe {
        let r = f.videoSupportedFrameRateRanges().iter().find(|r| r.minFrameRate() <= 30.01 && r.maxFrameRate() >= 29.99)?;
        let fixed = r.maxFrameRate() - r.minFrameRate() < 0.5;
        let d = if r.maxFrameRate() < 30.5 { r.minFrameDuration() } else { objc2_core_media::CMTime::new(1, 30) };
        Some((d, fixed))
    }
}

/// A device's configuration lock, released on drop.
struct ConfigLock<'a>(&'a AVCaptureDevice);

impl Drop for ConfigLock<'_> {
    fn drop(&mut self) {
        unsafe { self.0.unlockForConfiguration() };
    }
}

/// Set `dev` (added to a session, inside its begin/commit configuration)
/// to the format `pick_format` chooses for `res`, at 30 fps where it can. Returns
/// the format and the device's lock. Hold the lock until the session runs:
/// on macOS, setting a format doesn't switch the session to the
/// input-priority preset as it does on iOS, so the session would put back
/// the format its own preset wants. None when the device can't be
/// configured; the session's preset then decides.
unsafe fn use_best_format(dev: &AVCaptureDevice, res: CamResolution) -> Option<(FormatInfo, ConfigLock<'_>)> {
    unsafe {
        let formats: Vec<Retained<AVCaptureDeviceFormat>> = dev.formats().iter().collect();
        let infos: Vec<FormatInfo> = formats.iter().map(|f| format_info(f)).collect();
        let i = pick_format(&infos, res)?;
        let format = &formats[i];
        if let Err(e) = dev.lockForConfiguration() {
            eprintln!("rec: warning: camera settings locked ({}); using its default format", e.localizedDescription());
            return None;
        }
        let lock = ConfigLock(dev);
        let set = objc2::exception::catch(std::panic::AssertUnwindSafe(|| {
            dev.setActiveFormat(format);
            if let Some((d, fixed)) = thirty_fps(format) {
                dev.setActiveVideoMinFrameDuration(d);
                if fixed {
                    dev.setActiveVideoMaxFrameDuration(d);
                }
            }
        }));
        match set {
            Ok(()) => Some((infos[i], lock)),
            Err(e) => {
                eprintln!("rec: warning: could not set the camera format: {}", e.map(|e| format!("{e:?}")).unwrap_or_default());
                None
            }
        }
    }
}

pub struct MicIvars {
    shared: Arc<Shared>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "RecMicOutput"]
    #[ivars = MicIvars]
    pub struct MicOutput;

    unsafe impl NSObjectProtocol for MicOutput {}

    unsafe impl AVCaptureAudioDataOutputSampleBufferDelegate for MicOutput {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        unsafe fn did_output(&self, _o: &AVCaptureOutput, sb: &CMSampleBuffer, _c: &AVCaptureConnection) {
            self.ivars().shared.with(|s| s.push_audio(crate::writer::AudioKind::Mic, sb));
        }
    }
);

/// Microphone via AVCaptureSession (fallback / test path; the default mic path
/// is ScreenCaptureKit's built-in microphone capture).
pub struct Mic {
    session: Retained<AVCaptureSession>,
    _out: Retained<MicOutput>,
    _queue: dispatch2::DispatchRetained<DispatchQueue>,
}

unsafe impl Send for Mic {}

impl Mic {
    pub fn start(id: Option<&str>, shared: Arc<Shared>) -> Result<Self> {
        unsafe {
            let media = AVMediaTypeAudio.unwrap();
            let dev = match id {
                Some(id) => AVCaptureDevice::deviceWithUniqueID(&objc2_foundation::NSString::from_str(id)),
                None => AVCaptureDevice::defaultDeviceWithMediaType(media),
            }
            .ok_or_else(|| anyhow!("microphone not found"))?;
            let session = AVCaptureSession::new();
            let input = AVCaptureDeviceInput::deviceInputWithDevice_error(&dev)
                .map_err(|e| anyhow!("microphone: {}", e.localizedDescription()))?;
            if !session.canAddInput(&input) {
                bail!("cannot use microphone");
            }
            session.addInput(&input);
            let output = AVCaptureAudioDataOutput::new();
            let this = MicOutput::alloc().set_ivars(MicIvars { shared });
            let delegate: Retained<MicOutput> = msg_send![super(this), init];
            let queue = DispatchQueue::new("rec.mic", None);
            output.setSampleBufferDelegate_queue(Some(ProtocolObject::from_ref(&*delegate)), Some(&queue));
            if !session.canAddOutput(&output) {
                bail!("cannot add microphone output");
            }
            session.addOutput(&output);
            session.startRunning();
            Ok(Mic { session, _out: delegate, _queue: queue })
        }
    }

    pub fn stop(&self) {
        unsafe { self.session.stopRunning() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: &str, name: &str) -> Device {
        Device { id: id.into(), name: name.into() }
    }

    #[test]
    fn mic_matching() {
        let mics = [dev("BuiltInMicrophoneDevice", "MacBook Pro Microphone"), dev("AppleUSBAudioEngine:DJI:DJIPocket3:123456789ABCDEF:2", "OsmoPocket3")];
        let dji = Some("AppleUSBAudioEngine:DJI:DJIPocket3:123456789ABCDEF:2".to_string());
        // Same device on another port.
        assert_eq!(match_mic(&mics, "AppleUSBAudioEngine:DJI:DJIPocket3:123456789ABCDEF:4", None), dji);
        // By saved name.
        assert_eq!(match_mic(&mics, "gone-id", Some("OsmoPocket3")), dji);
        // By name substring (CLI).
        assert_eq!(match_mic(&mics, "macbook", None).as_deref(), Some("BuiltInMicrophoneDevice"));
        assert_eq!(match_mic(&mics[..1], "AppleUSBAudioEngine:DJI:DJIPocket3:123456789ABCDEF:4", Some("OsmoPocket3")), None);
    }

    fn fmt(width: u32, height: u32, max_fps: f64) -> FormatInfo {
        FormatInfo { width, height, max_fps }
    }

    fn picked_for(formats: &[FormatInfo], target: CamResolution) -> Option<(u32, u32, f64)> {
        pick_format(formats, target).map(|i| (formats[i].width, formats[i].height, formats[i].max_fps))
    }

    /// The default target, 720p.
    fn picked(formats: &[FormatInfo]) -> Option<(u32, u32, f64)> {
        picked_for(formats, CamResolution::Hd720)
    }

    /// The formats a DJI Osmo Pocket 3 lists, in its order: 420v then 2vuy
    /// at each size, with their top frame rate ("30" is 30.00003). Its idle
    /// format is 1080x1920, a portrait one.
    const OSMO: [(u32, u32, f64); 10] = [
        (720, 1280, 30.00003),
        (720, 1280, 30.00003),
        (1080, 1920, 30.00003),
        (1080, 1920, 30.00003),
        (1280, 720, 30.00003),
        (1280, 720, 30.00003),
        (1920, 1080, 30.00003),
        (1920, 1080, 30.00003),
        (3840, 2160, 30.00003),
        (3840, 2160, 60.0),
    ];

    fn osmo() -> Vec<FormatInfo> {
        OSMO.iter().map(|&(w, h, f)| fmt(w, h, f)).collect()
    }

    #[test]
    fn osmo_pocket_3_gets_landscape_720p() {
        let osmo = osmo();
        // The first 1280x720 (420v), not a portrait format.
        assert_eq!(pick_format(&osmo, CamResolution::Hd720), Some(4));
        assert_eq!(picked(&osmo), Some((1280, 720, 30.00003)));
        // Without 720p: 1080p, not 4K and not portrait.
        let no_720: Vec<FormatInfo> = osmo.iter().copied().filter(|f| f.height != 720).collect();
        assert_eq!(picked(&no_720), Some((1920, 1080, 30.00003)));
        // 4K only when nothing else is landscape.
        let portrait_and_4k: Vec<FormatInfo> = osmo.iter().copied().filter(|f| f.width < f.height || f.width > 1920).collect();
        assert_eq!(portrait_and_4k.len(), 6);
        assert_eq!(picked(&portrait_and_4k), Some((3840, 2160, 30.00003)));
    }

    #[test]
    fn built_in_and_continuity_cameras_keep_720p() {
        // A MacBook camera (shapes for Center Stage and Desk View too).
        let mac = [
            fmt(1920, 1080, 30.0),
            fmt(1760, 1328, 30.0),
            fmt(1552, 1552, 30.0),
            fmt(1080, 1920, 30.0),
            fmt(1280, 720, 30.0),
            fmt(640, 480, 30.0),
        ];
        assert_eq!(picked(&mac), Some((1280, 720, 30.0)));
        // An iPhone as a Continuity Camera.
        let phone = [fmt(640, 480, 60.0), fmt(1920, 1440, 30.0), fmt(3840, 2160, 30.0), fmt(1920, 1080, 60.0), fmt(1280, 720, 60.0)];
        assert_eq!(picked(&phone), Some((1280, 720, 60.0)));
        // An old 4:3 webcam: the size closest to 720p, larger first.
        let old = [fmt(640, 480, 30.0), fmt(1600, 1200, 15.0), fmt(1024, 768, 30.0), fmt(800, 600, 30.0)];
        assert_eq!(picked(&old), Some((1024, 768, 30.0)));
    }

    #[test]
    fn frame_rate_and_fallbacks() {
        // 720p only at 15 fps: 1080p at 30 instead.
        assert_eq!(picked(&[fmt(1280, 720, 15.0), fmt(1920, 1080, 30.0)]), Some((1920, 1080, 30.0)));
        // Nothing at 30 fps: still landscape 720p.
        assert_eq!(picked(&[fmt(720, 1280, 15.0), fmt(1280, 720, 15.0), fmt(1920, 1080, 10.0)]), Some((1280, 720, 15.0)));
        // Portrait only: the one closest to 720p (the bars, if any, are
        // cropped by `bars`).
        assert_eq!(picked(&[fmt(1080, 1920, 30.0), fmt(720, 1280, 30.0)]), Some((720, 1280, 30.0)));
        // A broken entry never wins.
        assert_eq!(picked(&[fmt(0, 0, 30.0), fmt(320, 240, 5.0)]), Some((320, 240, 5.0)));
        assert_eq!(pick_format(&[], CamResolution::Hd720), None);
        assert_eq!(pick_format(&[], CamResolution::Hd1080), None);
    }

    #[test]
    fn osmo_pocket_3_gets_landscape_1080p() {
        let osmo = osmo();
        // The first 1920x1080 (420v): not portrait, not 4K.
        assert_eq!(pick_format(&osmo, CamResolution::Hd1080), Some(6));
        assert_eq!(picked_for(&osmo, CamResolution::Hd1080), Some((1920, 1080, 30.00003)));
        // Without 1080p: 720p, the closest smaller size, not 4K.
        let no_1080: Vec<FormatInfo> = osmo.iter().copied().filter(|f| f.height != 1080).collect();
        assert_eq!(picked_for(&no_1080, CamResolution::Hd1080), Some((1280, 720, 30.00003)));
    }

    #[test]
    fn built_in_and_continuity_cameras_at_1080p() {
        // The MacBook camera has 1920x1080 at 30 fps.
        let mac =
            [fmt(1920, 1080, 30.0), fmt(1760, 1328, 30.0), fmt(1552, 1552, 30.0), fmt(1080, 1920, 30.0), fmt(1280, 720, 30.0), fmt(640, 480, 30.0)];
        assert_eq!(picked_for(&mac, CamResolution::Hd1080), Some((1920, 1080, 30.0)));
        let phone = [fmt(640, 480, 60.0), fmt(1920, 1440, 30.0), fmt(3840, 2160, 30.0), fmt(1920, 1080, 60.0), fmt(1280, 720, 60.0)];
        assert_eq!(picked_for(&phone, CamResolution::Hd1080), Some((1920, 1080, 60.0)));
        // A 720p-only webcam stays at 720p.
        let old = [fmt(640, 480, 30.0), fmt(1280, 720, 30.0)];
        assert_eq!(picked_for(&old, CamResolution::Hd1080), Some((1280, 720, 30.0)));
        // 1080p only at 15 fps: 720p at 30 is better than a slow 1080p.
        assert_eq!(picked_for(&[fmt(1920, 1080, 15.0), fmt(1280, 720, 30.0)], CamResolution::Hd1080), Some((1280, 720, 30.0)));
        // An old 4:3 webcam: the size closest to 1080p, never above it.
        let old = [fmt(640, 480, 30.0), fmt(1600, 1200, 30.0), fmt(1024, 768, 30.0), fmt(2592, 1944, 30.0)];
        assert_eq!(picked_for(&old, CamResolution::Hd1080), Some((1600, 1200, 30.0)));
    }

    #[test]
    fn camera_resolution_names() {
        use clap::ValueEnum;
        for r in CamResolution::ALL {
            assert_eq!(r.to_possible_value().unwrap().get_name(), r.label());
            assert_eq!(serde_json::to_string(&r).unwrap(), format!("\"{}\"", r.label()));
        }
        assert_eq!(CamResolution::default(), CamResolution::Hd720);
        assert_eq!(CamResolution::Hd1080.size(), (1920, 1080));
    }

    /// Open a real camera through `Camera::start` for a few seconds and
    /// print its format and the bars found, after doing the same with the
    /// old preset-only setup. Run with --ignored --nocapture; REC_CAMERA
    /// names the camera and REC_CAMERA_RES=1080p asks for 1080p. Frames go
    /// nowhere and only sizes are printed.
    /// Skips without asking for camera access, and when another app is
    /// using the camera.
    #[test]
    #[ignore]
    fn live_camera() {
        use std::sync::Mutex;
        use std::sync::atomic::AtomicBool;
        use std::time::Duration;
        if crate::tcc::camera_status() != crate::tcc::Status::Granted {
            println!("camera access is {}; skipped", crate::tcc::camera_status());
            return;
        }
        let sel = std::env::var("REC_CAMERA").ok();
        let dev = Camera::device(sel.as_deref()).unwrap();
        let name = unsafe { dev.localizedName() };
        if unsafe { dev.isInUseByAnotherApplication() } {
            println!("{name} is in use by another app; skipped");
            return;
        }
        let shared = || Arc::new(Shared { sink: Mutex::new(None), error: Mutex::new(None), stop: Arc::new(AtomicBool::new(false)) });
        let report = |what: &str, out: &CameraOutput| unsafe {
            let f = format_info(&dev.activeFormat());
            let bars = out.ivars().bars.lock().unwrap().current();
            println!("{what}: {name} at {}x{} (up to {:.2} fps), bars cropped: {bars:?}", f.width, f.height, f.max_fps);
        };
        let formats: Vec<String> = unsafe {
            dev.formats().iter().map(|f| format_info(&f)).map(|f| format!("{}x{}@{:.2}", f.width, f.height, f.max_fps)).collect()
        };
        println!("{name} formats: {}", formats.join(", "));
        // The old setup: a 1280x720 preset and nothing else.
        unsafe {
            let session = AVCaptureSession::new();
            session.beginConfiguration();
            session.setSessionPreset(AVCaptureSessionPreset1280x720);
            let input = AVCaptureDeviceInput::deviceInputWithDevice_error(&dev).unwrap();
            session.addInput(&input);
            let output = AVCaptureVideoDataOutput::new();
            output.setVideoSettings(Some(&dict(vec![(cf_ns(kCVPixelBufferPixelFormatTypeKey), num_i(PIX_BGRA as i64))])));
            let delegate = Camera::make_delegate(shared());
            let queue = DispatchQueue::new("rec.camera.test", None);
            output.setSampleBufferDelegate_queue(Some(ProtocolObject::from_ref(&*delegate)), Some(&queue));
            session.addOutput(&output);
            session.commitConfiguration();
            session.startRunning();
            std::thread::sleep(Duration::from_secs(3));
            report("preset only", &delegate);
            session.stopRunning();
        }
        let res = if std::env::var("REC_CAMERA_RES").as_deref() == Ok("1080p") { CamResolution::Hd1080 } else { CamResolution::Hd720 };
        let cam = Camera::start(sel.as_deref(), res, shared()).unwrap();
        std::thread::sleep(Duration::from_secs(3));
        report("Camera::start", &cam.out);
        cam.stop();
    }
}
