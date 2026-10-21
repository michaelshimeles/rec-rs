//! AVAssetWriter sink: constant-frame-rate video pacing plus audio tracks.
//!
//! Video is emitted on an exact `1/fps` grid (what editors expect, like OBS),
//! repeating the newest screen frame when ScreenCaptureKit delivers nothing
//! (it only sends frames when pixels change). All timestamps stay on the host
//! clock, which ScreenCaptureKit audio/video and AVCapture camera share, so
//! audio is appended untouched and stays in sync.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use block2::RcBlock;
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_av_foundation::*;
use objc2_core_foundation::CFRetained;
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferPool, kCVPixelBufferHeightKey,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
};
use objc2_foundation::NSURL;

use crate::compose::{Compositor, Scene};
use crate::mixer::{Fader, Mixer, Volume};
use crate::util::*;

pub const TIMESCALE: i32 = 60000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Codec {
    H264,
    Hevc,
    /// Apple ProRes 422 (intra-frame, large files, smoothest editing).
    Prores,
}

#[derive(Clone, Debug)]
pub struct VideoSpec {
    pub width: usize,
    pub height: usize,
    pub fps: u32,
    pub codec: Codec,
    pub bitrate: u64,
    /// Pixel format of buffers handed to the encoder.
    pub pixel_format: u32,
}

/// Audio track layout when both system audio and mic are recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AudioLayout {
    /// Track 1 = system+mic mix (what players use), tracks 2-3 = system and mic
    /// stems for editors. QuickTime plays only the mix.
    Both,
    /// Only the system+mic mix (like OBS's default single track).
    Mix,
    /// Separate system and mic tracks only.
    Stems,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AudioKind {
    System,
    Mic,
}

#[derive(Default, Debug, Clone)]
pub struct Stats {
    pub screen_frames_in: u64,
    pub cam_frames_in: u64,
    pub video_frames_out: u64,
    pub video_dups: u64,
    pub video_drops: u64,
    pub audio_sys_buffers: u64,
    pub audio_mic_buffers: u64,
    pub audio_drops: u64,
    pub audio_mix_buffers: u64,
    pub audio_clipped_samples: u64,
    /// Audio gaps (late start, dropped device buffers) filled with silence.
    pub audio_gaps_filled: u64,
    /// Pacer passes that waited for a busy encoder.
    pub video_retries: u64,
    /// Grid slots skipped while paused.
    pub paused_slots: u64,
    /// Arrival latency (host now - capture pts), max and sum, in seconds.
    pub screen_latency_max: f64,
    pub screen_latency_sum: f64,
    pub cam_latency_max: f64,
    pub cam_latency_sum: f64,
}

/// Pause spans on the real frame grid, in slot indices `[pause, resume)`.
/// Everything captured inside a span is dropped and everything after it is
/// shifted earlier by the span's length, so the output has no gap. Spans are
/// whole slots, which keeps the video grid exact across pauses.
#[derive(Default, Debug, Clone)]
pub struct Pauses {
    spans: Vec<(i64, Option<i64>)>,
}

impl Pauses {
    pub fn is_paused(&self) -> bool {
        self.spans.last().is_some_and(|s| s.1.is_none())
    }

    /// Pause from `slot` on. Returns false if already paused.
    pub fn pause(&mut self, slot: i64) -> bool {
        if self.is_paused() {
            return false;
        }
        let floor = self.spans.last().and_then(|s| s.1).unwrap_or(0);
        self.spans.push((slot.max(floor), None));
        true
    }

    /// Resume at `slot`. Returns false if not paused.
    pub fn resume(&mut self, slot: i64) -> bool {
        let Some(last) = self.spans.last_mut() else { return false };
        if last.1.is_some() {
            return false;
        }
        if slot <= last.0 {
            self.spans.pop(); // resumed before the pause took effect
        } else {
            last.1 = Some(slot);
        }
        true
    }

    /// Slots to shift a capture at real grid position `slot` (fractional for
    /// audio), or None if it falls inside a pause and must be dropped.
    pub fn offset_at(&self, slot: f64) -> Option<i64> {
        let mut off = 0;
        for &(p, r) in &self.spans {
            if slot < p as f64 {
                break;
            }
            match r {
                Some(r) if slot >= r as f64 => off += r - p,
                _ => return None,
            }
        }
        Some(off)
    }

    /// The longest part of a capture spanning real slots `[a, b)` that lies
    /// outside pauses: `(from, to, offset)`. None if it is entirely paused.
    pub fn keep(&self, a: f64, b: f64) -> Option<(f64, f64, i64)> {
        let mut best: Option<(f64, f64, i64)> = None;
        let mut from = f64::NEG_INFINITY;
        let mut off = 0;
        let mut consider = |lo: f64, hi: f64, off: i64| {
            let (x, y) = (lo.max(a), hi.min(b));
            if y > x && best.is_none_or(|(p, q, _)| y - x > q - p) {
                best = Some((x, y, off));
            }
        };
        for &(p, r) in &self.spans {
            consider(from, p as f64, off);
            match r {
                Some(r) => {
                    off += r - p;
                    from = r as f64;
                }
                None => return best,
            }
        }
        consider(from, f64::INFINITY, off);
        best
    }

    /// Total paused slots (closed spans only).
    #[cfg(test)]
    pub fn total(&self) -> i64 {
        self.spans.iter().filter_map(|&(p, r)| r.map(|r| r - p)).sum()
    }
}

type Pb = SendBox<CFRetained<CVPixelBuffer>>;

/// Receives every emitted (composited) video frame, throttled to ~30 fps.
pub type PreviewFn = std::sync::Arc<dyn Fn(CFRetained<CVPixelBuffer>) + Send + Sync>;

/// The file being written (absent for preview-only sessions).
/// See `Sink::wait_for_camera`.
struct CamGate {
    settle: f64,
    /// The newest screen frame held back meanwhile.
    pending: Option<Pb>,
    /// Host time the camera is ready (first frame + settle).
    ready_at: Option<f64>,
    give_up: f64,
}

/// Start without the camera if it delivers nothing for this long.
const CAMERA_TIMEOUT_SECS: f64 = 3.0;

struct Out {
    writer: Retained<AVAssetWriter>,
    video: Retained<AVAssetWriterInput>,
    adaptor: Retained<AVAssetWriterInputPixelBufferAdaptor>,
    sys: Option<Retained<AVAssetWriterInput>>,
    mic: Option<Retained<AVAssetWriterInput>>,
    mix_input: Option<Retained<AVAssetWriterInput>>,
}

pub struct Sink {
    out: Option<Out>,
    /// Compositor output pool when there is no writer adaptor pool.
    own_pool: Option<CFRetained<CVPixelBufferPool>>,
    preview: Option<PreviewFn>,
    last_preview: f64,
    mixer: Option<Mixer>,
    /// Sources in the mix [system, mic].
    mix_sources: [bool; 2],
    /// Each source's fader, applied before anything uses its audio: the
    /// mix, its own track, the live encode and the level meter.
    faders: [Fader; 2],
    /// Fader gains without mute, for the meters (a muted source still
    /// shows its level).
    meter_gain: [f32; 2],
    fps: i64,
    /// Session start on the host clock, in TIMESCALE units.
    t0: Option<i64>,
    next_slot: i64,
    /// Next slot on the output timeline (real slots minus paused ones).
    out_slot: i64,
    pauses: Pauses,
    scene: Scene,
    /// Stop writing after this many output slots (exact `--duration`).
    limit: Option<i64>,
    /// Output-timeline position (seconds) where each audio stem continues.
    audio_next: [Option<f64>; 2],
    /// Seconds added to [system, mic] audio timestamps.
    audio_offset: [f64; 2],
    /// Audio that arrived before the first video frame set the clock.
    preroll: VecDeque<(AudioKind, SendBox<CFRetained<CMSampleBuffer>>)>,
    /// Decaying peak arrival latency [screen, camera] and when it was set.
    latency_peak: [f64; 2],
    latency_at: [f64; 2],
    /// Shared live encode (replay buffer, streams).
    live: Option<std::sync::Arc<crate::live::Live>>,
    screen_q: VecDeque<(f64, Pb)>,
    screen_last: Option<Pb>,
    cam_q: VecDeque<(f64, Pb)>,
    cam_last: Option<Pb>,
    compositor: Option<Compositor>,
    /// Scales native-size screen frames to the output size.
    scaler: Option<crate::scale::FrameScaler>,
    /// Fills a window capture's transparent corners (see `CornerFill`).
    corner_fill: Option<crate::compose::CornerFill>,
    /// Hold the start until the camera is ready (see `wait_for_camera`).
    cam_gate: Option<CamGate>,
    /// Output-size buffers for scaled screen frames fed to the compositor.
    scaled_pool: Option<CFRetained<CVPixelBufferPool>>,
    /// Output size and pixel format (for a pool created later).
    out_spec: (usize, usize, u32),
    pub stats: Stats,
    failed: Option<String>,
    levels: [f32; 2],
}

unsafe impl Send for Sink {}

/// Minimum distance behind real time that emission runs, so late-arriving
/// frames still land in their own slot.
pub const LAG_SECS: f64 = 0.05;

/// Frames buffered per source before the oldest are dropped.
/// ScreenCaptureKit's queue depth is 8: hold at most 7, which covers the
/// emission lag at 60 fps up to about 115 ms of continuous motion.
const QUEUE_MAX: usize = 7;

/// How far behind its time a frame slot may fall while the encoder is busy
/// before it is dropped. Below this the slot is retried on the next pacer
/// pass (with its own timestamp), so brief encoder stalls cost no frames.
pub const MAX_BACKLOG_SECS: f64 = 0.5;

/// Audio timestamp jitter tolerated before a stem is padded or trimmed.
pub const SNAP_SECS: f64 = 0.0015;

fn video_settings(v: &VideoSpec) -> Retained<Dict> {
    unsafe {
        let mut compression = vec![
            (AVVideoAverageBitRateKey.unwrap(), num_i(v.bitrate as i64)),
            (AVVideoExpectedSourceFrameRateKey.unwrap(), num_i(v.fps as i64)),
            (AVVideoMaxKeyFrameIntervalDurationKey.unwrap(), num_f(2.0)),
            (AVVideoAllowFrameReorderingKey.unwrap(), num_b(false)),
        ];
        let codec = match v.codec {
            Codec::H264 => {
                compression.push((
                    AVVideoProfileLevelKey.unwrap(),
                    obj(AVVideoProfileLevelH264HighAutoLevel.unwrap()),
                ));
                AVVideoCodecTypeH264.unwrap()
            }
            Codec::Hevc => AVVideoCodecTypeHEVC.unwrap(),
            Codec::Prores => AVVideoCodecTypeAppleProRes422.unwrap(),
        };
        let color = dict(vec![
            (AVVideoColorPrimariesKey.unwrap(), obj(AVVideoColorPrimaries_ITU_R_709_2.unwrap())),
            (AVVideoTransferFunctionKey.unwrap(), obj(AVVideoTransferFunction_ITU_R_709_2.unwrap())),
            (AVVideoYCbCrMatrixKey.unwrap(), obj(AVVideoYCbCrMatrix_ITU_R_709_2.unwrap())),
        ]);
        let mut settings = vec![
            (AVVideoCodecKey.unwrap(), obj(codec)),
            (AVVideoWidthKey.unwrap(), num_i(v.width as i64)),
            (AVVideoHeightKey.unwrap(), num_i(v.height as i64)),
            (AVVideoColorPropertiesKey.unwrap(), obj(&*color)),
        ];
        // ProRes is fixed-quality intra-frame: no bitrate/GOP settings.
        if v.codec != Codec::Prores {
            settings.push((AVVideoCompressionPropertiesKey.unwrap(), obj(&*dict(compression))));
        }
        dict(settings)
    }
}

pub fn audio_settings(channels: i64, bitrate: i64) -> Retained<Dict> {
    // Key constants live in AVFAudio; their values equal their names.
    dict(vec![
        (&*ns("AVFormatIDKey"), num_i(fourcc(b"aac ") as i64)),
        (&*ns("AVSampleRateKey"), num_f(48000.0)),
        (&*ns("AVNumberOfChannelsKey"), num_i(channels)),
        (&*ns("AVEncoderBitRateKey"), num_i(bitrate)),
    ])
}

impl Sink {
    /// `path` None = preview-only session (nothing is written).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        path: Option<&Path>,
        v: &VideoSpec,
        system_audio: bool,
        mic: bool,
        layout: AudioLayout,
        volumes: [Volume; 2],
        compositor: Option<Compositor>,
        scene: Scene,
        preview: Option<PreviewFn>,
    ) -> Result<Self> {
        let out = match path {
            Some(p) => Some(open_output(p, v, system_audio, mic, layout)?),
            None => None,
        };
        let own_pool = if out.is_none() && compositor.is_some() {
            Some(make_pool(v.width, v.height, v.pixel_format))
        } else {
            None
        };
        Ok(Sink {
            out,
            own_pool,
            preview,
            last_preview: 0.0,
            mixer: None,
            mix_sources: [system_audio, mic],
            faders: volumes.map(|v| Fader::new(v.gain())),
            meter_gain: volumes.map(Volume::fader_gain),
            fps: v.fps as i64,
            t0: None,
            next_slot: 0,
            out_slot: 0,
            pauses: Pauses::default(),
            scene: if compositor.is_some() { scene } else { Scene::Screen },
            limit: None,
            audio_next: [None; 2],
            preroll: VecDeque::new(),
            live: None,
            latency_peak: [0.0; 2],
            latency_at: [0.0; 2],
            audio_offset: [0.0; 2],
            screen_q: VecDeque::new(),
            screen_last: None,
            cam_q: VecDeque::new(),
            cam_last: None,
            compositor,
            scaler: None,
            corner_fill: None,
            cam_gate: None,
            scaled_pool: None,
            out_spec: (v.width, v.height, v.pixel_format),
            stats: Stats::default(),
            failed: None,
            levels: [0.0; 2],
        })
    }

    /// Peak levels [system, mic] since the last call (linear, 0..1),
    /// after the faders but before mute.
    pub fn take_levels(&mut self) -> [f32; 2] {
        std::mem::take(&mut self.levels)
    }

    /// Move the faders: the new levels apply to everything captured from
    /// now on, in every track and the live encode.
    pub fn set_volumes(&mut self, volumes: [Volume; 2]) {
        for (i, v) in volumes.into_iter().enumerate() {
            self.faders[i].set(v.gain());
            self.meter_gain[i] = v.fader_gain();
        }
    }

    /// Current emission lag: webcams deliver frames ~70-90 ms after capture,
    /// so wait for the slowest source seen (plus margin) before emitting a
    /// slot, keeping the overlay in sync with audio.
    pub fn lag(&self) -> f64 {
        let worst = self.latency_peak[0].max(self.latency_peak[1]);
        (worst + 0.025).clamp(LAG_SECS, 0.3)
    }

    /// Track arrival latency as a peak that decays by 50 ms per second, so
    /// one stall (a busy main thread, a device starting) doesn't keep the
    /// output delayed for the rest of the session.
    fn note_latency(&mut self, i: usize, lat: f64, at: f64) {
        let p = &mut self.latency_peak[i];
        let decayed = (*p - 0.05 * (at - self.latency_at[i]).max(0.0)).max(0.0);
        *p = decayed.max(lat);
        self.latency_at[i] = at;
    }

    pub fn started(&self) -> bool {
        self.t0.is_some()
    }

    pub fn failure(&self) -> Option<&str> {
        self.failed.as_deref()
    }

    fn slot_ticks(&self, slot: i64) -> i64 {
        self.t0.unwrap() + slot * self.tps()
    }

    /// TIMESCALE ticks per frame slot (fps divides TIMESCALE).
    fn tps(&self) -> i64 {
        TIMESCALE as i64 / self.fps
    }

    /// First grid slot at or after host time `t` (seconds).
    fn slot_at(&self, t: f64) -> i64 {
        let ticks = t * TIMESCALE as f64 - self.t0.unwrap() as f64;
        (ticks / self.tps() as f64).ceil() as i64
    }

    /// Switch scenes live. Scenes with the camera need a compositor (the
    /// session was started with a webcam); returns false otherwise.
    pub fn set_scene(&mut self, scene: Scene) -> bool {
        if self.compositor.is_none() && scene != Scene::Screen {
            return false;
        }
        self.scene = scene;
        true
    }

    /// Pause at host time `now`: nothing captured from the next frame slot on
    /// is written until `resume`. The preview keeps running.
    pub fn pause(&mut self, now: f64) -> bool {
        if self.t0.is_none() {
            return false;
        }
        let slot = self.slot_at(now).max(self.next_slot);
        self.pauses.pause(slot)
    }

    pub fn resume(&mut self, now: f64) -> bool {
        if self.t0.is_none() {
            return false;
        }
        let slot = self.slot_at(now).max(self.next_slot);
        self.pauses.resume(slot)
    }

    pub fn is_paused(&self) -> bool {
        self.pauses.is_paused()
    }

    /// Screen frames arrive at their native size: scale them to the output.
    pub fn set_scaler(&mut self, scaler: crate::scale::FrameScaler) {
        let (w, h, f) = self.out_spec;
        if self.out.is_none() && self.own_pool.is_none() {
            self.own_pool = Some(make_pool(w, h, f));
        }
        if self.compositor.is_some() {
            self.scaled_pool = Some(make_pool(w, h, f));
        }
        self.scaler = Some(scaler);
    }

    /// Start only once the camera has delivered a frame and then run for
    /// `settle` seconds (its exposure settling). Gives up after a few
    /// seconds without camera frames rather than never starting.
    pub fn wait_for_camera(&mut self, settle: f64) {
        let now = secs(host_now());
        self.cam_gate = Some(CamGate { settle, pending: None, ready_at: None, give_up: now + settle + CAMERA_TIMEOUT_SECS });
    }

    /// Screen frames are a single window's: fill its transparent corners.
    pub fn fill_window_corners(&mut self) {
        self.corner_fill = Some(crate::compose::CornerFill::new());
    }

    pub fn set_live(&mut self, live: std::sync::Arc<crate::live::Live>) {
        self.live = Some(live);
    }

    /// Change the camera overlay live (no-op without a webcam).
    pub fn set_pip(&mut self, pip: crate::compose::PipSpec) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_pip(pip);
        }
    }

    /// Aspect ratio of the camera, once frames arrive.
    pub fn cam_aspect(&self) -> Option<f64> {
        self.compositor.as_ref().map(|c| c.cam_aspect())
    }

    /// Shift [system, mic] audio later by these seconds: the output device's
    /// latency for system audio (captured when mixed, heard later), plus
    /// any user sync offsets.
    pub fn set_audio_offsets(&mut self, offsets: [f64; 2]) {
        self.audio_offset = offsets;
    }

    /// Write at most `secs` of output (rounded to whole frames).
    pub fn limit_secs(&mut self, secs: Option<f64>) {
        self.limit = secs.map(|d| (d * self.fps as f64).round() as i64);
    }

    /// Seconds of output written so far (excludes paused time).
    pub fn output_secs(&self) -> f64 {
        self.out_slot as f64 / self.fps as f64
    }

    pub fn push_screen(&mut self, pts: CMTime, pb: CFRetained<CVPixelBuffer>) {
        self.stats.screen_frames_in += 1;
        if self.t0.is_none()
            && let Some(g) = &mut self.cam_gate
        {
            let now = secs(pts);
            if g.ready_at.is_none_or(|r| now < r) && now < g.give_up {
                // Kept: a still screen sends no more frames, so this one
                // starts the file when the camera is ready.
                g.pending = Some(SendBox(pb));
                return;
            }
            self.cam_gate = None;
        }
        let pts_s = secs(pts);
        let lat = secs(host_now()) - pts_s;
        self.stats.screen_latency_max = self.stats.screen_latency_max.max(lat);
        self.note_latency(0, lat, pts_s);
        self.stats.screen_latency_sum += lat;
        if self.t0.is_none() {
            let t0 = (pts_s * TIMESCALE as f64).round() as i64;
            self.t0 = Some(t0);
            if let Some(o) = &self.out {
                unsafe { o.writer.startSessionAtSourceTime(CMTime::new(t0, TIMESCALE)) };
                if o.mix_input.is_some() {
                    let act = self.mix_sources;
                    self.mixer = Some(Mixer::new(t0 as f64 / TIMESCALE as f64, act[0], act[1]));
                }
            }
        }
        for (kind, sb) in std::mem::take(&mut self.preroll) {
            self.push_audio(kind, &sb.0);
        }
        let pb = match self.corner_fill.as_mut() {
            Some(f) => f.fill(&pb).unwrap_or(pb),
            None => pb,
        };
        self.screen_q.push_back((pts_s, SendBox(pb)));
        // Never hold more than a few ScreenCaptureKit surfaces (its queue
        // depth is 8).
        let due = self.due_time();
        Self::cap(&mut self.screen_q, &mut self.screen_last, QUEUE_MAX, due);
    }

    pub fn push_cam(&mut self, pts: CMTime, pb: CFRetained<CVPixelBuffer>) {
        self.stats.cam_frames_in += 1;
        if let Some(g) = &mut self.cam_gate {
            let ready = *g.ready_at.get_or_insert(secs(pts) + g.settle);
            if secs(pts) >= ready.min(g.give_up)
                && let Some(screen) = g.pending.take()
            {
                // The waiting screen frame starts the file now.
                self.cam_gate = None;
                self.push_screen(pts, screen.0);
            }
        }
        let lat = secs(host_now()) - secs(pts);
        self.stats.cam_latency_max = self.stats.cam_latency_max.max(lat);
        self.note_latency(1, lat, secs(pts));
        self.stats.cam_latency_sum += lat;
        self.cam_q.push_back((secs(pts), SendBox(pb)));
        let due = self.due_time();
        Self::cap(&mut self.cam_q, &mut self.cam_last, QUEUE_MAX, due);
    }

    pub fn push_audio(&mut self, kind: AudioKind, sb: &CMSampleBuffer) {
        let Some(t0) = self.t0 else {
            // Keep it until the clock starts; the part after t0 is used.
            let sb = unsafe { CFRetained::retain(std::ptr::NonNull::from(sb)) };
            self.preroll.push_back((kind, SendBox(sb)));
            if self.preroll.len() > 400 {
                self.preroll.pop_front();
            }
            return;
        };
        let pts = unsafe { sb.presentation_time_stamp() };
        let i = (kind == AudioKind::Mic) as usize;
        // When the sound was heard (see `set_audio_offsets`).
        let real_s = secs(pts);
        let pts_s = real_s + self.audio_offset[i];
        let mut decoded = crate::mixer::decode(sb);
        if let Some((_, _, frames)) = &decoded {
            let peak = frames.iter().fold(0f32, |m, f| m.max(f[0].abs()).max(f[1].abs()));
            self.levels[i] = self.levels[i].max(peak * self.meter_gain[i]);
        }
        let n = unsafe { sb.num_samples() } as usize;
        let rate = decoded.as_ref().map_or(48000.0, |d| d.1);
        // One gain curve for the whole buffer, used by the mix, the live
        // encode and this source's track alike.
        let env = self.faders[i].envelope(n, rate);
        if let (Some(env), Some((_, _, frames))) = (&env, &mut decoded) {
            for (f, g) in frames.iter_mut().zip(env) {
                f[0] *= g;
                f[1] *= g;
            }
        }
        if let (Some(l), Some((_, rate, frames))) = (&self.live, &decoded)
            && l.active()
        {
            l.audio(kind, pts_s, *rate, frames.clone());
        }
        // Cut the buffer at pause boundaries (sample-accurate: AVAssetWriter
        // lays audio buffers end to end, so a dropped or overlapping piece
        // would shift everything after it).
        let to_slot = |t: f64| (t * TIMESCALE as f64 - t0 as f64) / self.tps() as f64;
        let (a, b) = (to_slot(pts_s), to_slot(pts_s + n as f64 / rate));
        let Some((x, y, off)) = self.pauses.keep(a, b) else { return };
        // Nothing before the session start (slot 0).
        let x = x.max(0.0);
        if y <= x {
            return;
        }
        let slot_secs = 1.0 / self.fps as f64;
        let mut i0 = (((x - a) * slot_secs * rate).round() as usize).min(n);
        let mut i1 = (((y - a) * slot_secs * rate).round() as usize).min(n);
        if (x, y) == (a, b) {
            (i0, i1) = (0, n);
        }
        if i1 <= i0 {
            return;
        }
        let shift = off * self.tps();
        let out_s = pts_s + i0 as f64 / rate - shift as f64 / TIMESCALE as f64;
        if let (Some(m), Some((_, rate, frames))) = (self.mixer.as_mut(), decoded) {
            m.push(kind, out_s, rate, frames[i0.min(frames.len())..i1.min(frames.len())].to_vec());
            self.drain_mix(false);
        }
        let Some(o) = &self.out else { return };
        let input = match kind {
            AudioKind::System => o.sys.as_ref(),
            AudioKind::Mic => o.mic.as_ref(),
        };
        let Some(input) = input else { return };
        let input = input.clone();
        // The track gets the faded audio in the device's own format.
        let faded;
        let sb = match &env {
            Some(env) => match crate::mixer::scaled(sb, env) {
                Some(b) => {
                    faded = b;
                    &*faded
                }
                None => {
                    self.stats.audio_drops += 1;
                    return;
                }
            },
            None => sb,
        };
        // AVAssetWriter lays audio buffers end to end whatever their
        // timestamps say, so keep the track on the capture clock here: fill
        // a gap (late start, dropped device buffer) with silence and trim
        // an overlap, both sample-accurate. Error stays under SNAP_SECS.
        let start = self.audio_next[i].unwrap_or(t0 as f64 / TIMESCALE as f64);
        let gap = out_s - start;
        let (mut i0, mut pad) = (i0, 0usize);
        if gap > SNAP_SECS {
            pad = (gap * rate).round() as usize;
        } else if gap < -SNAP_SECS {
            i0 += (-gap * rate).round() as usize;
            if i0 >= i1 {
                return;
            }
        }
        let at = |t: f64| unsafe { CMTime::new((t * TIMESCALE as f64).round() as i64, TIMESCALE) };
        let mut bufs = vec![];
        if pad > 0 {
            match silence_like(sb, pad, at(start)) {
                Some(b) => bufs.push(b),
                None => {
                    self.stats.audio_drops += 1;
                    return;
                }
            }
            self.stats.audio_gaps_filled += 1;
        }
        // The writer packs this buffer at `start`, whatever its timestamp.
        let body_start = start + pad as f64 / rate;
        match retime(sb, i0, i1, ((real_s + i0 as f64 / rate - body_start) * TIMESCALE as f64).round() as i64) {
            Some(b) => bufs.push(b),
            None => {
                self.stats.audio_drops += 1;
                return;
            }
        }
        self.audio_next[i] = Some(body_start + (i1 - i0) as f64 / rate);
        for b in bufs {
            unsafe {
                if input.isReadyForMoreMediaData() && input.appendSampleBuffer(&b) {
                    match kind {
                        AudioKind::System => self.stats.audio_sys_buffers += 1,
                        AudioKind::Mic => self.stats.audio_mic_buffers += 1,
                    }
                } else {
                    self.stats.audio_drops += 1;
                    self.check_writer();
                }
            }
        }
    }

    fn drain_mix(&mut self, flush: bool) {
        let Some(input) = self.out.as_ref().and_then(|o| o.mix_input.clone()) else { return };
        let Some(m) = self.mixer.as_mut() else { return };
        while let Some(buf) = m.pull(flush) {
            unsafe {
                if input.isReadyForMoreMediaData() && input.appendSampleBuffer(&buf) {
                    self.stats.audio_mix_buffers += 1;
                } else {
                    self.stats.audio_drops += 1;
                }
            }
        }
        self.stats.audio_clipped_samples = m.clipped;
        self.check_writer();
    }

    /// Latest capture time the next slot to be emitted may show.
    fn due_time(&self) -> f64 {
        match self.t0 {
            Some(_) => self.slot_ticks(self.next_slot) as f64 / TIMESCALE as f64 + 0.5 / self.fps as f64,
            None => f64::INFINITY,
        }
    }

    /// Bound a frame queue. A dropped frame becomes the current one only
    /// if it is due by the next slot: when emission runs behind, a newer
    /// frame must not be shown under an earlier timestamp (that put
    /// content up to several frames early); it is skipped instead.
    fn cap(q: &mut VecDeque<(f64, Pb)>, last: &mut Option<Pb>, max: usize, due: f64) {
        while q.len() > max {
            let (pts, pb) = q.pop_front().unwrap();
            if pts <= due || last.is_none() {
                *last = Some(pb);
            }
        }
    }

    /// Pick the newest queued frame captured at or before `t`.
    fn pick(q: &mut VecDeque<(f64, Pb)>, last: &mut Option<Pb>, t: f64) {
        while let Some((pts, _)) = q.front() {
            if *pts <= t {
                *last = q.pop_front().map(|x| x.1);
            } else {
                break;
            }
        }
    }

    fn new_output_buffer(&self) -> Option<CFRetained<CVPixelBuffer>> {
        let mut buf = std::ptr::null_mut();
        unsafe {
            match (&self.out, &self.own_pool) {
                (Some(o), _) => {
                    let pool = o.adaptor.pixelBufferPool()?;
                    CVPixelBufferPool::create_pixel_buffer(None, &pool, std::ptr::NonNull::from(&mut buf));
                }
                (None, Some(pool)) => {
                    CVPixelBufferPool::create_pixel_buffer(None, pool, std::ptr::NonNull::from(&mut buf));
                }
                (None, None) => return None,
            }
            std::ptr::NonNull::new(buf).map(|b| CFRetained::from_raw(b))
        }
    }

    /// Emit every grid slot whose time is at or before `until` (host seconds).
    // `compositor.unwrap()` follows an `is_some` check because the output
    // buffer must be taken (borrowing self) before the compositor is borrowed.
    #[allow(clippy::unnecessary_unwrap)]
    pub fn emit_until(&mut self, until: f64) {
        if self.t0.is_none() || self.failed.is_some() {
            return;
        }
        let half = 0.5 / self.fps as f64;
        loop {
            let slot = self.next_slot;
            let slot_t = self.slot_ticks(slot) as f64 / TIMESCALE as f64;
            if slot_t > until {
                break;
            }
            // Encoder busy (e.g. a GPU load spike): retry this slot later
            // unless it is already too far behind.
            if self.pauses.offset_at(slot as f64).is_some()
                && let Some(o) = &self.out
                && !unsafe { o.video.isReadyForMoreMediaData() }
                && slot_t > until - MAX_BACKLOG_SECS
            {
                self.stats.video_retries += 1;
                break;
            }
            let before = self.screen_q.len();
            Self::pick(&mut self.screen_q, &mut self.screen_last, slot_t + half);
            let fresh = self.screen_q.len() != before;
            Self::pick(&mut self.cam_q, &mut self.cam_last, slot_t + half);
            self.next_slot += 1;
            let write = self.pauses.offset_at(slot as f64).is_some() && self.limit.is_none_or(|l| self.out_slot < l);
            if !write {
                self.stats.paused_slots += 1;
            }
            let want_preview = self.preview.is_some() && slot_t - self.last_preview >= 1.0 / 30.0;
            // The replay buffer and streams get every frame, paused or not.
            let want_live = self.live.as_ref().is_some_and(|l| l.active());
            if !write && !want_preview && !want_live {
                continue;
            }
            let mut write = write;
            let mut screen = self.screen_last.as_ref().map(|s| s.0.clone());
            // A native-size capture going through the compositor: scale it
            // first (the compositor then draws it 1:1).
            if let (Some(sc), Some(pool), Some(src)) = (self.scaler.as_mut(), &self.scaled_pool, screen.as_deref())
                && sc.needed(src)
            {
                let mut buf = std::ptr::null_mut();
                unsafe { CVPixelBufferPool::create_pixel_buffer(None, pool, std::ptr::NonNull::from(&mut buf)) };
                if let Some(b) = std::ptr::NonNull::new(buf) {
                    let b: CFRetained<CVPixelBuffer> = unsafe { CFRetained::from_raw(b) };
                    if sc.scale(src, &b) {
                        screen = Some(b);
                    }
                }
            }
            if screen.is_none() && self.scene != Scene::Cam {
                continue;
            }
            if write
                && let Some(o) = &self.out
                && !unsafe { o.video.isReadyForMoreMediaData() }
            {
                self.stats.video_drops += 1;
                self.out_slot += 1;
                self.check_writer();
                write = false;
                if !want_preview && !want_live {
                    continue;
                }
            }
            let frame: CFRetained<CVPixelBuffer> = if self.compositor.is_some() {
                let Some(buf) = self.new_output_buffer() else {
                    if write {
                        self.stats.video_drops += 1;
                        self.out_slot += 1;
                    }
                    continue;
                };
                let cam = self.cam_last.as_ref().map(|b| b.0.clone());
                let scene = self.scene;
                self.compositor.as_mut().unwrap().render(scene, screen.as_deref(), cam.as_deref(), &buf);
                buf
            } else if self.scaled_pool.is_none()
                && let Some(sc) = &self.scaler
                && sc.needed(screen.as_deref().unwrap())
            {
                let Some(buf) = self.new_output_buffer() else {
                    if write {
                        self.stats.video_drops += 1;
                        self.out_slot += 1;
                    }
                    continue;
                };
                self.scaler.as_mut().unwrap().scale(screen.as_deref().unwrap(), &buf);
                buf
            } else {
                screen.unwrap()
            };
            if write {
                let pts = unsafe { CMTime::new(self.slot_ticks(self.out_slot), TIMESCALE) };
                self.out_slot += 1;
                if let Some(o) = &self.out {
                    if unsafe { o.adaptor.appendPixelBuffer_withPresentationTime(&frame, pts) } {
                        self.stats.video_frames_out += 1;
                        if !fresh {
                            self.stats.video_dups += 1;
                        }
                    } else {
                        self.stats.video_drops += 1;
                        self.check_writer();
                    }
                } else {
                    self.stats.video_frames_out += 1;
                }
            }
            if want_live && let Some(l) = &self.live {
                l.video(&frame, slot_t, self.fps as u32);
            }
            if want_preview && let Some(p) = &self.preview {
                self.last_preview = slot_t;
                p(frame);
            }
        }
    }

    fn check_writer(&mut self) {
        let Some(o) = &self.out else { return };
        unsafe {
            if o.writer.status() == AVAssetWriterStatus::Failed && self.failed.is_none() {
                self.failed = Some(err_text(&o.writer));
            }
        }
    }

    /// Flush video up to `end` (host seconds), trim all tracks there, and
    /// finalize the file. Blocks until the movie is written. Preview-only
    /// sessions just return their stats.
    pub fn finish(mut self, end: f64) -> Result<Stats> {
        if self.out.is_none() {
            return Ok(self.stats.clone());
        }
        if self.t0.is_none() {
            unsafe { self.out.as_ref().unwrap().writer.cancelWriting() };
            bail!("no screen frames were captured, nothing written");
        }
        self.preview = None;
        self.emit_until(end);
        self.drain_mix(true);
        let end_t = unsafe { CMTime::new(self.slot_ticks(self.out_slot), TIMESCALE) };
        let o = self.out.as_ref().unwrap();
        unsafe {
            o.writer.endSessionAtSourceTime(end_t);
            o.video.markAsFinished();
            for a in [&o.sys, &o.mic, &o.mix_input].into_iter().flatten() {
                a.markAsFinished();
            }
            let (tx, rx) = mpsc::channel();
            let block = RcBlock::new(move || {
                let _ = tx.send(());
            });
            o.writer.finishWritingWithCompletionHandler(&block);
            rx.recv_timeout(Duration::from_secs(60))
                .context("timed out finalizing movie")?;
            if o.writer.status() != AVAssetWriterStatus::Completed {
                bail!("writing failed: {}", err_text(&o.writer));
            }
        }
        Ok(self.stats.clone())
    }
}

fn open_output(path: &Path, v: &VideoSpec, system_audio: bool, mic: bool, layout: AudioLayout) -> Result<Out> {
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    let abs = std::path::absolute(path)?;
    let file_type = match path.extension().and_then(|e| e.to_str()) {
        Some("mp4") | Some("m4v") => unsafe { AVFileTypeMPEG4 }.unwrap(),
        _ => unsafe { AVFileTypeQuickTimeMovie }.unwrap(),
    };
    unsafe {
        let url = NSURL::fileURLWithPath(&ns(abs.to_str().context("non-UTF8 path")?));
        let writer = AVAssetWriter::initWithURL_fileType_error(AVAssetWriter::alloc(), &url, file_type)
            .map_err(|e| anyhow!("AVAssetWriter: {}", e.localizedDescription()))?;
        // Fragmented writing: a crash or power loss leaves a playable file
        // up to the last fragment (at most 2 s, one keyframe interval, old).
        writer.setMovieFragmentInterval(CMTime::new(2 * TIMESCALE as i64, TIMESCALE));

        let video = AVAssetWriterInput::initWithMediaType_outputSettings(
            AVAssetWriterInput::alloc(),
            AVMediaTypeVideo.unwrap(),
            Some(&video_settings(v)),
        );
        video.setExpectsMediaDataInRealTime(true);
        video.setMediaTimeScale(TIMESCALE);
        let attrs = dict(vec![
            (cf_ns(kCVPixelBufferPixelFormatTypeKey), num_i(v.pixel_format as i64)),
            (cf_ns(kCVPixelBufferWidthKey), num_i(v.width as i64)),
            (cf_ns(kCVPixelBufferHeightKey), num_i(v.height as i64)),
            (cf_ns(kCVPixelBufferIOSurfacePropertiesKey), obj(&*dict(vec![]))),
        ]);
        let adaptor = AVAssetWriterInputPixelBufferAdaptor::initWithAssetWriterInput_sourcePixelBufferAttributes(
            AVAssetWriterInputPixelBufferAdaptor::alloc(),
            &video,
            Some(&attrs),
        );
        if !writer.canAddInput(&video) {
            bail!("cannot add video input (unsupported codec/size {}x{}?)", v.width, v.height);
        }
        writer.addInput(&video);

        let media_audio = AVMediaTypeAudio.unwrap();
        let mk_audio = |ch: i64, br: i64| -> Result<Retained<AVAssetWriterInput>> {
            let a = AVAssetWriterInput::initWithMediaType_outputSettings(
                AVAssetWriterInput::alloc(),
                media_audio,
                Some(&audio_settings(ch, br)),
            );
            a.setExpectsMediaDataInRealTime(true);
            if !writer.canAddInput(&a) {
                bail!("cannot add audio input");
            }
            writer.addInput(&a);
            Ok(a)
        };
        // Track order (documented in README): mix, system, mic.
        let both = system_audio && mic;
        let want_mix = both && layout != AudioLayout::Stems;
        let want_stems = !both || layout != AudioLayout::Mix;
        let mix_input = if want_mix { Some(mk_audio(2, 256_000)?) } else { None };
        let sys = if system_audio && want_stems { Some(mk_audio(2, 256_000)?) } else { None };
        let mic = if mic && want_stems { Some(mk_audio(1, 192_000)?) } else { None };
        if let (Some(mixi), true) = (&mix_input, want_stems) {
            // One alternate group, mix as default: QuickTime/AVPlayer play
            // only the mix; editors still see every track.
            let mut members: Vec<&AVAssetWriterInput> = vec![mixi];
            members.extend(sys.as_deref());
            members.extend(mic.as_deref());
            let arr = objc2_foundation::NSArray::from_slice(&members);
            let group =
                AVAssetWriterInputGroup::initWithInputs_defaultInput(AVAssetWriterInputGroup::alloc(), &arr, Some(mixi));
            if writer.canAddInputGroup(&group) {
                writer.addInputGroup(&group);
            } else {
                bail!("cannot add audio track group");
            }
        }
        if !writer.startWriting() {
            bail!("startWriting failed: {}", err_text(&writer));
        }
        Ok(Out { writer, video, adaptor, sys, mic, mix_input })
    }
}

/// `frames` of silence in the same format as `like`, starting at `pts`.
fn silence_like(like: &CMSampleBuffer, frames: usize, pts: CMTime) -> Option<CFRetained<CMSampleBuffer>> {
    use objc2_core_media::*;
    unsafe {
        let fmt = like.format_description()?;
        let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(&fmt).as_ref()?;
        let non_inter = asbd.mFormatFlags & objc2_core_audio_types::kAudioFormatFlagIsNonInterleaved != 0;
        let per_frame = asbd.mBytesPerFrame as usize * if non_inter { asbd.mChannelsPerFrame as usize } else { 1 };
        let bytes = frames * per_frame;
        if bytes == 0 {
            return None;
        }
        let mut block: *mut CMBlockBuffer = std::ptr::null_mut();
        if CMBlockBuffer::create_with_memory_block(
            None,
            std::ptr::null_mut(),
            bytes,
            None,
            std::ptr::null(),
            0,
            bytes,
            kCMBlockBufferAssureMemoryNowFlag,
            std::ptr::NonNull::from(&mut block),
        ) != 0
        {
            return None;
        }
        let block = CFRetained::from_raw(std::ptr::NonNull::new(block)?);
        CMBlockBuffer::fill_data_bytes(0, &block, 0, bytes);
        let mut out: *mut CMSampleBuffer = std::ptr::null_mut();
        let fd: &CMAudioFormatDescription = &*(&*fmt as *const CMFormatDescription as *const CMAudioFormatDescription);
        if CMAudioSampleBufferCreateReadyWithPacketDescriptions(
            None,
            &block,
            fd,
            frames as isize,
            pts,
            std::ptr::null(),
            std::ptr::NonNull::from(&mut out),
        ) != 0
        {
            return None;
        }
        std::ptr::NonNull::new(out).map(|b| CFRetained::from_raw(b))
    }
}

/// Copy of a sample buffer with timestamps moved `shift` ticks
/// (1/TIMESCALE s) earlier.
pub fn shifted(sb: &CMSampleBuffer, shift: i64) -> Option<CFRetained<CMSampleBuffer>> {
    let n = unsafe { sb.num_samples() } as usize;
    retime(sb, 0, n, shift)
}

/// Copy samples `[i0, i1)` of an audio buffer with timestamps moved `shift`
/// ticks earlier.
fn retime(sb: &CMSampleBuffer, i0: usize, i1: usize, shift: i64) -> Option<CFRetained<CMSampleBuffer>> {
    unsafe {
        let part;
        let sb = if (i0, i1) == (0, sb.num_samples() as usize) {
            sb
        } else {
            let mut out: *mut CMSampleBuffer = std::ptr::null_mut();
            let range = objc2_core_foundation::CFRange { location: i0 as _, length: (i1 - i0) as _ };
            if CMSampleBuffer::copy_sample_buffer_for_range(None, sb, range, std::ptr::NonNull::from(&mut out)) != 0 {
                return None;
            }
            part = CFRetained::from_raw(std::ptr::NonNull::new(out)?);
            &*part
        };
        let pts = sb.presentation_time_stamp().subtract(CMTime::new(shift, TIMESCALE));
        let n = sb.num_samples().max(1);
        let dur = sb.duration();
        let per = CMTime::new(dur.value / n as i64, dur.timescale);
        let timing = objc2_core_media::CMSampleTimingInfo {
            duration: per,
            presentationTimeStamp: pts,
            decodeTimeStamp: objc2_core_media::kCMTimeInvalid,
        };
        let mut out: *mut CMSampleBuffer = std::ptr::null_mut();
        if CMSampleBuffer::create_copy_with_new_timing(None, sb, 1, &timing, std::ptr::NonNull::from(&mut out)) != 0 {
            return None;
        }
        std::ptr::NonNull::new(out).map(|b| CFRetained::from_raw(b))
    }
}

fn err_text(w: &AVAssetWriter) -> String {
    unsafe {
        w.error()
            .map(|e| {
                let mut s = e.localizedDescription().to_string();
                if let Some(r) = e.localizedFailureReason() {
                    s.push_str(&format!(" ({r})"));
                }
                s.push_str(&format!(" [{} {}]", e.domain(), e.code()));
                s
            })
            .unwrap_or_else(|| "unknown error".into())
    }
}

#[cfg(test)]
mod tests {
    /// Color levels through the encoder for BGRA frames untagged, tagged
    /// sRGB (the encoder converts those to the BT.709 curve: darker
    /// midtones) and BT.709 (a camera's tags). PROBE_CAM=1 sends them
    /// through the compositor as the camera first, PROBE_SCREEN=1 as the
    /// screen. Run with --ignored --nocapture; prints decoded levels.
    #[test]
    #[ignore]
    fn bgra_encode_levels() {
        use objc2_core_video::*;
        let vals = [32u8, 64, 128, 192, 235];
        let dir = std::env::temp_dir();
        unsafe {
            for (name, tag) in [
                ("untagged", None),
                ("sRGB", Some((kCVImageBufferColorPrimaries_ITU_R_709_2, kCVImageBufferTransferFunction_sRGB))),
                ("709", Some((kCVImageBufferColorPrimaries_ITU_R_709_2, kCVImageBufferTransferFunction_ITU_R_709_2))),
            ] {
                let path = dir.join(format!("rec-probe-{name}.mov"));
                let _ = std::fs::remove_file(&path);
                let spec = super::VideoSpec { width: 320, height: 64, fps: 30, codec: crate::writer::Codec::H264, bitrate: 20_000_000, pixel_format: crate::util::PIX_BGRA };
                let o = super::open_output(&path, &spec, false, false, super::AudioLayout::Both).unwrap();
                o.writer.startSessionAtSourceTime(objc2_core_media::CMTime::new(0, 30));
                let src = crate::compose::color_probe::stripes(320, 64, &vals, tag);
                // Through the compositor as the camera (full frame) or as is.
                let f = if std::env::var_os("PROBE_CAM").is_some() {
                    let out = crate::compose::color_probe::stripes(320, 64, &[0], None);
                    let pip = crate::compose::PipSpec { mirror: false, ..Default::default() };
                    crate::compose::Compositor::new(320, 64, pip).render(crate::compose::Scene::Cam, None, Some(&src), &out);
                    let tf = out.attachment(kCVImageBufferTransferFunctionKey, std::ptr::null_mut());
                    println!("compositor output transfer tag: {:?}", tf);
                    out
                } else if std::env::var_os("PROBE_SCREEN").is_some() {
                    let out = crate::compose::color_probe::stripes(320, 64, &[0], None);
                    crate::compose::Compositor::new(320, 64, Default::default()).render(crate::compose::Scene::Screen, Some(&src), None, &out);
                    out
                } else {
                    src
                };
                for i in 0..10 {
                    while !o.video.isReadyForMoreMediaData() {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    assert!(o.adaptor.appendPixelBuffer_withPresentationTime(&f, objc2_core_media::CMTime::new(i, 30)));
                }
                o.video.markAsFinished();
                let (tx, rx) = std::sync::mpsc::channel();
                let block = block2::RcBlock::new(move || { let _ = tx.send(()); });
                o.writer.finishWritingWithCompletionHandler(&block);
                rx.recv().unwrap();
                let out = std::process::Command::new("ffmpeg")
                    .args(["-v", "error", "-i"]).arg(&path)
                    .args(["-frames:v", "1", "-vf", "extractplanes=y", "-f", "rawvideo", "-"])
                    .output().unwrap().stdout;
                let ys: Vec<u8> = (0..vals.len()).map(|i| out[32 * 320 + i * 64 + 32]).collect();
                println!("{name}: rgb {vals:?} -> Y {ys:?} (full-range equivalent of BT.709 limited: {:?})",
                    vals.iter().map(|&v| 16 + (v as u32 * 219 + 127) / 255).collect::<Vec<_>>());
            }
        }
    }

    use super::Pauses;

    #[test]
    fn no_pauses_means_no_offset() {
        let p = Pauses::default();
        assert_eq!(p.offset_at(0.0), Some(0));
        assert_eq!(p.offset_at(1e6), Some(0));
        assert!(!p.is_paused());
    }

    #[test]
    fn pause_drops_span_and_shifts_later_captures() {
        let mut p = Pauses::default();
        assert!(p.pause(100));
        assert!(!p.pause(120), "double pause is a no-op");
        assert!(p.is_paused());
        assert_eq!(p.offset_at(99.9), Some(0));
        assert_eq!(p.offset_at(100.0), None);
        assert_eq!(p.offset_at(5000.0), None, "open span drops everything after it");
        assert!(p.resume(160));
        assert_eq!(p.offset_at(159.5), None);
        assert_eq!(p.offset_at(160.0), Some(60));
        assert_eq!(p.total(), 60);
        assert!(p.pause(200));
        assert!(p.resume(230));
        assert_eq!(p.offset_at(180.0), Some(60));
        assert_eq!(p.offset_at(231.0), Some(90));
        assert_eq!(p.total(), 90);
    }

    #[test]
    fn output_slots_stay_contiguous_across_pauses() {
        let mut p = Pauses::default();
        p.pause(10);
        p.resume(25);
        p.pause(30);
        p.resume(31);
        let out: Vec<i64> = (0..40).filter_map(|s| p.offset_at(s as f64).map(|o| s - o)).collect();
        assert_eq!(out, (0..out.len() as i64).collect::<Vec<_>>());
        assert_eq!(out.len(), 40 - 15 - 1);
    }

    #[test]
    fn keep_cuts_captures_at_pause_boundaries() {
        let mut p = Pauses::default();
        assert_eq!(p.keep(1.0, 2.0), Some((1.0, 2.0, 0)));
        p.pause(10);
        assert_eq!(p.keep(9.4, 10.6), Some((9.4, 10.0, 0)), "tail inside the pause is cut");
        assert_eq!(p.keep(10.0, 11.0), None);
        p.resume(20);
        assert_eq!(p.keep(19.5, 20.7), Some((20.0, 20.7, 10)), "head inside the pause is cut");
        assert_eq!(p.keep(30.0, 31.0), Some((30.0, 31.0, 10)));
        // A capture spanning a whole pause keeps its longer side.
        assert_eq!(p.keep(9.0, 20.5), Some((9.0, 10.0, 0)));
    }

    /// Overflowing the frame queue while emission runs behind must never
    /// make a frame from the future the current one (shown early).
    #[test]
    fn queue_cap_never_shows_future_frames() {
        use super::{Pb, Sink};
        use objc2_core_foundation::CFRetained;
        let pool = crate::util::make_pool(16, 16, crate::util::PIX_BGRA);
        let mut q: std::collections::VecDeque<(f64, Pb)> = Default::default();
        let mut last: Option<Pb> = None;
        let mut ids = std::collections::HashMap::new();
        for i in 0..12 {
            let mut buf = std::ptr::null_mut();
            unsafe { objc2_core_video::CVPixelBufferPool::create_pixel_buffer(None, &pool, std::ptr::NonNull::from(&mut buf)) };
            let pb: CFRetained<objc2_core_video::CVPixelBuffer> = unsafe { CFRetained::from_raw(std::ptr::NonNull::new(buf).unwrap()) };
            ids.insert(CFRetained::as_ptr(&pb).as_ptr() as usize, i);
            q.push_back((i as f64, crate::util::SendBox(pb)));
            // Emission is at frame 2: nothing newer than 2.5 may become current.
            Sink::cap(&mut q, &mut last, 4, 2.5);
        }
        let cur = ids[&(CFRetained::as_ptr(&last.as_ref().unwrap().0).as_ptr() as usize)];
        assert_eq!(cur, 2, "current frame stays due, later ones are skipped");
        assert_eq!(q.len(), 4);
    }

    #[test]
    fn instant_resume_cancels_the_pause() {
        let mut p = Pauses::default();
        p.pause(50);
        assert!(p.resume(50));
        assert!(!p.is_paused());
        assert_eq!(p.offset_at(60.0), Some(0));
        assert!(!p.resume(70), "resume without pause is a no-op");
    }
}
