//! Real-time audio mixer: system audio + microphone -> one stereo track.
//!
//! Each source's buffers are decoded to 48 kHz stereo f32 and placed on a
//! common sample timeline derived from their host-clock timestamps, so the mix
//! stays in sync with video. Per-source continuity snapping avoids clicks from
//! sub-millisecond timestamp jitter.

use std::collections::VecDeque;
use std::ptr::NonNull;

use objc2_core_audio_types::*;
use objc2_core_foundation::CFRetained;
use objc2_core_media::*;

use crate::util::*;
use crate::writer::AudioKind;

pub const RATE: f64 = 48000.0;
/// Emit even if a source is silent/stalled for longer than this.
const MAX_WAIT: i64 = 24000; // 0.5 s
/// Treat timestamp jumps up to this many samples as continuous.
const SNAP: i64 = 96; // 2 ms

pub struct Mixer {
    t0: i64, // session start in samples on the host clock
    base: i64,
    buf: VecDeque<[f32; 2]>,
    horizon: [Option<i64>; 2],
    active: [bool; 2],
    fmt: CFRetained<CMAudioFormatDescription>,
    /// Samples that would have clipped (the limiter held them back).
    pub clipped: u64,
    resamplers: [Option<Resampler>; 2],
    limiter: Limiter,
}

unsafe impl Send for Mixer {}

fn idx(k: AudioKind) -> usize {
    match k {
        AudioKind::System => 0,
        AudioKind::Mic => 1,
    }
}

/// LPCM sample encodings.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Sample {
    F32,
    F64,
    I16,
    I24,
    I32,
    /// Read as silence, never written.
    Other,
}

impl Sample {
    fn of(asbd: &AudioStreamBasicDescription) -> Sample {
        match (asbd.mFormatFlags & kAudioFormatFlagIsFloat != 0, asbd.mBitsPerChannel) {
            (true, 32) => Sample::F32,
            (true, 64) => Sample::F64,
            (false, 16) => Sample::I16,
            (false, 24) => Sample::I24,
            (false, 32) => Sample::I32,
            _ => Sample::Other,
        }
    }

    fn size(self) -> usize {
        match self {
            Sample::F64 => 8,
            Sample::F32 | Sample::I32 => 4,
            Sample::I24 => 3,
            Sample::I16 => 2,
            Sample::Other => 0,
        }
    }

    /// # Safety
    /// `p` points at `self.size()` readable bytes.
    unsafe fn read(self, p: *const u8) -> f32 {
        unsafe {
            match self {
                Sample::F32 => (p as *const f32).read_unaligned(),
                Sample::F64 => (p as *const f64).read_unaligned() as f32,
                Sample::I16 => (p as *const i16).read_unaligned() as f32 / 32768.0,
                Sample::I32 => (p as *const i32).read_unaligned() as f32 / 2147483648.0,
                Sample::I24 => {
                    let v = ((*p as i32) | ((*p.add(1) as i32) << 8) | ((*p.add(2) as i32) << 16)) << 8 >> 8;
                    v as f32 / 8388608.0
                }
                Sample::Other => 0.0,
            }
        }
    }

    /// Samples saturate at full scale (a boosted source's own track has
    /// no limiter).
    /// # Safety
    /// `p` points at `self.size()` writable bytes.
    unsafe fn write(self, p: *mut u8, v: f32) {
        let int = |full: f64| (v as f64 * full).round().clamp(-full, full - 1.0) as i32;
        unsafe {
            match self {
                Sample::F32 => (p as *mut f32).write_unaligned(v.clamp(-1.0, 1.0)),
                Sample::F64 => (p as *mut f64).write_unaligned(v.clamp(-1.0, 1.0) as f64),
                Sample::I16 => (p as *mut i16).write_unaligned(int(32768.0) as i16),
                Sample::I32 => (p as *mut i32).write_unaligned(int(2147483648.0)),
                Sample::I24 => {
                    let b = int(8388608.0).to_le_bytes();
                    std::ptr::copy_nonoverlapping(b.as_ptr(), p, 3);
                }
                Sample::Other => {}
            }
        }
    }
}

/// Run `f` on a PCM sample buffer's format, audio buffers and frame count
/// (None for compressed or empty buffers).
fn with_pcm<R>(sb: &CMSampleBuffer, f: impl FnOnce(&AudioStreamBasicDescription, &[AudioBuffer], usize) -> R) -> Option<R> {
    unsafe {
        let fd = sb.format_description()?;
        let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(&fd).as_ref()?;
        if asbd.mFormatID != kAudioFormatLinearPCM {
            return None;
        }
        let frames = sb.num_samples() as usize;
        if frames == 0 {
            return None;
        }
        let mut needed = 0usize;
        sb.audio_buffer_list_with_retained_block_buffer(&mut needed, std::ptr::null_mut(), 0, None, None, 0, std::ptr::null_mut());
        if needed == 0 {
            return None;
        }
        let mut storage = vec![0u64; needed.div_ceil(8)];
        let abl = storage.as_mut_ptr() as *mut AudioBufferList;
        let mut block: *mut CMBlockBuffer = std::ptr::null_mut();
        let st = sb.audio_buffer_list_with_retained_block_buffer(
            std::ptr::null_mut(),
            abl,
            needed,
            None,
            None,
            1, // kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment
            &mut block,
        );
        if st != 0 {
            return None;
        }
        let _block = NonNull::new(block).map(|b| CFRetained::from_raw(b));
        let nbuf = (*abl).mNumberBuffers as usize;
        let bufs = std::slice::from_raw_parts((*abl).mBuffers.as_ptr(), nbuf);
        if bufs.iter().any(|b| b.mData.is_null()) {
            return None;
        }
        Some(f(asbd, bufs, frames))
    }
}

/// Channels interleaved in each audio buffer.
fn channels_per_buffer(asbd: &AudioStreamBasicDescription) -> usize {
    if asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0 { 1 } else { asbd.mChannelsPerFrame.max(1) as usize }
}

/// Decode a PCM sample buffer to (pts seconds, sample rate, stereo frames).
pub fn decode(sb: &CMSampleBuffer) -> Option<(f64, f64, Vec<[f32; 2]>)> {
    let pts = secs(unsafe { sb.presentation_time_stamp() });
    with_pcm(sb, |asbd, bufs, frames| {
        let fmt = Sample::of(asbd);
        let ch = channels_per_buffer(asbd);
        let read = |b: &AudioBuffer, i: usize| -> f32 {
            if (i + 1) * fmt.size() > b.mDataByteSize as usize {
                return 0.0;
            }
            unsafe { fmt.read((b.mData as *const u8).add(i * fmt.size())) }
        };
        let mut out = Vec::with_capacity(frames);
        for f in 0..frames {
            let (l, r) = if ch == 1 {
                let l = read(&bufs[0], f);
                let r = if bufs.len() > 1 { read(&bufs[1], f) } else { l };
                (l, r)
            } else {
                (read(&bufs[0], f * ch), read(&bufs[0], f * ch + 1))
            };
            out.push([l, r]);
        }
        (pts, asbd.mSampleRate, out)
    })
}

/// Copy of a PCM sample buffer, in its own format and with its timestamp,
/// with frame `k` multiplied by `env[k]` (samples saturate at full scale).
/// None for formats it can't write.
pub fn scaled(sb: &CMSampleBuffer, env: &[f32]) -> Option<CFRetained<CMSampleBuffer>> {
    let (fd, pts) = unsafe { (sb.format_description()?, sb.presentation_time_stamp()) };
    let (data, frames) = with_pcm(sb, |asbd, bufs, frames| {
        let fmt = Sample::of(asbd);
        if fmt == Sample::Other {
            return None;
        }
        let ch = channels_per_buffer(asbd);
        // The buffers end to end: how a block buffer holds non-interleaved
        // channels.
        let mut data: Vec<u8> = Vec::new();
        for b in bufs {
            let start = data.len();
            data.extend_from_slice(unsafe { std::slice::from_raw_parts(b.mData as *const u8, b.mDataByteSize as usize) });
            let buf = &mut data[start..];
            for (f, g) in env.iter().enumerate().take(frames) {
                for c in 0..ch {
                    let at = (f * ch + c) * fmt.size();
                    if at + fmt.size() <= buf.len() {
                        unsafe {
                            let p = buf.as_mut_ptr().add(at);
                            fmt.write(p, fmt.read(p) * g);
                        }
                    }
                }
            }
        }
        Some((data, frames))
    })??;
    let fd: &CMAudioFormatDescription = unsafe { &*(&*fd as *const CMFormatDescription as *const CMAudioFormatDescription) };
    sample_buffer(fd, &data, frames, pts)
}

/// Fader travel in dB. The bottom is silence (shown as -inf), the top a
/// boost for quiet sources.
pub const MIN_DB: f32 = -60.0;
pub const MAX_DB: f32 = 24.0;

/// A source's level as set on its fader: gain in dB, and mute.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Volume {
    pub db: f32,
    pub muted: bool,
}

impl Volume {
    pub const UNITY: Volume = Volume { db: 0.0, muted: false };

    /// Linear gain of the fader alone: silence at or below MIN_DB.
    pub fn fader_gain(self) -> f32 {
        if self.db > MIN_DB { 10f32.powf(self.db.min(MAX_DB) / 20.0) } else { 0.0 }
    }

    /// Linear gain applied to the source's audio.
    pub fn gain(self) -> f32 {
        if self.muted { 0.0 } else { self.fader_gain() }
    }
}

/// How long a gain change takes to complete.
const RAMP_SECS: f32 = 0.02;

/// Applies a source's gain. A new gain is reached over RAMP_SECS instead
/// of at once, so moving a fader or muting never clicks.
pub struct Fader {
    gain: f32,
    target: f32,
    /// Size of the change in progress, which sets its pace for every
    /// buffer it spans.
    span: f32,
}

impl Fader {
    pub fn new(gain: f32) -> Self {
        Fader { gain, target: gain, span: 0.0 }
    }

    pub fn set(&mut self, gain: f32) {
        if gain != self.target {
            self.target = gain;
            self.span = (gain - self.gain).abs();
        }
    }

    /// The gain for each of the next `n` frames at `rate`, or None while it
    /// is unity (the audio passes through untouched).
    pub fn envelope(&mut self, n: usize, rate: f64) -> Option<Vec<f32>> {
        if self.gain == 1.0 && self.target == 1.0 {
            return None;
        }
        // A floor so that a tiny change still lands.
        let step = (self.span / (RAMP_SECS * rate.max(1.0) as f32)).max(1e-5);
        Some(
            (0..n)
                .map(|_| {
                    let d = self.target - self.gain;
                    self.gain = if d.abs() <= step { self.target } else { self.gain + step.copysign(d) };
                    self.gain
                })
                .collect(),
        )
    }
}

/// Taps on each side of a resampled output sample.
const HALF_TAPS: usize = 24;

/// Streaming sinc resampler to 48 kHz for one source (e.g. a 44.1 kHz USB
/// mic or a 16/24 kHz Bluetooth headset). Kaiser-windowed sinc, low-passed
/// below the lower Nyquist rate, with its filter history and fractional
/// position carried across buffers, so buffer boundaries are seamless.
struct Resampler {
    from: f64,
    /// Input samples not yet fully used, starting at absolute input index
    /// `first`.
    hist: Vec<[f32; 2]>,
    first: i64,
    /// Absolute input position of the next output sample.
    pos: f64,
    /// Host time (seconds) of input sample 0.
    t0: f64,
    /// Absolute input index the next buffer is expected to start at.
    next_in: i64,
}

fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term) = (1.0, 1.0);
    for k in 1..32 {
        term *= (x / (2.0 * k as f64)).powi(2);
        sum += term;
    }
    sum
}

impl Resampler {
    fn new(from: f64, t0: f64) -> Self {
        Resampler { from, hist: vec![], first: 0, pos: 0.0, t0, next_in: 0 }
    }

    /// Resample a buffer starting at host time `pts`. Returns the output
    /// frames and the host time of the first one. A buffer that does not
    /// continue the stream (gap, overlap, rate change) restarts it.
    fn push(&mut self, pts: f64, rate: f64, src: Vec<[f32; 2]>) -> (f64, Vec<[f32; 2]>) {
        let expected = self.t0 + self.next_in as f64 / self.from;
        if rate != self.from || (pts - expected).abs() > SNAP as f64 / RATE {
            *self = Resampler::new(rate, pts);
        }
        self.next_in += src.len() as i64;
        self.hist.extend(src);
        let step = self.from / RATE;
        // Cutoff (cycles per input sample): below both Nyquist rates.
        let fc = 0.5 * (RATE / self.from).min(1.0) * 0.94;
        let beta = 8.0;
        let norm = bessel_i0(beta);
        let mut out = vec![];
        let t_out = self.t0 + self.pos / self.from;
        let h = HALF_TAPS as f64 / (2.0 * fc).min(1.0);
        let reach = h.ceil() as i64;
        // Taps before the stream's start read as silence.
        while (self.pos.floor() as i64) + reach < self.first + self.hist.len() as i64 {
            let c = self.pos.floor() as i64;
            let (mut l, mut r, mut wsum) = (0.0f64, 0.0f64, 0.0f64);
            for j in (c - reach + 1)..=(c + reach) {
                let x = j as f64 - self.pos;
                if x.abs() >= h {
                    continue;
                }
                let arg = 2.0 * fc * x;
                let sinc = if arg.abs() < 1e-12 { 1.0 } else { (std::f64::consts::PI * arg).sin() / (std::f64::consts::PI * arg) };
                let w = bessel_i0(beta * (1.0 - (x / h).powi(2)).max(0.0).sqrt()) / norm;
                let k = 2.0 * fc * sinc * w;
                wsum += k;
                if j >= self.first {
                    let s = self.hist[(j - self.first) as usize];
                    l += k * s[0] as f64;
                    r += k * s[1] as f64;
                }
            }
            // Unity gain at DC whatever the fractional phase.
            let g = if wsum > 0.0 { 1.0 / wsum } else { 0.0 };
            out.push([(l * g) as f32, (r * g) as f32]);
            self.pos += step;
        }
        let keep_from = (self.pos.floor() as i64 - reach).max(self.first);
        let drop = (keep_from - self.first) as usize;
        self.hist.drain(..drop.min(self.hist.len()));
        self.first += drop as i64;
        (t_out, out)
    }
}

/// Mix bus limiter: keeps summed sources under the ceiling with smooth gain
/// changes (fast attack, slow release) and a soft knee for the rare peak
/// the envelope misses, instead of clipping them flat.
struct Limiter {
    gain: f32,
}

/// -1 dBFS: headroom for the AAC encoder, whose output overshoots its
/// input peaks slightly.
const CEILING: f32 = 0.891;
const ATTACK: f32 = 0.35; // per sample, ~0.06 ms
const RELEASE: f32 = 1.0 / (0.15 * RATE as f32); // full recovery in ~150 ms

impl Limiter {
    fn process(&mut self, fr: [f32; 2]) -> [f32; 2] {
        let peak = fr[0].abs().max(fr[1].abs());
        let want = if peak > CEILING { CEILING / peak } else { 1.0 };
        self.gain = if want < self.gain { self.gain + (want - self.gain) * ATTACK } else { (self.gain + RELEASE).min(want) };
        fr.map(|s| soft_clip(s * self.gain))
    }
}

/// Identity below the knee, then a smooth curve that never passes 1.0.
fn soft_clip(s: f32) -> f32 {
    const KNEE: f32 = 0.9;
    let a = s.abs();
    if a <= KNEE {
        return s;
    }
    let room = 1.0 - KNEE;
    (KNEE + room * ((a - KNEE) / room).tanh()).copysign(s)
}

impl Mixer {
    pub fn new(t0_secs: f64, system: bool, mic: bool) -> Self {
        Mixer {
            t0: (t0_secs * RATE).round() as i64,
            base: 0,
            buf: VecDeque::new(),
            horizon: [None, None],
            active: [system, mic],
            fmt: stereo_format(),
            clipped: 0,
            resamplers: [None, None],
            limiter: Limiter { gain: 1.0 },
        }
    }
    /// Add decoded frames that start at `pts` (seconds on the output timeline).
    pub fn push(&mut self, kind: AudioKind, pts: f64, rate: f64, frames: Vec<[f32; 2]>) {
        let i = idx(kind);
        let (pts, frames) = if (rate - RATE).abs() < 0.5 || rate <= 0.0 {
            self.resamplers[i] = None;
            (pts, frames)
        } else {
            self.resamplers[i].get_or_insert_with(|| Resampler::new(rate, pts)).push(pts, rate, frames)
        };
        if frames.is_empty() {
            return;
        }
        let mut start = (pts * RATE).round() as i64 - self.t0;
        if let Some(h) = self.horizon[i]
            && (start - h).abs() <= SNAP {
                start = h;
            }
        let end = start + frames.len() as i64;
        self.horizon[i] = Some(self.horizon[i].map_or(end, |h| h.max(end)));
        let need = (end - self.base).max(0) as usize;
        if self.buf.len() < need {
            self.buf.resize(need, [0.0, 0.0]);
        }
        for (n, fr) in frames.iter().enumerate() {
            let pos = start + n as i64 - self.base;
            if pos < 0 {
                continue; // already emitted
            }
            let slot = &mut self.buf[pos as usize];
            slot[0] += fr[0];
            slot[1] += fr[1];
        }
    }

    /// Mixed frames ready (see `pull`) with their start time in host seconds.
    pub fn pull_frames(&mut self, flush: bool) -> Option<(f64, Vec<[f32; 2]>)> {
        let hs: Vec<i64> = (0..2).filter(|&i| self.active[i]).map(|i| self.horizon[i].unwrap_or(0)).collect();
        let max_h = hs.iter().copied().max().unwrap_or(0);
        let until = if flush { max_h } else { hs.iter().copied().min().unwrap_or(0).max(max_h - MAX_WAIT) };
        let n = (until - self.base).min(self.buf.len() as i64);
        if n <= 0 {
            return None;
        }
        let t = (self.t0 + self.base) as f64 / RATE;
        self.base += n;
        let mut frames: Vec<[f32; 2]> = self.buf.drain(..n as usize).collect();
        for fr in &mut frames {
            self.clipped += fr.iter().filter(|s| s.abs() > 1.0).count() as u64;
            *fr = self.limiter.process(*fr);
        }
        Some((t, frames))
    }

    /// Mixed PCM ready to encode: everything all active sources have reached,
    /// or anything older than MAX_WAIT behind the newest source.
    pub fn pull(&mut self, flush: bool) -> Option<CFRetained<CMSampleBuffer>> {
        let (t, frames) = self.pull_frames(flush)?;
        let data: Vec<f32> = frames.into_iter().flatten().collect();
        let pts = unsafe { CMTime::new((t * RATE).round() as i64, RATE as i32) };
        pcm_buffer(&self.fmt, data, pts)
    }
}

/// 48 kHz interleaved stereo f32 PCM.
pub fn stereo_format() -> CFRetained<CMAudioFormatDescription> {
    audio_format(&AudioStreamBasicDescription {
        mSampleRate: RATE,
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
        mBytesPerPacket: 8,
        mFramesPerPacket: 1,
        mBytesPerFrame: 8,
        mChannelsPerFrame: 2,
        mBitsPerChannel: 32,
        mReserved: 0,
    })
}

fn audio_format(asbd: &AudioStreamBasicDescription) -> CFRetained<CMAudioFormatDescription> {
    let mut fd: *const CMAudioFormatDescription = std::ptr::null();
    unsafe {
        CMAudioFormatDescriptionCreate(
            None,
            NonNull::from(asbd),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            None,
            NonNull::from(&mut fd),
        );
        CFRetained::from_raw(NonNull::new(fd as *mut CMAudioFormatDescription).expect("audio format"))
    }
}

/// Wrap interleaved stereo samples (`stereo_format`) starting at `pts` in a
/// sample buffer.
pub fn pcm_buffer(fmt: &CMAudioFormatDescription, data: Vec<f32>, pts: CMTime) -> Option<CFRetained<CMSampleBuffer>> {
    let bytes = unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 4) };
    sample_buffer(fmt, bytes, data.len() / 2, pts)
}

/// `frames` of PCM in `fmt`, copied from `data`, starting at `pts`.
fn sample_buffer(fmt: &CMAudioFormatDescription, data: &[u8], frames: usize, pts: CMTime) -> Option<CFRetained<CMSampleBuffer>> {
    let bytes = data.len();
    unsafe {
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
            NonNull::from(&mut block),
        ) != 0
        {
            return None;
        }
        let block = CFRetained::from_raw(NonNull::new(block)?);
        CMBlockBuffer::replace_data_bytes(NonNull::new(data.as_ptr() as *mut _)?, &block, 0, bytes);
        let mut out: *mut CMSampleBuffer = std::ptr::null_mut();
        if CMAudioSampleBufferCreateReadyWithPacketDescriptions(
            None,
            &block,
            fmt,
            frames as isize,
            pts,
            std::ptr::null(),
            NonNull::from(&mut out),
        ) != 0
        {
            return None;
        }
        Some(CFRetained::from_raw(NonNull::new(out)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pull_all(m: &mut Mixer, flush: bool) -> (Option<f64>, Vec<[f32; 2]>) {
        let mut first = None;
        let mut out = vec![];
        while let Some(b) = m.pull(flush) {
            let (pts, rate, frames) = decode(&b).expect("mix buffer decodes");
            assert_eq!(rate, RATE);
            first.get_or_insert(pts);
            out.extend(frames);
        }
        (first, out)
    }

    #[test]
    fn sums_sources_on_a_shared_timeline() {
        let mut m = Mixer::new(10.0, true, true);
        m.push(AudioKind::System, 10.0, RATE, vec![[0.25, 0.25]; 480]);
        // Nothing until the mic reaches the same point (it may still arrive).
        assert!(m.pull(false).is_none());
        // Mic starts 5 ms later, mono duplicated to both channels.
        m.push(AudioKind::Mic, 10.005, RATE, vec![[0.5, 0.5]; 480]);
        let (pts, out) = pull_all(&mut m, false);
        assert_eq!(pts, Some(10.0));
        assert_eq!(out.len(), 480, "only up to the slower source's horizon");
        assert_eq!(out[0], [0.25, 0.25]);
        assert_eq!(out[240], [0.75, 0.75], "overlap is summed");
        let (pts, rest) = pull_all(&mut m, true);
        assert_eq!(pts, Some(10.01));
        assert_eq!(rest.len(), 240);
        assert_eq!(rest[0], [0.5, 0.5]);
    }

    #[test]
    fn limiting() {
        // Under the ceiling: untouched.
        let mut m = Mixer::new(0.0, false, true);
        m.push(AudioKind::Mic, 0.0, RATE, vec![[0.4, -0.6]; 100]);
        let (_, out) = pull_all(&mut m, true);
        assert_eq!(out[50], [0.4, -0.6]);
        assert_eq!(m.clipped, 0);
        // Too loud (a source boosted on its fader): held under full scale
        // (both channels by the same gain, so the stereo image keeps), never
        // flattened at 1.0.
        let mut m = Mixer::new(0.0, false, true);
        m.push(AudioKind::Mic, 0.0, RATE, vec![[0.8, -1.2]; 4800]);
        let (_, out) = pull_all(&mut m, true);
        assert!(out.iter().all(|f| f[0].abs() < 1.0 && f[1].abs() < 1.0));
        let last = out[4799];
        assert!((last[1].abs() - CEILING).abs() < 0.01, "{last:?}");
        assert!((last[0] / last[1] + 0.8 / 1.2).abs() < 0.01);
        assert_eq!(m.clipped, 4800);
    }

    #[test]
    fn volume_gain() {
        let v = |db, muted| Volume { db, muted };
        assert_eq!(Volume::UNITY.gain(), 1.0);
        assert!((v(-6.0, false).gain() - 0.501).abs() < 0.001);
        assert!((v(12.0, false).gain() - 3.981).abs() < 0.001);
        assert_eq!(v(MAX_DB + 20.0, false).gain(), v(MAX_DB, false).gain(), "boost is capped");
        assert_eq!(v(MIN_DB, false).gain(), 0.0, "the bottom of the fader is silence");
        assert_eq!(v(f32::NEG_INFINITY, false).gain(), 0.0);
        assert_eq!(v(f32::NAN, false).gain(), 0.0);
        assert_eq!(v(0.0, true).gain(), 0.0);
        assert_eq!(v(-6.0, true).fader_gain(), v(-6.0, false).gain(), "the meter still sees a muted source");
    }

    #[test]
    fn fader_glides_and_passes_unity_through() {
        let mut f = Fader::new(1.0);
        assert!(f.envelope(480, RATE).is_none(), "unity: untouched");
        // Mute: down to silence within 20 ms, never a jump.
        f.set(0.0);
        let e = f.envelope(1920, RATE).unwrap();
        assert!(e.windows(2).all(|w| w[1] <= w[0] && w[0] - w[1] < 0.0011), "smooth");
        assert!(e[0] > 0.99 && e[950] > 0.0);
        assert!(e[961..].iter().all(|g| *g == 0.0));
        // A boost glides as fast, whatever its size, and settles exactly.
        let boost = Volume { db: 24.0, muted: false }.gain();
        f.set(boost);
        let e = f.envelope(1920, RATE).unwrap();
        assert!(e[950] < boost && e[961..].iter().all(|g| *g == boost));
        // Muting a boosted source in device-sized buffers still takes 20 ms.
        let mut g = vec![];
        f.set(0.0);
        for _ in 0..4 {
            g.extend(f.envelope(480, RATE).unwrap());
        }
        assert!(g[950] > 0.0 && g[961..].iter().all(|g| *g == 0.0), "{} {}", g[959], g[961]);
        f.set(boost);
        f.envelope(960, RATE);
        // Back to unity: once there, the audio passes through again.
        f.set(1.0);
        assert_eq!(*f.envelope(960, 16000.0).unwrap().last().unwrap(), 1.0);
        assert!(f.envelope(480, RATE).is_none());
    }

    /// A sample buffer in an arbitrary PCM layout, for `scaled`.
    fn buffer(flags: u32, bits: u32, channels: u32, data: &[u8], frames: usize) -> CFRetained<CMSampleBuffer> {
        let non_inter = flags & kAudioFormatFlagIsNonInterleaved != 0;
        let bytes = bits / 8 * if non_inter { 1 } else { channels };
        let fmt = audio_format(&AudioStreamBasicDescription {
            mSampleRate: RATE,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: flags | kAudioFormatFlagIsPacked,
            mBytesPerPacket: bytes,
            mFramesPerPacket: 1,
            mBytesPerFrame: bytes,
            mChannelsPerFrame: channels,
            mBitsPerChannel: bits,
            mReserved: 0,
        });
        let pts = unsafe { CMTime::new(48000 * 7, 48000) };
        sample_buffer(&fmt, data, frames, pts).unwrap()
    }

    #[test]
    fn scales_in_the_buffers_own_format() {
        let env = [0.5, 0.25, 2.0, 0.0];
        // Float stereo, interleaved.
        let f: Vec<f32> = vec![0.2, -0.4, 0.2, -0.4, 0.6, -0.6, 0.2, -0.4];
        let sb = buffer(kAudioFormatFlagIsFloat, 32, 2, &f.iter().flat_map(|v| v.to_ne_bytes()).collect::<Vec<_>>(), 4);
        let out = scaled(&sb, &env).unwrap();
        let (pts, rate, frames) = decode(&out).unwrap();
        assert_eq!((pts, rate), (7.0, RATE), "same timing");
        assert_eq!(frames, vec![[0.1, -0.2], [0.05, -0.1], [1.0, -1.0], [0.0, 0.0]], "saturates");
        // Float stereo, one buffer per channel.
        let f: Vec<f32> = vec![0.2, 0.2, 0.2, 0.2, -0.4, -0.4, -0.4, -0.4];
        let flags = kAudioFormatFlagIsFloat | kAudioFormatFlagIsNonInterleaved;
        let sb = buffer(flags, 32, 2, &f.iter().flat_map(|v| v.to_ne_bytes()).collect::<Vec<_>>(), 4);
        let (_, _, frames) = decode(&scaled(&sb, &env).unwrap()).unwrap();
        assert_eq!(frames, vec![[0.1, -0.2], [0.05, -0.1], [0.4, -0.8], [0.0, 0.0]]);
        // 16-bit mono: saturates instead of wrapping.
        let i: Vec<i16> = vec![16384, -16384, 30000, 1000];
        let sb = buffer(kAudioFormatFlagIsSignedInteger, 16, 1, &i.iter().flat_map(|v| v.to_ne_bytes()).collect::<Vec<_>>(), 4);
        let (_, _, frames) = decode(&scaled(&sb, &env).unwrap()).unwrap();
        assert_eq!(frames.iter().map(|f| f[0]).collect::<Vec<_>>(), vec![0.25, -0.125, 32767.0 / 32768.0, 0.0]);
        // 24-bit packed mono.
        let i: Vec<u8> = [4194304i32, -4194304, 4194304, 4194304].iter().flat_map(|v| v.to_le_bytes()[..3].to_vec()).collect();
        let sb = buffer(kAudioFormatFlagIsSignedInteger, 24, 1, &i, 4);
        let (_, _, frames) = decode(&scaled(&sb, &env).unwrap()).unwrap();
        assert_eq!(frames.iter().map(|f| f[0]).collect::<Vec<_>>(), vec![0.25, -0.125, 8388607.0 / 8388608.0, 0.0]);
    }

    /// A sine through the resampler in uneven buffers: the right length and
    /// pitch, seamless across buffers (error against the ideal 48 kHz sine).
    fn resampled_sine_error(from: f64, freq: f64, chunk: usize) -> (usize, f64) {
        let mut m = Mixer::new(0.0, false, true);
        let n = from as usize; // 1 s
        let sine = |t: f64| (2.0 * std::f64::consts::PI * freq * t).sin() as f32 * 0.5;
        let mut i = 0;
        while i < n {
            let len = chunk + (i / chunk) % 7; // uneven buffer sizes
            let end = (i + len).min(n);
            let frames: Vec<[f32; 2]> = (i..end).map(|k| { let v = sine(k as f64 / from); [v, v] }).collect();
            m.push(AudioKind::Mic, i as f64 / from, from, frames);
            i = end;
        }
        let (pts, out) = pull_all(&mut m, true);
        let t0 = pts.unwrap();
        // Compare away from the stream's start and end (filter ramp).
        let err = (2400..out.len() - 2400)
            .map(|k| (out[k][0] - sine(t0 + k as f64 / RATE)).abs() as f64)
            .fold(0.0, f64::max);
        (out.len(), err)
    }

    #[test]
    fn resamples_seamlessly() {
        for (from, chunk) in [(44100.0, 441), (16000.0, 160), (24000.0, 512), (96000.0, 1024)] {
            let (len, err) = resampled_sine_error(from, 1000.0, chunk);
            // The last ~1.6 ms wait in the filter for input that never came.
            assert!((len as i64 - 48000).abs() <= 100, "{from}: {len} samples");
            assert!(err < 0.002, "{from}: max error {err}");
        }
    }

    #[test]
    fn resampler_removes_content_above_nyquist() {
        // 20 kHz at 96 kHz cannot pass to 48 kHz unaliased, but is under
        // 24 kHz; 30 kHz must be filtered out rather than fold to 18 kHz.
        let mut m = Mixer::new(0.0, false, true);
        let frames: Vec<[f32; 2]> = (0..96000)
            .map(|k| { let v = (2.0 * std::f64::consts::PI * 30000.0 * k as f64 / 96000.0).sin() as f32 * 0.5; [v, v] })
            .collect();
        m.push(AudioKind::Mic, 0.0, 96000.0, frames);
        let (_, out) = pull_all(&mut m, true);
        let peak = out[2400..out.len() - 2400].iter().fold(0f32, |a, f| a.max(f[0].abs()));
        assert!(peak < 0.005, "aliased residue {peak}");
    }

    #[test]
    fn small_jitter_snaps_to_continuous() {
        let mut m = Mixer::new(0.0, true, false);
        m.push(AudioKind::System, 0.0, RATE, vec![[0.1, 0.1]; 480]);
        // 1 ms late: within SNAP, so no gap of silence is inserted.
        m.push(AudioKind::System, 0.011, RATE, vec![[0.1, 0.1]; 480]);
        let (_, out) = pull_all(&mut m, true);
        assert_eq!(out.len(), 960);
        assert!(out.iter().all(|f| f[0] == 0.1));
    }


}
