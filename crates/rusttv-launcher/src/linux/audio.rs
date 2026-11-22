//! Mutes an app's sound while it runs in the background: an app in
//! [`super::tasks::Limits::keep_running`] (a game stream) keeps playing when
//! its window leaves the front, since it is not frozen. Uses PipeWire's own
//! tools (`pw-dump`, `wpctl`), so it works for any app on the standard
//! Linux audio stack, Flatpaks included.

use std::collections::HashSet;
use std::fs;
use std::process::{Command, Stdio};

use serde_json::Value;

/// Mutes (or unmutes) every audio output stream of the processes in process
/// group `group`, on a thread of its own.
pub fn set_muted(name: String, group: i32, muted: bool) {
    let spawned = std::thread::Builder::new()
        .name("rusttv-app-mute".into())
        .spawn(move || {
            let pids = group_pids(group);
            let Some(dump) = pw_dump() else {
                return;
            };
            for node in output_streams(&dump, &pids) {
                let ok = Command::new("wpctl")
                    .args(["set-mute", &node.to_string(), if muted { "1" } else { "0" }])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|status| status.success());
                if ok {
                    let what = if muted { "muted" } else { "unmuted" };
                    log::info!("{what} {name} (stream {node})");
                } else {
                    log::warn!("could not mute stream {node} of {name}");
                }
            }
        });
    if let Err(error) = spawned {
        log::warn!("no thread to mute an app: {error}");
    }
}

/// The processes in process group `group`.
fn group_pids(group: i32) -> HashSet<u64> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return HashSet::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let pid: u64 = entry.file_name().to_str()?.parse().ok()?;
            let stat = fs::read_to_string(entry.path().join("stat")).ok()?;
            (stat_group(&stat)? == i64::from(group)).then_some(pid)
        })
        .collect()
}

/// The process group in a `/proc/<pid>/stat` line.
fn stat_group(stat: &str) -> Option<i64> {
    // "pid (comm) state ppid pgrp …"; comm may hold spaces and parentheses.
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace().nth(2)?.parse().ok()
}

fn pw_dump() -> Option<Value> {
    let output = Command::new("pw-dump")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        log::warn!("pw-dump failed");
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

/// The audio output stream nodes in `dump` (`pw-dump` output) that belong
/// to one of `pids`: by the node's own process ID or that of its client.
fn output_streams(dump: &Value, pids: &HashSet<u64>) -> Vec<u64> {
    let Some(objects) = dump.as_array() else {
        return Vec::new();
    };
    let props = |object: &Value| object.pointer("/info/props").cloned();
    let pid_of = |props: &Value| {
        let pid = props.get("application.process.id")?;
        pid.as_u64().or_else(|| pid.as_str()?.parse().ok())
    };
    let clients: HashSet<u64> = objects
        .iter()
        .filter(|object| {
            object
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.ends_with(":Client"))
        })
        .filter_map(|object| {
            let id = object.get("id")?.as_u64()?;
            pids.contains(&pid_of(&props(object)?)?).then_some(id)
        })
        .collect();
    objects
        .iter()
        .filter_map(|object| {
            let props = props(object)?;
            if props.get("media.class")?.as_str()? != "Stream/Output/Audio" {
                return None;
            }
            let client = props
                .get("client.id")
                .and_then(|id| id.as_u64().or_else(|| id.as_str()?.parse().ok()));
            let ours = pid_of(&props).is_some_and(|pid| pids.contains(&pid))
                || client.is_some_and(|client| clients.contains(&client));
            ours.then(|| object.get("id")?.as_u64()).flatten()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_process_group() {
        assert_eq!(stat_group("42 (a (b) c) S 1 40 40 0"), Some(40));
    }

    #[test]
    fn finds_the_output_streams_of_a_process_group() {
        let dump: Value = serde_json::from_str(
            r#"[
              {"id": 30, "type": "PipeWire:Interface:Client",
               "info": {"props": {"application.process.id": 7}}},
              {"id": 31, "type": "PipeWire:Interface:Client",
               "info": {"props": {"application.process.id": 9}}},
              {"id": 50, "type": "PipeWire:Interface:Node",
               "info": {"props": {"media.class": "Stream/Output/Audio", "client.id": 30}}},
              {"id": 51, "type": "PipeWire:Interface:Node",
               "info": {"props": {"media.class": "Stream/Output/Audio",
                                  "application.process.id": "8"}}},
              {"id": 52, "type": "PipeWire:Interface:Node",
               "info": {"props": {"media.class": "Stream/Input/Audio", "client.id": 30}}},
              {"id": 53, "type": "PipeWire:Interface:Node",
               "info": {"props": {"media.class": "Stream/Output/Audio", "client.id": 31}}}
            ]"#,
        )
        .unwrap_or(Value::Null);
        let pids = HashSet::from([7, 8]);
        assert_eq!(output_streams(&dump, &pids), vec![50, 51]);
    }
}
