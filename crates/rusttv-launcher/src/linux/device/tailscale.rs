//! Settings → Tailscale, through the `tailscale` command as the launcher's
//! user (Tailscale's operator: `tailscale set --operator=<user>` once, as
//! root): whether it is connected, this device's Tailscale address and
//! name, the other devices on the tailnet, and on and off.
//!
//! Signing in shows Tailscale's sign-in link as a QR code
//! ([`ControlKind::Code`]) to open on a phone: there is no browser on the
//! TV. The tab is there only where `tailscale` is installed.

use std::collections::BTreeMap;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;

use super::{control, run};
use crate::device::{Control, ControlKind, Tab, Value};

/// The tab's id.
pub(super) const TAB: &str = "tailscale";

/// The most other devices the tab lists (online ones first).
const MAX_PEERS: usize = 16;

/// How long signing in waits for Tailscale's sign-in link.
const LINK_WAIT: Duration = Duration::from_secs(10);

/// What `tailscale status --json` says (the parts shown).
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct Status {
    backend_state: String,
    #[serde(rename = "AuthURL")]
    auth_url: String,
    #[serde(rename = "Self")]
    me: Option<Node>,
    peer: BTreeMap<String, Node>,
    current_tailnet: Option<Tailnet>,
}

/// A device on the tailnet.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct Node {
    host_name: String,
    #[serde(rename = "DNSName")]
    dns_name: String,
    #[serde(rename = "OS")]
    os: String,
    #[serde(rename = "TailscaleIPs")]
    addresses: Vec<String>,
    online: bool,
    /// RFC 3339; the zero time ("0001-01-01…") when never seen.
    last_seen: String,
    /// RFC 3339; absent when the key never expires.
    key_expiry: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct Tailnet {
    /// The account (an e-mail address for a personal tailnet).
    name: String,
}

impl Node {
    /// Its IPv4 Tailscale address, else its first one.
    fn address(&self) -> &str {
        self.addresses
            .iter()
            .find(|address| !address.contains(':'))
            .or_else(|| self.addresses.first())
            .map_or("", String::as_str)
    }
}

/// The tab, or `None` when Tailscale's daemon doesn't answer.
pub(super) fn tab() -> Option<Tab> {
    let status = run("tailscale", &["status", "--json"])
        .ok()
        .and_then(|text| parse(&text))?;
    Some(Tab {
        id: TAB.to_owned(),
        title: "Tailscale".to_owned(),
        controls: rows(&status),
    })
}

fn parse(text: &str) -> Option<Status> {
    serde_json::from_str(text)
        .map_err(|error| log::warn!("tailscale status: {error}"))
        .ok()
}

/// Whether it signs in before it can connect.
fn needs_login(state: &str) -> bool {
    matches!(state, "NeedsLogin" | "NoState")
}

fn rows(status: &Status) -> Vec<Control> {
    let state = status.backend_state.as_str();
    let (on, detail) = match state {
        "Running" => (true, "Connected"),
        "Starting" => (true, "Connecting…"),
        "NeedsMachineAuth" => (true, "Waiting for approval in Tailscale's admin console"),
        "Stopped" => (false, "Off"),
        _ if needs_login(state) => (false, "Not signed in"),
        _ => (false, state),
    };
    let mut rows = vec![control(
        "tailscale",
        "Tailscale",
        detail,
        ControlKind::Toggle { on },
    )];
    if needs_login(state) {
        rows.push(if status.auth_url.is_empty() {
            control("login", "Sign in", "", ControlKind::Action)
        } else {
            control(
                "login-code",
                "Scan with your phone to sign in",
                &status.auth_url,
                ControlKind::Code,
            )
        });
        return rows;
    }
    if let Some(me) = &status.me {
        rows.push(control(
            "address",
            "This device",
            &format!("{} · {}", me.host_name, me.address()),
            ControlKind::Info,
        ));
        let name = me.dns_name.trim_end_matches('.');
        if !name.is_empty() {
            rows.push(control("name", "Name", name, ControlKind::Info));
        }
        if let Some(expiry) = me.key_expiry.as_deref().and_then(date) {
            rows.push(control(
                "expiry",
                "Sign-in expires",
                expiry,
                ControlKind::Info,
            ));
        }
    }
    if let Some(tailnet) = status
        .current_tailnet
        .as_ref()
        .filter(|t| !t.name.is_empty())
    {
        rows.push(control(
            "account",
            "Account",
            &tailnet.name,
            ControlKind::Info,
        ));
    }
    let mut peers: Vec<&Node> = status.peer.values().collect();
    peers.sort_by(|a, b| {
        b.online
            .cmp(&a.online)
            .then_with(|| a.host_name.to_lowercase().cmp(&b.host_name.to_lowercase()))
    });
    for peer in peers.into_iter().take(MAX_PEERS) {
        let seen = if peer.online {
            "Online".to_owned()
        } else {
            date(&peer.last_seen).map_or_else(
                || "Offline".to_owned(),
                |day| format!("Offline, last seen {day}"),
            )
        };
        rows.push(control(
            &format!("peer:{}", peer.address()),
            &peer.host_name,
            &format!("{} · {} · {seen}", peer.address(), os_name(&peer.os)),
            ControlKind::Info,
        ));
    }
    rows
}

/// The day of an RFC 3339 time ("2026-11-15"); `None` for the zero time.
fn date(time: &str) -> Option<&str> {
    time.get(..10)
        .filter(|day| !day.starts_with("0001") && day.as_bytes().get(4) == Some(&b'-'))
}

fn os_name(os: &str) -> &str {
    match os {
        "linux" => "Linux",
        "windows" => "Windows",
        "android" => "Android",
        "macOS" | "darwin" => "macOS",
        "iOS" | "ios" => "iOS",
        "freebsd" => "FreeBSD",
        other => other,
    }
}

/// Changes a Tailscale control; `None` when `control` isn't one.
pub(super) fn set(control: &str, value: &Value) -> Option<Result<(), String>> {
    Some(match (control, value) {
        ("tailscale", Value::Toggle(false)) => run("tailscale", &["down"]).map(drop),
        ("tailscale", Value::Toggle(true)) => {
            let state = run("tailscale", &["status", "--json"])
                .ok()
                .and_then(|text| parse(&text))
                .map(|status| status.backend_state)
                .unwrap_or_default();
            if needs_login(&state) {
                sign_in()
            } else {
                run("tailscale", &["up", "--timeout=10s"]).map(drop)
            }
        }
        ("login", Value::Action) => sign_in(),
        _ => return None,
    })
}

/// Starts signing in: `tailscale up` waits (up to ten minutes) for the
/// sign-in on a phone, in the background; returns once Tailscale has the
/// sign-in link, which the tab then shows as a QR code.
///
/// Plain `up`, so the settings made at install (`--netfilter-mode` and
/// the like) stay as they are.
fn sign_in() -> Result<(), String> {
    let mut child = Command::new("tailscale")
        .args(["up", "--timeout=10m"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("tailscale: {error}"))?;
    // Reaped when it ends: signed in, or given up after ten minutes.
    let reaper = std::thread::Builder::new()
        .name("rusttv-tailscale".to_owned())
        .spawn(move || child.wait());
    if let Err(error) = reaper {
        return Err(format!("no thread to sign in: {error}"));
    }
    let deadline = Instant::now() + LINK_WAIT;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
        let status = run("tailscale", &["status", "--json"])
            .ok()
            .and_then(|text| parse(&text));
        match status {
            Some(status) if !status.auth_url.is_empty() || !needs_login(&status.backend_state) => {
                return Ok(());
            }
            _ => {}
        }
    }
    Err("Tailscale gave no sign-in link".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUNNING: &str = r#"{
        "BackendState": "Running",
        "AuthURL": "",
        "Self": {
            "HostName": "hy320f",
            "DNSName": "hy320f.tail123.ts.net.",
            "OS": "linux",
            "TailscaleIPs": ["100.95.140.16", "fd7a:115c:a1e0::1"],
            "Online": true,
            "KeyExpiry": "2027-05-17T17:17:14Z"
        },
        "Peer": {
            "nodekey:a": {
                "HostName": "phone",
                "OS": "android",
                "TailscaleIPs": ["fd7a:115c:a1e0::2", "100.64.44.124"],
                "Online": false,
                "LastSeen": "2026-11-03T10:00:00Z"
            },
            "nodekey:b": {
                "HostName": "OpenWrt",
                "OS": "linux",
                "TailscaleIPs": ["100.119.42.124"],
                "Online": true,
                "LastSeen": "0001-01-01T00:00:00Z"
            },
            "nodekey:c": {
                "HostName": "old-laptop",
                "OS": "windows",
                "TailscaleIPs": ["100.70.110.126"],
                "Online": false,
                "LastSeen": "0001-01-01T00:00:00Z"
            }
        },
        "CurrentTailnet": { "Name": "someone@example.com", "MagicDNSSuffix": "tail123.ts.net" },
        "Unknown": [1, 2, 3]
    }"#;

    fn row<'a>(rows: &'a [Control], id: &str) -> &'a Control {
        rows.iter()
            .find(|row| row.id == id)
            .unwrap_or_else(|| panic!("no row {id}"))
    }

    #[test]
    fn a_connected_device_shows_its_address_and_the_others_online_first() {
        let status = parse(RUNNING).expect("status");
        let rows = rows(&status);
        assert_eq!(
            row(&rows, "tailscale").kind,
            ControlKind::Toggle { on: true }
        );
        assert_eq!(row(&rows, "tailscale").detail, "Connected");
        assert_eq!(row(&rows, "address").detail, "hy320f · 100.95.140.16");
        assert_eq!(row(&rows, "name").detail, "hy320f.tail123.ts.net");
        assert_eq!(row(&rows, "expiry").detail, "2027-05-17");
        assert_eq!(row(&rows, "account").detail, "someone@example.com");
        let peers: Vec<&str> = rows
            .iter()
            .filter(|row| row.id.starts_with("peer:"))
            .map(|row| row.label.as_str())
            .collect();
        assert_eq!(peers, ["OpenWrt", "old-laptop", "phone"]);
        assert_eq!(
            row(&rows, "peer:100.64.44.124").detail,
            "100.64.44.124 · Android · Offline, last seen 2026-11-03"
        );
        assert_eq!(
            row(&rows, "peer:100.70.110.126").detail,
            "100.70.110.126 · Windows · Offline"
        );
        assert!(!rows.iter().any(|row| row.kind == ControlKind::Code));
    }

    #[test]
    fn signing_in_shows_the_link_as_a_code_once_tailscale_has_one() {
        let waiting = parse(r#"{"BackendState": "NeedsLogin", "AuthURL": ""}"#).expect("status");
        let rows_now = rows(&waiting);
        assert_eq!(rows_now.len(), 2);
        assert_eq!(row(&rows_now, "tailscale").detail, "Not signed in");
        assert_eq!(row(&rows_now, "login").kind, ControlKind::Action);

        let link = parse(
            r#"{"BackendState": "NeedsLogin", "AuthURL": "https://login.tailscale.com/a/abc"}"#,
        )
        .expect("status");
        let rows_then = rows(&link);
        let code = row(&rows_then, "login-code");
        assert_eq!(code.kind, ControlKind::Code);
        assert_eq!(code.detail, "https://login.tailscale.com/a/abc");
    }

    #[test]
    fn a_stopped_tailscale_is_off_and_odd_states_show_as_they_are() {
        let stopped = parse(r#"{"BackendState": "Stopped"}"#).expect("status");
        assert_eq!(
            row(&rows(&stopped), "tailscale").kind,
            ControlKind::Toggle { on: false }
        );
        let odd = parse(r#"{"BackendState": "InUseOtherUser"}"#).expect("status");
        assert_eq!(row(&rows(&odd), "tailscale").detail, "InUseOtherUser");
        assert!(parse("not json").is_none());
    }

    #[test]
    fn only_tailscale_controls_are_taken() {
        assert!(set("wifi", &Value::Toggle(true)).is_none());
        assert!(set("login", &Value::Toggle(true)).is_none());
    }

    #[test]
    fn dates_skip_the_zero_time() {
        assert_eq!(date("2026-11-03T10:00:00Z"), Some("2026-11-03"));
        assert_eq!(date("0001-01-01T00:00:00Z"), None);
        assert_eq!(date(""), None);
    }
}
