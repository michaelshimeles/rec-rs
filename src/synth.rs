//! Synthetic "screen": a generated moving test pattern on the host clock.
//! Used for self-tests and encoder stress tests without Screen Recording access.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use objc2_core_foundation::CFRetained;
use objc2_core_video::*;

use crate::record::Shared;
use crate::util::*;

pub struct Synth {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

/// Length of the beep that starts with every white flash.
pub const BEEP_SECS: f64 = 0.04;

/// Samples [start, start+n) of the synthetic "system audio": a 1 kHz beep at
/// the start of every second (when the video flashes), silence otherwise.
pub fn beep_samples(start: i64, n: usize) -> Vec<f32> {
    let rate = crate::mixer::RATE as i64;
    let len = (BEEP_SECS * rate as f64) as i64;
    let mut out = Vec::with_capacity(n * 2);
    for i in 0..n as i64 {
        let s = start + i;
        let in_sec = s.rem_euclid(rate);
        let v = if in_sec < len {
            0.5 * (2.0 * std::f64::consts::PI * 1000.0 * in_sec as f64 / rate as f64).sin() as f32
        } else {
            0.0
        };
        out.push(v);
        out.push(v);
    }
    out
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Draw frame `n`, captured at `ticks` (host time in 1/TIMESCALE s). Every
/// `fps` frames (once per second) the frame is solid white so A/V offsets
/// can be measured against an audio click. The time code goes on top.
fn draw(pb: &CVPixelBuffer, n: u64, fps: u64, ticks: i64, noise: bool, rng: &mut Rng) {
    unsafe {
        CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags(0));
        let w = CVPixelBufferGetWidth(pb);
        let h = CVPixelBufferGetHeight(pb);
        let flash = n.is_multiple_of(fps);
        let box_x = ((n * 12) as usize) % w.saturating_sub(200).max(1);
        let box_y = h / 3;
        let planar = CVPixelBufferGetPlaneCount(pb) > 0;
        let block = 32usize;
        let row_noise = |rng: &mut Rng, x: usize, y: usize| -> u8 {
            if noise && ((x / block) + (y / block) + n as usize).is_multiple_of(3) {
                (rng.next() & 0xff) as u8
            } else {
                0
            }
        };
        if planar {
            let y_base = CVPixelBufferGetBaseAddressOfPlane(pb, 0) as *mut u8;
            let y_stride = CVPixelBufferGetBytesPerRowOfPlane(pb, 0);
            for y in 0..h {
                let row = std::slice::from_raw_parts_mut(y_base.add(y * y_stride), w);
                for (x, px) in row.iter_mut().enumerate() {
                    let in_box = x >= box_x && x < box_x + 200 && y >= box_y && y < box_y + 200;
                    *px = if flash || in_box {
                        235
                    } else {
                        let g = (((x + n as usize * 4) % 256) as u8 / 2).wrapping_add(16);
                        g ^ row_noise(rng, x, y)
                    };
                }
            }
            let uv_base = CVPixelBufferGetBaseAddressOfPlane(pb, 1) as *mut u8;
            let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(pb, 1);
            for y in 0..h / 2 {
                let row = std::slice::from_raw_parts_mut(uv_base.add(y * uv_stride), w);
                for (i, px) in row.iter_mut().enumerate() {
                    *px = if flash { 128 } else if i % 2 == 0 { 100 + (y % 56) as u8 } else { 150 };
                }
            }
            code_rects(w, h, ticks, |x, y, cw, ch, on| {
                for row in y..y + ch {
                    std::ptr::write_bytes(y_base.add(row * y_stride + x), if on { 235 } else { 16 }, cw);
                }
                for row in y / 2..(y + ch) / 2 {
                    std::ptr::write_bytes(uv_base.add(row * uv_stride + x), 128, cw);
                }
            });
        } else {
            let base = CVPixelBufferGetBaseAddress(pb) as *mut u8;
            let stride = CVPixelBufferGetBytesPerRow(pb);
            for y in 0..h {
                let row = std::slice::from_raw_parts_mut(base.add(y * stride) as *mut u32, w);
                for (x, px) in row.iter_mut().enumerate() {
                    let in_box = x >= box_x && x < box_x + 200 && y >= box_y && y < box_y + 200;
                    *px = if flash || in_box {
                        0xffff_ffff
                    } else {
                        let g = ((x + n as usize * 4) % 256) as u32;
                        let nz = row_noise(rng, x, y) as u32;
                        0xff00_0000 | ((g ^ nz) << 16) | (((y * 255 / h) as u32) << 8) | (120 ^ nz)
                    };
                }
            }
            fill_code_bgra(base, stride, (0, 0), (w, h), ticks);
        }
        CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags(0));
    }
}

/// Blocks in a time code: a start marker (white, white, black), the
/// `CODE_BITS` bits of the capture time, most significant first (white is
/// 1), and a black end. The marker reads backwards in a mirrored picture.
pub const CODE_BITS: usize = 32;
pub const CODE_BLOCKS: usize = 3 + CODE_BITS + 1;

/// Side of a code block for a picture `w` pixels wide.
pub fn code_block(w: usize) -> usize {
    (w / 48).max(4)
}

/// The blocks (true = white) of the code for capture time `ticks`: its low
/// `CODE_BITS` bits. `scripts/trackcheck.py` reads them back.
pub fn code_bits(ticks: i64) -> [bool; CODE_BLOCKS] {
    let mut b = [false; CODE_BLOCKS];
    b[0] = true;
    b[1] = true;
    for i in 0..CODE_BITS {
        b[3 + i] = (ticks as u64 >> (CODE_BITS - 1 - i)) & 1 == 1;
    }
    b
}

/// Each block of the code for `ticks` in a `w`x`h` picture, as
/// (x, y, width, height, white) from the picture's top-left: one block in
/// from the left and the top, two blocks tall. Nothing when the code
/// doesn't fit (a frame of a few pixels).
fn code_rects(w: usize, h: usize, ticks: i64, mut f: impl FnMut(usize, usize, usize, usize, bool)) {
    let b = code_block(w);
    if (CODE_BLOCKS + 1) * b > w || 3 * b > h {
        return;
    }
    for (i, on) in code_bits(ticks).into_iter().enumerate() {
        f(b + i * b, b, b, 2 * b, on);
    }
}

/// Draw the code for `ticks` into BGRA pixels at `base`, in a picture of
/// `size` whose top-left corner is at `at`.
unsafe fn fill_code_bgra(base: *mut u8, stride: usize, at: (usize, usize), size: (usize, usize), ticks: i64) {
    code_rects(size.0, size.1, ticks, |x, y, cw, ch, on| {
        for row in at.1 + y..at.1 + y + ch {
            let px = unsafe { std::slice::from_raw_parts_mut(base.add(row * stride + (at.0 + x) * 4) as *mut u32, cw) };
            px.fill(if on { 0xffff_ffff } else { 0xff00_0000 });
        }
    });
}

impl Synth {
    /// `audio`: also generate system audio with a beep on every flash.
    pub fn start(w: usize, h: usize, fps: u32, pix: u32, noise: bool, audio: bool, shared: Arc<Shared>) -> Synth {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let pool = SendBox(make_pool(w, h, pix));
        // Video and audio share this origin, so flash k and beep k are both
        // stamped exactly start + k seconds.
        let start = secs(host_now()).ceil();
        let mut threads = vec![];
        if audio {
            let (stop3, shared3) = (stop.clone(), shared.clone());
            threads.push(std::thread::spawn(move || {
                let fmt = crate::mixer::stereo_format();
                let rate = crate::mixer::RATE;
                let chunk = 480usize; // 10 ms
                let mut k = 0i64;
                // Self-test hook: drop every Nth buffer like a glitching device.
                let drop_every: i64 = std::env::var("REC_SYNTH_DROP_AUDIO").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
                while !stop3.load(Ordering::SeqCst) {
                    let t = start + (k * chunk as i64) as f64 / rate;
                    let now = secs(host_now());
                    // Deliver each chunk once it has "played", like a real device.
                    let ready = t + chunk as f64 / rate;
                    if ready > now {
                        std::thread::sleep(Duration::from_secs_f64(ready - now));
                    }
                    objc2::rc::autoreleasepool(|_| {
                        let pts = unsafe { objc2_core_media::CMTime::new(start as i64 * rate as i64 + k * chunk as i64, rate as i32) };
                        let dropped = drop_every > 0 && k % drop_every == drop_every - 1;
                        if dropped {
                        } else if let Some(sb) = crate::mixer::pcm_buffer(&fmt, beep_samples(k * chunk as i64, chunk), pts) {
                            shared3.with(|s| s.push_audio(crate::writer::AudioKind::System, &sb));
                        }
                    });
                    k += 1;
                }
            }));
        }
        threads.push(std::thread::spawn(move || {
            let pool = pool;
            let mut rng = Rng(0x9e3779b97f4a7c15);
            let period = 1.0 / fps as f64;
            let mut n = 0u64;
            while !stop2.load(Ordering::SeqCst) {
                let due = start + n as f64 * period;
                let now = secs(host_now());
                if due > now {
                    std::thread::sleep(Duration::from_secs_f64(due - now));
                }
                objc2::rc::autoreleasepool(|_| {
                    let mut buf = std::ptr::null_mut();
                    unsafe { CVPixelBufferPool::create_pixel_buffer(None, &pool.0, std::ptr::NonNull::from(&mut buf)) };
                    if let Some(buf) = std::ptr::NonNull::new(buf) {
                        let pb: CFRetained<CVPixelBuffer> = unsafe { CFRetained::from_raw(buf) };
                        draw(&pb, n, fps as u64, (due * crate::writer::TIMESCALE as f64).round() as i64, noise, &mut rng);
                        let pts = unsafe { objc2_core_media::CMTime::with_seconds(due, 1_000_000_000) };
                        shared.with(|s| s.push_screen(pts, pb));
                    }
                });
                n += 1;
            }
        }));
        Synth { stop, threads }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// A generated camera (`--test-camera`), for self-tests without a webcam
/// or the Camera permission. It sends 30 fps on the host clock like a real
/// camera: a 1280x720 picture, or with `letterbox` a 720x1280 frame with
/// a 16:9 picture in the middle and black above and below (what a DJI
/// Osmo Pocket 3 sends on a portrait format). Like the screen pattern, it
/// flashes white at the start of every second and carries the time code
/// of each frame's capture time, so the screen's and the camera's files
/// can be lined up frame by frame.
pub struct TestCamera {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

pub const TEST_CAMERA_FPS: i64 = 30;

/// The test camera's frame size and its picture inside it (x, y, w, h).
pub fn test_camera_frame(letterbox: bool) -> ((usize, usize), (usize, usize, usize, usize)) {
    if letterbox { ((720, 1280), (0, 437, 720, 405)) } else { ((1280, 720), (0, 0, 1280, 720)) }
}

/// Draw test camera frame `m`, captured at `ticks`, into `pb` (BGRA).
/// `background` is the picture without the moving disc.
fn draw_camera(pb: &CVPixelBuffer, pic: (usize, usize, usize, usize), background: &[u32], m: i64, ticks: i64) {
    let (px, py, pw, ph) = pic;
    let flash = m % TEST_CAMERA_FPS == 0;
    unsafe {
        CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags(0));
        let (w, h) = (CVPixelBufferGetWidth(pb), CVPixelBufferGetHeight(pb));
        let base = CVPixelBufferGetBaseAddress(pb) as *mut u8;
        let stride = CVPixelBufferGetBytesPerRow(pb);
        // A disc that crosses the picture every 4 s.
        let r = (ph / 5) as i64;
        let cx = px as i64 + r + (m % 120) * (pw as i64 - 2 * r) / 120;
        let cy = (py + ph / 2) as i64;
        for y in 0..h {
            let row = std::slice::from_raw_parts_mut(base.add(y * stride) as *mut u32, w);
            if y < py || y >= py + ph {
                row.fill(0xff00_0000);
                continue;
            }
            if flash {
                row.fill(0xff00_0000);
                row[px..px + pw].fill(0xffff_ffff);
                continue;
            }
            row.fill(0xff00_0000);
            row[px..px + pw].copy_from_slice(&background[(y - py) * pw..(y - py + 1) * pw]);
            let dy = y as i64 - cy;
            if dy.abs() < r {
                let half = ((r * r - dy * dy) as f64).sqrt() as i64;
                let (a, b) = ((cx - half).max(px as i64) as usize, ((cx + half) as usize).min(px + pw));
                row[a..b].fill(0xfff0_9628);
            }
        }
        fill_code_bgra(base, stride, (px, py), (pw, ph), ticks);
        CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags(0));
    }
}

impl TestCamera {
    pub fn start(letterbox: bool, shared: Arc<Shared>) -> TestCamera {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let ((w, h), pic) = test_camera_frame(letterbox);
        let pool = SendBox(make_pool(w, h, PIX_BGRA));
        // A muted blue-to-olive gradient: bright enough everywhere that its
        // edges never read as black bars, dark enough (luma under 140) for
        // `scripts/synccheck.py` to see the white flashes.
        let (pw, ph) = (pic.2, pic.3);
        let background: Vec<u32> = (0..pw * ph)
            .map(|i| {
                let (u, v) = ((i % pw) as f64 / pw as f64, (i / pw) as f64 / ph as f64);
                let (r, g, b) = (50.0 + 100.0 * u, 90.0 + 40.0 * v, 140.0 - 60.0 * u);
                0xff00_0000 | ((r as u32) << 16) | ((g as u32) << 8) | b as u32
            })
            .collect();
        let thread = std::thread::spawn(move || {
            let pool = pool;
            let mut bars = crate::bars::Tracker::default();
            let rate = TEST_CAMERA_FPS;
            let ts = crate::writer::TIMESCALE as i64;
            // Frames on the 1/30 s grid from a whole second, the same instants
            // as every other frame of the screen pattern.
            let start = secs(host_now()).ceil() as i64;
            let mut m = 0i64;
            while !stop2.load(Ordering::SeqCst) {
                let ticks = start * ts + m * ts / rate;
                let due = ticks as f64 / ts as f64;
                let now = secs(host_now());
                if due > now {
                    std::thread::sleep(Duration::from_secs_f64(due - now));
                }
                objc2::rc::autoreleasepool(|_| {
                    let mut buf = std::ptr::null_mut();
                    unsafe { CVPixelBufferPool::create_pixel_buffer(None, &pool.0, std::ptr::NonNull::from(&mut buf)) };
                    if let Some(buf) = std::ptr::NonNull::new(buf) {
                        let pb: CFRetained<CVPixelBuffer> = unsafe { CFRetained::from_raw(buf) };
                        draw_camera(&pb, pic, &background, m, ticks);
                        let crop = bars.frame(due, &pb);
                        let pts = unsafe { objc2_core_media::CMTime::new(ticks, ts as i32) };
                        shared.with(|s| s.push_cam(pts, pb, crop));
                    }
                });
                m += 1;
            }
        });
        TestCamera { stop, thread: Some(thread) }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beep_starts_on_each_second() {
        let s = beep_samples(48000 - 2, 6);
        assert_eq!(&s[..4], &[0.0; 4], "silent before the second");
        assert_eq!(s[4], 0.0, "sine starts at zero phase");
        assert!(s[6] > 0.0);
        let len = (BEEP_SECS * 48000.0) as i64;
        assert_eq!(beep_samples(len, 1), vec![0.0, 0.0]);
    }

    /// Read the time code back from a BGRA frame's picture at `at`, the
    /// way `scripts/trackcheck.py` does: the middle of each block.
    fn read_code(pb: &CVPixelBuffer, at: (usize, usize), w: usize) -> Option<u32> {
        let b = code_block(w);
        let bits: Vec<bool> = unsafe {
            CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags(1));
            let base = CVPixelBufferGetBaseAddress(pb) as *const u8;
            let stride = CVPixelBufferGetBytesPerRow(pb);
            let bits = (0..CODE_BLOCKS).map(|i| *base.add((at.1 + 2 * b) * stride + (at.0 + b + i * b + b / 2) * 4 + 1) > 128).collect();
            CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags(1));
            bits
        };
        (bits[..3] == [true, true, false] && !bits[CODE_BLOCKS - 1])
            .then(|| bits[3..3 + CODE_BITS].iter().fold(0u32, |v, &on| v << 1 | on as u32))
    }

    #[test]
    fn time_code_stays_inside_the_frame() {
        for (w, h) in [(2, 2), (64, 36), (148, 12), (320, 180), (720, 400), (1920, 1080)] {
            code_rects(w, h, 123_456, |x, y, cw, ch, _| {
                assert!(x + cw <= w && y + ch <= h, "{w}x{h}: block at {x},{y} {cw}x{ch} is outside");
            });
        }
    }

    #[test]
    fn time_code_reads_back() {
        for letterbox in [false, true] {
            let ((w, h), pic) = test_camera_frame(letterbox);
            let pool = make_pool(w, h, PIX_BGRA);
            let mut buf = std::ptr::null_mut();
            unsafe { CVPixelBufferPool::create_pixel_buffer(None, &pool, std::ptr::NonNull::from(&mut buf)) };
            let pb: CFRetained<CVPixelBuffer> = unsafe { CFRetained::from_raw(std::ptr::NonNull::new(buf).unwrap()) };
            let background = vec![0xff80_8080u32; pic.2 * pic.3];
            // A host time a few days after boot, past 32 bits of ticks.
            let ticks = 3 * 86_400 * 60_000i64 + 123_456;
            for m in [0, 7] {
                draw_camera(&pb, pic, &background, m, ticks);
                assert_eq!(read_code(&pb, (pic.0, pic.1), pic.2), Some(ticks as u32), "letterbox {letterbox}, frame {m}");
            }
            // The camera's bars are found like a real camera's.
            let bars = crate::bars::measure_bgra(&pb).unwrap();
            assert_eq!((bars.top, bars.bottom), (pic.1, h - pic.1 - pic.3));
        }
    }
}
