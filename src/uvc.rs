//! Hardware controls of USB (UVC) cameras: zoom, roll and focus, for
//! cameras whose settings AVFoundation doesn't expose.
//!
//! A UVC camera's AVFoundation unique ID encodes where it is plugged in and
//! what it is (`0x1000002ca30023`: USB location 0x00100000, vendor 0x2ca3,
//! product 0x0023). That finds its USB device in IOKit, whose configuration
//! descriptor names the VideoControl interface and its camera terminal.
//! Control requests go over the default control pipe, which works while
//! AVFoundation streams from the camera.
//!
//! Only controls the camera answers GET_INFO for (and allows both GET and
//! SET on) are offered, whatever its descriptor advertises: a DJI Osmo
//! Pocket 3 advertises zoom, pan/tilt and roll but also answers for focus.
//! Nothing is ever sent to extension units (vendor protocols) or the
//! processing unit. Requests run on a worker thread (`Hardware`) with
//! timeouts, so a camera that stalls or disappears never blocks the UI.

use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A USB device as AVFoundation's unique ID for a UVC camera names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsbId {
    pub location: u32,
    pub vendor: u16,
    pub product: u16,
}

/// The USB device behind a camera's unique ID, if it is a UVC camera's
/// (`0x` and up to 16 hex digits: location << 32 | vendor << 16 | product).
/// Built-in and Continuity cameras have UUID-style IDs and give None.
pub fn usb_id(unique_id: &str) -> Option<UsbId> {
    let hex = unique_id.strip_prefix("0x")?;
    if hex.is_empty() || hex.len() > 16 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let v = u64::from_str_radix(hex, 16).ok()?;
    let id = UsbId { location: (v >> 32) as u32, vendor: (v >> 16) as u16, product: v as u16 };
    (id.location != 0 && id.vendor != 0).then_some(id)
}

/// A unit or terminal of the VideoControl interface and the controls its
/// descriptor advertises (bit n = control selector n + 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unit {
    pub id: u8,
    pub controls: u32,
}

/// What a camera's configuration descriptor says about its controls.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VideoControl {
    /// The VideoControl interface number (wIndex's low byte).
    pub interface: u8,
    pub camera: Option<Unit>,
    pub processing: Option<Unit>,
    /// Extension units (IDs and GUIDs), listed but never addressed.
    pub extensions: Vec<(u8, [u8; 16])>,
}

const DESC_INTERFACE: u8 = 0x04;
const CS_INTERFACE: u8 = 0x24;
const CC_VIDEO: u8 = 0x0e;
const SC_VIDEOCONTROL: u8 = 0x01;
const VC_INPUT_TERMINAL: u8 = 0x02;
const VC_PROCESSING_UNIT: u8 = 0x05;
const VC_EXTENSION_UNIT: u8 = 0x06;
const ITT_CAMERA: u16 = 0x0201;

/// Little-endian bitmap of `n` bytes at `at` (up to 4 bytes kept).
fn bitmap(d: &[u8], at: usize, n: usize) -> Option<u32> {
    let bytes = d.get(at..at + n)?;
    Some(bytes.iter().take(4).enumerate().fold(0u32, |m, (i, &b)| m | (b as u32) << (8 * i)))
}

/// Parse a configuration descriptor (with everything after it, as
/// wTotalLength covers) for the first VideoControl interface. None when
/// there is none: not a UVC camera.
pub fn parse_config(desc: &[u8]) -> Option<VideoControl> {
    let mut vc: Option<VideoControl> = None;
    // The interface the descriptors being walked belong to:
    // (number, class, subclass).
    let mut current: Option<(u8, u8, u8)> = None;
    let mut i = 0;
    while i + 2 <= desc.len() {
        let len = desc[i] as usize;
        if len < 2 || i + len > desc.len() {
            break;
        }
        let d = &desc[i..i + len];
        match d[1] {
            DESC_INTERFACE if len >= 9 => {
                current = Some((d[2], d[5], d[6]));
                if d[5] == CC_VIDEO && d[6] == SC_VIDEOCONTROL && vc.is_none() {
                    vc = Some(VideoControl { interface: d[2], ..Default::default() });
                }
            }
            CS_INTERFACE if len >= 3 => {
                let ours = matches!((current, &vc), (Some((n, CC_VIDEO, SC_VIDEOCONTROL)), Some(v)) if v.interface == n);
                if let (true, Some(v)) = (ours, vc.as_mut()) {
                    match d[2] {
                        VC_INPUT_TERMINAL if len >= 15 && u16::from_le_bytes([d[4], d[5]]) == ITT_CAMERA => {
                            let n = d[14] as usize;
                            v.camera.get_or_insert(Unit { id: d[3], controls: bitmap(d, 15, n).unwrap_or(0) });
                        }
                        VC_PROCESSING_UNIT if len >= 8 => {
                            let n = d[7] as usize;
                            v.processing.get_or_insert(Unit { id: d[3], controls: bitmap(d, 8, n).unwrap_or(0) });
                        }
                        VC_EXTENSION_UNIT if len >= 20 => {
                            let mut guid = [0u8; 16];
                            guid.copy_from_slice(&d[4..20]);
                            v.extensions.push((d[3], guid));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        i += len;
    }
    vc
}

/// A camera-terminal control Rec offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Control {
    /// 1 = autofocus, 0 = manual.
    FocusAuto,
    /// Manual focus position (camera units).
    Focus,
    /// Zoom (camera units; 100 is 1x on an Osmo Pocket 3).
    Zoom,
    /// Roll (degrees on an Osmo Pocket 3).
    Roll,
}

impl Control {
    /// In the order the app shows them.
    pub const ALL: [Control; 4] = [Control::Zoom, Control::Roll, Control::FocusAuto, Control::Focus];

    /// The UVC camera terminal control selector.
    pub fn selector(self) -> u8 {
        match self {
            Control::Focus => 0x06,
            Control::FocusAuto => 0x08,
            Control::Zoom => 0x0b,
            Control::Roll => 0x0f,
        }
    }

    /// Size of its value in bytes.
    fn len(self) -> usize {
        match self {
            Control::FocusAuto => 1,
            _ => 2,
        }
    }

    fn signed(self) -> bool {
        self == Control::Roll
    }

    pub fn name(self) -> &'static str {
        match self {
            Control::FocusAuto => "autofocus",
            Control::Focus => "focus",
            Control::Zoom => "zoom",
            Control::Roll => "roll",
        }
    }

    pub fn from_name(s: &str) -> Option<Control> {
        Control::ALL.into_iter().find(|c| c.name() == s)
    }

    /// The value "Reset" sends: the camera's own default, except that
    /// autofocus goes back on (an Osmo Pocket 3 reports 0 as its default).
    pub fn reset_value(self, range: &Range) -> i32 {
        if self == Control::FocusAuto { 1 } else { range.def }
    }
}

/// UVC class-specific requests.
pub const SET_CUR: u8 = 0x01;
pub const GET_CUR: u8 = 0x81;
pub const GET_MIN: u8 = 0x82;
pub const GET_MAX: u8 = 0x83;
pub const GET_RES: u8 = 0x84;
pub const GET_INFO: u8 = 0x86;
pub const GET_DEF: u8 = 0x87;

/// A control's limits and current value, from the camera.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    pub min: i32,
    pub max: i32,
    pub def: i32,
    /// Step between values (at least 1).
    pub step: i32,
    pub cur: i32,
}

/// The device side of control requests: one class request to a unit of the
/// VideoControl interface. `IoUsbDevice` talks to a real camera; tests use
/// a scripted one. An error is the IOKit status (a STALL for a control the
/// camera doesn't have).
pub trait Requests {
    fn get(&self, request: u8, unit: u8, selector: u8, len: usize) -> Result<Vec<u8>, i32>;
    fn set(&self, unit: u8, selector: u8, data: &[u8]) -> Result<(), i32>;
}

fn decode(bytes: &[u8], signed: bool) -> i32 {
    let mut v: i64 = 0;
    for (i, &b) in bytes.iter().take(4).enumerate() {
        v |= (b as i64) << (8 * i);
    }
    let bits = 8 * bytes.len().min(4) as u32;
    if signed && bits < 64 && bits > 0 && v & (1 << (bits - 1)) != 0 {
        v -= 1 << bits;
    }
    v as i32
}

fn encode(v: i32, len: usize) -> Vec<u8> {
    v.to_le_bytes()[..len.min(4)].to_vec()
}

/// A camera's controls: its VideoControl layout and a way to send requests.
pub struct Camera<R: Requests> {
    pub dev: R,
    pub vc: VideoControl,
}

impl<R: Requests> Camera<R> {
    fn terminal(&self) -> Option<u8> {
        self.vc.camera.map(|u| u.id)
    }

    fn read(&self, c: Control, request: u8) -> Result<i32, i32> {
        let unit = self.terminal().ok_or(-1)?;
        let b = self.dev.get(request, unit, c.selector(), c.len())?;
        if b.len() < c.len() {
            return Err(-1);
        }
        Ok(decode(&b, c.signed()))
    }

    /// The controls this camera answers for, with their ranges. A control
    /// counts when GET_INFO succeeds with GET and SET supported and its
    /// limits read back sensibly.
    pub fn probe(&self) -> Vec<(Control, Range)> {
        let Some(unit) = self.terminal() else { return vec![] };
        let mut out = vec![];
        for c in Control::ALL {
            let Ok(info) = self.dev.get(GET_INFO, unit, c.selector(), 1) else { continue };
            if info.first().is_none_or(|i| i & 0x03 != 0x03) {
                continue;
            }
            let Ok(cur) = self.read(c, GET_CUR) else { continue };
            let range = if c == Control::FocusAuto {
                Range { min: 0, max: 1, def: self.read(c, GET_DEF).unwrap_or(1).clamp(0, 1), step: 1, cur: cur.clamp(0, 1) }
            } else {
                let (Ok(min), Ok(max)) = (self.read(c, GET_MIN), self.read(c, GET_MAX)) else { continue };
                if min >= max {
                    continue;
                }
                let def = self.read(c, GET_DEF).unwrap_or(min).clamp(min, max);
                let step = self.read(c, GET_RES).unwrap_or(1).max(1);
                Range { min, max, def, step, cur: cur.clamp(min, max) }
            };
            out.push((c, range));
        }
        out
    }

    /// Set a control, the value clamped to its range; returns the value
    /// sent. The camera can take a moment to get there (an Osmo Pocket 3
    /// ramps its zoom and turns its gimbal for roll), and GET_CUR reports
    /// where it is on the way, so reading back at once can return the old
    /// value.
    pub fn set(&self, c: Control, value: i32, range: &Range) -> Result<i32, i32> {
        let unit = self.terminal().ok_or(-1)?;
        let v = value.clamp(range.min, range.max);
        self.dev.set(unit, c.selector(), &encode(v, c.len()))?;
        Ok(v)
    }
}

/// One control as the app shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlState {
    pub control: Control,
    pub range: Range,
}

/// What the worker knows about the attached camera, for the UI.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    /// The attached camera's unique ID.
    pub camera: Option<String>,
    /// Still opening and probing it.
    pub probing: bool,
    /// It is a USB camera (shutter and ISO are worth explaining).
    pub usb: bool,
    pub controls: Vec<ControlState>,
    /// The last failure, for the UI and the log.
    pub error: Option<String>,
}

impl Snapshot {
    pub fn get(&self, c: Control) -> Option<&ControlState> {
        self.controls.iter().find(|s| s.control == c)
    }
}

enum Msg {
    /// Open a camera by unique ID and apply saved values.
    Attach(String, BTreeMap<Control, i32>),
    Detach,
    Set(Control, i32),
    /// Every control to its `reset_value`.
    Reset,
}

/// How long after the last change the worker reads the camera back (see
/// `Camera::set`): an Osmo Pocket 3 gets there in under a second.
const SETTLE: Duration = Duration::from_millis(1200);

/// The worker thread that owns the camera's USB connection.
pub struct Hardware {
    tx: Sender<Msg>,
    state: Arc<Mutex<Snapshot>>,
    /// Joined by tests; the app's worker lives as long as the process.
    #[cfg_attr(not(test), allow(dead_code))]
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Hardware {
    /// Start the worker; `changed` runs (on the worker thread) whenever
    /// the snapshot changes.
    pub fn new(changed: impl Fn() + Send + 'static) -> Self {
        Self::with_opener(changed, IoUsbDevice::open, SETTLE)
    }

    /// `new` with another way to open cameras and another settling time
    /// (tests). `open` runs on the worker thread, where the camera stays.
    fn with_opener<R: Requests>(
        changed: impl Fn() + Send + 'static,
        open: impl Fn(&str) -> Result<Camera<R>, String> + Send + 'static,
        settle: Duration,
    ) -> Self {
        let (tx, rx) = channel();
        let state = Arc::new(Mutex::new(Snapshot::default()));
        let st = state.clone();
        let thread = std::thread::Builder::new().name("uvc".into()).spawn(move || worker(rx, st, changed, open, settle)).expect("uvc thread");
        Hardware { tx, state, thread: Some(thread) }
    }

    /// Wait until every message sent so far is handled (tests).
    #[cfg(test)]
    fn finish(mut self) -> Snapshot {
        let (tx, _) = channel();
        drop(std::mem::replace(&mut self.tx, tx));
        self.thread.take().unwrap().join().unwrap();
        self.snapshot()
    }

    /// Use the camera with this unique ID (does nothing for one already
    /// attached), setting `saved` on it. A camera that isn't on USB has
    /// no controls, and the worker isn't bothered.
    pub fn attach(&self, unique_id: &str, saved: &BTreeMap<Control, i32>) {
        let mut s = self.state.lock().unwrap();
        if s.camera.as_deref() == Some(unique_id) {
            return;
        }
        if usb_id(unique_id).is_none() {
            *s = Snapshot { camera: Some(unique_id.to_string()), ..Default::default() };
            let _ = self.tx.send(Msg::Detach);
            return;
        }
        *s = Snapshot { camera: Some(unique_id.to_string()), probing: true, usb: true, ..Default::default() };
        let _ = self.tx.send(Msg::Attach(unique_id.to_string(), saved.clone()));
    }

    /// Let go of the camera (the next `attach` opens it again).
    pub fn detach(&self) {
        let mut s = self.state.lock().unwrap();
        if s.camera.is_none() {
            return;
        }
        *s = Snapshot::default();
        let _ = self.tx.send(Msg::Detach);
    }

    /// Set a control; the UI shows the value at once.
    pub fn set(&self, c: Control, value: i32) {
        if let Some(s) = self.state.lock().unwrap().controls.iter_mut().find(|s| s.control == c) {
            s.range.cur = value.clamp(s.range.min, s.range.max);
        }
        let _ = self.tx.send(Msg::Set(c, value));
    }

    /// Every control back to the camera's default (autofocus on).
    pub fn reset(&self) {
        let _ = self.tx.send(Msg::Reset);
    }

    pub fn snapshot(&self) -> Snapshot {
        self.state.lock().unwrap().clone()
    }
}

/// The worker: messages in arrival order, consecutive sets of the same
/// control merged into the last (a dragged slider sends many). The
/// snapshot shows the values sent; `settle` after the last change, the
/// worker reads where the camera actually is.
fn worker<R: Requests>(
    rx: Receiver<Msg>,
    state: Arc<Mutex<Snapshot>>,
    changed: impl Fn(),
    open: impl Fn(&str) -> Result<Camera<R>, String>,
    settle: Duration,
) {
    let mut cam: Option<Camera<R>> = None;
    let mut read_back_at: Option<Instant> = None;
    // Change the snapshot if it still shows camera `id` (an attach sent
    // later replaces it, and this result is stale).
    let publish = |id: &str, f: &mut dyn FnMut(&mut Snapshot)| {
        let mut s = state.lock().unwrap();
        if s.camera.as_deref() == Some(id) {
            f(&mut s);
            drop(s);
            changed();
        }
    };
    loop {
        let first = match read_back_at {
            Some(at) => match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(m) => m,
                Err(RecvTimeoutError::Timeout) => {
                    read_back_at = None;
                    if let Some(c) = &cam {
                        read_back(c, &state, &changed);
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            },
            None => match rx.recv() {
                Ok(m) => m,
                Err(_) => break,
            },
        };
        let mut batch = vec![first];
        batch.extend(rx.try_iter());
        let mut msgs: Vec<Msg> = vec![];
        for m in batch {
            if let (Msg::Set(c, _), Some(Msg::Set(last, _))) = (&m, msgs.last())
                && c == last
            {
                msgs.pop();
            }
            msgs.push(m);
        }
        for m in msgs {
            match m {
                Msg::Attach(id, saved) => {
                    cam = None;
                    if state.lock().unwrap().camera.as_deref() != Some(id.as_str()) {
                        continue;
                    }
                    match open(&id) {
                        Ok(c) => {
                            let mut controls: Vec<ControlState> =
                                c.probe().into_iter().map(|(control, range)| ControlState { control, range }).collect();
                            let errors = apply_saved(&c, &mut controls, &saved);
                            if let Some(e) = &errors {
                                eprintln!("rec: warning: camera control: {e}");
                            }
                            if !saved.is_empty() {
                                read_back_at = Some(Instant::now() + settle);
                            }
                            cam = Some(c);
                            publish(&id, &mut |s| {
                                s.probing = false;
                                s.controls = controls.clone();
                                s.error = errors.clone();
                            });
                        }
                        Err(e) => {
                            eprintln!("rec: warning: camera controls: {e}");
                            publish(&id, &mut |s| {
                                s.probing = false;
                                s.error = Some(e.clone());
                            })
                        }
                    }
                }
                // The snapshot was cleared (or set for a camera without
                // controls) when the message was sent.
                Msg::Detach => cam = None,
                Msg::Set(c, v) => {
                    let Some(cam) = &cam else { continue };
                    let Some(range) = state.lock().unwrap().get(c).map(|s| s.range) else { continue };
                    let result = cam.set(c, v, &range).map_err(|e| format!("could not set {}: IOKit error {e:#x}", c.name()));
                    sent(&state, &changed, &[(c, result)]);
                    read_back_at = Some(Instant::now() + settle);
                }
                Msg::Reset => {
                    let Some(cam) = &cam else { continue };
                    let controls = state.lock().unwrap().controls.clone();
                    let auto = controls.iter().any(|s| s.control == Control::FocusAuto);
                    // With autofocus back on, a focus position means nothing.
                    let results: Vec<(Control, Result<i32, String>)> = controls
                        .into_iter()
                        .filter(|s| !(auto && s.control == Control::Focus))
                        .map(|s| {
                            let r = cam.set(s.control, s.control.reset_value(&s.range), &s.range);
                            (s.control, r.map_err(|e| format!("could not reset {}: IOKit error {e:#x}", s.control.name())))
                        })
                        .collect();
                    sent(&state, &changed, &results);
                    read_back_at = Some(Instant::now() + settle);
                }
            }
        }
    }
}

/// Set saved values on a freshly attached camera, autofocus before focus.
/// Returns the last error, if any.
fn apply_saved<R: Requests>(cam: &Camera<R>, controls: &mut [ControlState], saved: &BTreeMap<Control, i32>) -> Option<String> {
    let mut error = None;
    // BTreeMap order: FocusAuto, Focus, Zoom, Roll.
    for (&c, &v) in saved {
        // A manual position means nothing while autofocus is on.
        if c == Control::Focus && saved.get(&Control::FocusAuto).is_some_and(|&a| a != 0) {
            continue;
        }
        let Some(s) = controls.iter_mut().find(|s| s.control == c) else { continue };
        match cam.set(c, v, &s.range) {
            Ok(v) => s.range.cur = v,
            Err(e) => error = Some(format!("could not set {}: IOKit error {e:#x}", c.name())),
        }
    }
    error
}

/// Show the values sent (or the failures) and publish.
fn sent(state: &Mutex<Snapshot>, changed: &impl Fn(), results: &[(Control, Result<i32, String>)]) {
    {
        let mut s = state.lock().unwrap();
        s.error = None;
        for (c, r) in results {
            match r {
                Ok(v) => {
                    if let Some(cs) = s.controls.iter_mut().find(|cs| cs.control == *c) {
                        cs.range.cur = *v;
                    }
                }
                Err(e) => {
                    eprintln!("rec: warning: camera control: {e}");
                    s.error = Some(e.clone());
                }
            }
        }
    }
    changed();
}

/// Read every control's current value once the camera has settled (it may
/// also move by itself: autofocus moves the focus position) and publish.
/// The reads happen without the snapshot's lock: each can take up to its
/// timeout, and the UI reads the snapshot every frame.
fn read_back<R: Requests>(cam: &Camera<R>, state: &Mutex<Snapshot>, changed: &impl Fn()) {
    let (camera, controls) = {
        let s = state.lock().unwrap();
        (s.camera.clone(), s.controls.iter().map(|c| c.control).collect::<Vec<_>>())
    };
    let values: Vec<(Control, i32)> = controls.into_iter().filter_map(|c| cam.read(c, GET_CUR).ok().map(|v| (c, v))).collect();
    {
        let mut s = state.lock().unwrap();
        // Another camera was attached meanwhile.
        if s.camera != camera {
            return;
        }
        for (c, v) in values {
            if let Some(cs) = s.controls.iter_mut().find(|cs| cs.control == c) {
                cs.range.cur = v.clamp(cs.range.min, cs.range.max);
            }
        }
    }
    changed();
}

/// IOKit's USB device interface (IOUSBDeviceInterface182), by hand: the
/// same vtable the C headers declare, used only for the configuration
/// descriptor and control requests.
mod iokit {
    use std::ffi::{c_char, c_void};

    pub type IOReturn = i32;
    pub const SUCCESS: IOReturn = 0;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct UuidBytes(pub [u8; 16]);

    /// IOCFPlugInInterface.
    #[repr(C)]
    pub struct PlugIn {
        _reserved: *mut c_void,
        pub query_interface: unsafe extern "C" fn(*mut c_void, UuidBytes, *mut *mut c_void) -> i32,
        _add_ref: unsafe extern "C" fn(*mut c_void) -> u32,
        pub release: unsafe extern "C" fn(*mut c_void) -> u32,
    }

    /// IOUSBDevRequestTO.
    #[repr(C)]
    pub struct DevRequestTO {
        pub bm_request_type: u8,
        pub b_request: u8,
        pub w_value: u16,
        pub w_index: u16,
        pub w_length: u16,
        pub p_data: *mut c_void,
        pub w_len_done: u32,
        pub no_data_timeout: u32,
        pub completion_timeout: u32,
    }

    /// IOUSBDeviceInterface182, up to DeviceRequestTO.
    #[repr(C)]
    pub struct Device {
        _reserved: *mut c_void,
        _query_interface: *const c_void,
        _add_ref: *const c_void,
        pub release: unsafe extern "C" fn(*mut c_void) -> u32,
        // CreateDeviceAsyncEventSource .. GetLocationID.
        _before_config: [*const c_void; 17],
        pub get_configuration_descriptor_ptr: unsafe extern "C" fn(*mut c_void, u8, *mut *const u8) -> IOReturn,
        // GetConfiguration, SetConfiguration, GetBusFrameNumber, ResetDevice.
        _before_request: [*const c_void; 4],
        _device_request: *const c_void,
        // DeviceRequestAsync, CreateInterfaceIterator, USBDeviceOpenSeize.
        _before_request_to: [*const c_void; 3],
        pub device_request_to: unsafe extern "C" fn(*mut c_void, *mut DevRequestTO) -> IOReturn,
    }

    // 9dc7b780-9ec0-11d4-a54f-000a27052861
    pub const USB_DEVICE_USER_CLIENT_TYPE: [u8; 16] =
        [0x9d, 0xc7, 0xb7, 0x80, 0x9e, 0xc0, 0x11, 0xd4, 0xa5, 0x4f, 0x00, 0x0a, 0x27, 0x05, 0x28, 0x61];
    // c244e858-109c-11d4-91d4-0050e4c6426f
    pub const CF_PLUGIN_INTERFACE: [u8; 16] = [0xc2, 0x44, 0xe8, 0x58, 0x10, 0x9c, 0x11, 0xd4, 0x91, 0xd4, 0x00, 0x50, 0xe4, 0xc6, 0x42, 0x6f];
    // 152fc496-4891-11d5-9d52-000a27801e86
    pub const USB_DEVICE_INTERFACE_182: [u8; 16] = [0x15, 0x2f, 0xc4, 0x96, 0x48, 0x91, 0x11, 0xd5, 0x9d, 0x52, 0x00, 0x0a, 0x27, 0x80, 0x1e, 0x86];

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        pub fn IOServiceMatching(name: *const c_char) -> *mut c_void;
        pub fn IOServiceGetMatchingServices(main_port: u32, matching: *mut c_void, existing: *mut u32) -> IOReturn;
        pub fn IOIteratorNext(iterator: u32) -> u32;
        pub fn IOObjectRelease(object: u32) -> IOReturn;
        pub fn IORegistryEntryCreateCFProperty(entry: u32, key: *const c_void, allocator: *const c_void, options: u32) -> *const c_void;
        pub fn IOCreatePlugInInterfaceForService(
            service: u32,
            plugin_type: *const c_void,
            interface_type: *const c_void,
            the_interface: *mut *mut *mut PlugIn,
            the_score: *mut i32,
        ) -> IOReturn;
    }
}

/// A USB device opened through IOKit. It lives on one thread (the worker).
pub struct IoUsbDevice {
    dev: *mut *mut iokit::Device,
    vc_interface: u8,
}

impl Drop for IoUsbDevice {
    fn drop(&mut self) {
        unsafe { ((**self.dev).release)(self.dev as *mut _) };
    }
}

/// An integer property of an IOKit registry entry.
fn registry_int(entry: u32, key: &str) -> Option<i64> {
    use objc2_core_foundation::{CFNumber, CFRetained, CFString, CFType};
    let key = CFString::from_str(key);
    let p = unsafe { iokit::IORegistryEntryCreateCFProperty(entry, &*key as *const CFString as *const _, std::ptr::null(), 0) };
    let v: CFRetained<CFType> = unsafe { CFRetained::from_raw(std::ptr::NonNull::new(p as *mut CFType)?) };
    v.downcast::<CFNumber>().ok()?.as_i64()
}

impl IoUsbDevice {
    /// The USB device `id` names, as an IOUSBDeviceInterface182.
    fn find(id: UsbId) -> Result<*mut *mut iokit::Device, String> {
        use objc2_core_foundation::CFUUID;
        let uuid = |b: [u8; 16]| {
            CFUUID::constant_uuid_with_bytes(
                None, b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15],
            )
        };
        unsafe {
            let mut iter = 0u32;
            let matching = iokit::IOServiceMatching(c"IOUSBHostDevice".as_ptr());
            if matching.is_null() || iokit::IOServiceGetMatchingServices(0, matching, &mut iter) != iokit::SUCCESS {
                return Err("could not list USB devices".into());
            }
            let mut found = 0u32;
            loop {
                let s = iokit::IOIteratorNext(iter);
                if s == 0 {
                    break;
                }
                let ours = registry_int(s, "idVendor") == Some(id.vendor as i64)
                    && registry_int(s, "idProduct") == Some(id.product as i64)
                    && registry_int(s, "locationID") == Some(id.location as i64);
                if ours && found == 0 {
                    found = s;
                } else {
                    iokit::IOObjectRelease(s);
                }
            }
            iokit::IOObjectRelease(iter);
            if found == 0 {
                return Err(format!("USB device {:04x}:{:04x} at {:#010x} not found", id.vendor, id.product, id.location));
            }
            let (Some(client), Some(plugin_if)) = (uuid(iokit::USB_DEVICE_USER_CLIENT_TYPE), uuid(iokit::CF_PLUGIN_INTERFACE)) else {
                iokit::IOObjectRelease(found);
                return Err("CFUUID failed".into());
            };
            let mut plugin: *mut *mut iokit::PlugIn = std::ptr::null_mut();
            let mut score = 0i32;
            let r = iokit::IOCreatePlugInInterfaceForService(
                found,
                &*client as *const CFUUID as *const _,
                &*plugin_if as *const CFUUID as *const _,
                &mut plugin,
                &mut score,
            );
            iokit::IOObjectRelease(found);
            if r != iokit::SUCCESS || plugin.is_null() {
                return Err(format!("could not open the USB device (IOKit error {r:#x})"));
            }
            let mut dev: *mut std::ffi::c_void = std::ptr::null_mut();
            let q = ((**plugin).query_interface)(plugin as *mut _, iokit::UuidBytes(iokit::USB_DEVICE_INTERFACE_182), &mut dev);
            ((**plugin).release)(plugin as *mut _);
            if q != 0 || dev.is_null() {
                return Err(format!("no USB device interface (error {q:#x})"));
            }
            Ok(dev as *mut *mut iokit::Device)
        }
    }

    /// The device's first configuration descriptor, whole.
    pub fn config_descriptor(&self) -> Result<Vec<u8>, String> {
        unsafe { Self::config_of(self.dev) }
    }

    unsafe fn config_of(dev: *mut *mut iokit::Device) -> Result<Vec<u8>, String> {
        unsafe {
            let mut p: *const u8 = std::ptr::null();
            let r = ((**dev).get_configuration_descriptor_ptr)(dev as *mut _, 0, &mut p);
            if r != iokit::SUCCESS || p.is_null() {
                return Err(format!("no configuration descriptor (IOKit error {r:#x})"));
            }
            let total = u16::from_le_bytes([*p.add(2), *p.add(3)]) as usize;
            Ok(std::slice::from_raw_parts(p, total.max(9)).to_vec())
        }
    }

    /// Open the UVC camera with this AVFoundation unique ID.
    pub fn open(unique_id: &str) -> Result<Camera<IoUsbDevice>, String> {
        let id = usb_id(unique_id).ok_or("not a USB camera")?;
        let dev = Self::find(id)?;
        let desc = match unsafe { Self::config_of(dev) } {
            Ok(d) => d,
            Err(e) => {
                unsafe { ((**dev).release)(dev as *mut _) };
                return Err(e);
            }
        };
        let Some(vc) = parse_config(&desc) else {
            unsafe { ((**dev).release)(dev as *mut _) };
            return Err("not a UVC camera".into());
        };
        Ok(Camera { dev: IoUsbDevice { dev, vc_interface: vc.interface }, vc })
    }

    fn request(&self, request_type: u8, request: u8, unit: u8, selector: u8, data: &mut [u8]) -> Result<usize, i32> {
        let mut req = iokit::DevRequestTO {
            bm_request_type: request_type,
            b_request: request,
            w_value: (selector as u16) << 8,
            w_index: (unit as u16) << 8 | self.vc_interface as u16,
            w_length: data.len() as u16,
            p_data: data.as_mut_ptr() as *mut _,
            w_len_done: 0,
            no_data_timeout: 500,
            completion_timeout: 1000,
        };
        let r = unsafe { ((**self.dev).device_request_to)(self.dev as *mut _, &mut req) };
        if r == iokit::SUCCESS { Ok(req.w_len_done as usize) } else { Err(r) }
    }
}

impl Requests for IoUsbDevice {
    fn get(&self, request: u8, unit: u8, selector: u8, len: usize) -> Result<Vec<u8>, i32> {
        let mut b = vec![0u8; len];
        let n = self.request(0xa1, request, unit, selector, &mut b)?;
        b.truncate(n);
        Ok(b)
    }

    fn set(&self, unit: u8, selector: u8, data: &[u8]) -> Result<(), i32> {
        let mut b = data.to_vec();
        self.request(0x21, SET_CUR, unit, selector, &mut b).map(|_| ())
    }
}

/// `rec-rs camera-controls`: list each camera's hardware controls, or set
/// some on one camera (`sets` as `control=value`). Prints the raw
/// configuration descriptor in hex with `dump`.
pub fn cli(sel: Option<&str>, sets: &[String], dump: bool) -> anyhow::Result<()> {
    let cams = crate::camera::cameras();
    let lower = sel.map(str::to_lowercase);
    let picked: Vec<_> =
        cams.iter().filter(|d| lower.as_ref().is_none_or(|l| d.id.to_lowercase() == *l || d.name.to_lowercase().contains(l.as_str()))).collect();
    if picked.is_empty() {
        anyhow::bail!("no camera matches (see `rec-rs devices`)");
    }
    if !sets.is_empty() && picked.len() != 1 {
        anyhow::bail!("name one camera to set controls on");
    }
    for d in picked {
        let Some(id) = usb_id(&d.id) else {
            if !sets.is_empty() {
                anyhow::bail!("{}: not a USB camera, no hardware controls to set", d.name);
            }
            println!("{}: not a USB camera, no hardware controls", d.name);
            continue;
        };
        println!("{} ({}): USB {:04x}:{:04x} at {:#010x}", d.name, d.id, id.vendor, id.product, id.location);
        let cam = match IoUsbDevice::open(&d.id) {
            Ok(c) => c,
            // Listing goes on to the next camera; a change that can't be
            // made is an error.
            Err(e) if !sets.is_empty() => anyhow::bail!("{}: {e}", d.name),
            Err(e) => {
                println!("  {e}");
                continue;
            }
        };
        if dump {
            let desc = cam.dev.config_descriptor().map_err(anyhow::Error::msg)?;
            for chunk in desc.chunks(32) {
                println!("  {}", chunk.iter().map(|b| format!("{b:02x}")).collect::<String>());
            }
        }
        let vc = &cam.vc;
        let unit = |u: Option<Unit>| u.map_or("none".to_string(), |u| format!("id {} (controls {:#x})", u.id, u.controls));
        println!("  VideoControl interface {}: camera terminal {}, processing unit {}", vc.interface, unit(vc.camera), unit(vc.processing));
        if !vc.extensions.is_empty() {
            let ids: Vec<String> = vc.extensions.iter().map(|(id, _)| id.to_string()).collect();
            println!("  extension units {} (vendor protocol; never used)", ids.join(", "));
        }
        let mut controls = cam.probe();
        for s in sets {
            let (name, v) = s.split_once('=').ok_or_else(|| anyhow::anyhow!("--set {s:?}: use CONTROL=VALUE"))?;
            let c = Control::from_name(name.trim()).ok_or_else(|| anyhow::anyhow!("--set {s:?}: controls are zoom, roll, autofocus, focus"))?;
            let v: i32 = v.trim().parse()?;
            let Some((_, range)) = controls.iter_mut().find(|(k, _)| *k == c) else {
                anyhow::bail!("{} has no {} control", d.name, c.name());
            };
            match cam.set(c, v, range) {
                Ok(sent) => println!("  set {} to {sent}", c.name()),
                Err(e) => anyhow::bail!("could not set {}: IOKit error {e:#x}", c.name()),
            }
        }
        // Where the camera got to, once it has had time to move.
        if !sets.is_empty() {
            std::thread::sleep(SETTLE);
            controls = cam.probe();
        }
        if controls.is_empty() {
            println!("  no controls it answers for");
        }
        for (c, r) in controls {
            if c == Control::FocusAuto {
                println!("  {:<10} {}", c.name(), if r.cur != 0 { "on" } else { "off" });
            } else {
                println!("  {:<10} {} (from {} to {}, default {}, step {})", c.name(), r.cur, r.min, r.max, r.def, r.step);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// A DJI Osmo Pocket 3's configuration descriptor, read from the camera
    /// (`rec-rs camera-controls osmo --dump`).
    const OSMO: &[u8] = include_bytes!("../testdata/osmo-pocket-3-config-descriptor.bin");

    #[test]
    fn unique_ids() {
        assert_eq!(usb_id("0x1000002ca30023"), Some(UsbId { location: 0x0010_0000, vendor: 0x2ca3, product: 0x0023 }));
        assert_eq!(usb_id("0x14200000046d0825"), Some(UsbId { location: 0x1420_0000, vendor: 0x046d, product: 0x0825 }));
        // Built-in, Continuity and virtual cameras.
        assert_eq!(usb_id("6C707041-05AC-0010-0007-000000000001"), None);
        assert_eq!(usb_id("86D0675C-DB4B-4A02-A31C-B2497C114163"), None);
        assert_eq!(usb_id("0x"), None);
        assert_eq!(usb_id("0x12345678901234567"), None);
        assert_eq!(usb_id("0x2ca30023"), None, "no location");
        assert_eq!(usb_id("0xzz"), None);
    }

    #[test]
    fn osmo_descriptor() {
        let vc = parse_config(OSMO).expect("a UVC camera");
        assert_eq!(vc.interface, 0);
        // Zoom (bit 9), pan/tilt (11) and roll (13) absolute.
        assert_eq!(vc.camera, Some(Unit { id: 1, controls: 0x2a00 }));
        // A processing unit with no controls: no white balance, brightness,
        // contrast, gain, saturation or sharpness over USB.
        assert_eq!(vc.processing, Some(Unit { id: 2, controls: 0 }));
        let guid = [0xa2, 0x9e, 0x76, 0x41, 0xde, 0x04, 0x47, 0xe3, 0x8b, 0x2b, 0xf4, 0x34, 0x1a, 0xff, 0x00, 0x3b];
        assert_eq!(vc.extensions, [(6, guid)]);
    }

    #[test]
    fn descriptors_that_are_not_uvc_or_broken() {
        assert_eq!(parse_config(&[]), None);
        // A configuration with an audio interface only.
        let audio = [9, 2, 18, 0, 1, 1, 0, 0x80, 50, 9, 4, 0, 0, 0, 1, 1, 0, 0];
        assert_eq!(parse_config(&audio), None);
        // Truncated in the middle of the camera terminal: the interface is
        // found and nothing is read past the end.
        let _ = parse_config(&OSMO[..40.min(OSMO.len())]);
        // A zero-length descriptor stops the walk.
        let mut bad = OSMO.to_vec();
        bad[9] = 0;
        assert_eq!(parse_config(&bad), None);
    }

    /// A scripted camera: answers like the Osmo Pocket 3 did when probed
    /// (see `.artifacts/uvc/probe-output.txt`), STALLs everything else,
    /// and records every request.
    struct FakeOsmo {
        values: RefCell<HashMap<u8, i32>>,
        log: RefCell<Vec<(u8, u8, u8)>>,
    }

    const STALL: i32 = 0xe000404f_u32 as i32;

    impl FakeOsmo {
        fn new() -> Self {
            FakeOsmo { values: RefCell::new(HashMap::from([(0x06, 2042), (0x08, 1), (0x0b, 100), (0x0f, 0)])), log: RefCell::new(vec![]) }
        }

        fn limits(sel: u8) -> Option<(i32, i32, i32)> {
            match sel {
                0x06 => Some((0, 4095, 2048)),
                0x08 => Some((0, 0, 0)),
                0x0b => Some((100, 400, 100)),
                0x0f => Some((-30, 30, 0)),
                _ => None,
            }
        }
    }

    impl Requests for FakeOsmo {
        fn get(&self, request: u8, unit: u8, selector: u8, len: usize) -> Result<Vec<u8>, i32> {
            self.log.borrow_mut().push((request, unit, selector));
            if unit != 1 {
                return Err(STALL);
            }
            let (min, max, def) = Self::limits(selector).ok_or(STALL)?;
            let v = match request {
                GET_INFO => return Ok(vec![0x03]),
                GET_CUR => self.values.borrow()[&selector],
                GET_MIN => min,
                GET_MAX => max,
                GET_DEF => def,
                GET_RES => return Err(STALL),
                _ => return Err(STALL),
            };
            Ok(encode(v, len))
        }

        fn set(&self, unit: u8, selector: u8, data: &[u8]) -> Result<(), i32> {
            self.log.borrow_mut().push((SET_CUR, unit, selector));
            if unit != 1 || Self::limits(selector).is_none() {
                return Err(STALL);
            }
            let v = decode(data, selector == 0x0f);
            self.values.borrow_mut().insert(selector, v);
            // Turning autofocus off leaves focus where it was; setting focus
            // moves it.
            Ok(())
        }
    }

    fn osmo() -> Camera<FakeOsmo> {
        Camera { dev: FakeOsmo::new(), vc: parse_config(OSMO).unwrap() }
    }

    #[test]
    fn probes_what_the_osmo_answers() {
        let cam = osmo();
        let controls = cam.probe();
        let names: Vec<&str> = controls.iter().map(|(c, _)| c.name()).collect();
        // Focus isn't advertised but answers; pan/tilt isn't offered.
        assert_eq!(names, ["zoom", "roll", "autofocus", "focus"]);
        let get = |c| controls.iter().find(|(k, _)| *k == c).unwrap().1;
        assert_eq!(get(Control::Zoom), Range { min: 100, max: 400, def: 100, step: 1, cur: 100 });
        assert_eq!(get(Control::Roll), Range { min: -30, max: 30, def: 0, step: 1, cur: 0 });
        assert_eq!(get(Control::Focus), Range { min: 0, max: 4095, def: 2048, step: 1, cur: 2042 });
        assert_eq!(get(Control::FocusAuto), Range { min: 0, max: 1, def: 0, step: 1, cur: 1 });
        // Autofocus resets to on, whatever the camera calls its default.
        assert_eq!(Control::FocusAuto.reset_value(&get(Control::FocusAuto)), 1);
        assert_eq!(Control::Zoom.reset_value(&get(Control::Zoom)), 100);
        // Only the camera terminal was asked; never the extension unit (6)
        // or the processing unit (2).
        assert!(cam.dev.log.borrow().iter().all(|&(_, unit, _)| unit == 1), "{:?}", cam.dev.log.borrow());
    }

    #[test]
    fn sets_clamp() {
        let cam = osmo();
        let controls = cam.probe();
        let range = |c| controls.iter().find(|(k, _)| *k == c).unwrap().1;
        assert_eq!(cam.set(Control::Zoom, 250, &range(Control::Zoom)), Ok(250));
        assert_eq!(cam.set(Control::Zoom, 9000, &range(Control::Zoom)), Ok(400));
        // Roll is signed.
        assert_eq!(cam.set(Control::Roll, -12, &range(Control::Roll)), Ok(-12));
        assert_eq!(cam.set(Control::Roll, -99, &range(Control::Roll)), Ok(-30));
        assert_eq!(cam.dev.values.borrow()[&0x0f], -30);
    }

    #[test]
    fn values_encode_little_endian() {
        assert_eq!(encode(400, 2), [0x90, 0x01]);
        assert_eq!(encode(-30, 2), [0xe2, 0xff]);
        assert_eq!(decode(&[0xe2, 0xff], true), -30);
        assert_eq!(decode(&[0xe2, 0xff], false), 65506);
        assert_eq!(decode(&[1], false), 1);
    }

    #[test]
    fn a_camera_without_answers_has_no_controls() {
        struct Mute;
        impl Requests for Mute {
            fn get(&self, _: u8, _: u8, _: u8, _: usize) -> Result<Vec<u8>, i32> {
                Err(STALL)
            }
            fn set(&self, _: u8, _: u8, _: &[u8]) -> Result<(), i32> {
                Err(STALL)
            }
        }
        let cam = Camera { dev: Mute, vc: parse_config(OSMO).unwrap() };
        assert!(cam.probe().is_empty());
        // No camera terminal at all.
        let cam = Camera { dev: FakeOsmo::new(), vc: VideoControl::default() };
        assert!(cam.probe().is_empty());
    }

    /// The worker attaches, applies saved values (autofocus before focus,
    /// focus skipped while autofocus is on), merges a burst of slider
    /// moves and survives a camera that can't be opened.
    #[test]
    fn worker_applies_saved_values_and_merges_sets() {
        // The fake can't cross threads, so the worker builds it; sets are
        // logged where the test can see them.
        struct Logged(FakeOsmo, Arc<Mutex<Vec<(u8, i32)>>>);
        impl Requests for Logged {
            fn get(&self, r: u8, u: u8, s: u8, l: usize) -> Result<Vec<u8>, i32> {
                self.0.get(r, u, s, l)
            }
            fn set(&self, u: u8, s: u8, d: &[u8]) -> Result<(), i32> {
                self.1.lock().unwrap().push((s, decode(d, s == 0x0f)));
                self.0.set(u, s, d)
            }
        }
        let sets = Arc::new(Mutex::new(Vec::<(u8, i32)>::new()));
        let log = sets.clone();
        let opener = move |id: &str| {
            if id == "0x1000002ca30023" {
                Ok(Camera { dev: Logged(FakeOsmo::new(), log.clone()), vc: parse_config(OSMO).unwrap() })
            } else {
                Err("gone".to_string())
            }
        };
        let hw = Hardware::with_opener(|| {}, opener.clone(), Duration::from_millis(20));
        let saved = BTreeMap::from([(Control::Zoom, 200), (Control::FocusAuto, 1), (Control::Focus, 1000), (Control::Roll, 5)]);
        hw.attach("0x1000002ca30023", &saved);
        assert!(hw.snapshot().probing && hw.snapshot().usb);
        // Attaching the same camera again does nothing.
        hw.attach("0x1000002ca30023", &BTreeMap::new());
        let s = hw.finish();
        assert!(!s.probing && s.error.is_none());
        assert_eq!(s.get(Control::Zoom).unwrap().range.cur, 200);
        assert_eq!(s.get(Control::Roll).unwrap().range.cur, 5);
        // Saved: autofocus on, zoom, roll; no manual focus while autofocus.
        assert_eq!(sets.lock().unwrap().clone(), [(0x08, 1), (0x0b, 200), (0x0f, 5)]);

        sets.lock().unwrap().clear();
        let hw = Hardware::with_opener(|| {}, opener.clone(), Duration::from_millis(20));
        hw.attach("0x1000002ca30023", &BTreeMap::new());
        for v in [110, 120, 130, 140] {
            hw.set(Control::Zoom, v);
        }
        // The UI sees the newest value at once.
        let shown = hw.snapshot().get(Control::Zoom).map(|c| c.range.cur);
        assert!(shown.is_none() || shown == Some(140), "{shown:?}");
        hw.reset();
        let s = hw.finish();
        let sets = sets.lock().unwrap().clone();
        // The burst ends at 140 (merged when it arrives at once), then the
        // reset puts zoom, roll and autofocus back.
        let zooms: Vec<i32> = sets.iter().filter(|(sel, _)| *sel == 0x0b).map(|(_, v)| *v).collect();
        assert_eq!(zooms.last(), Some(&100), "{sets:?}");
        assert_eq!(zooms[zooms.len() - 2], 140, "{sets:?}");
        assert!(sets.contains(&(0x08, 1)) && sets.contains(&(0x0f, 0)), "{sets:?}");
        assert_eq!(s.get(Control::Zoom).unwrap().range.cur, 100);

        // A camera that can't be opened: no controls and an error; one that
        // isn't on USB doesn't reach the worker at all.
        let hw = Hardware::with_opener(|| {}, opener.clone(), Duration::from_millis(20));
        hw.attach("0x1000004ca30023", &BTreeMap::new());
        let s = hw.finish();
        assert_eq!(s.camera.as_deref(), Some("0x1000004ca30023"));
        assert!(s.controls.is_empty() && !s.probing);
        assert_eq!(s.error.as_deref(), Some("gone"));
        let hw = Hardware::with_opener(|| {}, opener, Duration::from_millis(20));
        hw.attach("6C707041-05AC-0010-0007-000000000001", &BTreeMap::new());
        let s = hw.finish();
        assert!(!s.usb && !s.probing && s.controls.is_empty() && s.error.is_none());
        // A stale attach doesn't overwrite a newer camera's snapshot.
        let hw = Hardware::with_opener(
            || {},
            |id: &str| {
                std::thread::sleep(std::time::Duration::from_millis(50));
                if id == "0x1000002ca30023" { Ok(Camera { dev: FakeOsmo::new(), vc: parse_config(OSMO).unwrap() }) } else { Err("gone".to_string()) }
            },
            Duration::from_millis(20),
        );
        hw.attach("0x1000002ca30023", &BTreeMap::new());
        hw.attach("6C707041-05AC-0010-0007-000000000001", &BTreeMap::new());
        let s = hw.finish();
        assert_eq!(s.camera.as_deref(), Some("6C707041-05AC-0010-0007-000000000001"));
        assert!(s.controls.is_empty());
    }

    /// Like an Osmo Pocket 3: after a SET, GET_CUR reports the old value for
    /// a while (the zoom ramps, the gimbal turns), and autofocus moves the
    /// focus position by itself.
    struct Slow {
        osmo: FakeOsmo,
        moving: RefCell<HashMap<u8, (i32, Instant)>>,
    }

    impl Requests for Slow {
        fn get(&self, request: u8, unit: u8, selector: u8, len: usize) -> Result<Vec<u8>, i32> {
            if request == GET_CUR
                && let Some(&(old, until)) = self.moving.borrow().get(&selector)
                && Instant::now() < until
            {
                return Ok(encode(old, len));
            }
            if request == GET_CUR && selector == 0x06 && self.osmo.values.borrow()[&0x08] == 1 {
                return Ok(encode(1500, len));
            }
            self.osmo.get(request, unit, selector, len)
        }

        fn set(&self, unit: u8, selector: u8, data: &[u8]) -> Result<(), i32> {
            let old = self.osmo.values.borrow()[&selector];
            self.moving.borrow_mut().insert(selector, (old, Instant::now() + Duration::from_millis(150)));
            self.osmo.set(unit, selector, data)
        }
    }

    /// The UI shows the value sent while the camera gets there, not the
    /// position it reports on the way, then where it ended up.
    #[test]
    fn a_slow_camera_doesnt_snap_the_slider_back() {
        let hw = Hardware::with_opener(
            || {},
            |_: &str| Ok(Camera { dev: Slow { osmo: FakeOsmo::new(), moving: RefCell::new(HashMap::new()) }, vc: parse_config(OSMO).unwrap() }),
            Duration::from_millis(400),
        );
        hw.attach("0x1000002ca30023", &BTreeMap::new());
        let wait_for = |f: &dyn Fn(&Snapshot) -> bool| {
            let t = Instant::now();
            while !f(&hw.snapshot()) && t.elapsed() < Duration::from_secs(3) {
                std::thread::sleep(Duration::from_millis(5));
            }
            hw.snapshot()
        };
        let s = wait_for(&|s| !s.probing);
        assert_eq!(s.get(Control::Focus).unwrap().range.cur, 1500, "where autofocus put it");
        let t = Instant::now();
        hw.set(Control::Zoom, 250);
        hw.set(Control::FocusAuto, 0);
        // The camera still reports zoom 100 and autofocus on; the snapshot
        // keeps what was sent.
        std::thread::sleep(Duration::from_millis(60));
        let s = hw.snapshot();
        assert_eq!(s.get(Control::Zoom).unwrap().range.cur, 250);
        assert_eq!(s.get(Control::FocusAuto).unwrap().range.cur, 0);
        // Once settled, the read-back shows the manual focus position.
        let s = wait_for(&|s| s.get(Control::Focus).is_some_and(|c| c.range.cur == 2042));
        assert_eq!(s.get(Control::Focus).unwrap().range.cur, 2042);
        assert!(t.elapsed() >= Duration::from_millis(350), "read back only after settling: {:?}", t.elapsed());
        assert_eq!(s.get(Control::Zoom).unwrap().range.cur, 250);
        // Autofocus back on moves the focus again.
        hw.set(Control::FocusAuto, 1);
        let s = wait_for(&|s| s.get(Control::Focus).is_some_and(|c| c.range.cur == 1500));
        assert_eq!(s.get(Control::Focus).unwrap().range.cur, 1500);
        assert_eq!(s.get(Control::FocusAuto).unwrap().range.cur, 1);
        hw.finish();
    }

    /// A camera whose every read takes 200 ms (a stalling USB device):
    /// while the worker reads it back, the UI's snapshot never waits.
    #[test]
    fn slow_reads_never_block_the_ui() {
        struct Sluggish(FakeOsmo, Arc<std::sync::atomic::AtomicBool>);
        impl Requests for Sluggish {
            fn get(&self, r: u8, u: u8, s: u8, l: usize) -> Result<Vec<u8>, i32> {
                if r == GET_CUR && self.1.load(std::sync::atomic::Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(200));
                }
                self.0.get(r, u, s, l)
            }
            fn set(&self, u: u8, s: u8, d: &[u8]) -> Result<(), i32> {
                self.0.set(u, s, d)
            }
        }
        let slow = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = slow.clone();
        let hw = Hardware::with_opener(
            || {},
            move |_: &str| Ok(Camera { dev: Sluggish(FakeOsmo::new(), flag.clone()), vc: parse_config(OSMO).unwrap() }),
            Duration::from_millis(10),
        );
        hw.attach("0x1000002ca30023", &BTreeMap::new());
        let t = Instant::now();
        while hw.snapshot().probing && t.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(5));
        }
        slow.store(true, std::sync::atomic::Ordering::SeqCst);
        hw.set(Control::Zoom, 150);
        // The read-back starts after 10 ms and takes 4 x 200 ms.
        std::thread::sleep(Duration::from_millis(60));
        let mut worst = Duration::ZERO;
        let t = Instant::now();
        while t.elapsed() < Duration::from_millis(600) {
            let s = Instant::now();
            let snap = hw.snapshot();
            worst = worst.max(s.elapsed());
            assert_eq!(snap.get(Control::Zoom).unwrap().range.cur, 150);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(worst < Duration::from_millis(50), "the UI waited {worst:?} for the snapshot");
        hw.finish();
    }

    /// Read the real camera named by REC_UVC_CAMERA (default: the first USB
    /// camera) without changing anything. Run with --ignored --nocapture.
    #[test]
    #[ignore]
    fn live_probe() {
        let sel = std::env::var("REC_UVC_CAMERA").ok();
        cli(sel.as_deref(), &[], false).unwrap();
    }
}
