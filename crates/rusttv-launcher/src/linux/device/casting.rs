//! Settings → Screen Mirroring: AirPlay (`uxplay`), Samsung Smart View /
//! Miracast (`miraclecast` / `wfd-sink` / `lazycast`), and Smart View Media
//! Cast / DLNA (built-in `mpv`-backed UPnP MediaRenderer with HEIC/HEIF photo
//! and video support, falling back to `gmediarender` if `mpv` is absent).
//!
//! Receivers run as background tasks/processes supervised by RustTV and persist
//! their enabled state and advertised device name in
//! `$XDG_CONFIG_HOME/rusttv/casting.toml` so they come back on boot.
//! See `CASTING.md` for OS package setup on Linux / HY320F.

mod dlna;

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use super::{control, program_exists};
use crate::device::{Control, ControlKind, Tab, Value};

/// The Settings tab's id.
pub(super) const TAB: &str = "casting";

/// Maximum length of the advertised AirPlay / Smart View device name.
const MAX_NAME_LEN: usize = 40;

/// Supported Miracast (Wi-Fi Display / Smart View) sink backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MiracastBackend {
    /// `miracle-sinkctl` (from MiracleCast, paired with `miracle-wifid`).
    Miraclecast,
    /// `wfd-sink` (standalone Wi-Fi Display sink).
    WfdSink,
    /// `lazycast` (ARM Linux Miracast receiver).
    Lazycast,
}

impl MiracastBackend {
    fn detect() -> Option<Self> {
        if program_exists("miracle-sinkctl") {
            Some(Self::Miraclecast)
        } else if program_exists("wfd-sink") {
            Some(Self::WfdSink)
        } else if program_exists("lazycast") {
            Some(Self::Lazycast)
        } else {
            None
        }
    }

    fn program(self) -> &'static str {
        match self {
            Self::Miraclecast => "miracle-sinkctl",
            Self::WfdSink => "wfd-sink",
            Self::Lazycast => "lazycast",
        }
    }
}

/// Supported DLNA / UPnP MediaRenderer backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DlnaBackend {
    /// Built-in UPnP MediaRenderer backed by `mpv` (supports HEIC/HEIF, AVIF,
    /// JPEG, PNG, and all video formats cleanly).
    BuiltinMpv,
    /// External `gmediarender` fallback when `mpv` is not installed.
    Gmediarender,
}

impl DlnaBackend {
    fn detect() -> Option<Self> {
        if program_exists("mpv") {
            Some(Self::BuiltinMpv)
        } else if program_exists("gmediarender") {
            Some(Self::Gmediarender)
        } else {
            None
        }
    }
}

/// Persisted screen-mirroring configuration (`casting.toml`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct CastingConfig {
    /// Name shown in AirPlay, Smart View / Miracast, and DLNA pickers.
    device_name: String,
    /// Whether AirPlay (`uxplay`) should run in the background.
    airplay: bool,
    /// Whether AirPlay opens fullscreen (`-fs`).
    airplay_fullscreen: bool,
    /// Whether Smart View / Miracast should run in the background.
    miracast: bool,
    /// Whether Smart View / DLNA media casting should run.
    dlna: bool,
}

impl Default for CastingConfig {
    fn default() -> Self {
        Self {
            device_name: default_device_name(),
            airplay: false,
            airplay_fullscreen: true,
            miracast: false,
            dlna: false,
        }
    }
}

/// Managed receiver child processes and servers, stopped cleanly on toggle-off
/// or Drop.
#[derive(Debug, Default)]
struct Processes {
    airplay: Option<Child>,
    miracast: Option<Child>,
    dlna_builtin: Option<dlna::Server>,
    dlna_ext: Option<Child>,
}

impl Processes {
    fn is_alive(slot: &mut Option<Child>) -> bool {
        let Some(child) = slot.as_mut() else {
            return false;
        };
        match child.try_wait() {
            Ok(None) => true,
            Ok(Some(_)) | Err(_) => {
                *slot = None;
                false
            }
        }
    }

    fn dlna_is_alive(&mut self) -> bool {
        if let Some(server) = self.dlna_builtin.as_ref() {
            if server.is_running() {
                return true;
            }
            self.dlna_builtin = None;
        }
        Self::is_alive(&mut self.dlna_ext)
    }

    fn stop(slot: &mut Option<Child>) {
        if let Some(mut child) = slot.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn stop_dlna(&mut self) {
        self.dlna_builtin = None;
        Self::stop(&mut self.dlna_ext);
    }

    fn stop_all(&mut self) {
        Self::stop(&mut self.airplay);
        Self::stop(&mut self.miracast);
        self.stop_dlna();
    }
}

impl Drop for Processes {
    fn drop(&mut self) {
        self.stop_all();
    }
}

/// Screen-mirroring receiver manager for [`super::LinuxDevice`].
#[derive(Debug)]
pub(super) struct Casting {
    config_path: Option<PathBuf>,
    has_uxplay: bool,
    miracast_backend: Option<MiracastBackend>,
    dlna_backend: Option<DlnaBackend>,
    config: Mutex<CastingConfig>,
    procs: Mutex<Processes>,
}

impl Default for Casting {
    fn default() -> Self {
        Self {
            config_path: None,
            has_uxplay: false,
            miracast_backend: None,
            dlna_backend: None,
            config: Mutex::new(CastingConfig::default()),
            procs: Mutex::new(Processes::default()),
        }
    }
}

impl Casting {
    /// Detects installed casting backends, loads saved preferences, and starts
    /// any receivers the user previously enabled.
    pub(super) fn detect() -> Self {
        let config_path = default_config_path();
        let config = config_path
            .as_deref()
            .and_then(load_config)
            .unwrap_or_default();
        let casting = Self {
            config_path,
            has_uxplay: program_exists("uxplay"),
            miracast_backend: MiracastBackend::detect(),
            dlna_backend: DlnaBackend::detect(),
            config: Mutex::new(config),
            procs: Mutex::new(Processes::default()),
        };
        casting.sync_enabled_receivers();
        casting
    }

    /// Starts any receivers marked enabled in the saved config.
    fn sync_enabled_receivers(&self) {
        let cfg = self
            .config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if cfg.airplay
            && self.has_uxplay
            && let Err(error) = self.start_airplay(&cfg)
        {
            log::warn!("auto-starting AirPlay receiver failed: {error}");
        }
        if cfg.miracast
            && self.miracast_backend.is_some()
            && let Err(error) = self.start_miracast(&cfg)
        {
            log::warn!("auto-starting Smart View / Miracast receiver failed: {error}");
        }
        if cfg.dlna
            && self.dlna_backend.is_some()
            && let Err(error) = self.start_dlna(&cfg)
        {
            log::warn!("auto-starting DLNA receiver failed: {error}");
        }
    }

    /// Builds Settings → Screen Mirroring.
    pub(super) fn tab(&self) -> Tab {
        let cfg = self
            .config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let mut procs = self.procs.lock().unwrap_or_else(PoisonError::into_inner);
        let airplay_running = Processes::is_alive(&mut procs.airplay);
        let miracast_running = Processes::is_alive(&mut procs.miracast);
        let dlna_running = procs.dlna_is_alive();
        drop(procs);

        let avahi_active = check_avahi_running();
        let p2p_ready = check_wifi_p2p_supported();

        let controls = build_controls(
            &cfg,
            BackendState {
                has_uxplay: self.has_uxplay,
                airplay_running,
                miracast_backend: self.miracast_backend,
                miracast_running,
                dlna_backend: self.dlna_backend,
                dlna_running,
                avahi_active,
                p2p_ready,
            },
        );
        Tab {
            id: TAB.to_owned(),
            title: "Screen Mirroring".to_owned(),
            controls,
        }
    }

    /// Handles a Settings change for the `casting` tab.
    pub(super) fn set(&self, control: &str, value: &Value) -> Result<(), String> {
        match (control, value) {
            ("device-name", Value::Text(text)) => {
                let name = clean_device_name(text);
                if name.is_empty() {
                    return Err("Device name cannot be empty".to_owned());
                }
                let cfg = {
                    let mut cfg = self.config.lock().unwrap_or_else(PoisonError::into_inner);
                    cfg.device_name = name;
                    cfg.clone()
                };
                self.save(&cfg);
                // Restart active receivers so phones see the new name at once.
                self.restart_active(&cfg)?;
                Ok(())
            }
            ("airplay", Value::Toggle(on)) => {
                if *on && !self.has_uxplay {
                    return Err("Install uxplay first (see CASTING.md)".to_owned());
                }
                let cfg = {
                    let mut cfg = self.config.lock().unwrap_or_else(PoisonError::into_inner);
                    cfg.airplay = *on;
                    cfg.clone()
                };
                if *on {
                    self.start_airplay(&cfg)?;
                } else {
                    let mut procs = self.procs.lock().unwrap_or_else(PoisonError::into_inner);
                    Processes::stop(&mut procs.airplay);
                }
                self.save(&cfg);
                Ok(())
            }
            ("airplay-fullscreen", Value::Toggle(on)) => {
                let cfg = {
                    let mut cfg = self.config.lock().unwrap_or_else(PoisonError::into_inner);
                    cfg.airplay_fullscreen = *on;
                    cfg.clone()
                };
                self.save(&cfg);
                if cfg.airplay && self.has_uxplay {
                    self.start_airplay(&cfg)?;
                }
                Ok(())
            }
            ("miracast", Value::Toggle(on)) => {
                if *on && self.miracast_backend.is_none() {
                    return Err("Install miraclecast or wfd-sink first (see CASTING.md)".to_owned());
                }
                let cfg = {
                    let mut cfg = self.config.lock().unwrap_or_else(PoisonError::into_inner);
                    cfg.miracast = *on;
                    cfg.clone()
                };
                if *on {
                    self.start_miracast(&cfg)?;
                } else {
                    let mut procs = self.procs.lock().unwrap_or_else(PoisonError::into_inner);
                    Processes::stop(&mut procs.miracast);
                }
                self.save(&cfg);
                Ok(())
            }
            ("dlna", Value::Toggle(on)) => {
                if *on && self.dlna_backend.is_none() {
                    return Err("Install mpv or gmediarender first (see CASTING.md)".to_owned());
                }
                let cfg = {
                    let mut cfg = self.config.lock().unwrap_or_else(PoisonError::into_inner);
                    cfg.dlna = *on;
                    cfg.clone()
                };
                if *on {
                    self.start_dlna(&cfg)?;
                } else {
                    let mut procs = self.procs.lock().unwrap_or_else(PoisonError::into_inner);
                    procs.stop_dlna();
                }
                self.save(&cfg);
                Ok(())
            }
            _ => Err(format!("unknown Screen Mirroring control: {control}")),
        }
    }

    fn restart_active(&self, cfg: &CastingConfig) -> Result<(), String> {
        let mut procs = self.procs.lock().unwrap_or_else(PoisonError::into_inner);
        let airplay_was_on = Processes::is_alive(&mut procs.airplay);
        let miracast_was_on = Processes::is_alive(&mut procs.miracast);
        let dlna_was_on = procs.dlna_is_alive();
        drop(procs);
        if airplay_was_on && self.has_uxplay {
            self.start_airplay(cfg)?;
        }
        if miracast_was_on && self.miracast_backend.is_some() {
            self.start_miracast(cfg)?;
        }
        if dlna_was_on && self.dlna_backend.is_some() {
            self.start_dlna(cfg)?;
        }
        Ok(())
    }

    fn start_airplay(&self, cfg: &CastingConfig) -> Result<(), String> {
        let args = uxplay_args(&cfg.device_name, cfg.airplay_fullscreen, is_wayland());
        let mut procs = self.procs.lock().unwrap_or_else(PoisonError::into_inner);
        Processes::stop(&mut procs.airplay);
        let mut cmd = Command::new("uxplay");
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        apply_gstreamer_env(&mut cmd);
        let child = cmd.spawn().map_err(|error| format!("uxplay: {error}"))?;
        procs.airplay = Some(child);
        log::info!("AirPlay receiver started as {:?}", cfg.device_name);
        Ok(())
    }

    fn start_miracast(&self, cfg: &CastingConfig) -> Result<(), String> {
        let backend = self
            .miracast_backend
            .ok_or_else(|| "no Miracast sink installed".to_owned())?;
        let args = miracast_args(backend, &cfg.device_name);
        let mut procs = self.procs.lock().unwrap_or_else(PoisonError::into_inner);
        Processes::stop(&mut procs.miracast);
        let child = Command::new(backend.program())
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("{}: {error}", backend.program()))?;
        procs.miracast = Some(child);
        log::info!(
            "Smart View / Miracast ({}) started as {:?}",
            backend.program(),
            cfg.device_name
        );
        Ok(())
    }

    fn start_dlna(&self, cfg: &CastingConfig) -> Result<(), String> {
        let backend = self
            .dlna_backend
            .ok_or_else(|| "no DLNA backend installed".to_owned())?;
        let mut procs = self.procs.lock().unwrap_or_else(PoisonError::into_inner);
        procs.stop_dlna();
        match backend {
            DlnaBackend::BuiltinMpv => {
                let server = dlna::Server::start(&cfg.device_name, is_wayland())?;
                procs.dlna_builtin = Some(server);
            }
            DlnaBackend::Gmediarender => {
                let args = dlna_args(&cfg.device_name, is_wayland());
                let mut cmd = Command::new("gmediarender");
                cmd.args(&args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                apply_gstreamer_env(&mut cmd);
                let child = cmd
                    .spawn()
                    .map_err(|error| format!("gmediarender: {error}"))?;
                procs.dlna_ext = Some(child);
                log::info!(
                    "DLNA / Smart View media cast (gmediarender) started as {:?}",
                    cfg.device_name
                );
            }
        }
        Ok(())
    }

    fn save(&self, cfg: &CastingConfig) {
        let Some(path) = &self.config_path else {
            return;
        };
        if let Err(error) = save_config(path, cfg) {
            log::warn!("saving {} failed: {error}", path.display());
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct BackendState {
    has_uxplay: bool,
    airplay_running: bool,
    miracast_backend: Option<MiracastBackend>,
    miracast_running: bool,
    dlna_backend: Option<DlnaBackend>,
    dlna_running: bool,
    avahi_active: bool,
    p2p_ready: Option<bool>,
}

fn build_controls(cfg: &CastingConfig, state: BackendState) -> Vec<Control> {
    let airplay_detail = if !state.has_uxplay {
        "Not installed · install uxplay for iPhone/iPad/Mac AirPlay".to_owned()
    } else if state.airplay_running {
        format!("Ready · visible as {} in AirPlay", cfg.device_name)
    } else {
        "Off · streams video, audio and screen mirroring from Apple devices".to_owned()
    };

    let miracast_detail = match state.miracast_backend {
        None => "Not installed · install miraclecast or wfd-sink for Samsung Smart View".to_owned(),
        Some(backend) if state.miracast_running => {
            format!(
                "Ready ({}) · visible as {} in Smart View / Cast",
                backend.program(),
                cfg.device_name
            )
        }
        Some(backend) => format!(
            "Off · Wi-Fi Display mirroring via {} (Samsung Smart View)",
            backend.program()
        ),
    };

    let dlna_detail = match state.dlna_backend {
        None => "Not installed · install mpv (recommended) or gmediarender for DLNA media cast"
            .to_owned(),
        Some(DlnaBackend::BuiltinMpv) if state.dlna_running => format!(
            "Ready (built-in mpv) · visible as {} for photos (HEIC/JPEG) and videos",
            cfg.device_name
        ),
        Some(DlnaBackend::Gmediarender) if state.dlna_running => {
            format!(
                "Ready (gmediarender) · visible as {} for media casting",
                cfg.device_name
            )
        }
        Some(_) => {
            "Off · cast photos, videos and music over normal Wi-Fi without Wi-Fi Direct".to_owned()
        }
    };

    let mut rows = vec![
        control(
            "device-name",
            "Device name",
            &cfg.device_name,
            ControlKind::Text { secret: false },
        ),
        control(
            "airplay",
            "AirPlay",
            &airplay_detail,
            ControlKind::Toggle {
                on: state.airplay_running,
            },
        ),
        control(
            "airplay-fullscreen",
            "AirPlay fullscreen",
            if cfg.airplay_fullscreen {
                "Open mirroring fullscreen over RustTV"
            } else {
                "Windowed"
            },
            ControlKind::Toggle {
                on: cfg.airplay_fullscreen,
            },
        ),
        control(
            "miracast",
            "Smart View (Miracast)",
            &miracast_detail,
            ControlKind::Toggle {
                on: state.miracast_running,
            },
        ),
        control(
            "dlna",
            "Smart View / DLNA media cast",
            &dlna_detail,
            ControlKind::Toggle {
                on: state.dlna_running,
            },
        ),
        control("status-heading", "Readiness", "", ControlKind::Heading),
        control(
            "avahi",
            "AirPlay discovery (Avahi mDNS)",
            if state.avahi_active {
                "Running"
            } else {
                "Not running · enable avahi-daemon for AirPlay discovery"
            },
            ControlKind::Info,
        ),
        control(
            "wifi-p2p",
            "Smart View Wi-Fi Direct (P2P)",
            match state.p2p_ready {
                Some(true) => "Supported by Wi-Fi adapter",
                Some(false) => "Not reported by Wi-Fi driver (use DLNA media cast over Wi-Fi)",
                None => "Unknown (iw not installed)",
            },
            ControlKind::Info,
        ),
    ];
    if !state.has_uxplay || state.miracast_backend.is_none() {
        rows.push(control(
            "setup-hint",
            "Setup guide",
            "See CASTING.md for Debian/Armbian package setup",
            ControlKind::Info,
        ));
    }
    rows
}

/// Legacy Allwinner BSP OpenMAX video decoders that crash with SIGSEGV inside
/// `libVE.so` on mainline Linux kernels lacking `/dev/cedar_dev` (which instead
/// expose hardware decoding via V4L2 stateless `v4l2codecs`).
const LEGACY_OMX_DECODERS: &str = "\
omxavsvideodec:NONE,omxh263videodec:NONE,omxh264dec:NONE,omxhevcvideodec:NONE,\
omxmjpegvideodec:NONE,omxmpeg1videodec:NONE,omxmpeg2videodec:NONE,\
omxmpeg4videodec:NONE,omxvp8videodec:NONE,omxvp9videodec:NONE";

fn is_wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
}

/// Guards GStreamer-backed receivers (`uxplay`, `gmediarender`) against legacy
/// Allwinner BSP OpenMAX plugins when running on a mainline kernel without
/// `/dev/cedar_dev`.
fn apply_gstreamer_env(cmd: &mut Command) {
    let cedar_missing = !Path::new("/dev/cedar_dev").exists();
    let has_allwinner_ve = Path::new("/usr/lib/aarch64-linux-gnu/libVE.so").exists()
        || Path::new("/vendor/etc/cedarc.conf").exists();
    let existing = std::env::var("GST_PLUGIN_FEATURE_RANK").ok();
    if let Some(ranks) =
        gstreamer_rank_override(cedar_missing && has_allwinner_ve, existing.as_deref())
    {
        cmd.env("GST_PLUGIN_FEATURE_RANK", ranks);
    }
}

fn gstreamer_rank_override(disable_legacy_omx: bool, existing: Option<&str>) -> Option<String> {
    if !disable_legacy_omx {
        return None;
    }
    match existing.map(str::trim).filter(|s| !s.is_empty()) {
        Some(prev) => Some(format!("{prev},{LEGACY_OMX_DECODERS}")),
        None => Some(LEGACY_OMX_DECODERS.to_owned()),
    }
}

fn uxplay_args(device_name: &str, fullscreen: bool, wayland: bool) -> Vec<String> {
    let mut args = vec![
        "-n".to_owned(),
        device_name.to_owned(),
        // Do not append "@hostname" to the advertised AirPlay name.
        "-nh".to_owned(),
        // Enable HLS streaming (e.g. mobile video apps) alongside mirroring.
        "-hls".to_owned(),
    ];
    if wayland {
        // Prefer direct Wayland output over glimagesink to avoid Panfrost GL
        // teardown stalls and keep V4L2 DMABUF decoding zero-copy.
        args.push("-vs".to_owned());
        args.push("waylandsink".to_owned());
    }
    if fullscreen {
        args.push("-fs".to_owned());
    }
    args
}

fn miracast_args(backend: MiracastBackend, device_name: &str) -> Vec<String> {
    match backend {
        MiracastBackend::Miraclecast => vec!["run".to_owned(), "0".to_owned()],
        MiracastBackend::WfdSink => vec!["--name".to_owned(), device_name.to_owned()],
        MiracastBackend::Lazycast => Vec::new(),
    }
}

fn dlna_args(device_name: &str, wayland: bool) -> Vec<String> {
    // gmediarender defaults to audio-only (fakesink for video) unless a video
    // sink/pipeline is explicitly specified. On Wayland, `waylandsink
    // fullscreen=true` pairs directly with V4L2 stateless hardware decoders and
    // avoids Panfrost glimagesink teardown hangs.
    let video = if wayland {
        "--gstout-videopipe=waylandsink fullscreen=true"
    } else {
        "--gstout-videosink=autovideosink"
    };
    vec![
        "-f".to_owned(),
        device_name.to_owned(),
        video.to_owned(),
        "--gstout-audiosink=autoaudiosink".to_owned(),
    ]
}

fn clean_device_name(text: &str) -> String {
    let cleaned: String = text
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME_LEN)
        .collect();
    // Never start with '-' so the name cannot be parsed as a CLI flag.
    cleaned.trim_start_matches('-').trim().to_owned()
}

fn default_device_name() -> String {
    let host = fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| clean_device_name(&s))
        .filter(|s| !s.is_empty());
    host.unwrap_or_else(|| "RustTV".to_owned())
}

fn default_config_path() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
    let base = var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| var("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("rusttv").join("casting.toml"))
}

fn load_config(path: &Path) -> Option<CastingConfig> {
    let text = fs::read_to_string(path).ok()?;
    let mut cfg: CastingConfig = toml::from_str(&text)
        .map_err(|error| log::warn!("ignoring {}: {error}", path.display()))
        .ok()?;
    cfg.device_name = clean_device_name(&cfg.device_name);
    if cfg.device_name.is_empty() {
        cfg.device_name = default_device_name();
    }
    Some(cfg)
}

fn save_config(path: &Path, cfg: &CastingConfig) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(cfg).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("toml.new");
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)
}

/// Checks whether Avahi's mDNS daemon socket is present (needed for AirPlay).
fn check_avahi_running() -> bool {
    Path::new("/run/avahi-daemon/socket").exists()
        || Path::new("/var/run/avahi-daemon/socket").exists()
}

/// Checks whether the Wi-Fi driver advertises P2P-client / P2P-GO (Miracast).
/// Checks `PATH` as well as `/usr/sbin/iw` and `/sbin/iw` (non-root TV sessions
/// often omit `/usr/sbin` from `PATH`).
fn check_wifi_p2p_supported() -> Option<bool> {
    let iw = if program_exists("iw") {
        "iw"
    } else if Path::new("/usr/sbin/iw").is_file() {
        "/usr/sbin/iw"
    } else if Path::new("/sbin/iw").is_file() {
        "/sbin/iw"
    } else {
        return None;
    };
    let output = Command::new(iw)
        .arg("list")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(parse_iw_p2p(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_iw_p2p(iw_list: &str) -> bool {
    iw_list.contains("P2P-client") && iw_list.contains("P2P-GO")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_device_names_and_blocks_leading_dashes() {
        assert_eq!(clean_device_name("  Living Room TV \n"), "Living Room TV");
        assert_eq!(clean_device_name("--bad-flag"), "bad-flag");
        assert_eq!(clean_device_name("---"), "");
    }

    #[test]
    fn builds_uxplay_and_dlna_arguments() {
        assert_eq!(
            uxplay_args("HY320F", true, true),
            ["-n", "HY320F", "-nh", "-hls", "-vs", "waylandsink", "-fs"]
        );
        assert_eq!(
            uxplay_args("HY320F", false, false),
            ["-n", "HY320F", "-nh", "-hls"]
        );
        assert_eq!(
            dlna_args("HY320F", true),
            [
                "-f",
                "HY320F",
                "--gstout-videopipe=waylandsink fullscreen=true",
                "--gstout-audiosink=autoaudiosink"
            ]
        );
        assert_eq!(
            dlna_args("HY320F", false),
            [
                "-f",
                "HY320F",
                "--gstout-videosink=autovideosink",
                "--gstout-audiosink=autoaudiosink"
            ]
        );
    }

    #[test]
    fn disables_legacy_allwinner_omx_only_when_needed() {
        assert_eq!(gstreamer_rank_override(false, None), None);
        assert_eq!(gstreamer_rank_override(false, Some("avdec_h264:MAX")), None);
        assert_eq!(
            gstreamer_rank_override(true, None).as_deref(),
            Some(LEGACY_OMX_DECODERS)
        );
        assert_eq!(
            gstreamer_rank_override(true, Some("v4l2slh264dec:MAX")),
            Some(format!("v4l2slh264dec:MAX,{LEGACY_OMX_DECODERS}"))
        );
    }

    #[test]
    fn parses_wifi_p2p_support_from_iw_list() {
        let supported = "\
            Supported interface modes:\n\
            \t * managed\n\
            \t * P2P-client\n\
            \t * P2P-GO\n";
        assert!(parse_iw_p2p(supported));
        assert!(!parse_iw_p2p("Supported interface modes:\n\t * managed\n"));
    }

    #[test]
    fn saves_and_reloads_casting_config() {
        let dir = std::env::temp_dir().join(format!("rusttv-casting-{}", std::process::id()));
        let path = dir.join("casting.toml");
        let cfg = CastingConfig {
            device_name: "Projector".to_owned(),
            airplay: true,
            airplay_fullscreen: true,
            miracast: false,
            dlna: true,
        };
        save_config(&path, &cfg).expect("save");
        assert_eq!(load_config(&path), Some(cfg));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn controls_reflect_installed_and_running_receivers() {
        let cfg = CastingConfig {
            device_name: "HY320F".to_owned(),
            airplay: true,
            airplay_fullscreen: true,
            miracast: false,
            dlna: false,
        };
        let rows = build_controls(
            &cfg,
            BackendState {
                has_uxplay: true,
                airplay_running: true,
                miracast_backend: Some(MiracastBackend::WfdSink),
                miracast_running: false,
                dlna_backend: Some(DlnaBackend::BuiltinMpv),
                dlna_running: true,
                avahi_active: true,
                p2p_ready: Some(true),
            },
        );
        let airplay = rows
            .iter()
            .find(|r| r.id == "airplay")
            .expect("airplay row");
        assert_eq!(airplay.kind, ControlKind::Toggle { on: true });
        assert!(airplay.detail.contains("HY320F"));
        let miracast = rows
            .iter()
            .find(|r| r.id == "miracast")
            .expect("miracast row");
        assert_eq!(miracast.kind, ControlKind::Toggle { on: false });
        assert!(miracast.detail.contains("wfd-sink"));
        let dlna = rows.iter().find(|r| r.id == "dlna").expect("dlna row");
        assert_eq!(dlna.kind, ControlKind::Toggle { on: true });
        assert!(dlna.detail.contains("HEIC"));
    }
}
