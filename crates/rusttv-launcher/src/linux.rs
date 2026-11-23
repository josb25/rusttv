//! Linux platform: the apps of the XDG desktop entries, started as child
//! processes (feature `linux`).
//!
//! Apps are the `Type=Application` entries under `applications/` in
//! `$XDG_DATA_HOME` and `$XDG_DATA_DIRS` (Desktop Entry Specification 1.5),
//! so packages and Flatpaks show up without extra work. Icons come from the
//! `hicolor` theme, `pixmaps`, or the Breeze and Adwaita themes (PNG, else
//! SVG). Updates are the system package
//! manager's job, so the launcher's updater does nothing here.

mod audio;
mod device;
mod device_file;
mod gamepad;
mod keyboard;
mod overlay;
mod portal;
mod tasks;
mod toplevels;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::io::BufReader;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::SystemTime;

pub use device::LinuxDevice;
pub use gamepad::start as start_gamepads;
pub use keyboard::start as start_keyboard;
pub use portal::run as run_portal;
pub use tasks::Limits;
use tasks::Tasks;
use toplevels::Toplevels;

use crate::device::{DeviceSettings, QuickPanel};
use crate::model::{App, Category, QuickAction};
use crate::platform::{AppEntry, Artwork, Bitmap, Platform, PlatformError, UpdateStatus};

/// What happened to an app the launcher started (by desktop file ID).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppEvent {
    /// The app's process started; its window follows.
    Started(String),
    /// The app's process ended.
    Exited(String),
    /// A source started sending a picture to the device's video input
    /// (`device_file`'s `[[input]]`); arrives on the input's watcher thread.
    InputSignal,
}

/// Receives [`AppEvent`]s; `Exited` arrives on a waiter thread.
pub type AppEvents = Arc<dyn Fn(AppEvent) + Send + Sync>;

/// How long an app may take to show its window (Kodi on a slow TV box takes
/// about ten seconds); after that the launch counts as failed.
const LAUNCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Icon sizes to try, best first: big enough for a tile, small enough to keep
/// the decoded picture small.
const ICON_SIZES: [&str; 7] = [
    "128x128", "256x256", "96x96", "192x192", "512x512", "64x64", "48x48",
];

/// Where SVG app icons are, under a data dir: hicolor, KDE Breeze / Breeze
/// Dark (apps, places, devices, categories), GNOME Adwaita, and Papirus.
const SVG_ICON_DIRS: [&str; 22] = [
    "icons/hicolor/scalable/apps",
    "icons/hicolor/scalable/places",
    "icons/hicolor/scalable/devices",
    "icons/hicolor/scalable/categories",
    "icons/breeze/apps/64",
    "icons/breeze/apps/48",
    "icons/breeze/apps/32",
    "icons/breeze/places/64",
    "icons/breeze/places/48",
    "icons/breeze/devices/64",
    "icons/breeze/devices/48",
    "icons/breeze/categories/48",
    "icons/breeze-dark/apps/64",
    "icons/breeze-dark/apps/48",
    "icons/breeze-dark/apps/32",
    "icons/breeze-dark/places/64",
    "icons/breeze-dark/places/48",
    "icons/breeze-dark/devices/48",
    "icons/Adwaita/scalable/apps",
    "icons/Adwaita/scalable/places",
    "icons/Adwaita/scalable/devices",
    "icons/Papirus/64x64/apps",
];

/// Icon theme subdirectories checked for PNG icons under hicolor.
const PNG_ICON_SUBDIRS: [&str; 4] = ["apps", "places", "devices", "categories"];

/// The longer side, in pixels, SVG icons are drawn at (icon cards are 72 rpx,
/// so 128 px stays sharp at 1080p/4K while cutting rasterization work 4×).
const SVG_PIXELS: f32 = 128.0;

/// KDE Connect's desktop entry, opened by its entry in the top-right menu.
const KDE_CONNECT: &str = "org.kde.kdeconnect.app.desktop";

/// How long the video input's watcher waits before starting its command
/// again after it ended.
const WATCH_RESTART: std::time::Duration = std::time::Duration::from_secs(30);

/// Runs the video input's `watch` command on a thread of its own, again
/// whenever it ends, keeps [`LinuxDevice`] updated on live HDMI signal state,
/// and sends [`AppEvent::InputSignal`] on configured startup HDMI or newly
/// arriving HDMI signals.
fn watch_input(command: Vec<String>, device: Arc<LinuxDevice>, events: AppEvents) {
    let startup = device.startup_input_target();
    let startup_always = matches!(startup, Some((_, false)));
    let startup_on_signal = matches!(startup, Some((_, true)));

    if startup_always {
        let events_boot = events.clone();
        let _ = std::thread::Builder::new()
            .name("rusttv-input-boot".into())
            .spawn(move || {
                // Let the Slint window and compositor settle briefly, then open HDMI.
                std::thread::sleep(std::time::Duration::from_millis(350));
                log::info!("startup input configured: opening HDMI automatically");
                events_boot(AppEvent::InputSignal);
            });
    }

    if command.is_empty() {
        return;
    }

    let started = std::thread::Builder::new()
        .name("rusttv-input-watch".into())
        .spawn(move || {
            use std::io::BufRead as _;
            let Some((program, args)) = command.split_first() else {
                return;
            };
            let mut signal: Option<bool> = None;
            let mut seen_non_absent = false;
            loop {
                let child = std::process::Command::new(program)
                    .args(args)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .spawn();
                match child {
                    Ok(mut child) => {
                        if let Some(out) = child.stdout.take() {
                            for line in std::io::BufReader::new(out).lines() {
                                let Ok(line) = line else { break };
                                log::info!("video input: {line}");
                                let has_sig = device_file::has_signal(&line);
                                device.set_hdmi_status(has_sig, &line);
                                let auto = device.auto_switch_hdmi();
                                let (now, switch) = input_change_with_policy(
                                    signal,
                                    &line,
                                    seen_non_absent,
                                    startup_on_signal && !startup_always,
                                    auto,
                                );
                                if !device_file::is_absent(&line) {
                                    seen_non_absent = true;
                                }
                                signal = now;
                                if switch {
                                    events(AppEvent::InputSignal);
                                }
                            }
                        }
                        let status = child.wait();
                        log::warn!("{program} ended ({status:?}); again in {WATCH_RESTART:?}");
                    }
                    Err(error) => {
                        log::warn!("{program} failed: {error}; again in {WATCH_RESTART:?}")
                    }
                }
                std::thread::sleep(WATCH_RESTART);
            }
        });
    if let Err(error) = started {
        log::warn!("no thread to watch the video input: {error}");
    }
}

/// Evaluates an HDMI watcher status line with startup & auto-switch policy:
/// - On the very first non-`absent` line, switches only when `startup_on_signal`
///   is true (the user opted into "HDMI when signal detected" on boot).
/// - Afterward, when `auto_switch` is true, switches reliably both on
///   `no-signal`/`unsupported` → `signal` AND when the HDMI service finishes
///   starting (`absent` → `signal`).
fn input_change_with_policy(
    before: Option<bool>,
    line: &str,
    seen_non_absent: bool,
    startup_on_signal: bool,
    auto_switch: bool,
) -> (Option<bool>, bool) {
    if device_file::is_absent(line) {
        return (None, false);
    }
    let now = device_file::has_signal(line);
    let switch = if !seen_non_absent {
        now && startup_on_signal
    } else if !auto_switch {
        false
    } else {
        now && before != Some(true)
    };
    (Some(now), switch)
}

/// A line of the video input's watcher after `before` (whether a picture
/// arrived when last heard, `None` at start or while the input was
/// unknown): whether one arrives now (`None`: unknown, "absent"), and
/// whether to switch to it (only when it newly arrives after a line that
/// said none did; a picture already there when the input becomes known
/// doesn't switch, e.g. while the device starts).
#[cfg_attr(not(test), allow(dead_code, reason = "kept for policy baseline tests"))]
fn input_change(before: Option<bool>, line: &str) -> (Option<bool>, bool) {
    if device_file::is_absent(line) {
        return (None, false);
    }
    let now = device_file::has_signal(line);
    (Some(now), now && before == Some(false))
}

/// Where packages put the launcher's licence notices.
const NOTICES: &str = "/usr/share/doc/rusttv-launcher/NOTICES";

/// [`Platform`] over the XDG desktop entries of this machine.
pub struct LinuxPlatform {
    data_dirs: Vec<PathBuf>,
    desktops: Vec<String>,
    language: Option<String>,
    events: Option<AppEvents>,
    entries: RefCell<HashMap<String, DesktopEntry>>,
    seen: RefCell<Vec<Option<SystemTime>>>,
    tasks: Option<Arc<Tasks>>,
    windows: Option<Toplevels>,
    /// Audio, networks and power through the system's tools.
    device: Arc<LinuxDevice>,
    /// Each app's size on screen (`with_sizes`).
    sizes: Option<AppSizes>,
    /// The app overlay last opened (holding Home over an app).
    overlay: Option<overlay::Overlay>,
    /// The volume bar over apps, once a volume key was pressed over one.
    volume_bar: Option<overlay::VolumeBar>,
    /// The local time zone's offset from UTC as last read, and when: read
    /// again after [`UTC_OFFSET_FOR`], so a summer-time change shows within
    /// the hour without a `date` run for every clock tick or history row.
    utc_offset: RefCell<Option<(SystemTime, i32)>>,
    /// The test pattern, once a control that shows it moved.
    pattern: Option<overlay::PatternOverlay>,
    /// Whether `flatpak` is available on PATH (for uninstalling Flatpak apps).
    has_flatpak: bool,
    /// Whether non-interactive `apt-get remove` is available (root or
    /// passwordless sudo + dpkg) for uninstalling system `.desktop` packages.
    apt_remove: bool,
}

/// How long a read UTC offset is used ([`LinuxPlatform::utc_offset_secs`]).
const UTC_OFFSET_FOR: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// The offset `date +%z` prints ("+0200", "-0330"), in seconds.
fn parse_utc_offset(text: &str) -> Option<i32> {
    let text = text.trim();
    let (sign, digits) = match text.split_at_checked(1)? {
        ("+", digits) => (1, digits),
        ("-", digits) => (-1, digits),
        _ => return None,
    };
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hours: i32 = digits[..2].parse().ok()?;
    let minutes: i32 = digits[2..].parse().ok()?;
    let offset = sign * (hours * 3600 + minutes * 60);
    // Real offsets run from -12:00 to +14:00.
    (minutes < 60 && (-12 * 3600..=14 * 3600).contains(&offset)).then_some(offset)
}

impl fmt::Debug for LinuxPlatform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinuxPlatform")
            .field("data_dirs", &self.data_dirs)
            .field("desktops", &self.desktops)
            .field("language", &self.language)
            .finish_non_exhaustive()
    }
}

impl LinuxPlatform {
    /// Reads the XDG directories, `XDG_CURRENT_DESKTOP` and the message
    /// language from the environment.
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
        let home = var("HOME").map(PathBuf::from);
        let data_home = var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| home.map(|home| home.join(".local/share")));
        let data_dirs =
            var("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into());
        let mut dirs: Vec<PathBuf> = data_home.into_iter().collect();
        dirs.extend(
            data_dirs
                .split(':')
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from),
        );
        let desktops = var("XDG_CURRENT_DESKTOP")
            .map(|value| value.split(':').map(str::to_owned).collect())
            .unwrap_or_default();
        let mut platform = Self::new(dirs, desktops, ui_language());
        platform.has_flatpak = has_program("flatpak");
        platform.apt_remove = detect_apt_remove();
        platform
    }

    /// A platform over these data directories (most important first), desktop
    /// names and POSIX locale (e.g. `de_DE.UTF-8`).
    pub fn new(data_dirs: Vec<PathBuf>, desktops: Vec<String>, language: Option<String>) -> Self {
        Self {
            data_dirs,
            desktops,
            language,
            events: None,
            entries: RefCell::default(),
            seen: RefCell::default(),
            tasks: None,
            windows: None,
            device: Arc::new(LinuxDevice::detect()),
            sizes: None,
            overlay: None,
            volume_bar: None,
            utc_offset: RefCell::default(),
            pattern: None,
            has_flatpak: false,
            apt_remove: false,
        }
    }

    /// Lets each app have its own size on screen, kept in `path` (TOML).
    /// Qt apps get it as `QT_SCALE_FACTOR`, GTK apps as `GDK_DPI_SCALE`
    /// (their text; GTK scales the rest only by whole factors); other
    /// toolkits ignore it.
    #[must_use]
    pub fn with_sizes(mut self, path: PathBuf) -> Self {
        self.sizes = Some(AppSizes::load(path));
        self
    }

    /// Manages the apps it starts: opening a running app brings its window to
    /// the front instead of starting it again, and `limits` caps how many
    /// run. Needs a compositor with `wlr-foreign-toplevel-management` to find
    /// the windows.
    #[must_use]
    pub fn with_tasks(mut self, limits: Limits) -> Self {
        let tasks = Tasks::start(limits);
        let used = tasks.clone();
        self.windows = Toplevels::start(Box::new(move |app_id, open| {
            use std::sync::atomic::Ordering;
            let Some(app_id) = app_id else {
                used.window_closed(open);
                return;
            };
            // The home screen's own window has no app ID.
            let home = app_id.is_empty() || app_id.to_lowercase().contains("rusttv");
            gamepad::HOME_IN_FRONT.store(home, Ordering::Relaxed);
            // The launcher's own fields are opened on purpose.
            if !home {
                keyboard::window_switched();
            }
            let reads_pads = used.activated(app_id, open);
            gamepad::KEYS_TO_APP.store(reads_pads == Some(false), Ordering::Relaxed);
        }));
        self.tasks = Some(tasks);
        self
    }

    /// Reports app starts and exits to `events`, e.g. to pause and resume
    /// the home screen.
    #[must_use]
    pub fn with_events(mut self, events: AppEvents) -> Self {
        if let Some(input) = self.device.input() {
            watch_input(input.watch, Arc::clone(&self.device), events.clone());
        }
        self.events = Some(events);
        self
    }

    /// The desktop file ID of the app that shows the video input, if the
    /// device has one and the app is installed.
    fn input_app(&self) -> Option<String> {
        let input = self.device.input()?;
        device::has_desktop_entry(&self.data_dirs, &input.app).then_some(input.app)
    }

    /// Makes app `id`, running but not started by this launcher, one of its
    /// apps: found by its program in its own process group (as the launcher
    /// starts apps), and followed until it ends (a pidfd: it is no child
    /// to wait for). Whether it was found.
    fn adopt(&self, id: &str, entry: &DesktopEntry) -> bool {
        let Some(tasks) = &self.tasks else {
            return false;
        };
        let Some(program) = entry
            .argv()
            .first()
            .and_then(|program| Path::new(program).file_name())
            .map(|name| name.to_string_lossy().into_owned())
        else {
            return false;
        };
        // Never a group it already follows (two entries of one program).
        let Some(group) = tasks::find_group(&program, &tasks.groups()) else {
            return false;
        };
        let Some(leader) = i32::try_from(group)
            .ok()
            .and_then(rustix::process::Pid::from_raw)
        else {
            return false;
        };
        let pidfd = match rustix::process::pidfd_open(leader, rustix::process::PidfdFlags::empty())
        {
            Ok(pidfd) => pidfd,
            Err(error) => {
                log::warn!("following {id} (pid {group}) failed: {error}");
                return false;
            }
        };
        log::info!("{id} was already running (pid {group}); taken over");
        tasks.started(id, group, entry.window_keys(id), entry.reads_pads(id));
        let events = self.events.clone();
        let tasks = self.tasks.clone();
        let name = id.to_owned();
        let waiter = std::thread::Builder::new()
            .name("rusttv-app-wait".into())
            .spawn(move || {
                use rustix::event::{PollFd, PollFlags, poll};
                // Readable once the process has ended.
                let mut fds = [PollFd::new(&pidfd, PollFlags::IN)];
                while let Err(error) = poll(&mut fds, None) {
                    if error != rustix::io::Errno::INTR {
                        log::warn!("waiting for {name} failed: {error}");
                        break;
                    }
                }
                log::info!("{name} ended");
                if let Some(tasks) = tasks {
                    tasks.ended(group);
                }
                if let Some(events) = events {
                    events(AppEvent::Exited(name));
                }
            });
        if let Err(error) = waiter {
            log::warn!("no thread to wait for {id}: {error}");
        }
        true
    }

    fn application_dirs(&self) -> impl Iterator<Item = PathBuf> + '_ {
        self.data_dirs.iter().map(|dir| dir.join("applications"))
    }

    /// Modification times of the application directories: installing or
    /// removing a desktop entry changes its directory's.
    fn stamps(&self) -> Vec<Option<SystemTime>> {
        self.application_dirs()
            .map(|dir| fs::metadata(dir).and_then(|meta| meta.modified()).ok())
            .collect()
    }

    /// Every visible desktop entry by desktop file ID; an ID found in an
    /// earlier data directory hides later ones.
    fn scan(&self) -> HashMap<String, DesktopEntry> {
        let mut found = HashMap::new();
        let mut taken = HashSet::new();
        let input = self.device.input().map(|input| input.app);
        for dir in self.application_dirs() {
            let mut files = Vec::new();
            collect_desktop_files(&dir, &dir, &mut files);
            for (id, path) in files {
                if !taken.insert(id.clone()) {
                    continue;
                }
                let Ok(text) = fs::read_to_string(&path) else {
                    continue;
                };
                // The video input's app is opened from the top-right menu,
                // so it may hide from menus (`NoDisplay`).
                if let Some(entry) = DesktopEntry::parse(&text, &path, self.language.as_deref())
                    && (entry.is_visible(&self.desktops) || input.as_deref() == Some(id.as_str()))
                {
                    found.insert(id, entry);
                }
            }
        }
        found
    }

    /// A PNG of the icon in the hicolor theme or `pixmaps`, else an SVG
    /// there or in the Breeze, Adwaita or Papirus themes. Follows the
    /// freedesktop icon-naming fallback chain (`a-b-c` → `a-b` → `a`) when
    /// the exact name is not found.
    fn find_icon(&self, icon: &str) -> Option<PathBuf> {
        if icon.contains('/') {
            let path = PathBuf::from(icon);
            return path.is_file().then_some(path);
        }
        let mut candidate = icon;
        loop {
            if let Some(found) = self.find_icon_exact(candidate) {
                return Some(found);
            }
            match candidate.rsplit_once('-') {
                Some((parent, _)) if !parent.is_empty() => candidate = parent,
                _ => return None,
            }
        }
    }

    fn find_icon_exact(&self, icon: &str) -> Option<PathBuf> {
        let png = format!("{icon}.png");
        let svg = format!("{icon}.svg");
        for dir in &self.data_dirs {
            for size in ICON_SIZES {
                for sub in PNG_ICON_SUBDIRS {
                    let path = dir.join("icons/hicolor").join(size).join(sub).join(&png);
                    if path.is_file() {
                        return Some(path);
                    }
                }
            }
            let pixmap = dir.join("pixmaps").join(&png);
            if pixmap.is_file() {
                return Some(pixmap);
            }
        }
        for dir in &self.data_dirs {
            for place in SVG_ICON_DIRS {
                let path = dir.join(place).join(&svg);
                if path.is_file() {
                    return Some(path);
                }
            }
            let pixmap = dir.join("pixmaps").join(&svg);
            if pixmap.is_file() {
                return Some(pixmap);
            }
        }
        None
    }

    /// Determines how app `id` can be uninstalled/deleted, if at all.
    fn delete_kind(&self, id: &str, entry: &DesktopEntry) -> Option<DeleteKind> {
        if let Some((app_id, scope)) = entry.flatpak_target(id) {
            return self
                .has_flatpak
                .then_some(DeleteKind::Flatpak { app_id, scope });
        }
        if is_user_desktop_path(&entry.path) {
            return Some(DeleteKind::DesktopFile(entry.path.clone()));
        }
        if self.apt_remove && !is_protected_desktop_id(id) {
            return Some(DeleteKind::AptPackage(entry.path.clone()));
        }
        None
    }
}

impl Platform for LinuxPlatform {
    /// The system's time zone, summer time included, through `date +%z`
    /// (no time zone database of our own), kept for [`UTC_OFFSET_FOR`].
    fn utc_offset_secs(&self) -> i32 {
        let now = SystemTime::now();
        if let Some((read, offset)) = *self.utc_offset.borrow()
            && now
                .duration_since(read)
                .is_ok_and(|age| age < UTC_OFFSET_FOR)
        {
            return offset;
        }
        let offset = Command::new("date")
            .arg("+%z")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .and_then(|output| parse_utc_offset(&String::from_utf8_lossy(&output.stdout)))
            .unwrap_or(0);
        *self.utc_offset.borrow_mut() = Some((now, offset));
        offset
    }

    fn list_apps(&self) -> Result<Vec<AppEntry>, PlatformError> {
        *self.seen.borrow_mut() = self.stamps();
        let entries = self.scan();
        // The video input has its entry in the top-right menu, no tile.
        let input = self.device.input().map(|input| input.app);
        let apps = entries
            .iter()
            .filter(|(id, _)| input.as_deref() != Some(id.as_str()))
            .map(|(id, entry)| AppEntry {
                app: App {
                    id: id.clone(),
                    label: entry.name.clone(),
                    category: entry.category,
                },
                artwork: if entry.icon.is_some() {
                    Artwork::Later
                } else {
                    Artwork::Missing
                },
            })
            .collect();
        *self.entries.borrow_mut() = entries;
        Ok(apps)
    }

    /// Desktop apps such as Kodi can take many seconds on a small device.
    fn launch_timeout(&self) -> std::time::Duration {
        LAUNCH_TIMEOUT
    }

    fn leave_launch(&mut self, id: &str) {
        let Some(windows) = &self.windows else {
            return;
        };
        let Some(keys) = self
            .entries
            .borrow()
            .get(id)
            .map(|entry| entry.window_keys(id))
        else {
            return;
        };
        log::info!("Home while {id} opens: its window stays behind Home");
        windows.hold_back(keys, std::time::Instant::now() + LAUNCH_TIMEOUT);
    }

    fn launch(&mut self, id: &str) -> Result<(), PlatformError> {
        let entries = self.entries.borrow();
        let entry = entries
            .get(id)
            .ok_or_else(|| PlatformError::not_launchable(id))?;
        // Opened again after Home left it: it may come to the front.
        if let Some(windows) = &self.windows {
            windows.release(&entry.window_keys(id));
        }
        // Running, but not started by this launcher (e.g. before a restart
        // of the launcher): it becomes one of the launcher's apps, raised
        // below. Without its process found, its window is still raised.
        if !self.is_running(id)
            && !self.adopt(id, entry)
            && self
                .windows
                .as_ref()
                .is_some_and(|windows| windows.activate(&entry.window_keys(id)))
        {
            log::info!("{id} was already open; raised");
            return Ok(());
        }
        if let Some(keys) = self.tasks.as_ref().and_then(|tasks| tasks.reuse(id)) {
            // Already running: its window comes to the front. Without one
            // yet, it is still starting; the splash waits for it.
            let raised = self
                .windows
                .as_ref()
                .is_some_and(|windows| windows.activate(&keys));
            log::info!(
                "{id} is running; {}",
                if raised { "raised" } else { "no window yet" }
            );
            return Ok(());
        }
        let argv = entry.argv();
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| PlatformError::not_launchable(id))?;
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Apps start in their entry's Path, else at home, as a desktop's
        // menu starts them: not in whatever directory the launcher was
        // started from (a terminal's shell can't open one it may not enter).
        if let Some(dir) = entry
            .working_dir
            .clone()
            .or_else(|| std::env::var_os("HOME").map(Into::into))
        {
            command.current_dir(dir);
        }
        let percent = self.app_scale(id);
        if percent != 100 {
            let factor = format!("{}", f64::from(percent) / 100.0);
            command
                .env("QT_SCALE_FACTOR", &factor)
                .env("GDK_DPI_SCALE", &factor);
        }
        // A process group of its own, so closing the app also ends the
        // processes it started (e.g. a wrapper script's real program).
        command.process_group(0);
        let mut child = command
            .spawn()
            .map_err(|error| PlatformError::call("launch", format!("{id}: {error}")))?;
        let group = child.id();
        log::info!("started {id} (pid {group})");
        if let Some(tasks) = &self.tasks {
            tasks.started(id, group, entry.window_keys(id), entry.reads_pads(id));
        }
        if let Some(events) = &self.events {
            events(AppEvent::Started(id.to_owned()));
        }
        let events = self.events.clone();
        let tasks = self.tasks.clone();
        let name = id.to_owned();
        let waiter = std::thread::Builder::new()
            .name("rusttv-app-wait".into())
            .spawn(move || {
                match child.wait() {
                    Ok(status) => log::info!("{name} ended ({status})"),
                    Err(error) => log::warn!("waiting for {name} failed: {error}"),
                }
                if let Some(tasks) = tasks {
                    tasks.ended(group);
                }
                if let Some(events) = events {
                    events(AppEvent::Exited(name));
                }
            });
        if let Err(error) = waiter {
            log::warn!("no thread to wait for {id}: {error}");
        }
        Ok(())
    }

    fn take_apps_changed(&mut self) -> Result<bool, PlatformError> {
        let now = self.stamps();
        let changed = *self.seen.borrow() != now;
        *self.seen.borrow_mut() = now;
        Ok(changed)
    }

    fn artwork(&self, id: &str) -> Result<Artwork, PlatformError> {
        let entries = self.entries.borrow();
        let entry = entries
            .get(id)
            .ok_or_else(|| PlatformError::not_launchable(id))?;
        let Some(path) = entry.icon.as_deref().and_then(|icon| self.find_icon(icon)) else {
            return Ok(Artwork::Missing);
        };
        let decoded = if path.extension().is_some_and(|extension| extension == "svg") {
            decode_svg(&path)
        } else {
            decode_png(&path)
        };
        match decoded {
            Ok(bitmap) => Ok(Artwork::Icon(bitmap)),
            Err(error) => {
                log::warn!("icon {} of {id}: {error}", path.display());
                Ok(Artwork::Missing)
            }
        }
    }

    fn notices(&self) -> Result<String, PlatformError> {
        let path = std::env::var_os("RUSTTV_NOTICES")
            .map_or_else(|| PathBuf::from(NOTICES), PathBuf::from);
        fs::read_to_string(&path)
            .map_err(|error| PlatformError::call("notices", format!("{}: {error}", path.display())))
    }

    fn artwork_loaded(&self) {}

    fn update_status(&self) -> Result<UpdateStatus, PlatformError> {
        Ok(UpdateStatus::Idle)
    }

    /// Nothing to check: the system package manager updates the launcher.
    fn check_for_update(&mut self) -> Result<(), PlatformError> {
        Ok(())
    }

    fn install_update(&mut self) -> Result<(), PlatformError> {
        Err(PlatformError::call(
            "install_update",
            "the system package manager updates the launcher",
        ))
    }

    fn top_right_menu(&self) -> bool {
        true
    }

    /// The quick panels whose tools are installed, and KDE Connect when
    /// its desktop entry is.
    fn quick_actions(&self) -> Vec<QuickAction> {
        let mut actions = Vec::new();
        for (action, panel) in [
            (QuickAction::Audio, QuickPanel::Audio),
            (QuickAction::Network, QuickPanel::Network),
            (QuickAction::Power, QuickPanel::Power),
        ] {
            if self.device.has(panel) {
                actions.push(action);
            }
        }
        if device::has_desktop_entry(&self.data_dirs, KDE_CONNECT) {
            actions.push(QuickAction::KdeConnect);
        }
        if self.input_app().is_some() {
            actions.push(QuickAction::Input);
        }
        actions
    }

    fn quick_action_app(&self, action: QuickAction) -> Option<String> {
        match action {
            QuickAction::KdeConnect => Some(KDE_CONNECT.to_owned()),
            QuickAction::Input => {
                let input = self.device.input()?;
                let app = self.input_app()?;
                self.device.record_input_selected(&input.id);
                Some(app)
            }
            _ => None,
        }
    }

    fn input_label(&self) -> Option<String> {
        self.input_app()?;
        self.device.input().map(|input| input.label)
    }

    fn device_settings(&self) -> Option<Arc<dyn DeviceSettings>> {
        Some(self.device.clone())
    }

    fn note_activity(&self) {
        self.device.note_activity();
    }

    fn scales_apps(&self) -> bool {
        self.sizes.is_some()
    }

    fn app_scale(&self, id: &str) -> u32 {
        self.sizes
            .as_ref()
            .and_then(|sizes| sizes.percent.get(id).copied())
            .unwrap_or(100)
    }

    fn set_app_scale(&mut self, id: &str, percent: u32) {
        if let Some(sizes) = &mut self.sizes {
            sizes.set(id, percent);
        }
    }

    fn is_running(&self, id: &str) -> bool {
        self.tasks
            .as_ref()
            .is_some_and(|tasks| tasks.is_running(id))
    }

    fn close_app(&mut self, id: &str) {
        if let Some(tasks) = &self.tasks {
            tasks.close_app(id);
        }
    }

    fn can_delete_app(&self, id: &str) -> bool {
        let entries = self.entries.borrow();
        entries
            .get(id)
            .is_some_and(|entry| self.delete_kind(id, entry).is_some())
    }

    fn delete_app(
        &mut self,
        id: &str,
    ) -> Result<Box<dyn FnOnce() -> Result<(), PlatformError> + Send>, PlatformError> {
        let kind = {
            let entries = self.entries.borrow();
            let entry = entries
                .get(id)
                .ok_or_else(|| PlatformError::not_launchable(id))?;
            self.delete_kind(id, entry).ok_or_else(|| {
                PlatformError::call("delete_app", "this app cannot be deleted from RustTV")
            })?
        };
        // Stop the app if it is currently running and drop any saved UI scale.
        self.close_app(id);
        self.set_app_scale(id, 100);
        Ok(Box::new(move || execute_delete(kind)))
    }

    fn last_app(&self) -> Option<String> {
        if let Some(id) = self.tasks.as_ref().and_then(|tasks| tasks.front()) {
            return Some(id);
        }
        // An app left running by an earlier launcher, in front.
        let windows = self.windows.as_ref()?;
        let app_id = windows.front()?.to_lowercase();
        let open = windows.app_ids();
        let entries = self.entries.borrow();
        let (id, entry) = entries
            .iter()
            .find(|(id, entry)| entry.window_keys(id).contains(&app_id))?;
        if !self.adopt(id, entry) {
            return None;
        }
        if let Some(tasks) = &self.tasks {
            tasks.activated(&app_id, &open);
        }
        Some(id.clone())
    }

    fn open_overlay(&mut self, overlay: crate::platform::AppOverlay) -> bool {
        let device: Arc<dyn DeviceSettings> = self.device.clone();
        self.overlay = overlay::Overlay::open(overlay, device, self.device.side_menu());
        self.overlay.is_some()
    }

    fn close_overlay(&mut self, to_home: bool) {
        if let Some(overlay) = &self.overlay {
            overlay.close(to_home);
        }
    }

    fn nudged(&mut self, tab: &str, control: &str) {
        if !self.device.shows_pattern(tab, control) {
            return;
        }
        // The side menu draws the pattern itself, while that row is focused.
        if overlay::shown()
            && let Some(overlay) = &self.overlay
        {
            overlay.point(tab, control);
            return;
        }
        if self.pattern.is_none() {
            self.pattern = overlay::PatternOverlay::start();
        }
        if let Some(pattern) = &self.pattern {
            pattern.show();
        }
    }

    fn show_volume(&mut self, volume: crate::device::Volume, muted: &str) -> bool {
        if overlay::shown()
            && let Some(overlay) = &self.overlay
        {
            overlay.volume(volume);
        }
        if self.volume_bar.is_none() {
            self.volume_bar = overlay::VolumeBar::start();
        }
        self.volume_bar.as_ref().is_some_and(|bar| {
            bar.show(volume, muted);
            true
        })
    }
}

/// Each app's size on screen in percent, by desktop file ID, in a TOML file
/// (`"org.kde.dolphin.desktop" = 125`); 100 is not stored.
#[derive(Debug)]
struct AppSizes {
    path: PathBuf,
    percent: HashMap<String, u32>,
}

impl AppSizes {
    /// The sizes in `path`; none if it is missing or unreadable.
    fn load(path: PathBuf) -> Self {
        let percent = fs::read_to_string(&path)
            .ok()
            .and_then(|text| match toml::from_str(&text) {
                Ok(sizes) => Some(sizes),
                Err(error) => {
                    log::warn!("{}: {error}; app sizes reset", path.display());
                    None
                }
            })
            .unwrap_or_default();
        Self { path, percent }
    }

    /// Sets `id`'s size and writes the file (through a temporary file, so
    /// a crash leaves the old one).
    fn set(&mut self, id: &str, percent: u32) {
        if percent == 100 {
            self.percent.remove(id);
        } else {
            self.percent.insert(id.to_owned(), percent);
        }
        let written = toml::to_string(&self.percent)
            .map_err(std::io::Error::other)
            .and_then(|text| {
                let temporary = self.path.with_extension("toml.new");
                fs::write(&temporary, text)?;
                fs::rename(&temporary, &self.path)
            });
        if let Err(error) = written {
            log::warn!("app sizes not saved to {}: {error}", self.path.display());
        }
    }
}

/// Collects `(desktop file ID, path)` for every `.desktop` file under `dir`;
/// the ID is the path below `root` with `/` replaced by `-`.
fn collect_desktop_files(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) {
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = read
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect_desktop_files(root, &path, out);
        } else if path.extension().is_some_and(|ext| ext == "desktop")
            && let Ok(relative) = path.strip_prefix(root)
        {
            let id = relative.to_string_lossy().replace('/', "-");
            out.push((id, path));
        }
    }
}

/// The parts of a desktop entry the launcher uses.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DesktopEntry {
    name: String,
    exec: String,
    icon: Option<String>,
    category: Category,
    working_dir: Option<PathBuf>,
    path: PathBuf,
    hidden: bool,
    only_show_in: Vec<String>,
    not_show_in: Vec<String>,
    try_exec: Option<String>,
    wm_class: Option<String>,
    flatpak: Option<String>,
}

impl DesktopEntry {
    /// Reads the `[Desktop Entry]` group; `None` unless it is an application
    /// that can be started from a launcher (no terminal apps).
    fn parse(text: &str, path: &Path, language: Option<&str>) -> Option<Self> {
        let mut values: HashMap<&str, &str> = HashMap::new();
        let mut in_entry = false;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.starts_with('[') {
                in_entry = line == "[Desktop Entry]";
                continue;
            }
            if in_entry && let Some((key, value)) = line.split_once('=') {
                values.entry(key.trim_end()).or_insert(value.trim_start());
            }
        }
        let get = |key: &str| values.get(key).map(|value| unescape(value));
        let flag = |key: &str| values.get(key).is_some_and(|value| *value == "true");
        if get("Type").as_deref() != Some("Application") || flag("Terminal") {
            return None;
        }
        let name = localized(&values, "Name", language)?;
        let exec = get("Exec").filter(|exec| !exec.is_empty())?;
        let list = |key: &str| -> Vec<String> {
            get(key)
                .map(|value| {
                    value
                        .split(';')
                        .filter(|item| !item.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        };
        Some(Self {
            name,
            exec,
            icon: get("Icon").filter(|icon| !icon.is_empty()),
            category: category(&list("Categories")),
            working_dir: get("Path").filter(|dir| !dir.is_empty()).map(PathBuf::from),
            path: path.to_path_buf(),
            hidden: flag("Hidden") || flag("NoDisplay"),
            only_show_in: list("OnlyShowIn"),
            not_show_in: list("NotShowIn"),
            try_exec: get("TryExec").filter(|program| !program.is_empty()),
            wm_class: get("StartupWMClass").filter(|class| !class.is_empty()),
            flatpak: get("X-Flatpak").filter(|id| !id.is_empty()),
        })
    }

    /// Returns the Flatpak app ID and installation scope if this entry came
    /// from Flatpak (`X-Flatpak=` key or `flatpak/exports/share/applications`).
    fn flatpak_target(&self, desktop_id: &str) -> Option<(String, FlatpakScope)> {
        let path_str = self.path.to_string_lossy();
        let from_export = path_str.contains("/flatpak/exports/share/applications/");
        let app_id = self.flatpak.clone().or_else(|| {
            from_export.then(|| {
                desktop_id
                    .strip_suffix(".desktop")
                    .unwrap_or(desktop_id)
                    .to_owned()
            })
        })?;
        if !is_safe_pkg_id(&app_id) {
            return None;
        }
        let scope = if path_str.contains("/.local/share/flatpak/") {
            FlatpakScope::User
        } else if path_str.starts_with("/var/lib/flatpak/") {
            FlatpakScope::System
        } else {
            FlatpakScope::Auto
        };
        Some((app_id, scope))
    }

    /// Lower-case app IDs the app's windows may have: `StartupWMClass`, the
    /// desktop file ID without `.desktop` (and its last part for reverse-DNS
    /// IDs), and the program's name.
    fn window_keys(&self, id: &str) -> Vec<String> {
        let stem = id.strip_suffix(".desktop").unwrap_or(id);
        let program = split_exec(&self.exec)
            .first()
            .and_then(|program| Path::new(program).file_name())
            .map(|name| name.to_string_lossy().into_owned());
        let mut keys: Vec<String> = [
            self.wm_class.clone(),
            Some(stem.to_owned()),
            stem.rsplit('.').next().map(str::to_owned),
            program,
        ]
        .into_iter()
        .flatten()
        .map(|key| key.to_lowercase())
        .collect();
        keys.sort();
        keys.dedup();
        keys
    }

    /// Whether a launcher on these desktops shows the entry.
    fn is_visible(&self, desktops: &[String]) -> bool {
        let on = |list: &[String]| list.iter().any(|name| desktops.contains(name));
        !self.hidden
            && (self.only_show_in.is_empty() || on(&self.only_show_in))
            && !on(&self.not_show_in)
            && self.try_exec.as_deref().is_none_or(program_exists)
            && !self.is_launcher()
    }

    /// Whether the app reads game controllers itself (games, Kodi, RetroArch,
    /// Steam), so the launcher sends it no keys for them.
    fn reads_pads(&self, id: &str) -> bool {
        let id = id.to_lowercase();
        self.category == Category::Game
            || ["kodi", "retroarch", "steam"]
                .iter()
                .any(|name| id.contains(name))
    }

    /// The launcher's own entry, which the home screen leaves out.
    fn is_launcher(&self) -> bool {
        split_exec(&self.exec)
            .first()
            .and_then(|program| Path::new(program).file_name())
            .is_some_and(|name| name.to_string_lossy().starts_with("rusttv-launcher"))
    }

    /// The command line, with field codes expanded for a start without files.
    fn argv(&self) -> Vec<String> {
        let mut argv = Vec::new();
        for arg in split_exec(&self.exec) {
            match arg.as_str() {
                "%f" | "%F" | "%u" | "%U" | "%d" | "%D" | "%n" | "%N" | "%v" | "%m" => {}
                "%i" => {
                    if let Some(icon) = &self.icon {
                        argv.extend(["--icon".to_owned(), icon.clone()]);
                    }
                }
                "%c" => argv.push(self.name.clone()),
                "%k" => argv.push(self.path.to_string_lossy().into_owned()),
                _ => argv.push(expand_percent(&arg, self)),
            }
        }
        argv
    }
}

/// `%c`, `%k` and `%%` inside a longer argument; other codes are dropped.
fn expand_percent(arg: &str, entry: &DesktopEntry) -> String {
    let mut out = String::with_capacity(arg.len());
    let mut chars = arg.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('%') => out.push('%'),
            Some('c') => out.push_str(&entry.name),
            Some('k') => out.push_str(&entry.path.to_string_lossy()),
            _ => {}
        }
    }
    out
}

/// The value of `key` in the best matching locale: `lang_COUNTRY@MODIFIER`,
/// `lang_COUNTRY`, `lang@MODIFIER`, `lang`, then the plain key.
/// The UI's language from the environment (`LANG` style), unless it is the
/// plain C locale.
fn ui_language() -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
        .filter(|value| value != "C" && value != "POSIX")
}

fn localized(values: &HashMap<&str, &str>, key: &str, language: Option<&str>) -> Option<String> {
    if let Some(language) = language {
        let locale = language.split('.').next().unwrap_or(language);
        let (base, modifier) = match locale.split_once('@') {
            Some((base, modifier)) => (base, Some(modifier)),
            None => (locale, None),
        };
        let lang = base.split('_').next().unwrap_or(base);
        let mut tries = Vec::new();
        if let Some(modifier) = modifier {
            tries.push(format!("{base}@{modifier}"));
        }
        tries.push(base.to_owned());
        if let Some(modifier) = modifier {
            tries.push(format!("{lang}@{modifier}"));
        }
        tries.push(lang.to_owned());
        for locale in tries {
            if let Some(value) = values.get(format!("{key}[{locale}]").as_str()) {
                return Some(unescape(value));
            }
        }
    }
    values.get(key).map(|value| unescape(value))
}

/// The launcher category for the first freedesktop main or additional
/// category that has one.
fn category(categories: &[String]) -> Category {
    categories
        .iter()
        .find_map(|name| match name.as_str() {
            "Game" => Some(Category::Game),
            "Audio" | "Music" => Some(Category::Audio),
            "Video" | "TV" | "AudioVideo" | "Player" => Some(Category::Video),
            "Graphics" | "Photography" | "Viewer" => Some(Category::Image),
            "Chat" | "InstantMessaging" | "VideoConference" => Some(Category::Social),
            "News" | "Feed" => Some(Category::News),
            "Maps" => Some(Category::Maps),
            "Office" => Some(Category::Productivity),
            "Accessibility" => Some(Category::Accessibility),
            _ => None,
        })
        .unwrap_or_default()
}

/// Undoes the string escapes of a desktop entry value (`\s`, `\n`, `\t`,
/// `\r`, `\\`).
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// Splits an `Exec` value into arguments: spaces separate them, double
/// quotes group them, and inside quotes `\"`, `` \` ``, `\$` and `\\` stand
/// for the character.
fn split_exec(exec: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quoted = false;
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '\\' if quoted => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            ' ' | '\t' if !quoted => {
                if started {
                    args.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            _ => {
                current.push(c);
                started = true;
            }
        }
    }
    if started {
        args.push(current);
    }
    args
}

/// Whether `program` is an executable path or found in `PATH`.
fn program_exists(program: &str) -> bool {
    if program.contains('/') {
        return Path::new(program).is_file();
    }
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlatpakScope {
    User,
    System,
    Auto,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DeleteKind {
    Flatpak { app_id: String, scope: FlatpakScope },
    DesktopFile(PathBuf),
    AptPackage(PathBuf),
}

fn is_safe_pkg_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+'))
}

fn is_protected_desktop_id(id: &str) -> bool {
    let lower = id.to_ascii_lowercase();
    lower.contains("rusttv")
}

/// True when `path` is a user-owned `.desktop` file outside system read-only
/// directories (e.g. `~/.local/share/applications/` or a test temp dir).
fn is_user_desktop_path(path: &Path) -> bool {
    if path.starts_with("/usr/")
        || path.starts_with("/var/lib/flatpak/")
        || path.starts_with("/snap/")
        || path.starts_with("/var/lib/snapd/")
    {
        return false;
    }
    path.extension().is_some_and(|ext| ext == "desktop")
        && path
            .parent()
            .and_then(|dir| fs::metadata(dir).ok())
            .is_some_and(|meta| !meta.permissions().readonly())
}

fn has_program(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
}

fn detect_apt_remove() -> bool {
    if !has_program("dpkg") || !has_program("dpkg-query") || !has_program("apt-get") {
        return false;
    }
    if rustix::process::geteuid().is_root() {
        return true;
    }
    has_program("sudo")
        && Command::new("sudo")
            .args(["-n", "true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
}

fn execute_delete(kind: DeleteKind) -> Result<(), PlatformError> {
    match kind {
        DeleteKind::Flatpak { app_id, scope } => {
            let mut cmd = Command::new("flatpak");
            cmd.arg("uninstall").arg("-y").arg("--noninteractive");
            match scope {
                FlatpakScope::User => {
                    cmd.arg("--user");
                }
                FlatpakScope::System => {
                    cmd.arg("--system");
                }
                FlatpakScope::Auto => {}
            }
            let output = cmd
                .arg(&app_id)
                .stdin(Stdio::null())
                .output()
                .map_err(|error| PlatformError::call("flatpak", error.to_string()))?;
            if output.status.success() {
                log::info!("uninstalled Flatpak app {app_id}");
                Ok(())
            } else {
                let err = String::from_utf8_lossy(&output.stderr);
                let line = err.trim().lines().next().unwrap_or("uninstall failed");
                Err(PlatformError::call("flatpak", line))
            }
        }
        DeleteKind::DesktopFile(path) => {
            fs::remove_file(&path)
                .map_err(|error| PlatformError::call("delete_app", error.to_string()))?;
            log::info!("deleted desktop entry {}", path.display());
            Ok(())
        }
        DeleteKind::AptPackage(path) => {
            let owner = Command::new("dpkg")
                .arg("-S")
                .arg(&path)
                .stdin(Stdio::null())
                .output()
                .map_err(|error| PlatformError::call("dpkg", error.to_string()))?;
            if !owner.status.success() {
                return Err(PlatformError::call(
                    "delete_app",
                    "not owned by an installed package",
                ));
            }
            let stdout = String::from_utf8_lossy(&owner.stdout);
            let pkg = stdout
                .lines()
                .next()
                .and_then(|line| line.split(':').next())
                .map(str::trim)
                .filter(|name| is_safe_pkg_id(name))
                .ok_or_else(|| PlatformError::call("dpkg", "could not identify package"))?;
            if pkg.starts_with("rusttv") {
                return Err(PlatformError::call("delete_app", "cannot uninstall RustTV"));
            }
            // Refuse Essential packages so system integrity is never harmed.
            let essential = Command::new("dpkg-query")
                .args(["-W", "-f=${Essential}", pkg])
                .stdin(Stdio::null())
                .output()
                .ok()
                .is_some_and(|out| String::from_utf8_lossy(&out.stdout).trim() == "yes");
            if essential {
                return Err(PlatformError::call(
                    "delete_app",
                    format!("{pkg} is an essential system package"),
                ));
            }
            let mut cmd = if rustix::process::geteuid().is_root() {
                let mut c = Command::new("apt-get");
                c.args(["remove", "-y", pkg]);
                c
            } else {
                let mut c = Command::new("sudo");
                c.args(["-n", "apt-get", "remove", "-y", pkg]);
                c
            };
            let output = cmd
                .env("DEBIAN_FRONTEND", "noninteractive")
                .stdin(Stdio::null())
                .output()
                .map_err(|error| PlatformError::call("apt-get", error.to_string()))?;
            if output.status.success() {
                log::info!("removed apt package {pkg}");
                Ok(())
            } else {
                let err = String::from_utf8_lossy(&output.stderr);
                let line = err
                    .trim()
                    .lines()
                    .next()
                    .unwrap_or("package removal failed");
                Err(PlatformError::call("apt-get", line))
            }
        }
    }
}

/// Decodes a PNG file into an RGBA [`Bitmap`].
/// Draws an SVG icon with its longer side [`SVG_PIXELS`] long.
fn decode_svg(path: &Path) -> Result<Bitmap, String> {
    use resvg::{tiny_skia, usvg};

    let data = fs::read(path).map_err(|error| error.to_string())?;
    let tree = usvg::Tree::from_data(&data, &usvg::Options::default())
        .map_err(|error| error.to_string())?;
    let size = tree.size();
    let scale = SVG_PIXELS / size.width().max(size.height());
    let side = |length: f32| (length * scale).ceil().clamp(1.0, SVG_PIXELS) as u32;
    let (width, height) = (side(size.width()), side(size.height()));
    let mut pixmap = tiny_skia::Pixmap::new(width, height).ok_or("empty icon")?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    // tiny-skia keeps premultiplied alpha; bitmaps are straight.
    let rgba = pixmap
        .pixels()
        .iter()
        .flat_map(|pixel| {
            let color = pixel.demultiply();
            [color.red(), color.green(), color.blue(), color.alpha()]
        })
        .collect();
    Bitmap::new(width, height, rgba).ok_or_else(|| "wrong pixel count".to_owned())
}

fn decode_png(path: &Path) -> Result<Bitmap, String> {
    let file = File::open(path).map_err(|error| error.to_string())?;
    let mut decoder = png::Decoder::new(BufReader::new(file));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(|error| error.to_string())?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| "too large".to_owned())?;
    let mut pixels = vec![0; size];
    let info = reader
        .next_frame(&mut pixels)
        .map_err(|error| error.to_string())?;
    pixels.truncate(info.buffer_size());
    let rgba = match info.color_type {
        png::ColorType::Rgba => pixels,
        png::ColorType::Rgb => pixels
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|&[red, green, blue]| [red, green, blue, 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => pixels
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|&[gray, alpha]| [gray, gray, gray, alpha])
            .collect(),
        png::ColorType::Grayscale => pixels
            .iter()
            .flat_map(|&gray| [gray, gray, gray, 255])
            .collect(),
        png::ColorType::Indexed => return Err("indexed colours left after expansion".into()),
    };
    Bitmap::new(info.width, info.height, rgba).ok_or_else(|| "wrong pixel count".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_offsets_parse_from_date() {
        assert_eq!(parse_utc_offset("+0200\n"), Some(7200));
        assert_eq!(parse_utc_offset("-0330"), Some(-12_600));
        assert_eq!(parse_utc_offset("+0000"), Some(0));
        assert_eq!(parse_utc_offset("+0545"), Some(20_700));
        assert_eq!(parse_utc_offset("+1400"), Some(50_400));
        assert_eq!(parse_utc_offset("-1200"), Some(-43_200));
        for bad in [
            "",
            "0200",
            "+02",
            "+02:00",
            "UTC",
            "+0a00",
            "+1500",
            "+1459",
            "-1201",
            "+0260",
            "+02000",
            "\u{e9}0200",
        ] {
            assert_eq!(parse_utc_offset(bad), None, "{bad}");
        }
    }

    fn entry(text: &str) -> Option<DesktopEntry> {
        DesktopEntry::parse(
            text,
            Path::new("/usr/share/applications/test.desktop"),
            Some("de_DE.UTF-8"),
        )
    }

    #[test]
    fn an_svg_icon_is_drawn_128_pixels_wide_with_straight_alpha() {
        let path = std::env::temp_dir().join(format!("rusttv-icon-{}.svg", std::process::id()));
        fs::write(
            &path,
            "<svg xmlns='http://www.w3.org/2000/svg' width='48' height='24'>             <rect width='24' height='24' fill='#ff0000' fill-opacity='0.5'/></svg>",
        )
        .unwrap();
        let bitmap = decode_svg(&path).unwrap();
        fs::remove_file(&path).unwrap();
        assert_eq!((bitmap.width(), bitmap.height()), (128, 64));
        // Top left: half-transparent red, not premultiplied; top right: empty.
        let first = &bitmap.rgba()[..4];
        assert_eq!(first[0], 255);
        assert!((126..=129).contains(&first[3]), "{first:?}");
        assert_eq!(bitmap.rgba()[127 * 4 + 3], 0);
    }

    #[test]
    fn parses_an_application() {
        let parsed = entry(
            "[Desktop Entry]\nType=Application\nName=Player\nName[de]=Abspieler\n\
             Exec=mpv --player-operation-mode=pseudo-gui -- %U\nIcon=mpv\n\
             Categories=AudioVideo;Video;\n[Desktop Action new]\nName=Other\n",
        )
        .expect("an application");
        assert_eq!(parsed.name, "Abspieler");
        assert_eq!(parsed.category, Category::Video);
        assert_eq!(parsed.icon.as_deref(), Some("mpv"));
        assert_eq!(
            parsed.argv(),
            ["mpv", "--player-operation-mode=pseudo-gui", "--"]
        );
    }

    #[test]
    fn skips_terminal_apps_links_and_entries_without_exec() {
        assert!(
            entry("[Desktop Entry]\nType=Application\nName=Top\nExec=htop\nTerminal=true\n")
                .is_none()
        );
        assert!(
            entry("[Desktop Entry]\nType=Link\nName=Site\nURL=https://example.org\n").is_none()
        );
        assert!(entry("[Desktop Entry]\nType=Application\nName=Nothing\n").is_none());
    }

    #[test]
    fn honours_visibility_keys() {
        let visible = |text: &str, desktops: &[&str]| {
            let desktops: Vec<String> = desktops.iter().map(|name| (*name).to_owned()).collect();
            entry(text).is_some_and(|parsed| parsed.is_visible(&desktops))
        };
        let base = "[Desktop Entry]\nType=Application\nName=A\nExec=true\n";
        assert!(visible(base, &[]));
        assert!(!visible(&format!("{base}NoDisplay=true\n"), &[]));
        assert!(!visible(&format!("{base}OnlyShowIn=KDE;\n"), &["labwc"]));
        assert!(visible(
            &format!("{base}OnlyShowIn=KDE;labwc;\n"),
            &["labwc"]
        ));
        assert!(!visible(&format!("{base}NotShowIn=labwc;\n"), &["labwc"]));
        assert!(!visible(
            "[Desktop Entry]\nType=Application\nName=Me\nExec=/usr/bin/rusttv-launcher-linux\n",
            &[]
        ));
    }

    #[test]
    fn splits_and_expands_exec() {
        let parsed = entry(
            "[Desktop Entry]\nType=Application\nName=My App\nIcon=app\n\
             Exec=\"/opt/my app/run\" --title %c \"say \\\\\"hi\\\\\"\" %i 100%% %f\n",
        )
        .expect("an application");
        assert_eq!(
            parsed.argv(),
            [
                "/opt/my app/run",
                "--title",
                "My App",
                "say \"hi\"",
                "--icon",
                "app",
                "100%"
            ]
        );
    }

    #[test]
    fn maps_categories() {
        let names = |list: &[&str]| {
            list.iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(category(&names(&["Game", "ArcadeGame"])), Category::Game);
        assert_eq!(category(&names(&["AudioVideo", "Audio"])), Category::Video);
        assert_eq!(
            category(&names(&["Network", "WebBrowser"])),
            Category::Other
        );
    }

    #[test]
    fn keeps_app_sizes_in_a_file() {
        let path = std::env::temp_dir().join(format!("rusttv-sizes-{}.toml", std::process::id()));
        let mut sizes = AppSizes::load(path.clone());
        assert!(sizes.percent.is_empty());
        sizes.set("org.kde.dolphin.desktop", 150);
        sizes.set("vlc.desktop", 125);
        sizes.set("vlc.desktop", 100);
        let again = AppSizes::load(path.clone());
        assert_eq!(again.percent.len(), 1);
        assert_eq!(again.percent.get("org.kde.dolphin.desktop"), Some(&150));
        fs::remove_file(&path).expect("remove");
    }

    #[test]
    fn only_a_newly_arriving_picture_switches() {
        // Already there at start: no switch.
        assert_eq!(
            input_change(None, "signal 1920x1080@60"),
            (Some(true), false)
        );
        assert_eq!(input_change(None, "no-signal"), (Some(false), false));
        assert_eq!(
            input_change(Some(false), "signal 1920x1080@60"),
            (Some(true), true)
        );
        assert_eq!(
            input_change(Some(true), "signal 1280x720@60"),
            (Some(true), false)
        );
        assert_eq!(input_change(Some(true), "no-signal"), (Some(false), false));
        // The input unknown (its service starting): what follows doesn't
        // count as newly arriving.
        assert_eq!(input_change(Some(false), "absent"), (None, false));
        assert_eq!(
            input_change(None, "signal 1920x1080@60"),
            (Some(true), false)
        );
        // Startup-on-signal triggers on the very first signal line.
        assert_eq!(
            input_change_with_policy(None, "signal 1920x1080@60", false, true, true),
            (Some(true), true)
        );
        // After startup, absent -> signal triggers auto-switch when enabled.
        assert_eq!(
            input_change_with_policy(None, "signal 1920x1080@60", true, false, true),
            (Some(true), true)
        );
        // Auto-switch off suppresses switching on newly arriving signal.
        assert_eq!(
            input_change_with_policy(Some(false), "signal 1920x1080@60", true, false, false),
            (Some(true), false)
        );
    }

    #[test]
    fn lists_apps_and_notices_changes() {
        let root = std::env::temp_dir().join(format!("rusttv-linux-{}", std::process::id()));
        let apps = root.join("applications");
        fs::create_dir_all(apps.join("kde")).expect("temp dir");
        fs::write(
            apps.join("one.desktop"),
            "[Desktop Entry]\nType=Application\nName=One\nExec=true\n",
        )
        .expect("write");
        fs::write(
            apps.join("kde/two.desktop"),
            "[Desktop Entry]\nType=Application\nName=Two\nExec=true\n",
        )
        .expect("write");
        let mut platform = LinuxPlatform::new(vec![root.clone()], Vec::new(), None);
        let mut ids: Vec<String> = platform
            .list_apps()
            .expect("list")
            .into_iter()
            .map(|entry| entry.app.id)
            .collect();
        ids.sort();
        assert_eq!(ids, ["kde-two.desktop", "one.desktop"]);
        assert!(!platform.take_apps_changed().expect("changed"));
        assert!(platform.launch("missing.desktop").is_err());
        // User desktop entries can be deleted directly.
        assert!(platform.can_delete_app("one.desktop"));
        let work = platform.delete_app("one.desktop").expect("delete task");
        work().expect("delete succeeds");
        assert!(!apps.join("one.desktop").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn finds_breeze_places_and_fallback_icon_names() {
        let root = std::env::temp_dir().join(format!("rusttv-icons-{}", std::process::id()));
        let places = root.join("icons/breeze/places/48");
        fs::create_dir_all(&places).expect("icon dir");
        let target = places.join("system-file-manager.svg");
        fs::write(&target, "<svg xmlns='http://www.w3.org/2000/svg'/>").expect("write");
        let platform = LinuxPlatform::new(vec![root.clone()], Vec::new(), None);
        assert_eq!(
            platform.find_icon("system-file-manager"),
            Some(target.clone())
        );
        assert_eq!(
            platform.find_icon("system-file-manager-symbolic"),
            Some(target)
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detects_flatpak_app_id_and_scope() {
        let path = Path::new(
            "/home/tv/.local/share/flatpak/exports/share/applications/org.videolan.VLC.desktop",
        );
        let entry = DesktopEntry::parse(
            "[Desktop Entry]\nType=Application\nName=VLC\nExec=flatpak run org.videolan.VLC\nX-Flatpak=org.videolan.VLC\n",
            path,
            None,
        )
        .expect("entry");
        assert_eq!(
            entry.flatpak_target("org.videolan.VLC.desktop"),
            Some(("org.videolan.VLC".to_owned(), FlatpakScope::User))
        );
    }
}
