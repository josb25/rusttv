//! The apps the launcher started and keeps running: one instance per app,
//! at most [`Limits::max_apps`] at once, and background apps closed when
//! memory runs low. The least recently used app closes first. Apps in the
//! background can be frozen (SIGSTOP), like Android's cached-app freezer:
//! they keep their memory but take no CPU or GPU time from the app in front.

use std::fs;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process_group, test_kill_process_group};

/// How many apps run and how much memory they leave.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Limits {
    /// Apps kept running besides the launcher; starting one more closes the
    /// least recently used. `None`: no limit.
    pub max_apps: Option<usize>,
    /// Close background apps (least recently used first) while the kernel's
    /// `MemAvailable` is below this many MiB. `None`: never.
    pub min_available_mib: Option<u64>,
    /// Close background apps the same way while more than this many MiB of
    /// swap are in use, so the box stays responsive instead of swapping.
    /// `None`: never.
    pub max_swap_mib: Option<u64>,
    /// Stop (SIGSTOP) every app but the one in front, and continue it when
    /// its window comes to the front again. Also pauses background music.
    pub freeze_background: bool,
    /// Apps (desktop file IDs without `.desktop`, any case) that are never
    /// frozen (only muted) and are closed for memory only after every other
    /// background app: those that must keep talking to the network, such as a game
    /// stream, which a paused app loses.
    pub keep_running: Vec<String>,
}

impl Limits {
    fn keeps_running(&self, id: &str) -> bool {
        let app = id.strip_suffix(".desktop").unwrap_or(id);
        self.keep_running
            .iter()
            .any(|kept| kept.eq_ignore_ascii_case(app))
    }
}

/// How long an app has to quit after SIGTERM before it gets SIGKILL.
const GRACE: Duration = Duration::from_secs(5);

/// How often available memory is checked.
const MEMORY_POLL: Duration = Duration::from_secs(2);

/// After closing an app for memory, how long to wait before judging memory
/// again: the app needs time to exit and hand its memory back.
const MEMORY_SETTLE: Duration = Duration::from_secs(6);

/// The running apps, shared with the threads that wait for them.
#[derive(Debug, Default)]
pub struct Tasks {
    limits: Limits,
    running: Mutex<Vec<Task>>,
    /// The app whose window was last in front (not the launcher's).
    front: Mutex<Option<String>>,
}

#[derive(Debug, Clone)]
struct Task {
    id: String,
    /// The app's process group (its first process's PID).
    group: i32,
    /// Lower-case app IDs its windows may have.
    keys: Vec<String>,
    /// Reads game controllers itself.
    reads_pads: bool,
    used: Instant,
    closing: bool,
    frozen: bool,
    /// Its sound is muted while it keeps running in the background.
    muted: bool,
}

impl Tasks {
    /// Starts watching memory if `limits` asks for it.
    pub fn start(limits: Limits) -> Arc<Self> {
        let (min, max_swap) = (limits.min_available_mib, limits.max_swap_mib);
        let tasks = Arc::new(Self {
            limits,
            running: Mutex::default(),
            front: Mutex::default(),
        });
        if min.is_some() || max_swap.is_some() {
            let weak = Arc::downgrade(&tasks);
            let spawned = std::thread::Builder::new()
                .name("rusttv-memory".into())
                .spawn(move || {
                    while let Some(tasks) = weak.upgrade() {
                        let reason =
                            read_meminfo().and_then(|memory| memory_short(memory, min, max_swap));
                        let closed =
                            reason.is_some_and(|reason| tasks.close_one_in_background(reason));
                        drop(tasks);
                        std::thread::sleep(if closed { MEMORY_SETTLE } else { MEMORY_POLL });
                    }
                });
            if let Err(error) = spawned {
                log::warn!("no thread to watch memory: {error}");
            }
        }
        tasks
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Task>> {
        self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The running app whose window was last in front of the launcher.
    pub fn front(&self) -> Option<String> {
        let front = self
            .front
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()?;
        self.is_running(&front).then_some(front)
    }

    /// Closes app `id` if it runs.
    pub fn close_app(&self, id: &str) {
        let task = {
            let mut running = self.lock();
            let Some(task) = running
                .iter_mut()
                .find(|task| task.id == id && !task.closing)
            else {
                return;
            };
            task.closing = true;
            task.clone()
        };
        log::info!("closing {id} (asked for)");
        self.close(task);
    }

    /// Whether `id` runs (and is not being closed).
    pub fn is_running(&self, id: &str) -> bool {
        self.lock()
            .iter()
            .any(|task| task.id == id && !task.closing)
    }

    /// The window keys of `id` if it is running (and not being closed); marks
    /// it as just used.
    pub fn reuse(&self, id: &str) -> Option<Vec<String>> {
        let mut running = self.lock();
        let task = running
            .iter_mut()
            .find(|task| task.id == id && !task.closing)?;
        task.used = Instant::now();
        if task.frozen {
            task.frozen = false;
            signal(task.group, Signal::CONT);
        }
        Some(task.keys.clone())
    }

    /// Records a started app, then closes the least recently used others
    /// beyond [`Limits::max_apps`].
    pub fn started(&self, id: &str, group: u32, keys: Vec<String>, reads_pads: bool) {
        let Ok(group) = i32::try_from(group) else {
            return;
        };
        let victims: Vec<Task> = {
            let mut running = self.lock();
            running.push(Task {
                id: id.to_owned(),
                group,
                keys,
                reads_pads,
                used: Instant::now(),
                closing: false,
                frozen: false,
                muted: false,
            });
            let Some(max) = self.limits.max_apps else {
                return;
            };
            let mut open: Vec<&mut Task> = running
                .iter_mut()
                .filter(|task| !task.closing && task.group != group)
                .collect();
            open.sort_by_key(|task| task.used);
            let excess = (open.len() + 1).saturating_sub(max.max(1));
            open.into_iter()
                .take(excess)
                .map(|task| {
                    task.closing = true;
                    task.clone()
                })
                .collect()
        };
        for task in victims {
            log::info!(
                "closing {} (more than {:?} apps)",
                task.id,
                self.limits.max_apps
            );
            self.close(task);
        }
    }

    /// The app in process group `group` ended.
    pub fn ended(&self, group: u32) {
        let Ok(group) = i32::try_from(group) else {
            return;
        };
        self.lock().retain(|task| task.group != group);
    }

    /// A window with this app ID came to the front (the launcher's own too):
    /// its app was just used, and with [`Limits::freeze_background`] every
    /// other app stops; apps in [`Limits::keep_running`] are muted instead. Returns whether that app reads game controllers
    /// itself; `None` for a window of no app started here.
    pub fn activated(&self, app_id: &str) -> Option<bool> {
        let app_id = app_id.to_lowercase();
        let mut front_id = None;
        for task in self.lock().iter_mut() {
            let front = task.keys.contains(&app_id);
            if front {
                task.used = Instant::now();
                front_id = Some((task.id.clone(), task.reads_pads));
            }
            if front && task.muted {
                task.muted = false;
                super::audio::set_muted(task.id.clone(), task.group, false);
            } else if !front && !task.muted && !task.closing && self.limits.keeps_running(&task.id)
            {
                task.muted = true;
                super::audio::set_muted(task.id.clone(), task.group, true);
            }
            if front && task.frozen {
                task.frozen = false;
                signal(task.group, Signal::CONT);
            } else if !front
                && !task.frozen
                && !task.closing
                && self.limits.freeze_background
                && !self.limits.keeps_running(&task.id)
            {
                task.frozen = true;
                log::info!("freezing {} in the background", task.id);
                signal(task.group, Signal::STOP);
            }
        }
        let (id, reads_pads) = front_id?;
        *self.front.lock().unwrap_or_else(PoisonError::into_inner) = Some(id);
        Some(reads_pads)
    }

    /// Closes one background app: never the most recently used one, apps in
    /// [`Limits::keep_running`] only when no other is left, else the least
    /// recently used. Whether one was closed.
    fn close_one_in_background(&self, reason: &str) -> bool {
        let victim = {
            let mut running = self.lock();
            let mut open: Vec<&mut Task> =
                running.iter_mut().filter(|task| !task.closing).collect();
            if open.len() < 2 {
                return false;
            }
            open.sort_by_key(|task| task.used);
            open.pop();
            // `min_by_key` keeps the first of equals: the least recently used.
            let Some(task) = open
                .into_iter()
                .min_by_key(|task| self.limits.keeps_running(&task.id))
            else {
                return false;
            };
            task.closing = true;
            task.clone()
        };
        log::warn!("closing {} ({reason})", victim.id);
        self.close(victim);
        true
    }

    /// SIGTERM to the app's process group; SIGKILL if it is still there
    /// after [`GRACE`].
    fn close(&self, task: Task) {
        let Some(pid) = Pid::from_raw(task.group) else {
            return;
        };
        if let Err(error) = kill_process_group(pid, Signal::TERM) {
            log::warn!("stopping {} failed: {error}", task.id);
            return;
        }
        // A frozen app must run to handle SIGTERM.
        signal(task.group, Signal::CONT);
        let name = task.id.clone();
        let spawned = std::thread::Builder::new()
            .name("rusttv-app-close".into())
            .spawn(move || {
                std::thread::sleep(GRACE);
                if test_kill_process_group(pid).is_ok() {
                    log::warn!("{} ignored SIGTERM; killing it", task.id);
                    let _ = kill_process_group(pid, Signal::KILL);
                }
            });
        if let Err(error) = spawned {
            log::warn!("no thread to finish closing {name}: {error}");
        }
    }
}

/// The process group of a running `program` (a file name) of this user that
/// leads a group of its own, as apps the launcher starts do: an app left
/// running by an earlier launcher. The program is the first argument, or
/// the second for an interpreter running a script.
pub fn find_group(program: &str) -> Option<u32> {
    use std::os::unix::fs::MetadataExt as _;
    let uid = rustix::process::getuid().as_raw();
    let me = std::process::id();
    let mut found: Vec<u32> = fs::read_dir("/proc")
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            if pid == me || entry.metadata().ok()?.uid() != uid {
                return None;
            }
            let dir = entry.path();
            let stat = fs::read_to_string(dir.join("stat")).ok()?;
            let cmdline = fs::read(dir.join("cmdline")).ok()?;
            (leads_group(&stat, pid) && runs(&cmdline, program)).then_some(pid)
        })
        .collect();
    // The oldest, should there be more.
    found.sort_unstable();
    found.first().copied()
}

/// Whether `/proc/<pid>/stat` says `pid` leads its process group.
fn leads_group(stat: &str, pid: u32) -> bool {
    // "pid (comm) state ppid pgrp …"; comm may hold spaces and parentheses.
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().nth(2))
        .and_then(|group| group.parse::<u32>().ok())
        == Some(pid)
}

/// Whether a `/proc/<pid>/cmdline` runs `program`.
fn runs(cmdline: &[u8], program: &str) -> bool {
    let name = |arg: &[u8]| {
        std::path::Path::new(&*String::from_utf8_lossy(arg))
            .file_name()
            .is_some_and(|name| name == program)
    };
    let mut args = cmdline.split(|&byte| byte == 0);
    args.next().is_some_and(name) || args.next().is_some_and(name)
}

/// Sends `sig` to process group `group`; logs a failure.
fn signal(group: i32, sig: Signal) {
    let Some(pid) = Pid::from_raw(group) else {
        return;
    };
    if let Err(error) = kill_process_group(pid, sig) {
        log::warn!("signal {sig:?} to process group {group} failed: {error}");
    }
}

/// `MemAvailable` from `/proc/meminfo`, in MiB.
/// What `/proc/meminfo` says, in MiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Memory {
    available: u64,
    swap_used: u64,
}

fn read_meminfo() -> Option<Memory> {
    parse_meminfo(&fs::read_to_string("/proc/meminfo").ok()?)
}

fn parse_meminfo(text: &str) -> Option<Memory> {
    let field = |name: &str| -> Option<u64> {
        let line = text.lines().find(|line| line.starts_with(name))?;
        line.get(name.len()..)?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    };
    let available = field("MemAvailable:")?;
    // No swap at all reads as none in use.
    let swap_used = field("SwapTotal:")
        .zip(field("SwapFree:"))
        .map_or(0, |(total, free)| total.saturating_sub(free));
    Some(Memory {
        available: available / 1024,
        swap_used: swap_used / 1024,
    })
}

/// Why a background app must close now, if it must.
fn memory_short(memory: Memory, min: Option<u64>, max_swap: Option<u64>) -> Option<&'static str> {
    if min.is_some_and(|min| memory.available < min) {
        Some("memory is low")
    } else if max_swap.is_some_and(|max| memory.swap_used > max) {
        Some("swap is in use")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_apps_by_program_and_group() {
        assert!(leads_group("23840 (dolphin) S 1 23840 23840 0", 23840));
        assert!(leads_group("77 (odd ) name) S 1 77 77", 77));
        assert!(!leads_group("23841 (dolphin) S 23840 23840 23840", 23841));
        assert!(runs(b"dolphin\0", "dolphin"));
        assert!(runs(b"/usr/bin/dolphin\0--new-window\0", "dolphin"));
        // A wrapper script under its interpreter.
        assert!(runs(b"/bin/sh\0/usr/bin/kodi\0", "kodi"));
        assert!(!runs(b"/usr/bin/dolphin-helper\0", "dolphin"));
        assert!(!runs(b"", "dolphin"));
    }

    #[test]
    fn reads_available_memory_and_swap_in_use() {
        let text = "MemTotal:  941056 kB\nMemFree:  61164 kB\nMemAvailable:  201600 kB\n\
                    SwapTotal:  941052 kB\nSwapFree:  335900 kB\n";
        assert_eq!(
            parse_meminfo(text),
            Some(Memory {
                available: 196,
                swap_used: 590
            })
        );
        let no_swap = "MemAvailable:  512000 kB\n";
        assert_eq!(
            parse_meminfo(no_swap).map(|memory| memory.swap_used),
            Some(0)
        );
        assert_eq!(parse_meminfo("MemFree: 1 kB\n"), None);
    }

    #[test]
    fn closes_for_low_memory_or_for_swap_in_use() {
        let memory = |available, swap_used| Memory {
            available,
            swap_used,
        };
        assert_eq!(
            memory_short(memory(250, 0), Some(300), Some(100)),
            Some("memory is low")
        );
        assert_eq!(
            memory_short(memory(400, 150), Some(300), Some(100)),
            Some("swap is in use")
        );
        assert_eq!(memory_short(memory(400, 50), Some(300), Some(100)), None);
        assert_eq!(memory_short(memory(10, 900), None, None), None);
    }

    #[test]
    fn a_kept_app_is_never_frozen_and_closes_last() {
        let limits = Limits {
            freeze_background: true,
            keep_running: vec!["com.moonlight_stream.Moonlight".into()],
            ..Limits::default()
        };
        assert!(limits.keeps_running("com.moonlight_stream.Moonlight.desktop"));
        assert!(limits.keeps_running("COM.moonlight_stream.moonlight"));
        assert!(!limits.keeps_running("org.kde.dolphin.desktop"));
        let tasks = Tasks::start(limits);
        let moonlight = "com.moonlight_stream.Moonlight.desktop";
        tasks.started(
            moonlight,
            4_000_010,
            vec!["com.moonlight_stream.moonlight".into()],
            false,
        );
        tasks.started(
            "org.kde.dolphin.desktop",
            4_000_011,
            vec!["org.kde.dolphin".into()],
            false,
        );
        tasks.started(
            "org.gnome.Loupe.desktop",
            4_000_012,
            vec!["org.gnome.loupe".into()],
            false,
        );
        tasks.activated("org.gnome.loupe");
        let frozen = |id: &str| tasks.lock().iter().any(|task| task.id == id && task.frozen);
        assert!(!frozen(moonlight), "a game stream keeps running");
        assert!(frozen("org.kde.dolphin.desktop"));
        // Moonlight is the least recently used, yet Dolphin goes first; the
        // app in front never does.
        assert!(tasks.close_one_in_background("test"));
        assert!(!tasks.is_running("org.kde.dolphin.desktop"));
        assert!(tasks.is_running(moonlight));
        assert!(tasks.close_one_in_background("test"));
        assert!(!tasks.is_running(moonlight));
        assert!(!tasks.close_one_in_background("test"));
        assert!(tasks.is_running("org.gnome.Loupe.desktop"));
    }

    #[test]
    fn reuses_a_running_app_and_forgets_it_when_it_ends() {
        let tasks = Tasks::start(Limits::default());
        assert_eq!(tasks.reuse("kodi.desktop"), None);
        tasks.started("kodi.desktop", 4_000_000, vec!["kodi".into()], true);
        assert_eq!(tasks.reuse("kodi.desktop"), Some(vec!["kodi".to_owned()]));
        tasks.ended(4_000_000);
        assert_eq!(tasks.reuse("kodi.desktop"), None);
    }

    #[test]
    fn remembers_the_app_last_in_front_but_not_the_launcher() {
        let limits = Limits {
            freeze_background: false,
            ..Limits::default()
        };
        let tasks = Tasks::start(limits);
        assert_eq!(tasks.front(), None);
        tasks.started(
            "org.kde.dolphin.desktop",
            4_000_001,
            vec!["org.kde.dolphin".into()],
            false,
        );
        assert_eq!(tasks.activated("org.kde.dolphin"), Some(false));
        // The launcher's own window comes to the front: Dolphin stays the one.
        assert_eq!(tasks.activated(""), None);
        assert_eq!(tasks.front().as_deref(), Some("org.kde.dolphin.desktop"));
        tasks.ended(4_000_001);
        assert_eq!(tasks.front(), None);
    }
}
