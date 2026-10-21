//! The app: a main window (live preview, sources, record/pause, timer,
//! level meters), a settings window and a menu bar item, in egui on wgpu
//! with a transparent titlebar and Inter (the paper-rs look). ⇧⌘R toggles
//! recording from anywhere; ⇧⌘S saves the replay buffer while it is on.
//!
//! One live encode (`crate::live`) is attached to every capture session:
//! it feeds the replay buffer and the streams, so they keep running across
//! preview and recording sessions and while the window is closed.

mod dock;
mod macos;
mod menubar;
mod preview;
mod theme;
mod ui;
mod updater;
mod widgets;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

use crate::compose::Scene;
use crate::record::{self, Session};
use crate::settings::Settings;
use crate::writer::Stats;

/// Things that happen to the app from menus, the hotkey, signals and
/// buttons; handled on the main thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmd {
    ToggleRecording,
    TogglePause,
    ShowWindow,
    ShowSettings,
    RevealLast,
    OpenFolder,
    SaveReplay,
    ToggleStream,
    /// Start Sparkle, queued at launch so it doesn't hold up the first frame.
    StartUpdater,
    CheckForUpdates,
    Quit,
}

/// Hotkey registration IDs.
const HOTKEY_RECORD: u32 = 1;
const HOTKEY_REPLAY: u32 = 2;

static QUEUE: Mutex<Vec<Cmd>> = Mutex::new(Vec::new());
static PROXY: OnceLock<Mutex<EventLoopProxy<()>>> = OnceLock::new();
/// A capture device was plugged in or removed.
static DEVICES_CHANGED: AtomicBool = AtomicBool::new(false);

/// REC_GUI_BACKGROUND: open windows without taking focus from the app in
/// use (scripted runs and screenshots while someone works).
fn background() -> bool {
    std::env::var_os("REC_GUI_BACKGROUND").is_some()
}

/// Wake the event loop (from any thread).
fn wake() {
    if let Some(p) = PROXY.get() {
        let _ = p.lock().unwrap().send_event(());
    }
}

pub fn push(c: Cmd) {
    QUEUE.lock().unwrap().push(c);
    wake();
}

/// SIGINT (`rec-rs stop`) stops the recording; SIGTERM/SIGHUP quit after
/// saving it. SIGUSR1/2 (`rec-rs pause`/`resume`) use record's flags.
static SIG_STOP: AtomicBool = AtomicBool::new(false);
static SIG_QUIT: AtomicBool = AtomicBool::new(false);
extern "C" fn on_sigint(_: libc::c_int) {
    SIG_STOP.store(true, Ordering::SeqCst);
}
extern "C" fn on_sigterm(_: libc::c_int) {
    SIG_QUIT.store(true, Ordering::SeqCst);
}

/// One OS window rendered with egui.
struct Win {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    egui: egui_winit::State,
    ctx: egui::Context,
    renderer: egui_wgpu::Renderer,
    repaint_at: Option<Instant>,
    /// Fully covered, minimized or the screen locked: nothing would be
    /// presented, so don't draw.
    occluded: bool,
}

struct Gpu {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
}

/// Level meter state for one source.
#[derive(Default, Clone, Copy)]
pub struct Meter {
    /// Displayed level in dBFS (decays smoothly).
    pub db: f32,
    pub peak_db: f32,
    pub peak_at: Option<Instant>,
}

impl Meter {
    fn feed(&mut self, linear: f32, dt: f32) {
        let db = if linear <= 1e-5 { -90.0 } else { 20.0 * linear.log10() };
        self.db = db.max(self.db - 24.0 * dt).max(-90.0);
        if db >= self.peak_db || self.peak_at.is_none_or(|t| t.elapsed() > Duration::from_millis(1500)) {
            self.peak_db = db;
            self.peak_at = Some(Instant::now());
        }
    }
}

#[derive(Clone)]
pub struct Device {
    pub id: String,
    pub name: String,
}

/// A running stream to one destination.
pub struct Stream {
    /// Output ID in the live hub.
    pub id: u64,
    pub url: String,
    pub status: Option<crate::live::OutputStatus>,
}

/// Everything the UI shows and edits.
pub struct State {
    pub settings: Settings,
    pub recording: Option<Session>,
    pub preview: Option<Session>,
    /// A running camera between sessions (preview -> recording -> preview),
    /// handed over so it never restarts dark (see `Session::take_camera`).
    spare_cam: Option<crate::camera::Camera>,
    /// Settings the running preview session was started with.
    preview_settings: Option<Settings>,
    pub finishing: bool,
    pub last_file: Option<PathBuf>,
    pub error: Option<String>,
    pub meters: [Meter; 2],
    pub displays: Vec<String>,
    pub windows: Vec<(u32, String)>,
    pub cameras: Vec<Device>,
    pub mics: Vec<Device>,
    pub hotkey_ok: bool,
    /// The preview frame as a native (IOSurface-backed) egui texture.
    pub preview_tex: Option<egui::TextureId>,
    /// Its size in pixels.
    pub preview_px: [u32; 2],
    pub preview_aspect: f32,
    /// Preview frames shown (for the self-test).
    frames_shown: u64,
    /// UI requests handled after the frame.
    pub cmds: Vec<Cmd>,
    pub refresh_devices: bool,
    /// Overlay last sent to the running session.
    pip_applied: Option<crate::compose::PipSpec>,
    /// Audio levels last sent to the running session.
    volumes_applied: Option<[crate::mixer::Volume; 2]>,
    /// Camera aspect ratio from the running session (for drag hit-testing).
    pub cam_aspect: f64,
    pub permissions_missing: bool,
    /// The shared live encode (replay buffer, streams).
    pub live: Arc<crate::live::Live>,
    /// Replay buffer (seconds held, bytes) when on.
    pub replay_stats: Option<(f64, usize)>,
    /// ⇧⌘S is registered (it is only held while the replay buffer is on).
    pub replay_hotkey: Option<bool>,
    replay_saves: Vec<std::thread::JoinHandle<Result<PathBuf>>>,
    /// One per destination enabled at Go Live; empty when not streaming.
    pub streams: Vec<Stream>,
    /// Stream keys being typed, by destination URL; moved to the Keychain
    /// on save and cleared.
    pub key_inputs: std::collections::HashMap<String, String>,
    /// Whether the Keychain has a key, by destination URL.
    key_saved: std::collections::HashMap<String, bool>,
    /// A short confirmation (replay saved) and when it was shown.
    pub notice: Option<(String, Instant)>,
    /// The mic the running session opened (None = system default), and the
    /// device fallback warning last shown, so it isn't repeated on restarts.
    mic_in_use: Option<Option<String>>,
    mic_warned: Option<String>,
}

impl State {
    /// Whether the Keychain holds a stream key for the destination `url`
    /// (looked up once per URL).
    pub fn has_stream_key(&mut self, url: &str) -> bool {
        if url.is_empty() {
            return false;
        }
        *self.key_saved.entry(url.to_string()).or_insert_with(|| crate::live::keychain::get(url).is_some())
    }

    /// Store the key typed for `url` in the Keychain and forget it.
    pub fn save_stream_key(&mut self, url: &str) {
        let key = self.key_inputs.remove(url).unwrap_or_default();
        let key = key.trim();
        if url.is_empty() || key.is_empty() {
            return;
        }
        match crate::live::keychain::set(url, key) {
            Ok(()) => {
                self.key_saved.insert(url.to_string(), true);
            }
            Err(e) => self.error = Some(format!("Could not save the stream key: {e:#}")),
        }
    }

    pub fn delete_stream_key(&mut self, url: &str) {
        let _ = crate::live::keychain::delete(url);
        self.key_saved.insert(url.to_string(), false);
    }

    pub fn streaming(&self) -> bool {
        !self.streams.is_empty()
    }

    pub fn is_paused(&self) -> bool {
        self.recording.as_ref().is_some_and(|s| s.is_paused())
    }

    /// Recorded time (excluding pauses) of the running recording.
    pub fn clock(&self) -> String {
        self.recording.as_ref().map(|s| record::clock(s.recorded_secs())).unwrap_or_default()
    }

    /// Send the overlay settings to the running session if they changed:
    /// moving or restyling the camera is live in the preview and the file.
    pub fn apply_pip(&mut self) {
        let Some(sess) = self.recording.as_ref().or(self.preview.as_ref()) else {
            self.pip_applied = None;
            return;
        };
        if let Some(a) = sess.cam_aspect() {
            self.cam_aspect = a;
        }
        let spec = self.settings.pip_spec();
        if self.pip_applied.as_ref() != Some(&spec) {
            sess.set_pip(spec.clone());
            self.pip_applied = Some(spec);
        }
    }

    /// Send the fader levels to the running session if they changed: the
    /// preview's meters, the file and the live encode follow at once.
    pub fn apply_volumes(&mut self) {
        let Some(sess) = self.recording.as_ref().or(self.preview.as_ref()) else {
            self.volumes_applied = None;
            return;
        };
        let v = self.settings.volumes();
        if self.volumes_applied != Some(v) {
            sess.set_volumes(v);
            self.volumes_applied = Some(v);
        }
    }

    /// Whether the running session can switch to `scene` live.
    pub fn can_switch_live(&self, scene: Scene) -> bool {
        let cam_running = self.recording.as_ref().is_some_and(|s| s.camera_name().is_some());
        !scene.uses_cam() || cam_running
    }
}

/// A finalized recording: (file, outcome).
type Finished = (PathBuf, Result<Stats, String>);

struct App {
    gpu: Option<Gpu>,
    main: Option<Win>,
    settings_win: Option<Win>,
    main_visible: bool,
    st: State,
    menubar: Option<menubar::MenuBar>,
    _main_menu: Option<objc2::rc::Retained<menubar::Target>>,
    scaler: preview::Scaler,
    /// Textures of the last preview frames (egui may still draw them).
    preview_textures: std::collections::VecDeque<wgpu::Texture>,
    last_tick: Instant,
    last_meter: Instant,
    start_recording: bool,
    launched: Instant,
    menu_shown: bool,
    selftest: Option<SelfTest>,
    finished: Arc<Mutex<Option<Finished>>>,
}

fn load_devices(st: &mut State) {
    st.displays = crate::screen::displays()
        .map(|ds| {
            ds.iter()
                .enumerate()
                .map(|(i, d)| format!("Display {} — {}×{}", i + 1, d.pixels.0, d.pixels.1))
                .collect()
        })
        .unwrap_or_default();
    st.windows = crate::screen::windows()
        .map(|ws| {
            ws.into_iter()
                .filter(|w| !w.title.is_empty())
                .map(|w| (w.id, format!("{} — {}", w.app, w.title)))
                .collect()
        })
        .unwrap_or_default();
    st.cameras = crate::camera::cameras().into_iter().map(|d| Device { id: d.id, name: d.name }).collect();
    st.mics = crate::camera::microphones().into_iter().map(|d| Device { id: d.id, name: d.name }).collect();
}

/// The banner for a preview that didn't start, and whether it failed for
/// missing privacy grants, which shows the Request Permissions button. The
/// CLI's wording names a `rec` command, and installing the app doesn't put one
/// on the PATH.
fn preview_error(e: &anyhow::Error) -> (String, bool) {
    match e.downcast_ref::<record::MissingPermissions>() {
        Some(m) => (
            format!(
                "Preview unavailable: Rec needs permission for {}. Click Request Permissions, then allow Rec when macOS asks or in System Settings > Privacy & Security.",
                m.0.join(", ")
            ),
            true,
        ),
        None => (format!("Preview unavailable: {e:#}"), false),
    }
}

impl App {
    fn new(start_recording: bool) -> App {
        let mut st = State {
            settings: Settings::load(),
            recording: None,
            preview: None,
            spare_cam: None,
            preview_settings: None,
            finishing: false,
            last_file: None,
            error: None,
            meters: Default::default(),
            displays: vec![],
            windows: vec![],
            cameras: vec![],
            mics: vec![],
            hotkey_ok: true,
            preview_tex: None,
            preview_px: [0, 0],
            preview_aspect: 16.0 / 9.0,
            frames_shown: 0,
            cmds: vec![],
            refresh_devices: false,
            permissions_missing: false,
            pip_applied: None,
            volumes_applied: None,
            cam_aspect: 16.0 / 9.0,
            live: crate::live::Live::new(),
            replay_stats: None,
            replay_hotkey: None,
            replay_saves: vec![],
            streams: vec![],
            key_inputs: Default::default(),
            key_saved: Default::default(),
            notice: None,
            mic_in_use: None,
            mic_warned: None,
        };
        st.live.set_config(st.settings.live_config());
        load_devices(&mut st);
        crate::camera::on_device_change(|| {
            DEVICES_CHANGED.store(true, Ordering::SeqCst);
            wake();
        });
        App {
            gpu: None,
            main: None,
            settings_win: None,
            main_visible: true,
            st,
            menubar: None,
            _main_menu: None,
            scaler: preview::Scaler::new(),
            preview_textures: Default::default(),
            last_tick: Instant::now(),
            last_meter: Instant::now(),
            start_recording,
            launched: Instant::now(),
            menu_shown: false,
            selftest: std::env::var_os("REC_GUI_SELFTEST").map(|_| SelfTest::default()),
            finished: Arc::new(Mutex::new(None)),
        }
    }

    fn gpu(&mut self, surface: &wgpu::Surface<'static>) -> &Gpu {
        if self.gpu.is_none() {
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
            let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                compatible_surface: Some(surface),
                ..Default::default()
            }))
            .expect("no compatible GPU adapter");
            let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("rec-rs"),
                required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
                ..Default::default()
            }))
            .expect("create device");
            self.gpu = Some(Gpu { instance, adapter, device, queue });
        }
        self.gpu.as_ref().unwrap()
    }

    fn open_window(&mut self, el: &ActiveEventLoop, title: &str, size: (f64, f64), min: (f64, f64)) -> Win {
        use winit::platform::macos::WindowAttributesExtMacOS;
        let attrs = Window::default_attributes()
            .with_title(title)
            .with_inner_size(winit::dpi::LogicalSize::new(size.0, size.1))
            .with_min_inner_size(winit::dpi::LogicalSize::new(min.0, min.1))
            .with_titlebar_transparent(true)
            .with_fullsize_content_view(true)
            .with_title_hidden(true);
        let window = Arc::new(el.create_window(attrs).expect("create window"));
        // A throwaway instance creates the first surface; the shared one
        // after that (surfaces must come from the adapter's instance).
        let surface = match &self.gpu {
            Some(g) => g.instance.create_surface(window.clone()).expect("surface"),
            None => {
                let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
                let s = instance.create_surface(window.clone()).expect("surface");
                let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                    compatible_surface: Some(&s),
                    ..Default::default()
                }))
                .expect("no compatible GPU adapter");
                let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                    label: Some("rec-rs"),
                    required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
                    ..Default::default()
                }))
                .expect("create device");
                self.gpu = Some(Gpu { instance, adapter, device, queue });
                s
            }
        };
        let g = self.gpu(&surface);
        let caps = surface.get_capabilities(&g.adapter);
        let format = caps.formats.iter().copied().find(|f| !f.is_srgb()).unwrap_or(caps.formats[0]);
        let px = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: px.width.max(1),
            height: px.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            color_space: Default::default(),
        };
        surface.configure(&g.device, &config);
        let renderer = egui_wgpu::Renderer::new(&g.device, format, egui_wgpu::RendererOptions::default());
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let egui = egui_winit::State::new(
            ctx.clone(),
            egui::ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            window.theme(),
            Some(g.device.limits().max_texture_dimension_2d as usize),
        );
        let (x, y) = theme::TRAFFIC_LIGHTS;
        macos::inset_traffic_lights(&window, x, y);
        // egui and the preview are sRGB: let macOS convert them for the
        // display (wide-gamut panels otherwise show them oversaturated).
        macos::tag_layers_srgb(&window);
        window.request_redraw();
        Win { window, surface, config, egui, ctx, renderer, repaint_at: None, occluded: false }
    }

    fn show_main(&mut self, el: &ActiveEventLoop) {
        if self.main.is_none() {
            // REC_MAIN_SIZE=WxH or full: open at this size or fullscreen (layout checks).
            let size = std::env::var("REC_MAIN_SIZE")
                .ok()
                .and_then(|v| v.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))))
                .unwrap_or((1040.0, 660.0));
            let w = self.open_window(el, "Rec", size, (860.0, 560.0));
            if std::env::var("REC_MAIN_SIZE").as_deref() == Ok("full") {
                w.window.set_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
            }
            self.main = Some(w);
        }
        if let Some(w) = &self.main {
            w.window.set_visible(true);
            if !background() {
                w.window.focus_window();
            }
            w.window.request_redraw();
        }
        self.main_visible = true;
        preview::set_wanted(true);
        if !background() {
            macos::activate();
        }
        self.sync_preview();
    }

    fn hide_main(&mut self) {
        if let Some(w) = &self.main {
            w.window.set_visible(false);
        }
        self.main_visible = false;
        preview::set_wanted(false);
        self.sync_preview();
    }

    fn show_settings(&mut self, el: &ActiveEventLoop) {
        if self.settings_win.is_none() {
            let w = self.open_window(el, "Rec Settings", (520.0, 820.0), (460.0, 480.0));
            self.settings_win = Some(w);
        }
        if let Some(w) = &self.settings_win {
            if !background() {
                w.window.focus_window();
            }
            w.window.request_redraw();
        }
        if !background() {
            macos::activate();
        }
    }

    fn options(&mut self, output: Option<&std::path::Path>) -> Result<(record::Options, Option<crate::screen::Target>)> {
        // A display that was unplugged falls back to the main one.
        let mut settings = self.st.settings.clone();
        let mut warning = None;
        let count = crate::screen::displays().map(|d| d.len()).unwrap_or(0);
        if count > 0 && settings.window.is_none() && settings.display > count {
            warning = Some(format!("Display {} not connected; using display 1", settings.display));
            settings.display = 1;
            eprintln!("rec: warning: {}", warning.as_deref().unwrap_or_default());
        }
        let mut args = settings.record_args(output);
        // REC_TEST_PATTERN: the CLI's synthetic source, for self-tests that
        // need no privacy grants.
        if std::env::var_os("REC_TEST_PATTERN").is_some() {
            args.push("--test-pattern".into());
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let (mut o, t) = crate::prepare(crate::parse_record_args(&refs)?)?;
        o.live = Some(self.st.live.clone());
        // With the mutes, which the CLI doesn't take.
        o.volumes = settings.volumes();
        // Fallbacks never block: say so once.
        let warning = [warning, o.mic_warning.clone()].into_iter().flatten().collect::<Vec<_>>().join(" · ");
        let warning = Some(warning).filter(|w| !w.is_empty());
        if warning != self.st.mic_warned {
            if let Some(w) = &warning {
                self.st.notice = Some((w.clone(), Instant::now()));
            }
            self.st.mic_warned = warning;
        }
        self.st.mic_in_use = o.mic.clone();
        Ok((o, t))
    }

    /// After a device was plugged in or removed: refresh the lists, and
    /// restart the preview if the saved mic now resolves differently.
    fn devices_changed(&mut self) {
        load_devices(&mut self.st);
        let s = &self.st.settings;
        let Some(sel) = s.mic.as_deref().filter(|_| s.mic_enabled) else { return };
        if self.st.recording.is_some() || self.st.preview.is_none() {
            return;
        }
        let want = Some(crate::camera::pick_mic(sel, s.mic_name.as_deref()).id);
        if self.st.mic_in_use != want {
            self.st.preview_settings = None;
        }
    }

    /// Whether the live encode needs a capture session (replay or stream).
    fn live_wanted(&self) -> bool {
        self.st.settings.replay_enabled || self.st.streaming()
    }

    /// Apply the replay and live-encode settings, and hold ⇧⌘S while the
    /// replay buffer is on (it is "Save As" in most apps otherwise).
    fn sync_live(&mut self) {
        let cfg = self.st.settings.live_config();
        if self.st.live.config() != cfg {
            self.st.live.set_config(cfg);
        }
        let want = self.st.settings.replay_enabled.then(|| self.st.settings.replay_secs.clamp(15, 300) as f64);
        let have = self.st.live.replay_stats().is_some();
        if want.is_some() || have {
            self.st.live.set_replay(want);
        }
        self.st.replay_stats = self.st.live.replay_stats();
        match (want.is_some(), self.st.replay_hotkey.is_some()) {
            (true, false) => {
                let ok = crate::hotkey::register(HOTKEY_REPLAY, crate::hotkey::KEY_S, crate::hotkey::CMD | crate::hotkey::SHIFT, || {
                    push(Cmd::SaveReplay)
                });
                if let Err(code) = ok {
                    eprintln!("rec: could not register ⇧⌘S (error {code}); use the Save Replay button");
                }
                self.st.replay_hotkey = Some(ok.is_ok());
            }
            (false, true) => {
                crate::hotkey::unregister(HOTKEY_REPLAY);
                self.st.replay_hotkey = None;
            }
            _ => {}
        }
        if self.st.streaming() {
            let statuses = self.st.live.statuses();
            for s in &mut self.st.streams {
                s.status = statuses.iter().find(|(i, _)| *i == s.id).map(|(_, st)| st.clone());
            }
        }
    }

    fn save_replay(&mut self) {
        if self.st.replay_stats.is_none() {
            return;
        }
        let path = self.st.settings.output_dir.join(crate::settings::timestamped("replay", "mp4"));
        let r = std::fs::create_dir_all(&self.st.settings.output_dir)
            .map_err(anyhow::Error::from)
            .and_then(|_| self.st.live.save_replay(path));
        match r {
            Ok(h) => self.st.replay_saves.push(h),
            Err(e) => self.st.error = Some(format!("Replay: {e:#}")),
        }
    }

    /// Go live on every enabled destination, or stop them all. Starting is
    /// all or nothing: one that can't start (no key, bad URL) stops the
    /// rest, so a stream never runs on only some of them by accident.
    /// Network failures later show in each one's status instead.
    fn toggle_stream(&mut self) {
        if self.st.streaming() {
            for s in std::mem::take(&mut self.st.streams) {
                self.st.live.remove_output(s.id);
            }
            return;
        }
        let urls = self.st.settings.stream_urls();
        if urls.is_empty() {
            self.st.error = Some("Could not go live: turn on a destination in Stream first".into());
            return;
        }
        let start = |url: &str| crate::live::resolve_destination(url).and_then(|(u, key)| crate::live::start_output(&u, &key));
        match self.st.live.add_outputs(&urls, start) {
            Ok(added) => {
                self.st.streams = added.into_iter().map(|(id, url)| Stream { id, url, status: None }).collect();
                self.st.error = None;
                self.sync_preview();
            }
            Err(e) => self.st.error = Some(format!("{e:#}")),
        }
    }

    /// Run a preview session while the window is visible and nothing is
    /// recording; restart it when the settings change.
    fn sync_preview(&mut self) {
        // The replay buffer and streams keep a session running while the
        // window is closed.
        let want = (self.main_visible || self.live_wanted()) && self.st.recording.is_none() && !self.st.finishing;
        // Overlay changes are applied live (`apply_pip`), not by restarting.
        let stale = self.st.preview_settings.as_ref().map(Settings::capture_key) != Some(self.st.settings.capture_key());
        if self.st.preview.is_some() && (!want || stale) {
            if let Some(mut s) = self.st.preview.take() {
                if want {
                    self.st.spare_cam = s.take_camera();
                }
                let _ = s.finish();
            }
            self.st.preview_settings = None;
        }
        if want && self.st.preview.is_none() && self.st.error.is_none() {
            let cb = preview::callback(Arc::new(wake));
            let cam = self.st.spare_cam.take();
            match self.options(None).and_then(|(o, t)| Session::start_with_camera(&o, t, false, Some(cb), cam)) {
                Ok(s) => {
                    self.st.pip_applied = Some(self.st.settings.pip_spec());
                    self.st.volumes_applied = Some(self.st.settings.volumes());
                    self.st.preview = Some(s);
                    self.st.permissions_missing = false;
                }
                Err(e) => {
                    let (msg, missing) = preview_error(&e);
                    self.st.permissions_missing = missing;
                    self.st.error = Some(msg);
                }
            }
            self.st.preview_settings = Some(self.st.settings.clone());
        }
        // Nothing took the camera (window hidden, or a scene without it):
        // turn it off.
        if !self.st.finishing
            && let Some(c) = self.st.spare_cam.take()
        {
            c.stop();
        }
    }

    fn toggle_recording(&mut self) {
        if self.st.finishing {
            return;
        }
        if self.st.recording.is_some() {
            self.stop_recording();
        } else {
            self.start_recording();
        }
    }

    fn start_recording(&mut self) {
        // One capture session at a time: the recording feeds the preview,
        // and takes over its camera (already exposed for the room).
        let mut cam = self.st.spare_cam.take();
        if let Some(mut s) = self.st.preview.take() {
            cam = cam.or_else(|| s.take_camera());
            let _ = s.finish();
        }
        self.st.preview_settings = None;
        self.st.error = None;
        let file = self.st.settings.new_output_file();
        let cb = preview::callback(Arc::new(wake));
        let r = std::fs::create_dir_all(&self.st.settings.output_dir)
            .map_err(anyhow::Error::from)
            .and_then(|_| self.options(Some(&file)))
            .and_then(|(o, t)| Session::start_with_camera(&o, t, true, Some(cb), cam));
        match r {
            Ok(sess) => {
                let _ = std::fs::write(record::pidfile_path(), format!("{}\n{}\n", std::process::id(), file.display()));
                self.st.pip_applied = Some(self.st.settings.pip_spec());
                self.st.volumes_applied = Some(self.st.settings.volumes());
                self.st.recording = Some(sess);
                record::PAUSE_REQ.store(false, Ordering::SeqCst);
                record::RESUME_REQ.store(false, Ordering::SeqCst);
            }
            Err(e) => {
                eprintln!("rec: could not start recording: {e:#}");
                self.st.error = Some(format!("Could not start recording: {e:#}"));
                self.sync_preview();
            }
        }
    }

    fn stop_recording(&mut self) {
        let Some(mut sess) = self.st.recording.take() else { return };
        // The camera goes back to the preview once the file is finished.
        self.st.spare_cam = sess.take_camera();
        let file = sess.output.clone().unwrap_or_default();
        self.st.finishing = true;
        let done = self.finished.clone();
        // Finalizing can take a moment; keep the UI responsive.
        std::thread::spawn(move || {
            let r = objc2::rc::autoreleasepool(|_| sess.finish()).map_err(|e| format!("{e:#}"));
            *done.lock().unwrap() = Some((file, r));
            let _ = std::fs::remove_file(record::pidfile_path());
            wake();
        });
    }

    fn toggle_pause(&mut self) {
        if let Some(s) = &self.st.recording {
            if s.is_paused() {
                s.resume();
            } else {
                s.pause();
            }
        }
    }

    fn quit(&mut self, el: &ActiveEventLoop) {
        if let Some(s) = self.st.recording.take() {
            let file = s.output.clone();
            let r = objc2::rc::autoreleasepool(|_| s.finish());
            if self.selftest.is_some() {
                println!("selftest: saved on quit={file:?} ok={}", r.is_ok());
            }
            let _ = std::fs::remove_file(record::pidfile_path());
        }
        while self.st.finishing && self.finished.lock().unwrap().is_none() {
            std::thread::sleep(Duration::from_millis(20));
        }
        if let Some(s) = self.st.preview.take() {
            let _ = s.finish();
        }
        self.st.live.stop_all();
        for h in self.st.replay_saves.drain(..) {
            let _ = h.join();
        }
        dock::allow_terminate();
        el.exit();
    }

    fn handle(&mut self, el: &ActiveEventLoop, c: Cmd) {
        match c {
            Cmd::ToggleRecording => self.toggle_recording(),
            Cmd::TogglePause => self.toggle_pause(),
            Cmd::ShowWindow => self.show_main(el),
            Cmd::ShowSettings => self.show_settings(el),
            Cmd::RevealLast => {
                if let Some(f) = &self.st.last_file {
                    macos::reveal(f);
                }
            }
            Cmd::OpenFolder => macos::open_folder(&self.st.settings.output_dir),
            Cmd::SaveReplay => self.save_replay(),
            Cmd::ToggleStream => self.toggle_stream(),
            Cmd::StartUpdater => updater::start(),
            Cmd::CheckForUpdates => updater::check_for_updates(),
            Cmd::Quit => self.quit(el),
        }
    }

    /// Periodic work: commands, signals, finished recordings, failures,
    /// menu bar, meters.
    fn tick(&mut self, el: &ActiveEventLoop) {
        // REC_SHOW_MENU: open the menu bar menu once, 3 s after launch
        // (for screenshots). It tracks modally until dismissed.
        if !self.menu_shown && std::env::var_os("REC_SHOW_MENU").is_some() && self.launched.elapsed() > Duration::from_secs(3) {
            self.menu_shown = true;
            if let Some(m) = &self.menubar {
                m.pop_up();
            }
        }
        if let Some(mut t) = self.selftest.take() {
            t.step(self);
            self.selftest = Some(t);
        }
        if SIG_QUIT.swap(false, Ordering::SeqCst) {
            self.quit(el);
            return;
        }
        if SIG_STOP.swap(false, Ordering::SeqCst) && self.st.recording.is_some() {
            self.stop_recording();
        }
        if self.st.recording.as_ref().is_some_and(|s| s.has_frames()) {
            if record::PAUSE_REQ.swap(false, Ordering::SeqCst) {
                self.st.recording.as_ref().unwrap().pause();
            }
            if record::RESUME_REQ.swap(false, Ordering::SeqCst) {
                self.st.recording.as_ref().unwrap().resume();
            }
        }
        // `rec-rs save-replay` (SIGINFO).
        if record::REPLAY_REQ.swap(false, Ordering::SeqCst) {
            push(Cmd::SaveReplay);
        }
        self.sync_live();
        let cmds: Vec<Cmd> = std::mem::take(&mut *QUEUE.lock().unwrap());
        let ui_cmds = std::mem::take(&mut self.st.cmds);
        for c in cmds.into_iter().chain(ui_cmds) {
            self.handle(el, c);
        }
        let (finished, running): (Vec<_>, Vec<_>) = self.st.replay_saves.drain(..).partition(|h| h.is_finished());
        self.st.replay_saves = running;
        for h in finished {
            match h.join() {
                Ok(Ok(p)) => {
                    eprintln!("rec: replay saved {}", p.display());
                    self.st.notice = Some((format!("Replay saved: {}", p.file_name().unwrap_or_default().to_string_lossy()), Instant::now()));
                    self.st.last_file = Some(p);
                }
                Ok(Err(e)) => self.st.error = Some(format!("Replay: {e:#}")),
                Err(_) => self.st.error = Some("Replay: the writer crashed".into()),
            }
        }
        let done = self.finished.lock().unwrap().take();
        if let Some((file, r)) = done {
            self.st.finishing = false;
            match r {
                Ok(_) => self.st.last_file = Some(file),
                Err(e) => self.st.error = Some(format!("Recording failed: {e}")),
            }
        }
        let failed = self.st.recording.as_ref().is_some_and(|s| s.failed());
        if failed {
            let err = self.st.recording.as_ref().and_then(|s| s.error()).unwrap_or_default();
            self.stop_recording();
            self.st.error = Some(format!("Recording stopped: {err}"));
        }
        if self.st.preview.as_ref().is_some_and(|s| s.failed()) {
            let err = self.st.preview.as_ref().and_then(|s| s.error()).unwrap_or_default();
            if let Some(s) = self.st.preview.take() {
                let _ = s.finish();
            }
            self.st.error = Some(format!("Preview stopped: {err}"));
        }
        if self.st.refresh_devices {
            self.st.refresh_devices = false;
            load_devices(&mut self.st);
        }
        if DEVICES_CHANGED.swap(false, Ordering::SeqCst) {
            self.devices_changed();
        }
        // Meters.
        let dt = self.last_meter.elapsed().as_secs_f32();
        self.last_meter = Instant::now();
        let sess = self.st.recording.as_ref().or(self.st.preview.as_ref());
        let lv = sess.map(|s| s.poll().1).unwrap_or([0.0; 2]);
        for (m, l) in self.st.meters.iter_mut().zip(lv) {
            m.feed(l, dt);
        }
        self.sync_preview();
        self.st.apply_pip();
        self.st.apply_volumes();
        let clock = self.st.clock();
        let view = menubar::View {
            recording: self.st.recording.is_some(),
            paused: self.st.is_paused(),
            finishing: self.st.finishing,
            clock: &clock,
            has_last: self.st.last_file.is_some(),
            replay: self.st.replay_stats.is_some(),
            live: self.st.streaming(),
        };
        if let Some(m) = &mut self.menubar {
            m.update(&view);
        }
    }

    fn redraw(&mut self, which: WindowId) {
        let is_main = self.main.as_ref().is_some_and(|w| w.window.id() == which);
        let Some(gpu) = self.gpu.as_ref() else { return };
        preview::set_low_power(self.st.settings.preview_low_power);
        if is_main
            && let Some(f) = self.scaler.take(&gpu.device)
            && let Some(w) = self.main.as_mut()
        {
            self.st.preview_aspect = f.size[0] as f32 / f.size[1] as f32;
            self.st.preview_px = f.size;
            let view = f.texture.create_view(&Default::default());
            // Drawn pixel-aligned at its own size: nearest is exact, and
            // avoids a blur when the area is mid-resize.
            let filter = wgpu::FilterMode::Nearest;
            match self.st.preview_tex {
                Some(id) => w.renderer.update_egui_texture_from_wgpu_texture(&gpu.device, &view, filter, id),
                None => self.st.preview_tex = Some(w.renderer.register_native_texture(&gpu.device, &view, filter)),
            }
            self.preview_textures.push_back(f.texture);
            while self.preview_textures.len() > 2 {
                self.preview_textures.pop_front();
            }
            self.st.frames_shown += 1;
        }
        let win = if is_main { self.main.as_mut() } else { self.settings_win.as_mut() };
        let Some(win) = win else { return };
        let input = win.egui.take_egui_input(&win.window);
        let st = &mut self.st;
        let out = win.ctx.run_ui(input, |ui| {
            if is_main {
                ui::main_window(ui, st);
            } else {
                ui::settings_window(ui, st);
            }
        });
        win.egui.handle_platform_output(&win.window, out.platform_output);
        let delay = out.viewport_output.get(&egui::ViewportId::ROOT).map_or(Duration::MAX, |v| v.repaint_delay);
        win.repaint_at = if delay < Duration::from_secs(60) { Some(Instant::now() + delay) } else { None };
        let ppp = out.pixels_per_point;
        let prims = win.ctx.tessellate(out.shapes, ppp);
        // Upload every frame, even when the surface frame is skipped, or a
        // later partial update targets a texture that was never allocated.
        for (id, deltas) in &out.textures_delta.set {
            for d in deltas {
                win.renderer.update_texture(&gpu.device, &gpu.queue, *id, d);
            }
        }
        let frame = match win.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            _ => {
                win.surface.configure(&gpu.device, &win.config);
                for id in &out.textures_delta.free {
                    win.renderer.free_texture(id);
                }
                return;
            }
        };
        let (x, y) = theme::TRAFFIC_LIGHTS;
        macos::inset_traffic_lights(&win.window, x, y);
        let view = frame.texture.create_view(&Default::default());
        let sd = egui_wgpu::ScreenDescriptor { size_in_pixels: [win.config.width, win.config.height], pixels_per_point: ppp };
        let mut enc = gpu.device.create_command_encoder(&Default::default());
        let extra = win.renderer.update_buffers(&gpu.device, &gpu.queue, &mut enc, &prims, &sd);
        {
            let bg = theme::palette(win.ctx.theme() == egui::Theme::Dark).window;
            let [r, g, b, _] = bg.to_normalized_gamma_f32();
            let mut pass = enc
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("egui"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color { r: r as f64, g: g as f64, b: b as f64, a: 1.0 }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                })
                .forget_lifetime();
            win.renderer.render(&mut pass, &prims, &sd);
        }
        gpu.queue.submit(extra.into_iter().chain([enc.finish()]));
        win.window.pre_present_notify();
        gpu.queue.present(frame);
        for id in &out.textures_delta.free {
            win.renderer.free_texture(id);
        }
        // Drag the window by the toolbar (the titlebar is transparent).
        if ui::take_drag_request() {
            let _ = win.window.drag_window();
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.menubar.is_some() {
            return;
        }
        let mtm = objc2::MainThreadMarker::new().expect("main thread");
        let mb = menubar::MenuBar::new(mtm);
        let ok = crate::hotkey::register(HOTKEY_RECORD, crate::hotkey::KEY_R, crate::hotkey::CMD | crate::hotkey::SHIFT, || {
            push(Cmd::ToggleRecording)
        });
        if let Err(code) = ok {
            eprintln!("rec: could not register ⇧⌘R (error {code}); use the menu or the window instead");
            mb.hotkey_unavailable();
            self.st.hotkey_ok = false;
        }
        self.menubar = Some(mb);
        self._main_menu = Some(menubar::install_main_menu(mtm));
        macos::set_dock_icon();
        if std::env::var_os("REC_GUI_HIDDEN").is_some() {
            self.main_visible = false;
        } else {
            self.show_main(el);
        }
        if self.start_recording {
            push(Cmd::ToggleRecording);
        }
        if self.selftest.is_none() {
            push(Cmd::StartUpdater);
        }
        eprintln!("rec: app running (⇧⌘R toggles recording)");
    }

    fn user_event(&mut self, el: &ActiveEventLoop, _: ()) {
        self.tick(el);
        if let Some(w) = &self.main
            && self.main_visible
            && !w.occluded
        {
            w.window.request_redraw();
        }
        if let Some(w) = &self.settings_win {
            w.window.request_redraw();
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let is_main = self.main.as_ref().is_some_and(|w| w.window.id() == id);
        let win = if is_main { self.main.as_mut() } else { self.settings_win.as_mut() };
        let Some(win) = win else { return };
        let resp = win.egui.on_window_event(&win.window, &event);
        match event {
            WindowEvent::CloseRequested => {
                if is_main {
                    self.hide_main();
                } else {
                    self.settings_win = None;
                }
            }
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                let s = win.window.inner_size();
                win.config.width = s.width.max(1);
                win.config.height = s.height.max(1);
                if let Some(g) = &self.gpu {
                    win.surface.configure(&g.device, &win.config);
                }
                win.window.request_redraw();
            }
            WindowEvent::Occluded(o) => {
                win.occluded = o;
                if !o {
                    win.window.request_redraw();
                }
                if is_main {
                    preview::set_wanted(self.main_visible && !o);
                }
            }
            WindowEvent::ThemeChanged(_) => {
                theme::set_accent(theme::system_accent());
                theme::apply(&win.ctx);
                win.window.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                self.redraw(id);
                if !self.st.cmds.is_empty() {
                    self.tick(el);
                }
            }
            _ => {
                if resp.repaint {
                    win.window.request_redraw();
                }
            }
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if self.last_tick.elapsed() >= Duration::from_millis(100) {
            self.last_tick = Instant::now();
            self.tick(el);
        }
        // Animate while capturing (preview, meters, timer); otherwise idle
        // with a slow tick for signals.
        let live = self.st.recording.is_some() || self.st.preview.is_some() || self.st.finishing || self.st.streaming();
        let now = Instant::now();
        let mut next = now + Duration::from_millis(if live { 100 } else { 250 });
        // The occlusion event only reports changes (not a window created
        // while the screen is locked): ask AppKit each pass.
        for (w, main) in [(self.main.as_mut(), true), (self.settings_win.as_mut(), false)] {
            if let Some(w) = w {
                let o = macos::is_occluded(&w.window);
                if o != w.occluded {
                    w.occluded = o;
                    if main {
                        preview::set_wanted(self.main_visible && !o);
                    }
                }
            }
        }
        for w in [self.main.as_mut(), self.settings_win.as_mut()].into_iter().flatten() {
            let visible = w.window.is_visible().unwrap_or(true) && !w.occluded;
            if live && visible {
                // Timer, meters: ~15 fps; preview frames wake us on arrival.
                w.window.request_redraw();
            }
            if let Some(t) = w.repaint_at.filter(|_| !w.occluded) {
                if t <= now {
                    w.repaint_at = None;
                    w.window.request_redraw();
                } else {
                    next = next.min(t);
                }
            }
        }
        el.set_control_flow(ControlFlow::WaitUntil(next));
    }
}

/// REC_GUI_SELFTEST=1: drive the real app paths on a timer (preview,
/// record via the hotkey's command, pause/resume, stop, settings window,
/// close) and print what happened.
#[derive(Default)]
struct SelfTest {
    t0: Option<Instant>,
    step: u32,
    /// Start time of the preview session before an overlay change.
    session_before: Option<Instant>,
}

impl SelfTest {
    fn step(&mut self, app: &mut App) {
        let t = self.t0.get_or_insert_with(Instant::now).elapsed().as_secs_f64();
        // REC_GUI_SELFTEST=terminate: quit the way the Dock does
        // (-[NSApp terminate:]) mid-recording; the file must be finalized.
        if std::env::var("REC_GUI_SELFTEST").as_deref() == Ok("terminate") {
            match self.step {
                0 if t > 2.0 => {
                    push(Cmd::ToggleRecording);
                    self.step = 1;
                }
                1 if t > 5.0 => {
                    println!("selftest: recording={} -> NSApp terminate:", app.st.recording.is_some());
                    let mtm = objc2::MainThreadMarker::new().unwrap();
                    objc2_app_kit::NSApplication::sharedApplication(mtm).terminate(None);
                    self.step = 2;
                }
                _ => {}
            }
            return;
        }
        // REC_GUI_SELFTEST=live: replay buffer and a stream to every enabled
        // destination keep running across window close and a recording;
        // then the replay is saved with the hotkey's command.
        if std::env::var("REC_GUI_SELFTEST").as_deref() == Ok("live") {
            let streaming = !app.st.settings.stream_urls().is_empty();
            let live = app.st.streams.iter().filter(|s| s.status.as_ref().is_some_and(|s| s.live)).count();
            match self.step {
                0 if t > 2.0 => {
                    println!("selftest: replay on={}", app.st.replay_stats.is_some());
                    if streaming {
                        push(Cmd::ToggleStream);
                    }
                    self.step = 1;
                }
                // The window stays open until t = 6 s, so the live state
                // can be looked at.
                1 if live == app.st.streams.len() || t > 6.0 => {
                    println!("selftest: live on {live} of {} destinations, window open, error={:?}", app.st.streams.len(), app.st.error);
                    self.step = 2;
                }
                2 if t > 6.0 => {
                    app.hide_main();
                    println!("selftest: window hidden, live session={}", app.st.preview.is_some());
                    push(Cmd::ToggleRecording);
                    self.step = 3;
                }
                3 if t > 9.0 => {
                    push(Cmd::ToggleRecording);
                    self.step = 4;
                }
                4 if t > 11.0 && !app.st.finishing => {
                    println!("selftest: after recording live session={} saved={:?}", app.st.preview.is_some(), app.st.last_file);
                    push(Cmd::SaveReplay);
                    self.step = 5;
                }
                5 if app.st.notice.is_some() || app.st.error.is_some() || t > 27.0 => {
                    let held = app.st.replay_stats.map_or(0.0, |r| r.0);
                    println!("selftest: replay saved={:?} held={held:.1}s error={:?}", app.st.last_file, app.st.error);
                    for s in &app.st.streams {
                        let Some(st) = &s.status else { continue };
                        println!(
                            "selftest: stream {} live={} state={} bitrate={:.1}Mb/s dropped={} reconnects={}",
                            crate::live::destination_name(&s.url),
                            st.live,
                            st.state,
                            st.bitrate / 1e6,
                            st.dropped_frames,
                            st.reconnects
                        );
                    }
                    push(Cmd::Quit);
                    self.step = 6;
                }
                _ => {}
            }
            return;
        }
        // REC_GUI_SELFTEST=levels (with REC_TEST_PATTERN, whose beeps are the
        // system audio): move the system fader while recording, from 0 dB to
        // -12 dB, muted, then 0 dB again, and save the replay buffer.
        // scripts/levelcheck.py checks that both files follow the fader.
        if std::env::var("REC_GUI_SELFTEST").as_deref() == Ok("levels") {
            let s = &mut app.st.settings;
            let mut set = |db: f32, muted: bool| {
                (s.system_gain_db, s.system_muted) = (db, muted);
                println!("selftest: system fader {db} dB muted={muted} t={t:.2}");
            };
            match self.step {
                0 if t > 2.0 => {
                    push(Cmd::ToggleRecording);
                    self.step = 1;
                }
                1 if t > 4.5 => {
                    set(-12.0, false);
                    self.step = 2;
                }
                2 if t > 6.5 => {
                    set(-12.0, true);
                    self.step = 3;
                }
                3 if t > 8.5 => {
                    set(0.0, false);
                    self.step = 4;
                }
                4 if t > 10.5 => {
                    push(Cmd::ToggleRecording);
                    self.step = 5;
                }
                5 if !app.st.finishing && app.st.recording.is_none() => {
                    println!("selftest: saved={:?} error={:?}", app.st.last_file, app.st.error);
                    push(Cmd::SaveReplay);
                    self.step = 6;
                }
                6 if app.st.notice.is_some() || app.st.error.is_some() || t > 25.0 => {
                    println!("selftest: replay saved={:?} error={:?}", app.st.last_file, app.st.error);
                    push(Cmd::Quit);
                    self.step = 7;
                }
                _ => {}
            }
            return;
        }
        match self.step {
            0 if t > 3.0 => {
                println!("selftest: idle preview session={} frame_shown={}", app.st.preview.is_some(), app.st.frames_shown > 0);
                app.st.frames_shown = 0;
                push(Cmd::ToggleRecording);
                self.step = 1;
            }
            1 if t > 4.0 => {
                println!("selftest: recording={} error={:?}", app.st.recording.is_some(), app.st.error);
                push(Cmd::TogglePause);
                self.step = 2;
            }
            2 if t > 5.5 => {
                println!("selftest: paused={}", app.st.is_paused());
                push(Cmd::TogglePause);
                self.step = 3;
            }
            3 if t > 8.0 => {
                println!("selftest: preview during recording frame_shown={}", app.st.frames_shown > 0);
                println!(
                    "selftest: status title={:?} recorded={:.2}s wall={:.2}s",
                    app.menubar.as_ref().and_then(|m| m.title()),
                    app.st.recording.as_ref().map_or(0.0, |s| s.recorded_secs()),
                    app.st.recording.as_ref().map_or(0.0, |s| s.started.elapsed().as_secs_f64())
                );
                push(Cmd::ToggleRecording);
                self.step = 4;
            }
            4 if !app.st.finishing && app.st.recording.is_none() => {
                println!("selftest: saved={:?} error={:?}", app.st.last_file, app.st.error);
                push(Cmd::ShowSettings);
                self.step = 5;
            }
            5 if t > 9.0 && app.st.preview.is_some() => {
                // Move and restyle the overlay: must apply live, not restart.
                self.session_before = app.st.preview.as_ref().map(|s| s.started);
                app.st.settings.pip_place = crate::compose::Place::At([0.3, 0.4]);
                app.st.settings.pip_shape = crate::compose::Shape::Squircle;
                self.step = 6;
            }
            6 if t > 10.0 => {
                let now = app.st.preview.as_ref().map(|s| s.started);
                println!(
                    "selftest: overlay change live={} applied={}",
                    now.is_some() && now == self.session_before,
                    app.st.pip_applied.as_ref() == Some(&app.st.settings.pip_spec())
                );
                println!(
                    "selftest: settings window={} preview_restarted={}",
                    app.settings_win.is_some(),
                    app.st.preview.is_some()
                );
                app.hide_main();
                println!("selftest: after close preview_session={}", app.st.preview.is_some());
                push(Cmd::Quit);
                self.step = 7;
            }
            _ => {}
        }
    }
}

pub fn run(start_recording: bool, settings: bool, window: Option<u32>) -> Result<()> {
    use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
    unsafe {
        libc::signal(libc::SIGINT, on_sigint as *const () as usize);
        libc::signal(libc::SIGTERM, on_sigterm as *const () as usize);
        libc::signal(libc::SIGHUP, on_sigterm as *const () as usize);
    }
    record::install_pause_signals();
    let el = EventLoop::<()>::with_user_event()
        // A regular app: Dock icon (support/AppIcon.icns) and a menu bar.
        .with_activation_policy(ActivationPolicy::Regular)
        .with_default_menu(false)
        .build()?;
    let _ = PROXY.set(Mutex::new(el.create_proxy()));
    dock::install();
    let mut app = App::new(start_recording);
    app.st.settings.window = window;
    if settings {
        push(Cmd::ShowSettings);
    }
    el.run_app(&mut app)?;
    let _ = std::fs::remove_file(record::pidfile_path());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_permissions_banner_points_at_its_button() {
        let e = anyhow::Error::from(record::MissingPermissions(vec!["Screen & System Audio Recording", "Microphone"]));
        // The CLI keeps telling people to run `rec permissions --request`.
        assert!(format!("{e:#}").contains("run `rec permissions --request`"));
        let (msg, missing) = preview_error(&e);
        assert!(missing);
        assert_eq!(
            msg,
            "Preview unavailable: Rec needs permission for Screen & System Audio Recording, Microphone. Click Request Permissions, then allow Rec when macOS asks or in System Settings > Privacy & Security."
        );
        let (msg, missing) = preview_error(&anyhow::anyhow!("no displays"));
        assert!(!missing);
        assert_eq!(msg, "Preview unavailable: no displays");
    }
}
