//! Settings → Power & Input:
//! - Automatic HDMI startup (`Home screen`, `<HDMI> (always)`, `<HDMI> (when
//!   signal)`, or `Last selected input`) and reliable HDMI signal auto-switch.
//! - Automatic shutdown timers: a one-shot **Sleep timer** and a persisted
//!   **Auto power-off when idle** timer (respecting user activity and optional
//!   active HDMI signal keep-awake).
//!
//! Preferences persist in `$XDG_CONFIG_HOME/rusttv/power-input.toml` (usually
//! `~/.config/rusttv/power-input.toml`).

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::control;
use super::device_file::Input;
use crate::device::{Control, ControlKind, Tab, Value};

/// Settings tab id for Power & Input.
pub(super) const TAB: &str = "power-input";

/// Settings tab id for Display (UI render resolution).
pub(super) const DISPLAY_TAB: &str = "display";

/// Prefix for selectable render-resolution dropdown entries (`res:<height>`).
const RES_PREFIX: &str = "res:";

/// Standard 16:9 UI render resolutions (`height == 0` uses the panel's native
/// resolution). Video playback, HDMI, and launched apps stay native 1080p.
const RENDER_RESOLUTIONS: &[(u32, &str, &str)] = &[
    (0, "1920 × 1080 (1080p)", "Native panel resolution"),
    (900, "1600 × 900 (900p)", "HD+ · hardware-scaled"),
    (768, "1366 × 768 (768p)", "WXGA · hardware-scaled"),
    (720, "1280 × 720 (720p)", "HD · recommended for 60 fps"),
    (576, "1024 × 576 (576p)", "WSVGA · hardware-scaled"),
    (540, "960 × 540 (540p)", "qHD · fast hardware-scaled"),
    (480, "854 × 480 (480p)", "FWVGA · ultra fast"),
];

/// Whether the Render resolution dropdown in Settings → Display is expanded.
static RENDER_DROPDOWN_OPEN: AtomicBool = AtomicBool::new(false);

/// One-shot sleep timer choices (minutes, `0` = Off).
pub(super) const SLEEP_MINUTES: &[u32] = &[0, 15, 30, 45, 60, 90, 120];

/// Persisted idle auto-shutdown choices (minutes, `0` = Off).
pub(super) const IDLE_MINUTES: &[u32] = &[0, 15, 30, 60, 120, 180];

/// What input the projector opens automatically on startup.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "mode", content = "id", rename_all = "kebab-case")]
pub(super) enum StartupInput {
    /// Stay on the RustTV Home screen.
    #[default]
    Home,
    /// Always open this input (e.g. `"hdmi"`) on boot, even before signal lock.
    Always(String),
    /// Open this input on boot only if a live signal is already detected.
    OnSignal(String),
    /// Re-open the last selected HDMI input (falls back to the first HDMI input).
    LastInput,
}

/// Persisted HDMI startup and idle auto-shutdown settings (`power-input.toml`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct PowerInputConfig {
    startup_input: StartupInput,
    /// Last HDMI input id selected by the user (for `StartupInput::LastInput`).
    last_input: Option<String>,
    /// Automatically switch to HDMI whenever a signal newly arrives.
    auto_switch_hdmi: bool,
    /// Shut down after this many idle minutes (`0` = disabled).
    idle_shutdown_minutes: u32,
    /// Treat an active HDMI video signal as activity so movies don't trigger
    /// idle shutdown.
    keep_awake_on_hdmi: bool,
}

impl Default for PowerInputConfig {
    fn default() -> Self {
        Self {
            startup_input: StartupInput::Home,
            last_input: None,
            auto_switch_hdmi: true,
            idle_shutdown_minutes: 0,
            keep_awake_on_hdmi: true,
        }
    }
}

/// Live runtime state shared with the shutdown timer thread and HDMI watcher.
#[derive(Debug)]
struct Shared {
    epoch: Instant,
    /// Seconds since `epoch` of the last user interaction.
    last_activity_secs: AtomicU64,
    /// Whether any HDMI input currently has an active picture signal.
    hdmi_signal: AtomicBool,
    /// Shutdown watchdog stop flag.
    stop: AtomicBool,
    /// Mutable state protected by a mutex + condvar so the timer thread sleeps
    /// with zero polling overhead.
    timer: Mutex<TimerState>,
    wake: Condvar,
}

#[derive(Debug, Default)]
struct TimerState {
    /// Active one-shot sleep timer deadline (monotonically timed).
    sleep_deadline: Option<Instant>,
    /// Selected sleep duration in minutes (`0` = Off) for UI highlighting.
    sleep_minutes: u32,
    /// Persisted idle shutdown minutes (`0` = Off).
    idle_minutes: u32,
    /// Whether an active HDMI signal pauses the idle countdown.
    keep_awake_on_hdmi: bool,
    /// Most recent raw HDMI status text (e.g. `"signal 1920x1080@60"`).
    hdmi_status: String,
    /// Prevents issuing `systemctl poweroff` more than once.
    powering_off: bool,
}

/// Manages HDMI startup/auto-switch preferences and sleep/idle shutdown timers.
#[derive(Debug)]
pub(super) struct PowerInput {
    config_path: Option<PathBuf>,
    inputs: Vec<Input>,
    systemctl: bool,
    config: Mutex<PowerInputConfig>,
    shared: Arc<Shared>,
    watchdog: Mutex<Option<JoinHandle<()>>>,
}

impl Default for PowerInput {
    fn default() -> Self {
        Self::new(None, Vec::new(), false, false)
    }
}

impl PowerInput {
    /// Loads saved preferences and starts the shutdown watchdog thread (when
    /// `systemctl` is available).
    pub(super) fn detect(inputs: Vec<Input>, systemctl: bool) -> Self {
        let config_path = default_config_path();
        Self::new(config_path, inputs, systemctl, systemctl)
    }

    fn new(
        config_path: Option<PathBuf>,
        inputs: Vec<Input>,
        systemctl: bool,
        start_watchdog: bool,
    ) -> Self {
        let config = config_path
            .as_deref()
            .and_then(load_config)
            .unwrap_or_default();
        let shared = Arc::new(Shared {
            epoch: Instant::now(),
            last_activity_secs: AtomicU64::new(0),
            hdmi_signal: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            timer: Mutex::new(TimerState {
                sleep_deadline: None,
                sleep_minutes: 0,
                idle_minutes: config.idle_shutdown_minutes,
                keep_awake_on_hdmi: config.keep_awake_on_hdmi,
                hdmi_status: String::new(),
                powering_off: false,
            }),
            wake: Condvar::new(),
        });
        let watchdog = if start_watchdog {
            let shared_bg = Arc::clone(&shared);
            thread::Builder::new()
                .name("rusttv-shutdown-timer".to_owned())
                .spawn(move || run_watchdog(&shared_bg))
                .ok()
        } else {
            None
        };
        Self {
            config_path,
            inputs,
            systemctl,
            config: Mutex::new(config),
            shared,
            watchdog: Mutex::new(watchdog),
        }
    }

    /// Records user activity (key press, mouse, gamepad, phone remote) so the
    /// idle auto-shutdown countdown resets.
    pub(super) fn note_activity(&self) {
        let now = self.shared.epoch.elapsed().as_secs();
        self.shared.last_activity_secs.store(now, Ordering::Relaxed);
    }

    /// Updates the live HDMI signal status from the input watcher thread.
    pub(super) fn set_hdmi_status(&self, has_signal: bool, raw_status: &str) {
        self.shared.hdmi_signal.store(has_signal, Ordering::Relaxed);
        if has_signal {
            self.note_activity();
        }
        let mut timer = self
            .shared
            .timer
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        raw_status.clone_into(&mut timer.hdmi_status);
        self.shared.wake.notify_all();
    }

    /// Remembers the last HDMI input selected by the user for
    /// [`StartupInput::LastInput`].
    pub(super) fn record_input_selected(&self, input_id: &str) {
        self.note_activity();
        let cfg = {
            let mut cfg = self.config.lock().unwrap_or_else(PoisonError::into_inner);
            if cfg.last_input.as_deref() == Some(input_id) {
                return;
            }
            cfg.last_input = Some(input_id.to_owned());
            cfg.clone()
        };
        self.save(&cfg);
    }

    /// Whether HDMI auto-switch on newly detected signal is currently enabled.
    pub(super) fn auto_switch_hdmi(&self) -> bool {
        self.config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .auto_switch_hdmi
    }

    /// Startup behavior for HDMI: returns `Some((input_id, require_signal))`
    /// if RustTV should automatically open an HDMI input when starting.
    pub(super) fn startup_input_target(&self) -> Option<(String, bool)> {
        let cfg = self.config.lock().unwrap_or_else(PoisonError::into_inner);
        resolve_startup_target(&cfg, &self.inputs)
    }

    /// Returns the desktop entry ID of the active/preferred HDMI input (or the
    /// requested input ID if given).
    pub(super) fn preferred_input(&self) -> Option<&Input> {
        let cfg = self.config.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(last) = cfg.last_input.as_deref()
            && let Some(found) = self.inputs.iter().find(|i| i.id == last)
        {
            return Some(found);
        }
        self.inputs.first()
    }

    /// Short status summary of any active sleep timer (for the Power menu).
    pub(super) fn sleep_timer_detail(&self) -> String {
        let timer = self
            .shared
            .timer
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        format_sleep_detail(timer.sleep_deadline, Instant::now())
    }

    /// Index of the current sleep timer choice in [`SLEEP_MINUTES`].
    pub(super) fn sleep_timer_choice(&self) -> usize {
        let mins = self
            .shared
            .timer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .sleep_minutes;
        SLEEP_MINUTES.iter().position(|&m| m == mins).unwrap_or(0)
    }

    /// Builds the **Settings → Power & Input** tab (shown whenever the device
    /// has either `systemctl` or at least one HDMI input).
    pub(super) fn tab(&self) -> Option<Tab> {
        if !self.systemctl && self.inputs.is_empty() {
            return None;
        }
        let cfg = self
            .config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let now = Instant::now();
        let (sleep_minutes, sleep_deadline, hdmi_status) = {
            let timer = self
                .shared
                .timer
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            (
                timer.sleep_minutes,
                timer.sleep_deadline,
                timer.hdmi_status.clone(),
            )
        };
        let controls = build_controls(
            &cfg,
            &self.inputs,
            self.systemctl,
            sleep_minutes,
            sleep_deadline,
            &hdmi_status,
            now,
        );
        Some(Tab {
            id: TAB.to_owned(),
            title: "Power & Input".to_owned(),
            controls,
        })
    }

    /// Handles Settings changes for `power-input` (and sleep-timer choices from
    /// the Power quick panel).
    pub(super) fn set(&self, control_id: &str, value: &Value) -> Result<(), String> {
        self.note_activity();
        match (control_id, value) {
            ("startup-input", Value::Choice(index)) => {
                let choices = startup_choices(&self.inputs);
                let (_, target) = choices
                    .get(*index)
                    .ok_or_else(|| "unknown startup input choice".to_owned())?;
                let cfg = {
                    let mut cfg = self.config.lock().unwrap_or_else(PoisonError::into_inner);
                    cfg.startup_input = target.clone();
                    cfg.clone()
                };
                self.save(&cfg);
                Ok(())
            }
            ("auto-switch-hdmi", Value::Toggle(on)) => {
                let cfg = {
                    let mut cfg = self.config.lock().unwrap_or_else(PoisonError::into_inner);
                    cfg.auto_switch_hdmi = *on;
                    cfg.clone()
                };
                self.save(&cfg);
                Ok(())
            }
            ("sleep-timer", Value::Choice(index)) => {
                let &minutes = SLEEP_MINUTES
                    .get(*index)
                    .ok_or_else(|| "unknown sleep timer duration".to_owned())?;
                self.set_sleep_minutes(minutes);
                Ok(())
            }
            ("idle-shutdown", Value::Choice(index)) => {
                let &minutes = IDLE_MINUTES
                    .get(*index)
                    .ok_or_else(|| "unknown idle shutdown duration".to_owned())?;
                let cfg = {
                    let mut cfg = self.config.lock().unwrap_or_else(PoisonError::into_inner);
                    cfg.idle_shutdown_minutes = minutes;
                    cfg.clone()
                };
                {
                    let mut timer = self
                        .shared
                        .timer
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    timer.idle_minutes = minutes;
                    self.shared.wake.notify_all();
                }
                self.save(&cfg);
                Ok(())
            }
            ("keep-awake-hdmi", Value::Toggle(on)) => {
                let cfg = {
                    let mut cfg = self.config.lock().unwrap_or_else(PoisonError::into_inner);
                    cfg.keep_awake_on_hdmi = *on;
                    cfg.clone()
                };
                {
                    let mut timer = self
                        .shared
                        .timer
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    timer.keep_awake_on_hdmi = *on;
                    self.shared.wake.notify_all();
                }
                self.save(&cfg);
                Ok(())
            }
            _ => Err(format!("unknown Power & Input control: {control_id}")),
        }
    }

    /// Sets (or cancels with `0`) the one-shot sleep countdown.
    pub(super) fn set_sleep_minutes(&self, minutes: u32) {
        let mut timer = self
            .shared
            .timer
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        timer.sleep_minutes = minutes;
        timer.sleep_deadline = if minutes == 0 {
            None
        } else {
            Some(Instant::now() + Duration::from_secs(u64::from(minutes) * 60))
        };
        if minutes == 0 {
            log::info!("sleep timer cancelled");
        } else {
            log::info!("sleep timer set for {minutes} min");
        }
        self.shared.wake.notify_all();
    }

    fn save(&self, cfg: &PowerInputConfig) {
        let Some(path) = &self.config_path else {
            return;
        };
        if let Err(error) = save_config(path, cfg) {
            log::warn!("saving {} failed: {error}", path.display());
        }
    }
}

impl Drop for PowerInput {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.wake.notify_all();
        if let Some(handle) = self
            .watchdog
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            let _ = handle.join();
        }
    }
}

fn run_watchdog(shared: &Shared) {
    let mut timer = shared.timer.lock().unwrap_or_else(PoisonError::into_inner);
    while !shared.stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        let now_secs = shared.epoch.elapsed().as_secs();
        let last_act = shared.last_activity_secs.load(Ordering::Relaxed);
        let hdmi_active = shared.hdmi_signal.load(Ordering::Relaxed);

        let decision = next_shutdown_wait(
            timer.sleep_deadline,
            timer.idle_minutes,
            timer.keep_awake_on_hdmi && hdmi_active,
            now,
            now_secs.saturating_sub(last_act),
        );

        match decision {
            WatchdogStep::PowerOff(reason) => {
                if !timer.powering_off {
                    timer.powering_off = true;
                    timer.sleep_deadline = None;
                    timer.sleep_minutes = 0;
                    drop(timer);
                    log::info!(
                        "automatic shutdown triggered ({reason}); running systemctl poweroff"
                    );
                    let _ = Command::new("systemctl")
                        .arg("poweroff")
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                    return;
                }
                return;
            }
            WatchdogStep::Wait(wait) => {
                let (guard, _) = shared
                    .wake
                    .wait_timeout(timer, wait.min(Duration::from_secs(30)))
                    .unwrap_or_else(PoisonError::into_inner);
                timer = guard;
            }
            WatchdogStep::Idle => {
                timer = shared
                    .wake
                    .wait(timer)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum WatchdogStep {
    Idle,
    Wait(Duration),
    PowerOff(&'static str),
}

fn next_shutdown_wait(
    sleep_deadline: Option<Instant>,
    idle_minutes: u32,
    hdmi_keeps_awake: bool,
    now: Instant,
    idle_elapsed_secs: u64,
) -> WatchdogStep {
    let mut earliest: Option<Duration> = None;

    if let Some(deadline) = sleep_deadline {
        if now >= deadline {
            return WatchdogStep::PowerOff("sleep timer");
        }
        earliest = Some(deadline.saturating_duration_since(now));
    }

    if idle_minutes > 0 && !hdmi_keeps_awake {
        let limit_secs = u64::from(idle_minutes) * 60;
        if idle_elapsed_secs >= limit_secs {
            return WatchdogStep::PowerOff("idle auto power-off");
        }
        let remaining = Duration::from_secs(limit_secs - idle_elapsed_secs);
        earliest = Some(match earliest {
            Some(cur) => cur.min(remaining),
            None => remaining,
        });
    }

    match earliest {
        Some(wait) => WatchdogStep::Wait(wait.max(Duration::from_secs(1))),
        None if idle_minutes > 0 => WatchdogStep::Wait(Duration::from_secs(30)),
        None => WatchdogStep::Idle,
    }
}

fn startup_choices(inputs: &[Input]) -> Vec<(String, StartupInput)> {
    let mut choices = vec![("Home screen".to_owned(), StartupInput::Home)];
    for input in inputs {
        choices.push((
            format!("{} (always)", input.label),
            StartupInput::Always(input.id.clone()),
        ));
        choices.push((
            format!("{} (when signal detected)", input.label),
            StartupInput::OnSignal(input.id.clone()),
        ));
    }
    if !inputs.is_empty() {
        choices.push(("Last selected input".to_owned(), StartupInput::LastInput));
    }
    choices
}

fn resolve_startup_target(cfg: &PowerInputConfig, inputs: &[Input]) -> Option<(String, bool)> {
    match &cfg.startup_input {
        StartupInput::Home => None,
        StartupInput::Always(id) => inputs
            .iter()
            .find(|i| i.id == *id)
            .or_else(|| inputs.first())
            .map(|i| (i.id.clone(), false)),
        StartupInput::OnSignal(id) => inputs
            .iter()
            .find(|i| i.id == *id)
            .or_else(|| inputs.first())
            .map(|i| (i.id.clone(), true)),
        StartupInput::LastInput => cfg
            .last_input
            .as_deref()
            .and_then(|last| inputs.iter().find(|i| i.id == last))
            .or_else(|| inputs.first())
            .map(|i| (i.id.clone(), false)),
    }
}

pub(super) fn sleep_timer_options() -> Vec<String> {
    SLEEP_MINUTES.iter().map(|&m| format_minutes(m)).collect()
}

fn idle_shutdown_options() -> Vec<String> {
    IDLE_MINUTES.iter().map(|&m| format_minutes(m)).collect()
}

fn format_minutes(minutes: u32) -> String {
    if minutes == 0 {
        "Off".to_owned()
    } else if minutes.is_multiple_of(60) {
        let h = minutes / 60;
        if h == 1 {
            "1 hour".to_owned()
        } else {
            format!("{h} hours")
        }
    } else {
        format!("{minutes} min")
    }
}

fn format_sleep_detail(deadline: Option<Instant>, now: Instant) -> String {
    let Some(deadline) = deadline else {
        return "Off · shut down after a countdown".to_owned();
    };
    let rem_secs = deadline.saturating_duration_since(now).as_secs();
    if rem_secs == 0 {
        "Shutting down…".to_owned()
    } else if rem_secs < 60 {
        "Shuts down in <1 min".to_owned()
    } else {
        let mins = rem_secs.div_ceil(60);
        format!("Shuts down in {mins} min")
    }
}

fn build_controls(
    cfg: &PowerInputConfig,
    inputs: &[Input],
    systemctl: bool,
    sleep_minutes: u32,
    sleep_deadline: Option<Instant>,
    hdmi_status: &str,
    now: Instant,
) -> Vec<Control> {
    let mut rows = Vec::new();

    if !inputs.is_empty() {
        rows.push(control(
            "hdmi-heading",
            "HDMI Input",
            "",
            ControlKind::Heading,
        ));
        let choices = startup_choices(inputs);
        let selected = choices
            .iter()
            .position(|(_, target)| target == &cfg.startup_input)
            .unwrap_or(0);
        let startup_detail = match &cfg.startup_input {
            StartupInput::Home => "Start on RustTV Home screen".to_owned(),
            StartupInput::Always(_) => "Open HDMI automatically when powered on".to_owned(),
            StartupInput::OnSignal(_) => {
                "Open HDMI on boot if a source is already active".to_owned()
            }
            StartupInput::LastInput => {
                let name = cfg
                    .last_input
                    .as_deref()
                    .and_then(|id| inputs.iter().find(|i| i.id == id))
                    .or_else(|| inputs.first())
                    .map_or("HDMI", |i| i.label.as_str());
                format!("Start on last selected input ({name})")
            }
        };
        rows.push(control(
            "startup-input",
            "Startup input",
            &startup_detail,
            ControlKind::Choice {
                options: choices.into_iter().map(|(label, _)| label).collect(),
                selected,
            },
        ));
        let auto_detail = if !cfg.auto_switch_hdmi {
            "Off · switch to HDMI manually from the top bar".to_owned()
        } else if let Some(mode) = hdmi_status.strip_prefix("signal ") {
            format!("On · signal detected ({})", mode.trim())
        } else if hdmi_status.trim() == "signal" {
            "On · signal detected".to_owned()
        } else {
            "On · automatically switch whenever an HDMI source turns on".to_owned()
        };
        rows.push(control(
            "auto-switch-hdmi",
            "Auto-switch on HDMI signal",
            &auto_detail,
            ControlKind::Toggle {
                on: cfg.auto_switch_hdmi,
            },
        ));
    }

    if systemctl {
        rows.push(control(
            "shutdown-heading",
            "Automatic Shutdown",
            "",
            ControlKind::Heading,
        ));
        let sleep_idx = SLEEP_MINUTES
            .iter()
            .position(|&m| m == sleep_minutes)
            .unwrap_or(0);
        rows.push(control(
            "sleep-timer",
            "Sleep timer",
            &format_sleep_detail(sleep_deadline, now),
            ControlKind::Choice {
                options: sleep_timer_options(),
                selected: sleep_idx,
            },
        ));
        let idle_idx = IDLE_MINUTES
            .iter()
            .position(|&m| m == cfg.idle_shutdown_minutes)
            .unwrap_or(0);
        let idle_detail = if cfg.idle_shutdown_minutes == 0 {
            "Off · projector stays on until turned off manually".to_owned()
        } else {
            format!(
                "Power off after {} of inactivity",
                format_minutes(cfg.idle_shutdown_minutes)
            )
        };
        rows.push(control(
            "idle-shutdown",
            "Auto power-off when idle",
            &idle_detail,
            ControlKind::Choice {
                options: idle_shutdown_options(),
                selected: idle_idx,
            },
        ));
        if !inputs.is_empty() {
            rows.push(control(
                "keep-awake-hdmi",
                "Keep awake during HDMI playback",
                if cfg.keep_awake_on_hdmi {
                    "On · idle timer pauses while an HDMI signal is active"
                } else {
                    "Off · idle timer counts down even with HDMI signal"
                },
                ControlKind::Toggle {
                    on: cfg.keep_awake_on_hdmi,
                },
            ));
        }
    }

    rows
}

fn default_config_path() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
    let base = var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| var("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("rusttv").join("power-input.toml"))
}

fn load_config(path: &Path) -> Option<PowerInputConfig> {
    let text = fs::read_to_string(path).ok()?;
    toml::from_str(&text)
        .map_err(|error| log::warn!("ignoring {}: {error}", path.display()))
        .ok()
}

fn save_config(path: &Path, cfg: &PowerInputConfig) -> std::io::Result<()> {
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

fn render_height_path() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
    let base = var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| var("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("rusttv").join("render-height"))
}

fn load_render_height(path: Option<&Path>) -> u32 {
    let from_file = path
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| s.trim().parse::<u32>().ok());
    from_file
        .or_else(|| {
            std::env::var("SLINT_SKIA_MAX_HEIGHT")
                .ok()
                .and_then(|v| v.trim().parse::<u32>().ok())
        })
        .unwrap_or(0)
}

fn save_render_height(path: &Path, height: u32) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("new");
    {
        let mut file = fs::File::create(&tmp)?;
        writeln!(file, "{height}")?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)
}

fn resolution_label(height: u32) -> String {
    RENDER_RESOLUTIONS
        .iter()
        .find(|&&(h, _, _)| h == height)
        .map(|&(_, label, _)| label.to_owned())
        .unwrap_or_else(|| format!("{height}p"))
}

/// Builds the **Settings → Display** tab with a collapsible dropdown of common
/// 16:9 UI render resolutions (applied live without restart).
pub(super) fn display_tab() -> Tab {
    display_tab_for(
        render_height_path().as_deref(),
        RENDER_DROPDOWN_OPEN.load(Ordering::Relaxed),
    )
}

fn display_tab_for(path: Option<&Path>, expanded: bool) -> Tab {
    let current = load_render_height(path);
    let active_label = resolution_label(current);
    let arrow = if expanded { "▴" } else { "▾" };
    let mut controls = vec![control(
        "render-resolution",
        "Render resolution",
        &format!("{active_label}  {arrow}"),
        ControlKind::Action,
    )];
    if expanded {
        for &(height, label, detail) in RENDER_RESOLUTIONS {
            controls.push(control(
                &format!("{RES_PREFIX}{height}"),
                label,
                detail,
                ControlKind::Toggle {
                    on: current == height,
                },
            ));
        }
    }
    controls.push(control(
        "display-note",
        "Applies live to RustTV UI",
        "HDMI, video playback, and launched apps stay at native 1080p",
        ControlKind::Info,
    ));
    Tab {
        id: DISPLAY_TAB.to_owned(),
        title: "Display".to_owned(),
        controls,
    }
}

/// Handles changes inside **Settings → Display**.
pub(super) fn set_display(control_id: &str, value: &Value) -> Result<(), String> {
    let path = render_height_path().ok_or_else(|| "no config directory".to_owned())?;
    set_display_at(&path, control_id, value, &RENDER_DROPDOWN_OPEN)
}

fn set_display_at(
    path: &Path,
    control_id: &str,
    value: &Value,
    dropdown_open: &AtomicBool,
) -> Result<(), String> {
    match (control_id, value) {
        ("render-resolution", Value::Action) => {
            dropdown_open.fetch_xor(true, Ordering::Relaxed);
            Ok(())
        }
        (id, Value::Toggle(_) | Value::Action) if id.starts_with(RES_PREFIX) => {
            let height: u32 = id[RES_PREFIX.len()..]
                .parse()
                .map_err(|_| "invalid render resolution".to_owned())?;
            if !RENDER_RESOLUTIONS.iter().any(|&(h, _, _)| h == height) {
                return Err(format!("unsupported render resolution {height}p"));
            }
            save_render_height(path, height).map_err(|e| e.to_string())?;
            dropdown_open.store(false, Ordering::Relaxed);
            Ok(())
        }
        _ => Err(format!("unknown display setting {control_id} = {value:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdmi_input(id: &str, label: &str) -> Input {
        Input {
            id: id.to_owned(),
            label: label.to_owned(),
            app: format!("{id}.desktop"),
            watch: vec!["hy320f-hdmi".to_owned(), "watch".to_owned()],
        }
    }

    #[test]
    fn resolves_startup_input_modes() {
        let inputs = vec![hdmi_input("hdmi1", "HDMI 1"), hdmi_input("hdmi2", "HDMI 2")];
        let mut cfg = PowerInputConfig::default();
        assert_eq!(resolve_startup_target(&cfg, &inputs), None);

        cfg.startup_input = StartupInput::Always("hdmi1".to_owned());
        assert_eq!(
            resolve_startup_target(&cfg, &inputs),
            Some(("hdmi1".to_owned(), false))
        );

        cfg.startup_input = StartupInput::OnSignal("hdmi2".to_owned());
        assert_eq!(
            resolve_startup_target(&cfg, &inputs),
            Some(("hdmi2".to_owned(), true))
        );

        cfg.startup_input = StartupInput::LastInput;
        assert_eq!(
            resolve_startup_target(&cfg, &inputs),
            Some(("hdmi1".to_owned(), false))
        );
        cfg.last_input = Some("hdmi2".to_owned());
        assert_eq!(
            resolve_startup_target(&cfg, &inputs),
            Some(("hdmi2".to_owned(), false))
        );
    }

    #[test]
    fn watchdog_respects_sleep_idle_and_hdmi_keep_awake() {
        let now = Instant::now();
        assert_eq!(
            next_shutdown_wait(None, 0, false, now, 0),
            WatchdogStep::Idle
        );

        let expired = now.checked_sub(Duration::from_secs(1)).unwrap_or(now);
        assert_eq!(
            next_shutdown_wait(Some(expired), 0, false, now, 0),
            WatchdogStep::PowerOff("sleep timer")
        );

        assert_eq!(
            next_shutdown_wait(None, 30, false, now, 30 * 60),
            WatchdogStep::PowerOff("idle auto power-off")
        );

        // Active HDMI pauses idle auto power-off, but never blocks an explicit
        // sleep timer.
        assert_eq!(
            next_shutdown_wait(None, 30, true, now, 9999),
            WatchdogStep::Wait(Duration::from_secs(30))
        );
        assert_eq!(
            next_shutdown_wait(Some(expired), 30, true, now, 9999),
            WatchdogStep::PowerOff("sleep timer")
        );
    }

    #[test]
    fn saves_and_reloads_power_input_config() {
        let dir = std::env::temp_dir().join(format!("rusttv-power-input-{}", std::process::id()));
        let path = dir.join("power-input.toml");
        let cfg = PowerInputConfig {
            startup_input: StartupInput::Always("hdmi".to_owned()),
            last_input: Some("hdmi".to_owned()),
            auto_switch_hdmi: false,
            idle_shutdown_minutes: 60,
            keep_awake_on_hdmi: true,
        };
        save_config(&path, &cfg).expect("save");
        assert_eq!(load_config(&path), Some(cfg));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn builds_power_input_controls_and_persists_changes() {
        let dir = std::env::temp_dir().join(format!("rusttv-pi-tab-{}", std::process::id()));
        let path = dir.join("power-input.toml");
        let pi = PowerInput::new(
            Some(path.clone()),
            vec![hdmi_input("hdmi", "HDMI")],
            true,
            false,
        );
        pi.set("startup-input", &Value::Choice(1))
            .expect("always hdmi");
        pi.set("idle-shutdown", &Value::Choice(2))
            .expect("30 min idle");
        pi.set("sleep-timer", &Value::Choice(2))
            .expect("30 min sleep");
        assert_eq!(pi.startup_input_target(), Some(("hdmi".to_owned(), false)));
        assert!(pi.sleep_timer_detail().contains("30 min"));
        let tab = pi.tab().expect("tab");
        assert!(tab.controls.iter().any(|c| c.id == "startup-input"));
        assert!(tab.controls.iter().any(|c| c.id == "sleep-timer"));
        assert!(tab.controls.iter().any(|c| c.id == "idle-shutdown"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn display_tab_saves_and_reloads_render_resolution() {
        let dir = std::env::temp_dir().join(format!("rusttv-display-res-{}", std::process::id()));
        let path = dir.join("render-height");
        let open = AtomicBool::new(false);
        assert_eq!(load_render_height(Some(&path)), 0);

        // Collapsed shows the dropdown row + info note.
        let collapsed = display_tab_for(Some(&path), false);
        assert_eq!(collapsed.id, DISPLAY_TAB);
        assert_eq!(collapsed.controls.len(), 2);
        assert!(collapsed.controls[0].detail.contains("1920 × 1080"));
        assert!(collapsed.controls[0].detail.contains('▾'));

        // Opening the dropdown exposes all standard resolutions.
        set_display_at(&path, "render-resolution", &Value::Action, &open).expect("open");
        assert!(open.load(Ordering::Relaxed));
        let expanded = display_tab_for(Some(&path), true);
        assert_eq!(expanded.controls.len(), RENDER_RESOLUTIONS.len() + 2);
        assert!(expanded.controls[0].detail.contains('▴'));

        // Picking 720p saves it and collapses the dropdown.
        set_display_at(&path, "res:720", &Value::Toggle(true), &open).expect("720p");
        assert_eq!(load_render_height(Some(&path)), 720);
        assert!(!open.load(Ordering::Relaxed));
        let after = display_tab_for(Some(&path), true);
        assert!(
            after.controls.iter().any(|c| {
                c.id == "res:720" && matches!(c.kind, ControlKind::Toggle { on: true })
            })
        );

        // Picking 1366×768 works too.
        set_display_at(&path, "res:768", &Value::Toggle(true), &open).expect("768p");
        assert_eq!(load_render_height(Some(&path)), 768);
        let _ = fs::remove_dir_all(&dir);
    }
}
