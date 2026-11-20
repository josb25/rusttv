//! The device's settings on Linux (device.rs), through the system's own
//! tools, each only where it is installed: `wpctl` (PipeWire/WirePlumber)
//! for audio, `nmcli` (NetworkManager) for networks, `bluetoothctl` for
//! Bluetooth and `systemctl` to restart or power off. Facts for About come
//! from `/etc/os-release`, `/proc` and `/sys`. A device's own controls (a
//! projector's lens focus) come from its device files (`device_file`).
//!
//! Every method blocks on a tool (at most [`TOOL_TIMEOUT`]); the launcher
//! calls them on worker threads only.

use std::io::Read as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

mod net;

use super::device_file::{self, DeviceControls, Input, SideRow};
use super::program_exists;
use crate::device::{
    Control, ControlKind, DeviceSettings, Link, QuickPanel, Tab, Value, Volume, VolumeChange,
};

/// How long a tool may take before it is stopped. Listing Wi-Fi networks
/// can scan for a few seconds.
const TOOL_TIMEOUT: Duration = Duration::from_secs(15);

/// The default output, as wpctl names it.
const SINK: &str = "@DEFAULT_AUDIO_SINK@";

/// How much one volume key changes the volume, in percent.
const VOLUME_STEP: u32 = 5;

/// The most Wi-Fi networks a panel lists (strongest first).
const MAX_NETWORKS: usize = 8;

/// Control ids are a prefix and the SSID, interface or device address.
/// Connects to a Wi-Fi network (or asks for its password first).
const CONNECT: &str = "connect:";
/// The connected or a saved Wi-Fi network: OK shows or hides its details
/// and actions.
const WIFI: &str = "wifi:";
const DISCONNECT: &str = "disconnect:";
/// Deletes a saved Wi-Fi network.
const FORGET: &str = "forget:";
/// A wired interface's connection: OK shows or hides its details.
const WIRED: &str = "wired:";
/// A wired interface's switch.
const WIRED_SWITCH: &str = "wired-on:";
/// A paired Bluetooth device: OK shows or hides its details and actions.
const BT: &str = "bt:";
const BT_CONNECT: &str = "bt-connect:";
const BT_DISCONNECT: &str = "bt-disconnect:";
/// Removes a paired Bluetooth device.
const BT_FORGET: &str = "bt-forget:";
/// Pairs with a device found nearby.
const PAIR: &str = "pair:";

/// How long a Bluetooth search runs, in seconds.
const BT_SCAN_SECONDS: &str = "8";

/// Where an image names another system on the same device that the Power
/// panel restarts into (dual boot). Root-owned: it holds a command.
const OTHER_OS: &str = "/etc/rusttv/other-os.toml";

/// Another system to restart into, from [`OTHER_OS`]:
///
/// ```toml
/// label = "Restart into Android"
/// command = ["systemctl", "start", "--no-block", "switch-to-android.service"]
/// ```
///
/// The command runs as the launcher's user, without a shell; it switches
/// the boot target and restarts.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct OtherOs {
    label: String,
    command: Vec<String>,
}

/// [`OtherOs`] from its file; `None` if it is unreadable or has no command.
fn parse_other_os(text: &str) -> Option<OtherOs> {
    toml::from_str::<OtherOs>(text)
        .map_err(|error| log::warn!("{OTHER_OS}: {error}"))
        .ok()
        .filter(|other| !other.command.is_empty() && !other.label.is_empty())
}

/// [`DeviceSettings`] through the tools found in `PATH` at start.
#[derive(Debug, Clone, Default)]
pub struct LinuxDevice {
    wpctl: bool,
    nmcli: bool,
    bluetoothctl: bool,
    systemctl: bool,
    /// Another system to restart into (dual boot).
    other_os: Option<OtherOs>,
    /// Whether a Bluetooth search ran: Devices then lists what it found.
    scanned: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The device's own controls, from its device files.
    controls: std::sync::Arc<DeviceControls>,
    /// The row whose details and actions show (a network, an interface, a
    /// Bluetooth device), by control id.
    open: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// The name typed into Add network, waiting for its password.
    hidden: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

impl LinuxDevice {
    /// Looks for the tools in `PATH`.
    pub fn detect() -> Self {
        Self {
            wpctl: program_exists("wpctl"),
            nmcli: program_exists("nmcli"),
            bluetoothctl: program_exists("bluetoothctl"),
            systemctl: program_exists("systemctl"),
            other_os: std::fs::read_to_string(OTHER_OS)
                .ok()
                .and_then(|text| parse_other_os(&text)),
            scanned: std::sync::Arc::default(),
            controls: std::sync::Arc::new(DeviceControls::load(
                Path::new(device_file::DIR),
                super::ui_language(),
            )),
            open: std::sync::Arc::default(),
            hidden: std::sync::Arc::default(),
        }
    }

    fn open_row(&self) -> Option<String> {
        self.open.lock().ok().and_then(|open| open.clone())
    }

    /// Shows the details of row `id`, or hides them when they show.
    fn toggle_open(&self, id: &str) {
        if let Ok(mut open) = self.open.lock() {
            *open = if open.as_deref() == Some(id) {
                None
            } else {
                Some(id.to_owned())
            };
        }
    }

    /// Hides the details that show.
    fn close_open(&self) {
        if let Ok(mut open) = self.open.lock() {
            *open = None;
        }
    }

    fn hidden_network(&self) -> Option<String> {
        self.hidden.lock().ok().and_then(|hidden| hidden.clone())
    }

    fn set_hidden_network(&self, ssid: Option<String>) {
        if let Ok(mut hidden) = self.hidden.lock() {
            *hidden = ssid;
        }
    }

    /// Whether the device-file control `control` of tab `tab` shows the
    /// test pattern while it moves.
    pub(super) fn shows_pattern(&self, tab: &str, control: &str) -> bool {
        self.controls.pattern(tab, control)
    }

    /// The device's video input, from its device files.
    pub(super) fn input(&self) -> Option<Input> {
        self.controls.input()
    }

    /// The device-file controls of the side menu over apps.
    pub(super) fn side_menu(&self) -> Vec<SideRow> {
        self.controls.side_menu()
    }

    /// Whether this panel can be shown (its tool is installed).
    pub(super) fn has(&self, panel: QuickPanel) -> bool {
        match panel {
            QuickPanel::Audio => self.wpctl,
            QuickPanel::Network => self.nmcli,
            QuickPanel::Power => self.systemctl,
        }
    }

    fn audio(&self) -> Option<Tab> {
        if !self.wpctl {
            return None;
        }
        let volume = run("wpctl", &["get-volume", SINK])
            .ok()
            .and_then(|text| parse_volume(&text));
        let mut controls = Vec::new();
        if let Some(volume) = volume {
            controls.push(control(
                "volume",
                "Volume",
                "",
                ControlKind::Slider {
                    value: percent(volume.level),
                    min: 0,
                    max: 100,
                    step: VOLUME_STEP as i32,
                },
            ));
            controls.push(control(
                "mute",
                "Mute",
                "",
                ControlKind::Toggle { on: volume.muted },
            ));
        }
        let sinks = run("wpctl", &["status"])
            .map(|text| parse_sinks(&text))
            .unwrap_or_default();
        if !sinks.is_empty() {
            let selected = sinks.iter().position(|sink| sink.default).unwrap_or(0);
            controls.push(control(
                "output",
                "Output",
                "",
                ControlKind::Choice {
                    options: sinks.into_iter().map(|sink| sink.name).collect(),
                    selected,
                },
            ));
        }
        if controls.is_empty() {
            controls.push(control(
                "none",
                "No sound output",
                "PipeWire is not running",
                ControlKind::Info,
            ));
        }
        Some(tab(QuickPanel::Audio, "Audio", controls))
    }

    /// Network: Wi-Fi (on or off, the connected network, the ones nearby,
    /// the saved ones, Add network) and the wired interfaces. `interfaces`
    /// (Settings only) adds each interface's details; without nmcli they
    /// are all there is.
    fn network(&self, interfaces: Option<&[net::Details]>) -> Option<Tab> {
        let interfaces = interfaces.unwrap_or_default();
        let open = self.open_row();
        if !self.nmcli {
            if interfaces.is_empty() {
                return None;
            }
            let controls = interface_rows(interfaces, &[], open.as_deref());
            return Some(tab(QuickPanel::Network, "Network", controls));
        }
        let on =
            run("nmcli", &["-t", "-f", "WIFI", "radio"]).is_ok_and(|text| text.trim() == "enabled");
        let wifi = if on {
            let networks = run(
                "nmcli",
                &[
                    "-t",
                    "-f",
                    "IN-USE,SSID,SIGNAL,SECURITY",
                    "device",
                    "wifi",
                    "list",
                    "--rescan",
                    "no",
                ],
            )
            .map(|text| parse_networks(&text))
            .unwrap_or_default();
            let saved = run("nmcli", &["-t", "-f", "NAME,TYPE", "connection", "show"])
                .map(|text| parse_saved(&text))
                .unwrap_or_default();
            WifiState {
                on,
                networks,
                saved,
                details: interfaces
                    .iter()
                    .find(|interface| interface.wifi && interface.state == "Connected")
                    .map(|interface| interface.facts.clone())
                    .unwrap_or_default(),
                open: open.clone(),
                hidden: self.hidden_network(),
            }
        } else {
            WifiState::default()
        };
        let mut controls = wifi_rows(&wifi);
        let wired: Vec<net::Details> = interfaces
            .iter()
            .filter(|interface| !interface.wifi)
            .cloned()
            .collect();
        let states = run(
            "nmcli",
            &["-t", "-f", "DEVICE,TYPE,STATE,CONNECTION", "device"],
        )
        .map(|text| parse_wired(&text))
        .unwrap_or_default();
        controls.extend(interface_rows(&wired, &states, open.as_deref()));
        Some(tab(QuickPanel::Network, "Network", controls))
    }

    /// Bluetooth: on or off, the connected and the paired devices (OK
    /// shows a device's details and actions), the ones found nearby (OK
    /// pairs) and Search.
    fn devices(&self) -> Option<Tab> {
        if !self.bluetoothctl {
            return None;
        }
        let on = run("bluetoothctl", &["show"])
            .ok()
            .and_then(|text| parse_powered(&text))?;
        let mut state = BluetoothState {
            on,
            open: self.open_row(),
            ..BluetoothState::default()
        };
        if on {
            let list = |filter: &str| {
                run("bluetoothctl", &["devices", filter])
                    .map(|text| parse_bt_devices(&text))
                    .unwrap_or_default()
            };
            state.paired = list("Paired");
            state.connected = list("Connected")
                .into_iter()
                .map(|(address, _)| address)
                .collect();
            if self.scanned.load(std::sync::atomic::Ordering::Relaxed) {
                state.found = Some(
                    run("bluetoothctl", &["devices"])
                        .map(|text| parse_bt_devices(&text))
                        .unwrap_or_default(),
                );
            }
            if let Some(address) = state.open.as_deref().and_then(|id| id.strip_prefix(BT)) {
                state.info = run("bluetoothctl", &["info", address])
                    .map(|text| parse_bt_info(&text))
                    .unwrap_or_default();
            }
        }
        Some(Tab {
            id: "devices".to_owned(),
            title: "Bluetooth".to_owned(),
            controls: bluetooth_rows(&state),
        })
    }

    fn power(&self) -> Option<Tab> {
        self.systemctl.then(|| {
            let mut controls = vec![control("reboot", "Restart", "", ControlKind::Action)];
            if let Some(other) = &self.other_os {
                controls.push(control("other-os", &other.label, "", ControlKind::Action));
            }
            controls.push(control("poweroff", "Power off", "", ControlKind::Action));
            tab(QuickPanel::Power, "Power", controls)
        })
    }
}

impl DeviceSettings for LinuxDevice {
    /// Sound and Network, as in their quick panels.
    fn tabs(&self) -> Vec<Tab> {
        let mut tabs = Vec::new();
        if let Some(mut audio) = self.audio() {
            "Sound".clone_into(&mut audio.title);
            tabs.push(audio);
        }
        // Settings' Network tab also has each interface's details (not
        // the quick panel); it is there without nmcli too.
        tabs.extend(self.network(Some(&net::details())));
        tabs.extend(self.devices());
        tabs.extend(self.controls.tabs());
        tabs
    }

    fn panel(&self, panel: QuickPanel) -> Option<Tab> {
        match panel {
            QuickPanel::Audio => self.audio(),
            QuickPanel::Network => self.network(None),
            QuickPanel::Power => self.power(),
        }
    }

    fn set(&self, tab: &str, control: &str, value: Value) -> Result<(), String> {
        if let Value::Step(step) = value
            && let Some(command) = self.controls.command(tab, control, step)
        {
            let (program, args) = command.split_first().ok_or("no command")?;
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            return run(program, &args).map(drop);
        }
        let on_off = |on: bool| if on { "on" } else { "off" };
        match (control, value) {
            ("volume", Value::Number(level)) => run(
                "wpctl",
                &["set-volume", SINK, &format!("{}%", level.clamp(0, 100))],
            ),
            ("mute", Value::Toggle(on)) => {
                run("wpctl", &["set-mute", SINK, if on { "1" } else { "0" }])
            }
            ("output", Value::Choice(index)) => {
                let sinks = parse_sinks(&run("wpctl", &["status"])?);
                let sink = sinks.get(index).ok_or("that output is gone")?;
                run("wpctl", &["set-default", &sink.id.to_string()])
            }
            ("wifi", Value::Toggle(on)) => run("nmcli", &["radio", "wifi", on_off(on)]),
            (control, Value::Toggle(on)) if control.starts_with(WIRED_SWITCH) => {
                let device = &control[WIRED_SWITCH.len()..];
                if on {
                    // Without a cable there is nothing to connect yet; it
                    // connects by itself once one is plugged in.
                    run("nmcli", &["device", "set", device, "autoconnect", "yes"])?;
                    run("nmcli", &["device", "connect", device]).or(Ok(String::new()))
                } else {
                    run("nmcli", &["device", "disconnect", device])
                }
            }
            ("bluetooth", Value::Toggle(on)) => run("bluetoothctl", &["power", on_off(on)]),
            (control, Value::Action)
                if control.starts_with(WIFI)
                    || control.starts_with(WIRED)
                    || control.starts_with(BT) =>
            {
                self.toggle_open(control);
                Ok(String::new())
            }
            (control, Value::Action) if control.starts_with(CONNECT) => {
                let ssid = &control[CONNECT.len()..];
                // A saved network first; else a new, open one.
                let connected = run("nmcli", &["connection", "up", "id", ssid])
                    .or_else(|_| run("nmcli", &["device", "wifi", "connect", ssid]));
                if connected.is_ok() {
                    self.close_open();
                }
                connected
            }
            (control, Value::Text(password)) if control.starts_with(CONNECT) => {
                let ssid = &control[CONNECT.len()..];
                // `--ask` reads the password from stdin.
                run_with_input(
                    "nmcli",
                    &["--ask", "device", "wifi", "connect", ssid],
                    Some(&password),
                )
            }
            (control, Value::Action) if control.starts_with(DISCONNECT) => run(
                "nmcli",
                &["connection", "down", "id", &control[DISCONNECT.len()..]],
            ),
            (control, Value::Action) if control.starts_with(FORGET) => {
                let forgot = run(
                    "nmcli",
                    &["connection", "delete", "id", &control[FORGET.len()..]],
                );
                self.close_open();
                forgot
            }
            ("add-network", Value::Text(ssid)) => {
                let ssid = ssid.trim();
                if ssid.is_empty() {
                    return Err("no network name".to_owned());
                }
                self.set_hidden_network(Some(ssid.to_owned()));
                Ok(String::new())
            }
            ("add-password", Value::Text(password)) => {
                let ssid = self.hidden_network().ok_or("no network name")?;
                let connected = run_with_input(
                    "nmcli",
                    &["--ask", "device", "wifi", "connect", &ssid, "hidden", "yes"],
                    Some(&password),
                );
                if connected.is_ok() {
                    self.set_hidden_network(None);
                }
                connected
            }
            ("add-open", Value::Action) => {
                let ssid = self.hidden_network().ok_or("no network name")?;
                let connected = run(
                    "nmcli",
                    &["device", "wifi", "connect", &ssid, "hidden", "yes"],
                );
                if connected.is_ok() {
                    self.set_hidden_network(None);
                }
                connected
            }
            ("add-cancel", Value::Action) => {
                self.set_hidden_network(None);
                Ok(String::new())
            }
            ("rescan", Value::Action) => run(
                "nmcli",
                &[
                    "-t", "-f", "SSID", "device", "wifi", "list", "--rescan", "yes",
                ],
            ),
            ("bt-scan", Value::Action) => {
                let scanned = run(
                    "bluetoothctl",
                    &["--timeout", BT_SCAN_SECONDS, "scan", "on"],
                );
                self.scanned
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                scanned
            }
            (control, Value::Action) if control.starts_with(BT_CONNECT) => {
                run("bluetoothctl", &["connect", &control[BT_CONNECT.len()..]])
            }
            (control, Value::Action) if control.starts_with(BT_DISCONNECT) => run(
                "bluetoothctl",
                &["disconnect", &control[BT_DISCONNECT.len()..]],
            ),
            (control, Value::Action) if control.starts_with(BT_FORGET) => {
                let removed = run("bluetoothctl", &["remove", &control[BT_FORGET.len()..]]);
                self.close_open();
                removed
            }
            (control, Value::Action) if control.starts_with(PAIR) => {
                let address = &control[PAIR.len()..];
                run("bluetoothctl", &["pair", address])
                    .and_then(|_| run("bluetoothctl", &["trust", address]))
                    .and_then(|_| run("bluetoothctl", &["connect", address]))
            }
            ("reboot", Value::Action) => run("systemctl", &["reboot"]),
            ("poweroff", Value::Action) => run("systemctl", &["poweroff"]),
            ("other-os", Value::Action) => {
                let other = self.other_os.as_ref().ok_or("no other system")?;
                let (program, args) = other.command.split_first().ok_or("no command")?;
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                run(program, &args)
            }
            (control, value) => Err(format!("unknown setting {control} = {value:?}")),
        }
        .map(drop)
    }

    fn about(&self) -> Vec<(String, String)> {
        let read = |path: &str| std::fs::read_to_string(path).ok();
        let mut facts = Vec::new();
        if let Some(name) = read("/etc/os-release")
            .or_else(|| read("/usr/lib/os-release"))
            .and_then(|text| os_name(&text))
        {
            facts.push(("Operating system".to_owned(), name));
        }
        if let Some(kernel) = read("/proc/sys/kernel/osrelease") {
            facts.push(("Linux kernel".to_owned(), kernel.trim().to_owned()));
        }
        if let Some(model) = read("/proc/device-tree/model")
            .or_else(|| read("/sys/class/dmi/id/product_name"))
            .map(|text| {
                text.trim_matches(|c: char| c == '\0' || c.is_whitespace())
                    .to_owned()
            })
            .filter(|model| !model.is_empty())
        {
            facts.push(("Device".to_owned(), model));
        }
        if let Some(memory) = read("/proc/meminfo").and_then(|text| memory_total(&text)) {
            facts.push(("Memory".to_owned(), memory));
        }
        facts
    }

    fn volume(&self, change: VolumeChange) -> Option<Volume> {
        if !self.wpctl {
            return None;
        }
        let step = format!("{VOLUME_STEP}%");
        let changed = match change {
            VolumeChange::Up => run(
                "wpctl",
                &["set-volume", "-l", "1.0", SINK, &format!("{step}+")],
            ),
            VolumeChange::Down => run("wpctl", &["set-volume", SINK, &format!("{step}-")]),
            VolumeChange::Mute => run("wpctl", &["set-mute", SINK, "toggle"]),
        };
        if let Err(error) = changed {
            log::warn!("changing the volume failed: {error}");
        }
        run("wpctl", &["get-volume", SINK])
            .ok()
            .and_then(|text| parse_volume(&text))
    }

    fn volume_now(&self) -> Option<Volume> {
        if !self.wpctl {
            return None;
        }
        run("wpctl", &["get-volume", SINK])
            .ok()
            .and_then(|text| parse_volume(&text))
    }

    /// NetworkManager's Wi-Fi and Ethernet interfaces; the signal from its
    /// last scan (no new scan).
    fn links(&self) -> Vec<Link> {
        if !self.nmcli {
            return Vec::new();
        }
        let Ok(devices) = run("nmcli", &["-t", "-f", "DEVICE,TYPE,STATE", "device"]) else {
            return Vec::new();
        };
        let signals = run(
            "nmcli",
            &[
                "-t",
                "-f",
                "IN-USE,SIGNAL,DEVICE",
                "device",
                "wifi",
                "list",
                "--rescan",
                "no",
            ],
        )
        .unwrap_or_default();
        parse_links(&devices, &signals)
    }
}

/// The interfaces from `nmcli -t -f DEVICE,TYPE,STATE device`, with the
/// signal of each Wi-Fi's network in use from
/// `nmcli -t -f IN-USE,SIGNAL,DEVICE device wifi list`.
fn parse_links(devices: &str, signals: &str) -> Vec<Link> {
    let signal = |device: &str| {
        signals
            .lines()
            .map(split_terse)
            .find_map(|fields| match fields.as_slice() {
                [in_use, signal, name] if in_use.trim() == "*" && name == device => {
                    signal.parse::<u8>().ok()
                }
                _ => None,
            })
    };
    devices
        .lines()
        .map(split_terse)
        .filter_map(|fields| {
            let [device, kind, state] = fields.as_slice() else {
                return None;
            };
            let up = state.starts_with("connected");
            match kind.as_str() {
                _ if state == "unmanaged" => None,
                "wifi" => Some(Link::Wifi(up.then(|| {
                    // 1..4 bars, as most systems draw them; 4 when unknown.
                    signal(device).map_or(4, |signal| signal.div_ceil(25).clamp(1, 4))
                }))),
                "ethernet" => Some(Link::Wired(up)),
                _ => None,
            }
        })
        .collect()
}

fn tab(panel: QuickPanel, title: &str, controls: Vec<Control>) -> Tab {
    Tab {
        id: panel.id().to_owned(),
        title: title.to_owned(),
        controls,
    }
}

fn control(id: &str, label: &str, detail: &str, kind: ControlKind) -> Control {
    Control {
        id: id.to_owned(),
        label: label.to_owned(),
        detail: detail.to_owned(),
        kind,
    }
}

/// 0..1 as a whole percent.
fn percent(level: f32) -> i32 {
    // A volume is a small number; the cast cannot overflow.
    (level * 100.0).round() as i32
}

/// Runs a tool and returns its output; stops it after [`TOOL_TIMEOUT`].
/// `Err` is the tool's error message or why it didn't run.
fn run(program: &str, args: &[&str]) -> Result<String, String> {
    run_with_input(program, args, None)
}

/// [`run`] with `input` on the tool's stdin (a password, so it is never
/// on a command line other users can read).
fn run_with_input(program: &str, args: &[&str], input: Option<&str>) -> Result<String, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("{program}: {error}"))?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        use std::io::Write as _;
        // Dropped after writing: the tool sees the end of its input.
        let _ = writeln!(stdin, "{input}");
    }
    let deadline = Instant::now() + TOOL_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{program} took too long"));
            }
            Err(error) => return Err(format!("{program}: {error}")),
        }
    };
    let mut output = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut output);
    }
    if status.success() {
        Ok(output)
    } else {
        let mut error = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            let _ = stderr.read_to_string(&mut error);
        }
        let error = error.trim();
        Err(if error.is_empty() {
            format!("{program} failed ({status})")
        } else {
            error.lines().next().unwrap_or(error).to_owned()
        })
    }
}

/// `wpctl get-volume`: "Volume: 0.40" or "Volume: 0.40 [MUTED]".
fn parse_volume(text: &str) -> Option<Volume> {
    let rest = text.trim().strip_prefix("Volume:")?.trim();
    let level = rest.split_whitespace().next()?.parse::<f32>().ok()?;
    Some(Volume {
        level,
        muted: rest.contains("[MUTED]"),
    })
}

/// An audio output in `wpctl status`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Sink {
    id: u32,
    name: String,
    default: bool,
}

/// The "Sinks:" of the "Audio" section of `wpctl status`, e.g.
/// ` │  *   48. Built-in Audio Analog Stereo [vol: 0.40]`.
fn parse_sinks(text: &str) -> Vec<Sink> {
    let mut sinks = Vec::new();
    let mut in_audio = false;
    let mut in_sinks = false;
    for line in text.lines() {
        if !line.starts_with([' ', '│', '├', '└']) {
            in_audio = line.trim() == "Audio";
            in_sinks = false;
            continue;
        }
        let item = line.trim_start_matches([' ', '│', '├', '└', '─']);
        if item.ends_with(':') {
            in_sinks = in_audio && item == "Sinks:";
            continue;
        }
        if !in_sinks {
            continue;
        }
        let (default, item) = match item.strip_prefix('*') {
            Some(rest) => (true, rest.trim_start()),
            None => (false, item),
        };
        let Some((id, name)) = item.split_once(". ") else {
            continue;
        };
        let Ok(id) = id.trim().parse() else {
            continue;
        };
        let name = name.split(" [vol:").next().unwrap_or(name).trim();
        sinks.push(Sink {
            id,
            name: name.to_owned(),
            default,
        });
    }
    sinks
}

/// Splits a line of nmcli's terse output (`-t`) at unescaped colons.
fn split_terse(line: &str) -> Vec<String> {
    let mut fields = vec![String::new()];
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let (Some(next), Some(field)) = (chars.next(), fields.last_mut()) {
                    field.push(next);
                }
            }
            ':' => fields.push(String::new()),
            c => {
                if let Some(field) = fields.last_mut() {
                    field.push(c);
                }
            }
        }
    }
    fields
}

/// A Wi-Fi network from `nmcli -t -f IN-USE,SSID,SIGNAL,SECURITY device wifi list`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Network {
    in_use: bool,
    ssid: String,
    signal: u8,
    security: String,
}

/// The networks with a name, each SSID once (its strongest), strongest
/// first, at most [`MAX_NETWORKS`] besides the one in use.
fn parse_networks(text: &str) -> Vec<Network> {
    let mut networks: Vec<Network> = Vec::new();
    for fields in text.lines().map(split_terse) {
        let [in_use, ssid, signal, security] = fields.as_slice() else {
            continue;
        };
        if ssid.is_empty() {
            continue;
        }
        let network = Network {
            in_use: in_use.trim() == "*",
            ssid: ssid.clone(),
            signal: signal.parse().unwrap_or(0),
            security: security.clone(),
        };
        match networks.iter_mut().find(|known| known.ssid == network.ssid) {
            Some(known) => {
                known.in_use |= network.in_use;
                known.signal = known.signal.max(network.signal);
            }
            None => networks.push(network),
        }
    }
    networks.sort_by(|a, b| b.in_use.cmp(&a.in_use).then(b.signal.cmp(&a.signal)));
    let keep = MAX_NETWORKS + usize::from(networks.first().is_some_and(|n| n.in_use));
    networks.truncate(keep);
    networks
}

/// What the Wi-Fi rows of the Network tab are built from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct WifiState {
    on: bool,
    /// Nearby, strongest first ([`parse_networks`]).
    networks: Vec<Network>,
    /// Saved connections' names.
    saved: Vec<String>,
    /// The Wi-Fi interface's details while connected.
    details: Vec<Control>,
    /// The row whose details show.
    open: Option<String>,
    /// The name typed into Add network, waiting for its password.
    hidden: Option<String>,
}

/// "WPA2" as nmcli lists it; "Open" for none.
fn security_text(security: &str) -> &str {
    if security.is_empty() || security == "--" {
        "Open"
    } else {
        security
    }
}

fn heading(id: &str, label: &str) -> Control {
    control(id, label, "", ControlKind::Heading)
}

/// The Wi-Fi rows, in the order a connection goes: the switch, the
/// network in use (OK: its details, Disconnect, Forget), the networks
/// nearby (OK connects, asking for the password of a secured one), the
/// saved ones (OK: Connect, Forget), and Add network for one that hides
/// its name.
fn wifi_rows(state: &WifiState) -> Vec<Control> {
    let mut controls = vec![control(
        "wifi",
        "Wi-Fi",
        "",
        ControlKind::Toggle { on: state.on },
    )];
    if !state.on {
        return controls;
    }
    let open = state.open.as_deref();
    if let Some(current) = state.networks.iter().find(|network| network.in_use) {
        let id = format!("{WIFI}{}", current.ssid);
        controls.push(heading("wifi-connected", "Connected"));
        let shown = open == Some(id.as_str());
        controls.push(control(
            &id,
            &current.ssid,
            &format!(
                "Connected · {} % · {}",
                current.signal,
                security_text(&current.security)
            ),
            ControlKind::Action,
        ));
        if shown {
            controls.extend(
                state
                    .details
                    .iter()
                    .filter(|fact| fact.label != "Network")
                    .cloned(),
            );
            controls.push(control(
                &format!("{DISCONNECT}{}", current.ssid),
                "Disconnect",
                "",
                ControlKind::Action,
            ));
            controls.push(control(
                &format!("{FORGET}{}", current.ssid),
                "Forget network",
                "",
                ControlKind::Action,
            ));
        }
    }
    controls.push(heading("wifi-available", "Available networks"));
    let mut nearby = 0;
    for network in state
        .networks
        .iter()
        .filter(|network| !network.in_use && !state.saved.contains(&network.ssid))
    {
        nearby += 1;
        let security = security_text(&network.security);
        let kind = if security == "Open" {
            ControlKind::Action
        } else {
            ControlKind::Text { secret: true }
        };
        controls.push(control(
            &format!("{CONNECT}{}", network.ssid),
            &network.ssid,
            &format!("{security} · {} %", network.signal),
            kind,
        ));
    }
    if nearby == 0 {
        controls.push(control(
            "wifi-none",
            "No other networks found",
            "",
            ControlKind::Info,
        ));
    }
    controls.push(control("rescan", "Search again", "", ControlKind::Action));
    let saved: Vec<&String> = state
        .saved
        .iter()
        .filter(|name| {
            !state
                .networks
                .iter()
                .any(|network| network.in_use && &&network.ssid == name)
        })
        .collect();
    if !saved.is_empty() {
        controls.push(heading("wifi-saved", "Saved networks"));
    }
    for name in saved {
        let id = format!("{WIFI}{name}");
        let near = state.networks.iter().find(|network| &network.ssid == name);
        controls.push(control(
            &id,
            name,
            &near.map_or_else(
                || "Not in range".to_owned(),
                |network| format!("In range · {} %", network.signal),
            ),
            ControlKind::Action,
        ));
        if open == Some(id.as_str()) {
            if near.is_some() {
                controls.push(control(
                    &format!("{CONNECT}{name}"),
                    "Connect",
                    "",
                    ControlKind::Action,
                ));
            }
            controls.push(control(
                &format!("{FORGET}{name}"),
                "Forget network",
                "",
                ControlKind::Action,
            ));
        }
    }
    match &state.hidden {
        None => controls.push(control(
            "add-network",
            "Add network",
            "One that doesn't show its name",
            ControlKind::Text { secret: false },
        )),
        Some(ssid) => {
            controls.push(heading("wifi-add", &format!("Add network “{ssid}”")));
            controls.push(control(
                "add-password",
                "Password",
                "",
                ControlKind::Text { secret: true },
            ));
            controls.push(control(
                "add-open",
                "Connect without password",
                "",
                ControlKind::Action,
            ));
            controls.push(control("add-cancel", "Cancel", "", ControlKind::Action));
        }
    }
    controls
}

/// A wired interface as NetworkManager sees it, from
/// `nmcli -t -f DEVICE,TYPE,STATE,CONNECTION device`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Wired {
    device: String,
    /// Not switched off ("disconnected" is NetworkManager's off: the
    /// device doesn't connect by itself until switched on).
    on: bool,
    /// The connection's name while connected.
    connection: Option<String>,
}

/// The Ethernet devices NetworkManager manages.
fn parse_wired(text: &str) -> Vec<Wired> {
    text.lines()
        .map(split_terse)
        .filter_map(|fields| match fields.as_slice() {
            [device, kind, state, connection] if kind == "ethernet" => {
                let state = state.split_whitespace().next().unwrap_or_default();
                if state == "unmanaged" {
                    return None;
                }
                Some(Wired {
                    device: device.clone(),
                    on: state != "disconnected",
                    connection: (state == "connected" && !connection.is_empty())
                        .then(|| connection.clone()),
                })
            }
            _ => None,
        })
        .collect()
}

/// The wired interfaces, each like Wi-Fi: a heading, its switch (where
/// NetworkManager manages it, `states`), and while on, its connection
/// with its state; OK there shows the details.
fn interface_rows(
    interfaces: &[net::Details],
    states: &[Wired],
    open: Option<&str>,
) -> Vec<Control> {
    let mut controls = Vec::new();
    for interface in interfaces {
        let title = if interfaces
            .iter()
            .filter(|other| other.kind == interface.kind)
            .count()
            > 1
        {
            format!("{} ({})", interface.kind, interface.name)
        } else {
            interface.kind.to_owned()
        };
        controls.push(heading(
            &format!("wired-heading:{}", interface.name),
            &title,
        ));
        let managed = states.iter().find(|state| state.device == interface.name);
        if let Some(managed) = managed {
            controls.push(control(
                &format!("{WIRED_SWITCH}{}", interface.name),
                interface.kind,
                "",
                ControlKind::Toggle { on: managed.on },
            ));
            if !managed.on {
                continue;
            }
        }
        let id = format!("{WIRED}{}", interface.name);
        let label = managed
            .and_then(|managed| managed.connection.clone())
            .unwrap_or_else(|| interface.name.clone());
        controls.push(control(&id, &label, interface.state, ControlKind::Action));
        if open == Some(id.as_str()) {
            controls.extend(interface.facts.iter().cloned());
        }
    }
    controls
}

/// What the Bluetooth tab is built from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct BluetoothState {
    on: bool,
    /// (address, name).
    paired: Vec<(String, String)>,
    /// Addresses.
    connected: Vec<String>,
    /// Every device known after a search, paired ones too; `None` before.
    found: Option<Vec<(String, String)>>,
    /// The row whose details show.
    open: Option<String>,
    /// Facts about the device whose details show ([`parse_bt_info`]).
    info: Vec<(String, String)>,
}

/// The Bluetooth rows: the switch, the connected devices, the paired
/// ones that aren't (OK on either: its details, Connect or Disconnect,
/// Forget), the ones found nearby (OK pairs) and Search.
fn bluetooth_rows(state: &BluetoothState) -> Vec<Control> {
    let mut controls = vec![control(
        "bluetooth",
        "Bluetooth",
        "",
        ControlKind::Toggle { on: state.on },
    )];
    if !state.on {
        return controls;
    }
    let open = state.open.as_deref();
    for up in [true, false] {
        let devices: Vec<&(String, String)> = state
            .paired
            .iter()
            .filter(|(address, _)| state.connected.contains(address) == up)
            .collect();
        if devices.is_empty() {
            continue;
        }
        controls.push(if up {
            heading("bt-connected", "Connected devices")
        } else {
            heading("bt-paired", "Paired devices")
        });
        for (address, name) in devices {
            let id = format!("{BT}{address}");
            controls.push(control(
                &id,
                name,
                if up { "Connected" } else { "Not connected" },
                ControlKind::Action,
            ));
            if open != Some(id.as_str()) {
                continue;
            }
            for (n, (label, value)) in state.info.iter().enumerate() {
                controls.push(control(
                    &format!("bt-info:{n}"),
                    label,
                    value,
                    ControlKind::Info,
                ));
            }
            controls.push(control("bt-address", "Address", address, ControlKind::Info));
            controls.push(if up {
                control(
                    &format!("{BT_DISCONNECT}{address}"),
                    "Disconnect",
                    "",
                    ControlKind::Action,
                )
            } else {
                control(
                    &format!("{BT_CONNECT}{address}"),
                    "Connect",
                    "",
                    ControlKind::Action,
                )
            });
            controls.push(control(
                &format!("{BT_FORGET}{address}"),
                "Forget device",
                "",
                ControlKind::Action,
            ));
        }
    }
    controls.push(heading("bt-available", "Available devices"));
    match &state.found {
        Some(found) => {
            let new: Vec<&(String, String)> = found
                .iter()
                .filter(|(address, _)| !state.paired.iter().any(|(other, _)| other == address))
                .collect();
            if new.is_empty() {
                controls.push(control(
                    "bt-none",
                    "No new devices found",
                    "",
                    ControlKind::Info,
                ));
            }
            for (address, name) in new {
                controls.push(control(
                    &format!("{PAIR}{address}"),
                    name,
                    "Pair",
                    ControlKind::Action,
                ));
            }
            controls.push(control("bt-scan", "Search again", "", ControlKind::Action));
        }
        None => controls.push(control(
            "bt-scan",
            "Search for devices",
            "Puts nothing in pairing mode; do that on the device",
            ControlKind::Action,
        )),
    }
    controls
}

/// `bluetoothctl info`: the device's kind and battery, as (label, value).
fn parse_bt_info(text: &str) -> Vec<(String, String)> {
    let mut facts = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(icon) = line.strip_prefix("Icon:") {
            let kind = match icon.trim() {
                "audio-headset" | "audio-headphones" => "Headphones",
                "audio-card" => "Speaker",
                "input-gaming" => "Game controller",
                "input-keyboard" => "Keyboard",
                "input-mouse" => "Mouse",
                "input-tablet" => "Tablet",
                "phone" => "Phone",
                "computer" => "Computer",
                _ => continue,
            };
            facts.push(("Type".to_owned(), kind.to_owned()));
        } else if let Some(battery) = line.strip_prefix("Battery Percentage:") {
            // "0x64 (100)"
            if let Some(percent) = battery
                .split_once('(')
                .and_then(|(_, rest)| rest.strip_suffix(')'))
            {
                facts.push(("Battery".to_owned(), format!("{percent} %")));
            }
        }
    }
    facts
}

/// The names of saved Wi-Fi connections, from
/// `nmcli -t -f NAME,TYPE connection show`.
fn parse_saved(text: &str) -> Vec<String> {
    text.lines()
        .map(split_terse)
        .filter_map(|fields| match fields.as_slice() {
            [name, kind] if kind == "802-11-wireless" => Some(name.clone()),
            _ => None,
        })
        .collect()
}

/// `bluetoothctl devices`: "Device AA:BB:CC:DD:EE:FF Name" lines, as
/// (address, name); a device without a name shows its address.
fn parse_bt_devices(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("Device ")?;
            let (address, name) = rest.split_once(' ').unwrap_or((rest, rest));
            Some((address.to_owned(), name.trim().to_owned()))
        })
        .collect()
}

/// `bluetoothctl show`: "Powered: yes"; `None` without a controller.
fn parse_powered(text: &str) -> Option<bool> {
    text.lines()
        .find_map(|line| line.trim().strip_prefix("Powered:"))
        .map(|value| value.trim() == "yes")
}

/// `PRETTY_NAME` (else `NAME`) from os-release.
fn os_name(text: &str) -> Option<String> {
    let value = |key: &str| {
        text.lines().find_map(|line| {
            let value = line.strip_prefix(key)?.strip_prefix('=')?;
            let value = value.trim().trim_matches(['"', '\'']);
            (!value.is_empty()).then(|| value.to_owned())
        })
    };
    value("PRETTY_NAME").or_else(|| value("NAME"))
}

/// `MemTotal` from /proc/meminfo, as "1.0 GB" (GiB) or "512 MB" (MiB).
fn memory_total(text: &str) -> Option<String> {
    let kib: u64 = text
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    let mib = kib / 1024;
    Some(if mib >= 1024 {
        format!("{:.1} GB", mib as f64 / 1024.0)
    } else {
        format!("{mib} MB")
    })
}

/// Whether a desktop entry with this file name is installed in one of the
/// data directories' `applications`.
pub(super) fn has_desktop_entry(data_dirs: &[std::path::PathBuf], file: &str) -> bool {
    data_dirs
        .iter()
        .any(|dir| Path::new(dir).join("applications").join(file).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_wpctl_volumes() {
        assert_eq!(
            parse_volume("Volume: 0.40\n"),
            Some(Volume {
                level: 0.4,
                muted: false
            })
        );
        assert_eq!(
            parse_volume("Volume: 1.00 [MUTED]").map(|v| v.muted),
            Some(true)
        );
        assert_eq!(parse_volume("Error"), None);
    }

    #[test]
    fn reads_the_audio_sinks_of_wpctl_status() {
        let status = "PipeWire 'pipewire-0' [1.4.2, root@tvbox, cookie:1]\n \
                      └─ Clients:\n        31. WirePlumber [1.4.2, root@tvbox, pid:500]\n\n\
                      Audio\n \
                      ├─ Devices:\n \
                      │      42. Built-in Audio                      [alsa]\n \
                      │  \n \
                      ├─ Sinks:\n \
                      │  *   48. Built-in Audio Analog Stereo        [vol: 0.40]\n \
                      │      52. HDMI / DisplayPort                  [vol: 1.00 MUTED]\n \
                      │  \n \
                      ├─ Sources:\n \
                      │      49. Built-in Audio Analog Stereo        [vol: 1.00]\n\n\
                      Video\n \
                      ├─ Sinks:\n \
                      │      70. Not audio\n";
        assert_eq!(
            parse_sinks(status),
            [
                Sink {
                    id: 48,
                    name: "Built-in Audio Analog Stereo".into(),
                    default: true
                },
                Sink {
                    id: 52,
                    name: "HDMI / DisplayPort".into(),
                    default: false
                },
            ]
        );
    }

    #[test]
    fn reads_nmcli_networks_with_escaped_colons() {
        assert_eq!(split_terse(r"a\:b:c\\d:"), ["a:b", r"c\d", ""]);
        let list = " :Neighbour:30:WPA2\n*:Home:80:WPA2\n :Home:60:WPA2\n ::90:\n :Cafe:55:\n";
        let networks = parse_networks(list);
        let names: Vec<&str> = networks.iter().map(|n| n.ssid.as_str()).collect();
        assert_eq!(names, ["Home", "Cafe", "Neighbour"]);
        assert!(networks[0].in_use);
        assert_eq!(networks[0].signal, 80);
        assert_eq!(networks[1].security, "");
    }

    fn rows(controls: &[Control]) -> Vec<(String, String, String)> {
        controls
            .iter()
            .map(|control| {
                let kind = match control.kind {
                    ControlKind::Heading => "heading",
                    ControlKind::Toggle { .. } => "toggle",
                    ControlKind::Action => "action",
                    ControlKind::Text { secret: true } => "password",
                    ControlKind::Text { secret: false } => "text",
                    ControlKind::Info => "info",
                    _ => "other",
                };
                (
                    kind.to_owned(),
                    control.label.clone(),
                    control.detail.clone(),
                )
            })
            .collect()
    }

    fn row(kind: &str, label: &str, detail: &str) -> (String, String, String) {
        (kind.to_owned(), label.to_owned(), detail.to_owned())
    }

    fn wifi_state() -> WifiState {
        let network = |ssid: &str, security: &str, in_use: bool, signal: u8| Network {
            in_use,
            ssid: ssid.to_owned(),
            signal,
            security: security.to_owned(),
        };
        WifiState {
            on: true,
            networks: vec![
                network("Home", "WPA2", true, 80),
                network("Office", "WPA2", false, 60),
                network("Cafe", "", false, 55),
                network("Next door", "WPA2 WPA3", false, 40),
            ],
            saved: parse_saved(
                "Home:802-11-wireless\nWired:802-3-ethernet\nOffice:802-11-wireless\n\
                 Holiday:802-11-wireless\n",
            ),
            details: vec![
                control("net:wlan0:0", "Network", "Home", ControlKind::Info),
                control("net:wlan0:1", "IPv4", "192.168.1.125/24", ControlKind::Info),
            ],
            open: None,
            hidden: None,
        }
    }

    #[test]
    fn wifi_rows_follow_how_a_connection_goes() {
        let state = wifi_state();
        assert_eq!(
            rows(&wifi_rows(&state)),
            [
                row("toggle", "Wi-Fi", ""),
                row("heading", "Connected", ""),
                row("action", "Home", "Connected · 80 % · WPA2"),
                row("heading", "Available networks", ""),
                row("action", "Cafe", "Open · 55 %"),
                row("password", "Next door", "WPA2 WPA3 · 40 %"),
                row("action", "Search again", ""),
                row("heading", "Saved networks", ""),
                row("action", "Office", "In range · 60 %"),
                row("action", "Holiday", "Not in range"),
                row("text", "Add network", "One that doesn't show its name"),
            ]
        );
        assert_eq!(
            rows(&wifi_rows(&WifiState::default())),
            [row("toggle", "Wi-Fi", "")]
        );
    }

    #[test]
    fn ok_on_a_network_shows_its_details_and_actions() {
        let mut state = wifi_state();
        state.open = Some(format!("{WIFI}Home"));
        let controls = wifi_rows(&state);
        let shown: Vec<_> = rows(&controls)[2..6].to_vec();
        assert_eq!(
            shown,
            [
                row("action", "Home", "Connected · 80 % · WPA2"),
                row("info", "IPv4", "192.168.1.125/24"),
                row("action", "Disconnect", ""),
                row("action", "Forget network", ""),
            ]
        );
        assert!(controls.iter().any(|c| c.id == format!("{DISCONNECT}Home")));
        state.open = Some(format!("{WIFI}Holiday"));
        let controls = wifi_rows(&state);
        let ids: Vec<&str> = controls.iter().map(|c| c.id.as_str()).collect();
        assert!(ids.contains(&"forget:Holiday"));
        assert!(!ids.contains(&"connect:Holiday"), "not in range");
        state.open = Some(format!("{WIFI}Office"));
        let ids: Vec<String> = wifi_rows(&state).into_iter().map(|c| c.id).collect();
        assert!(ids.contains(&"connect:Office".to_owned()));
    }

    #[test]
    fn add_network_asks_for_the_name_then_the_password() {
        let mut state = wifi_state();
        state.hidden = Some("Attic".to_owned());
        let controls = rows(&wifi_rows(&state));
        assert_eq!(
            controls[controls.len() - 4..],
            [
                row("heading", "Add network “Attic”", ""),
                row("password", "Password", ""),
                row("action", "Connect without password", ""),
                row("action", "Cancel", ""),
            ]
        );
    }

    #[test]
    fn control_ids_are_unique() {
        let mut state = wifi_state();
        state.open = Some(format!("{WIFI}Home"));
        let mut controls = wifi_rows(&state);
        controls.extend(bluetooth_rows(&bluetooth_state()));
        let mut ids: Vec<&str> = controls.iter().map(|c| c.id.as_str()).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count);
    }

    fn wired(name: &str, state: &'static str) -> net::Details {
        net::Details {
            name: name.to_owned(),
            kind: "Ethernet",
            wifi: false,
            state,
            facts: vec![control(
                &format!("net:{name}:0"),
                "Link speed",
                "1000 Mbit/s",
                ControlKind::Info,
            )],
        }
    }

    #[test]
    fn ethernet_has_a_switch_and_its_connection_like_wifi() {
        let states = parse_wired(
            "eth0:ethernet:connected:Wired connection 1\neth1:ethernet:disconnected:\n\
             usb0:ethernet:unmanaged:\nwlan0:wifi:connected:Home\nlo:loopback:connected (externally):lo\n",
        );
        assert_eq!(
            states,
            [
                Wired {
                    device: "eth0".to_owned(),
                    on: true,
                    connection: Some("Wired connection 1".to_owned()),
                },
                Wired {
                    device: "eth1".to_owned(),
                    on: false,
                    connection: None,
                },
            ]
        );
        let interfaces = [
            wired("eth0", "Connected"),
            wired("eth1", "No cable"),
            wired("usb0", "Connected"),
        ];
        let open = format!("{WIRED}eth0");
        assert_eq!(
            rows(&interface_rows(&interfaces, &states, Some(&open))),
            [
                row("heading", "Ethernet (eth0)", ""),
                row("toggle", "Ethernet", ""),
                row("action", "Wired connection 1", "Connected"),
                row("info", "Link speed", "1000 Mbit/s"),
                row("heading", "Ethernet (eth1)", ""),
                row("toggle", "Ethernet", ""),
                // Unmanaged: no switch, just its state.
                row("heading", "Ethernet (usb0)", ""),
                row("action", "usb0", "Connected"),
            ]
        );
    }

    fn bluetooth_state() -> BluetoothState {
        BluetoothState {
            on: true,
            paired: vec![
                ("AA:AA".to_owned(), "Remote".to_owned()),
                ("BB:BB".to_owned(), "Headphones".to_owned()),
            ],
            connected: vec!["AA:AA".to_owned()],
            found: None,
            open: None,
            info: Vec::new(),
        }
    }

    #[test]
    fn bluetooth_rows_group_connected_paired_and_found_devices() {
        let mut state = bluetooth_state();
        assert_eq!(
            rows(&bluetooth_rows(&state)),
            [
                row("toggle", "Bluetooth", ""),
                row("heading", "Connected devices", ""),
                row("action", "Remote", "Connected"),
                row("heading", "Paired devices", ""),
                row("action", "Headphones", "Not connected"),
                row("heading", "Available devices", ""),
                row(
                    "action",
                    "Search for devices",
                    "Puts nothing in pairing mode; do that on the device"
                ),
            ]
        );
        state.found = Some(vec![
            ("AA:AA".to_owned(), "Remote".to_owned()),
            ("CC:CC".to_owned(), "Pad".to_owned()),
        ]);
        state.open = Some(format!("{BT}BB:BB"));
        state.info =
            parse_bt_info("Device BB:BB\n\tIcon: audio-headset\n\tBattery Percentage: 0x50 (80)\n");
        assert_eq!(
            rows(&bluetooth_rows(&state))[4..],
            [
                row("action", "Headphones", "Not connected"),
                row("info", "Type", "Headphones"),
                row("info", "Battery", "80 %"),
                row("info", "Address", "BB:BB"),
                row("action", "Connect", ""),
                row("action", "Forget device", ""),
                row("heading", "Available devices", ""),
                row("action", "Pad", "Pair"),
                row("action", "Search again", ""),
            ]
        );
        state.on = false;
        assert_eq!(bluetooth_rows(&state).len(), 1);
    }

    #[test]
    fn reads_bluetooth_devices() {
        let text =
            "Device AA:BB:CC:DD:EE:FF Bluetooth remote\nDevice 11:22:33:44:55:66\nController x\n";
        assert_eq!(
            parse_bt_devices(text),
            [
                (
                    "AA:BB:CC:DD:EE:FF".to_owned(),
                    "Bluetooth remote".to_owned()
                ),
                (
                    "11:22:33:44:55:66".to_owned(),
                    "11:22:33:44:55:66".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn reads_the_network_interfaces_with_their_signal() {
        let devices = "wlan0:wifi:connected\nwlan1:wifi:disconnected\n\
                       enx00e04c:ethernet:unavailable\neth0:ethernet:connected (externally)\n\
                       lo:loopback:connected (externally)\nusb9:ethernet:unmanaged\n\
                       p2p-dev-wlan0:wifi-p2p:disconnected\n";
        let signals = " :90:wlan0\n*:52:wlan0\n*:99:wlan1\n";
        assert_eq!(
            parse_links(devices, signals),
            [
                Link::Wifi(Some(3)),
                Link::Wifi(None),
                Link::Wired(false),
                Link::Wired(true),
            ]
        );
        // Connected but not in the last scan: full bars.
        assert_eq!(
            parse_links("wlan0:wifi:connected", ""),
            [Link::Wifi(Some(4))]
        );
    }

    #[test]
    fn reads_bluetooth_os_release_and_memory() {
        assert_eq!(
            parse_powered("Controller 00:11 (public)\n\tPowered: yes\n"),
            Some(true)
        );
        assert_eq!(parse_powered("No default controller available\n"), None);
        assert_eq!(
            os_name("NAME=\"Debian GNU/Linux\"\nPRETTY_NAME=\"Debian GNU/Linux 13 (trixie)\"\n"),
            Some("Debian GNU/Linux 13 (trixie)".into())
        );
        assert_eq!(os_name("NAME=Arch\n"), Some("Arch".into()));
        assert_eq!(
            memory_total("MemTotal:        1003296 kB\nMemFree: 1 kB\n"),
            Some("979 MB".into())
        );
        assert_eq!(
            memory_total("MemTotal:        2097152 kB\n"),
            Some("2.0 GB".into())
        );
    }

    #[test]
    fn reads_another_system_to_restart_into() {
        let other = parse_other_os(
            "label = \"Restart into Android\"\ncommand = [\"systemctl\", \"start\", \"x.service\"]\n",
        );
        assert_eq!(
            other,
            Some(OtherOs {
                label: "Restart into Android".into(),
                command: vec!["systemctl".into(), "start".into(), "x.service".into()],
            })
        );
        assert_eq!(parse_other_os("label = \"A\"\ncommand = []\n"), None);
        assert_eq!(parse_other_os("label = \"A\"\nshell = \"rm\"\n"), None);
        let device = LinuxDevice {
            systemctl: true,
            other_os: other,
            ..LinuxDevice::default()
        };
        let power = device.panel(QuickPanel::Power).expect("a Power panel");
        let ids: Vec<&str> = power.controls.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["reboot", "other-os", "poweroff"]);
    }

    #[test]
    fn a_missing_tool_hides_its_panel_and_fails_to_run() {
        let device = LinuxDevice::default();
        assert!(!device.has(QuickPanel::Audio));
        assert_eq!(device.panel(QuickPanel::Power), None);
        assert_eq!(device.volume(VolumeChange::Up), None);
        assert!(run("rusttv-no-such-tool", &[]).is_err());
        assert_eq!(run("echo", &["hi"]).as_deref(), Ok("hi\n"));
    }
}
