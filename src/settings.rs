//! App settings, persisted as JSON in ~/Library/Application Support/rec-rs.
//! The app turns them into the same `record` arguments the CLI takes, so
//! both share one code path (`crate::prepare`).

use std::path::PathBuf;

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::compose::{PipSpec, Place, Scene, Shape};
use crate::mixer::Volume;
use crate::writer::{AudioLayout, Codec};

pub const RESOLUTIONS: [(&str, &str); 5] =
    [("720p", "720p"), ("1080p", "1080p"), ("1440p", "1440p"), ("4K", "4k"), ("Native", "native")];
pub const FPS: [u32; 5] = [24, 25, 30, 50, 60];

/// A stream destination: a service id from `live::SERVICES` or an rtmp(s)://
/// or WHIP http(s):// URL. Its key lives in the Keychain, never here.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamDest {
    pub dest: String,
    /// Go Live streams to every enabled destination at once.
    pub enabled: bool,
}

impl StreamDest {
    pub fn is_service(&self) -> bool {
        crate::live::SERVICES.iter().any(|(id, ..)| *id == self.dest)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub output_dir: PathBuf,
    /// "mov" or "mp4".
    pub container: String,
    /// None = automatic (H.264 up to 1080p, HEVC above).
    pub codec: Option<Codec>,
    pub resolution: String,
    pub fps: u32,
    /// Mb/s; None = automatic.
    pub bitrate_mbps: Option<f64>,
    /// 1-based display number.
    pub display: usize,
    pub scene: Scene,
    /// Camera unique ID; None = system default.
    pub camera: Option<String>,
    pub mic_enabled: bool,
    /// Microphone unique ID; None = system default.
    pub mic: Option<String>,
    /// The mic's display name, to find it again if its unique ID changes.
    pub mic_name: Option<String>,
    pub system_audio: bool,
    pub audio_tracks: AudioLayout,
    /// Fader levels in dB (`mixer::MIN_DB` is silence) and mutes. They
    /// change live, also while recording.
    pub system_gain_db: f32,
    pub mic_gain_db: f32,
    pub system_muted: bool,
    pub mic_muted: bool,
    pub pip_size: f64,
    pub pip_place: Place,
    pub pip_shape: Shape,
    pub pip_roundness: f64,
    pub pip_border: f64,
    pub pip_border_color: [u8; 3],
    pub pip_shadow: bool,
    pub mirror: bool,
    pub cursor: bool,
    pub clicks: bool,
    /// Preview at half resolution and 15 fps.
    pub preview_low_power: bool,
    /// Keep the last `replay_secs` of the live encode in memory.
    pub replay_enabled: bool,
    pub replay_secs: u32,
    /// Every service, then any custom servers. Go Live starts the enabled
    /// ones, all fed by the one live encode.
    pub stream_dests: Vec<StreamDest>,
    /// Live encode (replay buffer and streams).
    pub live_resolution: String,
    pub stream_bitrate_kbps: u32,
    /// Use the platform's recommended bitrate (`live::preset`) instead of
    /// `stream_bitrate_kbps`.
    pub stream_bitrate_auto: bool,
    /// "auto" (HEVC for YouTube, else H.264), "h264" or "hevc".
    pub stream_codec: String,
    pub keyframe_secs: f64,
    pub stream_cbr: bool,
    pub stream_audio_kbps: u32,
    /// Window to record (ID from `rec-rs devices`); not persisted because
    /// window IDs change between launches.
    #[serde(skip)]
    pub window: Option<u32>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            output_dir: default_output_dir(),
            container: "mov".into(),
            codec: None,
            resolution: "1080p".into(),
            fps: 60,
            bitrate_mbps: None,
            display: 1,
            scene: Scene::Screen,
            camera: None,
            mic_enabled: true,
            mic: None,
            mic_name: None,
            system_audio: true,
            audio_tracks: AudioLayout::Both,
            system_gain_db: 0.0,
            mic_gain_db: 0.0,
            system_muted: false,
            mic_muted: false,
            pip_size: PipSpec::default().size,
            pip_place: PipSpec::default().place,
            pip_shape: PipSpec::default().shape,
            pip_roundness: PipSpec::default().roundness,
            pip_border: 0.0,
            pip_border_color: [255, 255, 255],
            pip_shadow: false,
            mirror: true,
            cursor: true,
            clicks: false,
            preview_low_power: false,
            replay_enabled: false,
            replay_secs: 60,
            stream_dests: crate::live::SERVICES.iter().map(|(id, ..)| StreamDest { dest: id.to_string(), enabled: *id == "youtube" }).collect(),
            live_resolution: "1080p".into(),
            stream_bitrate_kbps: 6000,
            stream_bitrate_auto: true,
            stream_codec: "auto".into(),
            keyframe_secs: 2.0,
            stream_cbr: true,
            stream_audio_kbps: 192,
            window: None,
        }
    }
}

/// `PREFIX-YYYY-MM-DD-HHMMSS.EXT` in local time.
pub fn timestamped(prefix: &str, ext: &str) -> String {
    let mut buf = [0u8; 64];
    let n = unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        libc::strftime(buf.as_mut_ptr() as *mut _, buf.len(), c"%Y-%m-%d-%H%M%S".as_ptr(), &tm)
    };
    format!("{prefix}-{}.{ext}", String::from_utf8_lossy(&buf[..n]))
}

pub fn default_output_dir() -> PathBuf {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Movies")).unwrap_or_default()
}

fn value<T: ValueEnum>(v: T) -> String {
    v.to_possible_value().map(|p| p.get_name().to_string()).unwrap_or_default()
}

impl Settings {
    /// $REC_SETTINGS, else ~/Library/Application Support/rec-rs/settings.json.
    pub fn path() -> PathBuf {
        if let Some(p) = std::env::var_os("REC_SETTINGS") {
            return PathBuf::from(p);
        }
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        home.join("Library/Application Support/rec-rs/settings.json")
    }

    pub fn load() -> Self {
        let mut s = std::fs::read_to_string(Self::path()).ok().and_then(|t| Self::from_json(&t)).unwrap_or_default();
        // 160 kb/s was the default live audio bitrate; 192 is now.
        if s.stream_audio_kbps == 160 {
            s.stream_audio_kbps = 192;
        }
        // REC_OUTPUT_DIR overrides the folder (self-tests keep files out of ~/Movies).
        if let Some(d) = std::env::var_os("REC_OUTPUT_DIR") {
            s.output_dir = PathBuf::from(d);
        }
        s
    }

    /// Parse a settings file. Files from before multi-destination streaming
    /// name one `stream_dest` ("youtube", "twitch" or "custom" with
    /// `stream_custom_url`): that one becomes the only enabled destination.
    /// Every service is listed, in `live::SERVICES` order, before any
    /// custom servers.
    fn from_json(text: &str) -> Option<Settings> {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        let mut s: Settings = serde_json::from_value(v.clone()).ok()?;
        if v.get("stream_dests").is_none()
            && let Some(old) = v.get("stream_dest").and_then(|d| d.as_str())
        {
            let custom = v.get("stream_custom_url").and_then(|u| u.as_str()).unwrap_or("").trim();
            for d in &mut s.stream_dests {
                d.enabled = d.dest == old;
            }
            if old == "custom" && !custom.is_empty() {
                s.stream_dests.push(StreamDest { dest: custom.to_string(), enabled: true });
            }
        }
        let mut dests: Vec<StreamDest> = crate::live::SERVICES
            .iter()
            .map(|(id, ..)| {
                let saved = s.stream_dests.iter().find(|d| d.dest == *id);
                StreamDest { dest: id.to_string(), enabled: saved.is_some_and(|d| d.enabled) }
            })
            .collect();
        dests.extend(s.stream_dests.iter().filter(|d| !d.is_service()).cloned());
        s.stream_dests = dests;
        Some(s)
    }

    pub fn save(&self) -> std::io::Result<()> {
        let p = Self::path();
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(p, serde_json::to_string_pretty(self).unwrap_or_default())
    }

    /// The camera overlay these settings describe.
    pub fn pip_spec(&self) -> PipSpec {
        PipSpec {
            size: self.pip_size,
            place: self.pip_place,
            shape: self.pip_shape,
            roundness: self.pip_roundness,
            border: self.pip_border,
            border_color: self.pip_border_color,
            shadow: self.pip_shadow,
            mirror: self.mirror,
        }
    }

    /// The [system, mic] fader levels.
    pub fn volumes(&self) -> [Volume; 2] {
        [
            Volume { db: self.system_gain_db, muted: self.system_muted },
            Volume { db: self.mic_gain_db, muted: self.mic_muted },
        ]
    }

    /// The settings that need a new capture session when they change. The
    /// overlay's look and position and the audio levels are applied live
    /// instead.
    pub fn capture_key(&self) -> Settings {
        let d = Settings::default();
        Settings {
            system_gain_db: d.system_gain_db,
            mic_gain_db: d.mic_gain_db,
            system_muted: d.system_muted,
            mic_muted: d.mic_muted,
            pip_size: d.pip_size,
            pip_place: d.pip_place,
            pip_shape: d.pip_shape,
            pip_roundness: d.pip_roundness,
            pip_border: d.pip_border,
            pip_border_color: d.pip_border_color,
            pip_shadow: d.pip_shadow,
            mirror: d.mirror,
            preview_low_power: d.preview_low_power,
            replay_enabled: d.replay_enabled,
            replay_secs: d.replay_secs,
            stream_dests: d.stream_dests.clone(),
            live_resolution: d.live_resolution.clone(),
            stream_bitrate_kbps: d.stream_bitrate_kbps,
            stream_bitrate_auto: d.stream_bitrate_auto,
            stream_codec: d.stream_codec.clone(),
            keyframe_secs: d.keyframe_secs,
            stream_cbr: d.stream_cbr,
            stream_audio_kbps: d.stream_audio_kbps,
            ..self.clone()
        }
    }

    /// Ingest URLs of the enabled destinations (services expanded), without
    /// blanks or repeats.
    pub fn stream_urls(&self) -> Vec<String> {
        let mut urls: Vec<String> = vec![];
        for d in self.stream_dests.iter().filter(|d| d.enabled) {
            let url = crate::live::destination_url(&d.dest);
            if !url.is_empty() && !urls.contains(&url) {
                urls.push(url);
            }
        }
        urls
    }

    /// The live encode these settings describe (sizes above the capture
    /// are clamped to it by the encoder): the limits, codec and recommended
    /// bitrate that suit every enabled destination, unless set by hand.
    pub fn live_config(&self) -> crate::live::LiveConfig {
        use crate::live::{platform, shared_preset};
        let height = match self.live_resolution.as_str() {
            "720p" => 720,
            "1440p" => 1440,
            "4k" => 2160,
            _ => 1080,
        };
        let platforms: Vec<_> = self.stream_urls().iter().map(|u| platform(u)).collect();
        let (kbps, height, hevc) = shared_preset(&platforms, height, self.fps.min(60), &self.stream_codec);
        let bitrate = if self.stream_bitrate_auto { kbps } else { self.stream_bitrate_kbps.clamp(500, 80_000) };
        crate::live::LiveConfig {
            width: height * 16 / 9,
            height,
            bitrate: bitrate as u64 * 1000,
            keyframe_secs: self.keyframe_secs.clamp(0.5, 4.0),
            cbr: self.stream_cbr,
            audio_bitrate: self.stream_audio_kbps.clamp(64, 320) * 1000,
            hevc,
        }
    }

    /// A fresh timestamped file in the output folder.
    pub fn new_output_file(&self) -> PathBuf {
        let ext = if self.container == "mp4" && self.codec != Some(Codec::Prores) { "mp4" } else { "mov" };
        self.output_dir.join(timestamped("rec", ext))
    }

    /// `rec-rs record` arguments for these settings (`output` None = preview).
    pub fn record_args(&self, output: Option<&std::path::Path>) -> Vec<String> {
        let mut a: Vec<String> = vec!["-q".into(), "-r".into(), self.resolution.clone()];
        a.extend(["--fps".into(), self.fps.to_string()]);
        a.extend(["--display".into(), self.display.to_string()]);
        if let Some(w) = self.window {
            a.extend(["--window".into(), w.to_string()]);
        }
        if let Some(c) = self.codec {
            a.extend(["--codec".into(), value(c)]);
        }
        if let Some(b) = self.bitrate_mbps {
            a.extend(["--bitrate".into(), format!("{b}")]);
        }
        if let Some(o) = output {
            a.extend(["-o".into(), o.to_string_lossy().into_owned()]);
        }
        a.extend(["--scene".into(), value(self.scene)]);
        if self.scene.uses_cam() {
            a.extend(["--webcam".into(), self.camera.clone().unwrap_or_else(|| "default".into())]);
        }
        if !self.mic_enabled {
            a.push("--no-mic".into());
        } else if let Some(m) = &self.mic {
            a.extend(["--mic".into(), m.clone()]);
            if let Some(n) = &self.mic_name {
                a.extend(["--mic-name".into(), n.clone()]);
            }
        }
        if !self.system_audio {
            a.push("--no-system-audio".into());
        }
        a.extend(["--audio-tracks".into(), value(self.audio_tracks)]);
        a.extend(["--system-gain".into(), format!("{}", self.system_gain_db)]);
        a.extend(["--mic-gain".into(), format!("{}", self.mic_gain_db)]);
        a.extend(["--pip-size".into(), format!("{}", self.pip_size)]);
        match self.pip_place {
            Place::Corner(c) => a.extend(["--pip-corner".into(), value(c)]),
            Place::At([x, y]) => a.extend(["--pip-pos".into(), format!("{},{}", x.clamp(0.0, 1.0), y.clamp(0.0, 1.0))]),
        }
        a.extend(["--pip-shape".into(), value(self.pip_shape)]);
        a.extend(["--pip-roundness".into(), format!("{}", self.pip_roundness)]);
        a.extend(["--pip-border".into(), format!("{}", self.pip_border)]);
        let [r, g, b] = self.pip_border_color;
        a.extend(["--pip-border-color".into(), format!("{r:02x}{g:02x}{b:02x}")]);
        if self.pip_shadow {
            a.push("--pip-shadow".into());
        }
        if !self.mirror {
            a.push("--no-mirror".into());
        }
        if !self.cursor {
            a.push("--no-cursor".into());
        }
        if self.clicks {
            a.push("--clicks".into());
        }
        a
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_roundtrip_and_defaults_for_missing_fields() {
        let mut s = Settings { codec: Some(Codec::Hevc), scene: Scene::Cam, bitrate_mbps: Some(40.0), ..Default::default() };
        s.window = Some(7);
        let t = serde_json::to_string(&s).unwrap();
        assert!(t.contains("\"hevc\"") && t.contains("\"cam\""));
        let back: Settings = serde_json::from_str(&t).unwrap();
        assert_eq!(back.window, None, "window IDs are not persisted");
        assert_eq!(Settings { window: None, ..s }, back);
        let partial: Settings = serde_json::from_str(r#"{"fps": 30}"#).unwrap();
        assert_eq!(partial.fps, 30);
        assert_eq!(partial.resolution, "1080p");
    }

    #[test]
    fn record_args_parse_with_the_cli() {
        let s = Settings {
            codec: Some(Codec::Prores),
            scene: Scene::ScreenCam,
            mic: Some("BuiltInMic".into()),
            system_gain_db: -6.0,
            window: Some(42),
            ..Default::default()
        };
        let args = s.record_args(Some(std::path::Path::new("/tmp/x.mov")));
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        crate::parse_record_args(&refs).expect("settings produce valid CLI arguments");
        assert!(args.windows(2).any(|w| w == ["--webcam", "default"]));
        assert!(args.windows(2).any(|w| w == ["--codec", "prores"]));
        assert!(args.windows(2).any(|w| w == ["--window", "42"]));
    }

    #[test]
    fn overlay_settings_round_trip_through_the_cli() {
        let s = Settings {
            scene: Scene::ScreenCam,
            pip_place: Place::At([0.3, 0.6]),
            pip_shape: Shape::Pill,
            pip_roundness: 0.7,
            pip_border: 6.0,
            pip_border_color: [10, 200, 30],
            pip_shadow: true,
            mirror: false,
            ..Default::default()
        };
        let args = s.record_args(None);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let opts = crate::parse_record_args(&refs).unwrap();
        assert_eq!(opts.pip_pos.as_deref(), Some("0.3,0.6"));
        assert_eq!(opts.pip_border_color, "0ac81e");
        assert!(opts.pip_shadow && opts.no_mirror);
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains(r#""pip_place":{"at":[0.3,0.6]}"#), "{json}");
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pip_spec(), s.pip_spec());
    }

    #[test]
    fn overlay_changes_do_not_restart_capture() {
        let a = Settings::default();
        let b = Settings { pip_place: Place::At([0.1, 0.1]), pip_shape: Shape::Circle, pip_shadow: true, ..a.clone() };
        assert_eq!(a.capture_key(), b.capture_key());
        let c = Settings { fps: 30, ..a.clone() };
        assert_ne!(a.capture_key(), c.capture_key());
    }

    fn enabled(s: &Settings) -> Vec<&str> {
        s.stream_dests.iter().filter(|d| d.enabled).map(|d| d.dest.as_str()).collect()
    }

    #[test]
    fn single_stream_dest_of_older_files_migrates() {
        let s = Settings::from_json(r#"{"stream_dest": "twitch", "stream_custom_url": "rtmp://old/live"}"#).unwrap();
        assert_eq!(enabled(&s), ["twitch"]);
        assert_eq!(s.stream_dests.len(), crate::live::SERVICES.len(), "an unused custom URL is not kept");
        let s = Settings::from_json(r#"{"stream_dest": "custom", "stream_custom_url": " rtmp://host/live "}"#).unwrap();
        assert_eq!(enabled(&s), ["rtmp://host/live"]);
        assert_eq!(s.stream_urls(), ["rtmp://host/live"]);
        // No stream settings at all: the defaults (YouTube on).
        assert_eq!(enabled(&Settings::from_json("{}").unwrap()), ["youtube"]);
        // Saved again, the old fields are gone and the list stays.
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("stream_dest\"") && !json.contains("stream_custom_url"), "{json}");
        assert_eq!(Settings::from_json(&json).unwrap(), s);
    }

    #[test]
    fn every_service_is_listed_before_custom_servers() {
        let s = Settings::from_json(r#"{"stream_dests": [{"dest": "rtmp://a/live", "enabled": true}, {"dest": "kick", "enabled": true}]}"#).unwrap();
        let dests: Vec<&str> = s.stream_dests.iter().map(|d| d.dest.as_str()).collect();
        assert_eq!(dests, ["youtube", "twitch", "kick", "rtmp://a/live"]);
        assert_eq!(enabled(&s), ["kick", "rtmp://a/live"]);
    }

    #[test]
    fn go_live_targets_every_enabled_destination_once() {
        let d = |dest: &str, enabled| StreamDest { dest: dest.into(), enabled };
        let s = Settings {
            stream_dests: vec![
                d("youtube", true),
                d("twitch", true),
                d("kick", false),
                d("  ", true),
                d("rtmp://a/live", true),
                d("rtmp://a/live", true),
                d("https://b/whip", false),
            ],
            ..Default::default()
        };
        assert_eq!(s.stream_urls(), ["rtmps://a.rtmps.youtube.com:443/live2", "rtmps://live.twitch.tv:443/app", "rtmp://a/live"]);
    }

    #[test]
    fn live_encode_fits_every_enabled_destination() {
        let only = |ids: &[&str]| Settings {
            fps: 60,
            live_resolution: "4k".into(),
            stream_dests: ids.iter().map(|id| StreamDest { dest: id.to_string(), enabled: true }).collect(),
            ..Default::default()
        };
        let c = only(&["youtube"]).live_config();
        assert_eq!((c.height, c.bitrate, c.hevc), (2160, 35_000_000, true));
        let c = only(&["youtube", "twitch", "kick"]).live_config();
        assert_eq!((c.width, c.height, c.bitrate, c.hevc), (1920, 1080, 6_000_000, false));
        let c = only(&["youtube", "kick"]).live_config();
        assert_eq!((c.height, c.bitrate, c.hevc), (1080, 8_000_000, false));
    }

    #[test]
    fn audio_levels_do_not_restart_capture() {
        let a = Settings::default();
        let b = Settings { system_gain_db: -12.0, mic_gain_db: 6.0, mic_muted: true, ..a.clone() };
        assert_eq!(a.capture_key(), b.capture_key());
        assert_eq!(b.volumes(), [Volume { db: -12.0, muted: false }, Volume { db: 6.0, muted: true }]);
        // Settings saved before mute existed load unmuted.
        let old: Settings = serde_json::from_str(r#"{"mic_gain_db": 3.5}"#).unwrap();
        assert_eq!(old.volumes()[1], Volume { db: 3.5, muted: false });
    }

    #[test]
    fn prores_forces_mov() {
        let s = Settings { container: "mp4".into(), codec: Some(Codec::Prores), ..Default::default() };
        assert!(s.new_output_file().extension().unwrap() == "mov");
        let s = Settings { container: "mp4".into(), ..Default::default() };
        assert!(s.new_output_file().extension().unwrap() == "mp4");
    }
}
