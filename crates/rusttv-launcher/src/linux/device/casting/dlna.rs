//! Built-in UPnP / DLNA MediaRenderer (`MediaRenderer:1`) backed by `mpv`.
//!
//! Why built-in instead of `gmediarender` when `mpv` is available:
//! - Full support for modern Samsung / iPhone photo formats (`image/heic`,
//!   `image/heif`, `image/avif`, `image/webp`) alongside `image/jpeg`,
//!   `image/png`, and `video/mp4` / `video/quicktime` / `video/x-matroska`
//!   (GStreamer on Debian lacks HEIC/HEIF typefind and decoders).
//! - Instant flicker-free photo swiping by reusing a single fullscreen `mpv`
//!   window over its JSON IPC socket (`loadfile ... replace`).
//! - Clean teardown back to RustTV Home on DLNA `Stop` or remote `Back`/`ESC`.
//!
//! Sockets use `rustix::net` + `std::net` with zero `unsafe`.

use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, poll};
use rustix::net::{AddressFamily, SocketFlags, SocketType, bind, socket_with, sockopt};

const SSDP_MCAST: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const SSDP_PORT: u16 = 1900;
const PREFERRED_HTTP_PORT: u16 = 49495;
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 256 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const SSDP_NOTIFY_INTERVAL: Duration = Duration::from_secs(60);

/// All media types advertised via `ConnectionManager:1#GetProtocolInfo`.
const PROTOCOL_INFO_SINK: &str = "\
http-get:*:image/heic:*,\
http-get:*:image/heif:*,\
http-get:*:image/avif:*,\
http-get:*:image/jpeg:*,\
http-get:*:image/png:*,\
http-get:*:image/webp:*,\
http-get:*:image/gif:*,\
http-get:*:image/bmp:*,\
http-get:*:video/mp4:*,\
http-get:*:video/quicktime:*,\
http-get:*:video/x-matroska:*,\
http-get:*:video/webm:*,\
http-get:*:video/mpeg:*,\
http-get:*:video/mp2t:*,\
http-get:*:video/3gpp:*,\
http-get:*:video/x-msvideo:*,\
http-get:*:audio/mpeg:*,\
http-get:*:audio/mp4:*,\
http-get:*:audio/aac:*,\
http-get:*:audio/flac:*,\
http-get:*:audio/ogg:*,\
http-get:*:audio/wav:*,\
http-get:*:*:*";

/// UPnP search targets our `MediaRenderer:1` answers to.
const DEVICE_TYPE: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
const AVT_TYPE: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const CM_TYPE: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";
const RC_TYPE: &str = "urn:schemas-upnp-org:service:RenderingControl:1";

/// Running built-in DLNA MediaRenderer instance. Dropping it stops both
/// background threads, sends SSDP `byebye`, and closes any active `mpv` window.
#[derive(Debug)]
pub(super) struct Server {
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<RendererState>>,
    http_thread: Option<JoinHandle<()>>,
    ssdp_thread: Option<JoinHandle<()>>,
}

impl Server {
    /// Starts the SSDP announcer/responder and UPnP HTTP/SOAP server.
    pub(super) fn start(device_name: &str, wayland: bool) -> Result<Self, String> {
        let listener = TcpListener::bind(SocketAddrV4::new(
            Ipv4Addr::UNSPECIFIED,
            PREFERRED_HTTP_PORT,
        ))
        .or_else(|_| TcpListener::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)))
        .map_err(|error| format!("DLNA HTTP bind: {error}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("DLNA HTTP nonblocking: {error}"))?;
        let http_port = listener
            .local_addr()
            .map_err(|error| format!("DLNA HTTP local_addr: {error}"))?
            .port();

        let udn = stable_udn(device_name);
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(RendererState::new(
            device_name.to_owned(),
            udn.clone(),
            wayland,
        )));

        let http_stop = Arc::clone(&stop);
        let http_state = Arc::clone(&state);
        let http_thread = thread::Builder::new()
            .name("rusttv-dlna-http".to_owned())
            .spawn(move || run_http(listener, http_stop, http_state))
            .map_err(|error| format!("DLNA HTTP thread: {error}"))?;

        let ssdp_stop = Arc::clone(&stop);
        let ssdp_udn = udn;
        let ssdp_thread = thread::Builder::new()
            .name("rusttv-dlna-ssdp".to_owned())
            .spawn(move || run_ssdp(http_port, &ssdp_udn, ssdp_stop))
            .map_err(|error| format!("DLNA SSDP thread: {error}"))?;

        log::info!("Built-in DLNA (mpv) renderer listening on port {http_port} as {device_name:?}");
        Ok(Self {
            stop,
            state,
            http_thread: Some(http_thread),
            ssdp_thread: Some(ssdp_thread),
        })
    }

    pub(super) fn is_running(&self) -> bool {
        !self.stop.load(Ordering::Relaxed)
            && self
                .http_thread
                .as_ref()
                .is_some_and(|handle| !handle.is_finished())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.http_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.ssdp_thread.take() {
            let _ = handle.join();
        }
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.stop_playback();
        state.cleanup_files();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransportState {
    Stopped,
    Playing,
    Paused,
}

impl TransportState {
    fn as_upnp(self) -> &'static str {
        match self {
            Self::Stopped => "STOPPED",
            Self::Playing => "PLAYING",
            Self::Paused => "PAUSED_PLAYBACK",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventService {
    AvTransport,
    ConnMgr,
    RenderingControl,
}

impl EventService {
    fn from_path(path: &str) -> Option<Self> {
        match path {
            "/event/avtransport" => Some(Self::AvTransport),
            "/event/connmgr" => Some(Self::ConnMgr),
            "/event/renderingcontrol" => Some(Self::RenderingControl),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CallbackTarget {
    host: String,
    port: u16,
    path: String,
}

#[derive(Debug, Clone)]
struct Subscription {
    sid: String,
    service: EventService,
    target: CallbackTarget,
    next_seq: u32,
}

#[derive(Debug, Clone)]
struct PendingNotify {
    sid: String,
    target: CallbackTarget,
    seq: u32,
    body: String,
}

const MAX_SUBSCRIPTIONS: usize = 16;

#[derive(Debug)]
struct RendererState {
    device_name: String,
    udn: String,
    wayland: bool,
    current_uri: String,
    current_metadata: String,
    loaded_uri: Option<String>,
    transport: TransportState,
    volume: u8,
    muted: bool,
    mpv: Option<Child>,
    ipc_path: PathBuf,
    input_conf_path: PathBuf,
    subscriptions: Vec<Subscription>,
    next_sub_id: u64,
    /// Incremented on every `set_uri` / `play` / `stop` so a delayed post-Stop
    /// window close only fires if no new photo arrived during a Gallery swipe.
    playback_gen: u64,
}

impl RendererState {
    fn new(device_name: String, udn: String, wayland: bool) -> Self {
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let pid = std::process::id();
        Self {
            device_name,
            udn,
            wayland,
            current_uri: String::new(),
            current_metadata: String::new(),
            loaded_uri: None,
            transport: TransportState::Stopped,
            volume: 100,
            muted: false,
            mpv: None,
            ipc_path: runtime_dir.join(format!("rusttv-dlna-{pid}.sock")),
            input_conf_path: runtime_dir.join(format!("rusttv-dlna-{pid}.conf")),
            subscriptions: Vec::new(),
            next_sub_id: 1,
            playback_gen: 0,
        }
    }

    fn subscribe(
        &mut self,
        service: EventService,
        callback_header: Option<&str>,
        sid_header: Option<&str>,
    ) -> Result<(String, Option<PendingNotify>), ()> {
        // Subscription renewal (SID present, no CALLBACK).
        if callback_header.is_none()
            && let Some(sid) = sid_header.map(str::trim).filter(|s| !s.is_empty())
        {
            if self.subscriptions.iter().any(|sub| sub.sid == sid) {
                return Ok((sid.to_owned(), None));
            }
            return Err(());
        }
        let target = parse_callback_url(callback_header.ok_or(())?).ok_or(())?;
        let sid = format!("uuid:rusttv-dlna-sub-{}", self.next_sub_id);
        self.next_sub_id = self.next_sub_id.wrapping_add(1).max(1);
        if self.subscriptions.len() >= MAX_SUBSCRIPTIONS {
            self.subscriptions.remove(0);
        }
        let body = self.event_xml(service);
        self.subscriptions.push(Subscription {
            sid: sid.clone(),
            service,
            target: target.clone(),
            next_seq: 1,
        });
        Ok((
            sid.clone(),
            Some(PendingNotify {
                sid,
                target,
                seq: 0,
                body,
            }),
        ))
    }

    fn unsubscribe(&mut self, sid_header: Option<&str>) {
        if let Some(sid) = sid_header.map(str::trim) {
            self.subscriptions.retain(|sub| sub.sid != sid);
        }
    }

    fn prepare_notifies(&mut self, service: EventService) -> Vec<PendingNotify> {
        let body = self.event_xml(service);
        let mut out = Vec::new();
        for sub in &mut self.subscriptions {
            if sub.service == service {
                let seq = sub.next_seq;
                sub.next_seq = sub.next_seq.wrapping_add(1).max(1);
                out.push(PendingNotify {
                    sid: sub.sid.clone(),
                    target: sub.target.clone(),
                    seq,
                    body: body.clone(),
                });
            }
        }
        out
    }

    fn event_xml(&self, service: EventService) -> String {
        match service {
            EventService::AvTransport => {
                avtransport_event_xml(self.transport, &self.current_uri, &self.current_metadata)
            }
            EventService::RenderingControl => renderingcontrol_event_xml(self.volume, self.muted),
            EventService::ConnMgr => connmgr_event_xml(),
        }
    }

    fn mpv_alive(&mut self) -> bool {
        let Some(child) = self.mpv.as_mut() else {
            return false;
        };
        match child.try_wait() {
            Ok(None) => true,
            Ok(Some(_)) | Err(_) => {
                self.mpv = None;
                self.loaded_uri = None;
                self.transport = TransportState::Stopped;
                false
            }
        }
    }

    fn set_uri(&mut self, uri: String, metadata: String) -> Result<(), String> {
        if !uri.is_empty() && !is_allowed_media_uri(&uri) {
            return Err("only http:// and https:// media URIs are supported".to_owned());
        }
        self.playback_gen = self.playback_gen.wrapping_add(1);
        self.current_uri = uri;
        self.current_metadata = metadata;
        // When the user swipes to the next photo in Samsung Gallery while mpv
        // is already open (even right after a soft Stop), load it immediately
        // in the warm window so transitions avoid a cold OpenGL/Wayland launch.
        if !self.current_uri.is_empty() && self.mpv_alive() {
            let next = self.current_uri.clone();
            if self.load_in_running_mpv(&next).is_ok() {
                self.loaded_uri = Some(next);
            }
        }
        Ok(())
    }

    fn play(&mut self) -> Result<(), String> {
        if self.current_uri.is_empty() {
            return Err("no media URI set".to_owned());
        }
        self.playback_gen = self.playback_gen.wrapping_add(1);
        let uri = self.current_uri.clone();
        if self.mpv_alive() {
            if self.loaded_uri.as_deref() != Some(uri.as_str()) {
                if self.load_in_running_mpv(&uri).is_err() {
                    self.stop_playback();
                    self.spawn_mpv(&uri)?;
                } else {
                    self.loaded_uri = Some(uri);
                }
            }
            let _ = self.ipc_command(&serde_json::json!(["set_property", "pause", false]));
        } else {
            self.spawn_mpv(&uri)?;
        }
        self.transport = TransportState::Playing;
        Ok(())
    }

    /// Marks playback stopped while keeping `mpv` warm for a short grace period
    /// (returns the generation token for delayed teardown) so Samsung Gallery's
    /// `Stop → SetAVTransportURI → Play` swipe sequence never cold-restarts mpv.
    fn soft_stop(&mut self) -> u64 {
        self.playback_gen = self.playback_gen.wrapping_add(1);
        self.loaded_uri = None;
        self.transport = TransportState::Stopped;
        self.playback_gen
    }

    fn pause(&mut self) {
        if self.mpv_alive() {
            let _ = self.ipc_command(&serde_json::json!(["set_property", "pause", true]));
            self.transport = TransportState::Paused;
        }
    }

    fn stop_playback(&mut self) {
        if self.mpv_alive() {
            let _ = self.ipc_command(&serde_json::json!(["quit"]));
        }
        if let Some(mut child) = self.mpv.take() {
            // Give mpv a short moment to exit cleanly after IPC quit before SIGKILL.
            let deadline = Instant::now() + Duration::from_millis(400);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) | Err(_) => break,
                    Ok(None) if Instant::now() >= deadline => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    Ok(None) => thread::sleep(Duration::from_millis(25)),
                }
            }
        }
        let _ = fs::remove_file(&self.ipc_path);
        self.loaded_uri = None;
        self.transport = TransportState::Stopped;
    }

    fn seek_seconds(&mut self, seconds: u64) {
        if self.mpv_alive() {
            let _ = self.ipc_command(&serde_json::json!(["seek", seconds, "absolute"]));
        }
    }

    fn set_volume(&mut self, vol: u8) {
        self.volume = vol.min(100);
        if self.mpv_alive() {
            let _ = self.ipc_command(&serde_json::json!(["set_property", "volume", self.volume]));
        }
    }

    fn set_mute(&mut self, muted: bool) {
        self.muted = muted;
        if self.mpv_alive() {
            let _ = self.ipc_command(&serde_json::json!(["set_property", "mute", self.muted]));
        }
    }

    fn position_info(&mut self) -> (String, String) {
        if !self.mpv_alive() {
            return ("00:00:00".to_owned(), "00:00:00".to_owned());
        }
        let pos = self
            .ipc_get_f64("time-pos")
            .map(format_hms)
            .unwrap_or_else(|| "00:00:00".to_owned());
        let dur = self
            .ipc_get_f64("duration")
            .map(format_hms)
            .unwrap_or_else(|| "00:00:00".to_owned());
        (pos, dur)
    }

    fn current_transport_state(&mut self) -> TransportState {
        if !self.mpv_alive() {
            return TransportState::Stopped;
        }
        if self.ipc_get_bool("eof-reached") == Some(true)
            && self.ipc_get_f64("duration").is_some_and(|d| d > 0.5)
        {
            return TransportState::Stopped;
        }
        if let Some(paused) = self.ipc_get_bool("pause") {
            self.transport = if paused {
                TransportState::Paused
            } else {
                TransportState::Playing
            };
        }
        self.transport
    }

    fn spawn_mpv(&mut self, uri: &str) -> Result<(), String> {
        let _ = fs::remove_file(&self.ipc_path);
        let _ = fs::write(
            &self.input_conf_path,
            "ESC quit\nBS quit\nXF86Back quit\nq quit\nSPACE cycle pause\nENTER cycle pause\n",
        );

        let args = mpv_args(
            uri,
            self.wayland,
            &self.ipc_path,
            &self.input_conf_path,
            self.volume,
            self.muted,
        );
        let child = Command::new("mpv")
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("mpv: {error}"))?;
        self.mpv = Some(child);
        self.loaded_uri = Some(uri.to_owned());
        Ok(())
    }

    fn load_in_running_mpv(&self, uri: &str) -> Result<(), ()> {
        self.ipc_command(&serde_json::json!(["loadfile", uri, "replace"]))
            .map(|_| ())
    }

    fn ipc_command(&self, command: &serde_json::Value) -> Result<serde_json::Value, ()> {
        let mut stream = UnixStream::connect(&self.ipc_path).map_err(|_| ())?;
        let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
        let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
        let mut line = serde_json::json!({ "command": command }).to_string();
        line.push('\n');
        stream.write_all(line.as_bytes()).map_err(|_| ())?;
        let mut reader = BufReader::new(stream);
        let mut buf = String::new();
        // Skip asynchronous mpv events until we read the command response.
        for _ in 0..8 {
            buf.clear();
            let read = reader.read_line(&mut buf).map_err(|_| ())?;
            if read == 0 {
                return Err(());
            }
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&buf)
                && val.get("error").is_some()
            {
                return if val.get("error").and_then(|e| e.as_str()) == Some("success") {
                    Ok(val)
                } else {
                    Err(())
                };
            }
        }
        Err(())
    }

    fn ipc_get_f64(&self, prop: &str) -> Option<f64> {
        let resp = self
            .ipc_command(&serde_json::json!(["get_property", prop]))
            .ok()?;
        resp.get("data").and_then(serde_json::Value::as_f64)
    }

    fn ipc_get_bool(&self, prop: &str) -> Option<bool> {
        let resp = self
            .ipc_command(&serde_json::json!(["get_property", prop]))
            .ok()?;
        resp.get("data").and_then(serde_json::Value::as_bool)
    }

    fn cleanup_files(&self) {
        let _ = fs::remove_file(&self.ipc_path);
        let _ = fs::remove_file(&self.input_conf_path);
    }
}

fn mpv_args(
    uri: &str,
    wayland: bool,
    ipc_path: &Path,
    input_conf_path: &Path,
    volume: u8,
    muted: bool,
) -> Vec<String> {
    let mut args = vec![
        "--profile=fast".to_owned(),
        "--fs".to_owned(),
        "--idle=yes".to_owned(),
        "--keep-open=yes".to_owned(),
        "--image-display-duration=inf".to_owned(),
        "--force-window=immediate".to_owned(),
        "--osc=no".to_owned(),
        "--osd-level=1".to_owned(),
        "--cursor-autohide=always".to_owned(),
        "--no-terminal".to_owned(),
        "--title=RustTV Cast".to_owned(),
        // Decode HEIC/HEVC tiles across all CPU cores and keep memory/GPU
        // scaling fast for 12MP–50MP phone photos on Mali-G31 Panfrost.
        "--vd-lavc-threads=4".to_owned(),
        "--hwdec=auto-safe".to_owned(),
        "--scale=bilinear".to_owned(),
        "--dscale=bilinear".to_owned(),
        "--cscale=bilinear".to_owned(),
        "--correct-downscaling=no".to_owned(),
        "--demuxer-max-bytes=16MiB".to_owned(),
        "--demuxer-max-back-bytes=4MiB".to_owned(),
        format!("--volume={volume}"),
        format!("--mute={}", if muted { "yes" } else { "no" }),
        format!("--input-ipc-server={}", ipc_path.display()),
        format!("--input-conf={}", input_conf_path.display()),
    ];
    if wayland {
        args.push("--wayland-app-id=rusttv-cast".to_owned());
        // Explicit OpenGL on Wayland avoids libplacebo/Vulkan probing delays on
        // Mali-G31 Panfrost while keeping GPU scaling for photos and videos.
        args.push("--vo=gpu".to_owned());
        args.push("--gpu-api=opengl".to_owned());
        args.push("--gpu-context=wayland".to_owned());
    }
    args.push("--".to_owned());
    args.push(uri.to_owned());
    args
}

/// Only allow HTTP(S) URLs from LAN controllers — never local `file://` paths,
/// `av://` filters, or leading `-` flags.
fn is_allowed_media_uri(uri: &str) -> bool {
    let lower = uri.to_ascii_lowercase();
    (lower.starts_with("http://") || lower.starts_with("https://"))
        && !uri.chars().any(char::is_control)
}

fn run_http(listener: TcpListener, stop: Arc<AtomicBool>, state: Arc<Mutex<RendererState>>) {
    while !stop.load(Ordering::Relaxed) {
        let mut fds = [PollFd::new(&listener, PollFlags::IN)];
        match poll(
            &mut fds,
            Some(&rustix::event::Timespec {
                tv_sec: 0,
                tv_nsec: 250_000_000,
            }),
        ) {
            Ok(0) => continue,
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => {
                log::warn!("DLNA HTTP poll error: {error}");
                break;
            }
        }
        loop {
            match listener.accept() {
                Ok((stream, _peer)) => {
                    let st = Arc::clone(&state);
                    let _ = thread::Builder::new()
                        .name("rusttv-dlna-req".to_owned())
                        .spawn(move || handle_http_connection(stream, st));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    log::warn!("DLNA HTTP accept error: {error}");
                    break;
                }
            }
        }
    }
}

fn handle_http_connection(mut stream: TcpStream, state: Arc<Mutex<RendererState>>) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let local_addr = stream.local_addr().ok();

    let Some(req) = read_http_request(&mut stream) else {
        return;
    };

    let host_base = req
        .header("host")
        .filter(|h| !h.is_empty())
        .map(|h| format!("http://{h}"))
        .or_else(|| local_addr.map(|a| format!("http://{a}")))
        .unwrap_or_else(|| format!("http://127.0.0.1:{PREFERRED_HTTP_PORT}"));

    let mut pending_notifies = Vec::new();
    let (status, content_type, extra_headers, body) = match (req.method.as_str(), req.path.as_str())
    {
        ("GET" | "HEAD", "/description.xml") => {
            let (name, udn) = {
                let st = state.lock().unwrap_or_else(PoisonError::into_inner);
                (st.device_name.clone(), st.udn.clone())
            };
            (
                "200 OK",
                "text/xml; charset=\"utf-8\"",
                Vec::new(),
                device_description_xml(&name, &udn, &host_base),
            )
        }
        ("GET" | "HEAD", "/scpd/avtransport.xml") => (
            "200 OK",
            "text/xml; charset=\"utf-8\"",
            Vec::new(),
            AVT_SCPD_XML.to_owned(),
        ),
        ("GET" | "HEAD", "/scpd/connmgr.xml") => (
            "200 OK",
            "text/xml; charset=\"utf-8\"",
            Vec::new(),
            CM_SCPD_XML.to_owned(),
        ),
        ("GET" | "HEAD", "/scpd/renderingcontrol.xml") => (
            "200 OK",
            "text/xml; charset=\"utf-8\"",
            Vec::new(),
            RC_SCPD_XML.to_owned(),
        ),
        ("SUBSCRIBE", path) => {
            if let Some(service) = EventService::from_path(path) {
                let mut st = state.lock().unwrap_or_else(PoisonError::into_inner);
                match st.subscribe(service, req.header("callback"), req.header("sid")) {
                    Ok((sid, initial)) => {
                        if let Some(notify) = initial {
                            pending_notifies.push(notify);
                        }
                        (
                            "200 OK",
                            "text/plain",
                            vec![format!("SID: {sid}"), "TIMEOUT: Second-1800".to_owned()],
                            String::new(),
                        )
                    }
                    Err(()) => (
                        "412 Precondition Failed",
                        "text/plain",
                        Vec::new(),
                        String::new(),
                    ),
                }
            } else {
                (
                    "404 Not Found",
                    "text/plain",
                    Vec::new(),
                    "Not Found".to_owned(),
                )
            }
        }
        ("UNSUBSCRIBE", path) => {
            let mut st = state.lock().unwrap_or_else(PoisonError::into_inner);
            st.unsubscribe(req.header("sid"));
            if path == "/event/avtransport"
                && !st
                    .subscriptions
                    .iter()
                    .any(|s| s.service == EventService::AvTransport)
            {
                st.stop_playback();
            }
            ("200 OK", "text/plain", Vec::new(), String::new())
        }
        ("POST", "/control/avtransport") => {
            let (res, notifies) = handle_avtransport(&req, &state);
            pending_notifies.extend(notifies);
            res
        }
        ("POST", "/control/connmgr") => handle_connmgr(&req),
        ("POST", "/control/renderingcontrol") => {
            let (res, notifies) = handle_renderingcontrol(&req, &state);
            pending_notifies.extend(notifies);
            res
        }
        _ => (
            "404 Not Found",
            "text/plain",
            Vec::new(),
            "Not Found".to_owned(),
        ),
    };

    let send_body = if req.method == "HEAD" {
        ""
    } else {
        body.as_str()
    };
    let mut response = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Server: Linux/6.0 UPnP/1.0 RustTV/1.0\r\n\
         Connection: close\r\n",
        body.len()
    );
    for hdr in extra_headers {
        let _ = write!(response, "{hdr}\r\n");
    }
    response.push_str("\r\n");
    let _ = stream.write_all(response.as_bytes());
    if !send_body.is_empty() {
        let _ = stream.write_all(send_body.as_bytes());
    }
    let _ = stream.flush();
    drop(stream);

    // Send UPnP GENA NOTIFY callbacks after the HTTP 200 OK response is sent
    // so controllers (notably Samsung Gallery / Smart View) receive both the
    // initial SEQ:0 snapshot and every TransportState transition.
    if !pending_notifies.is_empty() {
        let notify_state = Arc::clone(&state);
        let _ = thread::Builder::new()
            .name("rusttv-dlna-notify".to_owned())
            .spawn(move || dispatch_notifies(pending_notifies, &notify_state));
    }
}

struct HttpRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn soap_action(&self) -> Option<&str> {
        let raw = self.header("soapaction")?.trim().trim_matches('"');
        raw.rsplit_once('#').map(|(_, action)| action)
    }
}

fn read_http_request(stream: &mut TcpStream) -> Option<HttpRequest> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    let header_end = loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_header_end(&buf) {
            break pos;
        }
        if buf.len() > MAX_HEADER_BYTES {
            return None;
        }
    };

    let head = std::str::from_utf8(&buf[..header_end]).ok()?;
    let mut lines = head.split("\r\n");
    let first = lines.next()?;
    let mut parts = first.split_whitespace();
    let method = parts.next()?.to_ascii_uppercase();
    let raw_target = parts.next()?;
    let path = raw_target
        .split('?')
        .next()
        .unwrap_or(raw_target)
        .to_owned();

    let mut headers = Vec::new();
    let mut content_length: usize = 0;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_owned();
            let val = v.trim().to_owned();
            if key.eq_ignore_ascii_case("content-length") {
                content_length = val.parse().unwrap_or(0).min(MAX_BODY_BYTES);
            }
            headers.push((key, val));
        }
    }

    let body_start = header_end + 4;
    let mut body_bytes = buf[body_start..].to_vec();
    while body_bytes.len() < content_length {
        let remaining = content_length - body_bytes.len();
        let to_read = remaining.min(chunk.len());
        let n = stream.read(&mut chunk[..to_read]).ok()?;
        if n == 0 {
            break;
        }
        body_bytes.extend_from_slice(&chunk[..n]);
    }
    body_bytes.truncate(content_length);
    let body = String::from_utf8_lossy(&body_bytes).into_owned();

    Some(HttpRequest {
        method,
        path,
        headers,
        body,
    })
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

type SoapResult = (&'static str, &'static str, Vec<String>, String);

fn soap_ok(service: &str, action: &str, inner: &str) -> SoapResult {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body><u:{action}Response xmlns:u=\"{service}\">{inner}</u:{action}Response></s:Body>\
         </s:Envelope>"
    );
    ("200 OK", "text/xml; charset=\"utf-8\"", Vec::new(), body)
}

fn soap_fault(code: u32, desc: &str) -> SoapResult {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring>\
         <detail><UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\">\
         <errorCode>{code}</errorCode><errorDescription>{}</errorDescription>\
         </UPnPError></detail></s:Fault></s:Body></s:Envelope>",
        xml_escape(desc)
    );
    (
        "500 Internal Server Error",
        "text/xml; charset=\"utf-8\"",
        Vec::new(),
        body,
    )
}

fn handle_connmgr(req: &HttpRequest) -> SoapResult {
    match req.soap_action().unwrap_or("") {
        "GetProtocolInfo" => soap_ok(
            CM_TYPE,
            "GetProtocolInfo",
            &format!("<Source></Source><Sink>{PROTOCOL_INFO_SINK}</Sink>"),
        ),
        "GetCurrentConnectionIDs" => soap_ok(
            CM_TYPE,
            "GetCurrentConnectionIDs",
            "<ConnectionIDs>0</ConnectionIDs>",
        ),
        "GetCurrentConnectionInfo" => soap_ok(
            CM_TYPE,
            "GetCurrentConnectionInfo",
            "<RcsID>0</RcsID><AVTransportID>0</AVTransportID><ProtocolInfo></ProtocolInfo>\
             <PeerConnectionManager></PeerConnectionManager><PeerConnectionID>-1</PeerConnectionID>\
             <Direction>Input</Direction><Status>OK</Status>",
        ),
        _ => soap_fault(401, "Invalid Action"),
    }
}

fn handle_avtransport(
    req: &HttpRequest,
    state: &Arc<Mutex<RendererState>>,
) -> (SoapResult, Vec<PendingNotify>) {
    let action = req.soap_action().unwrap_or("");
    match action {
        "SetAVTransportURI" => {
            let uri = xml_tag_text(&req.body, "CurrentURI").unwrap_or_default();
            let meta = xml_tag_text(&req.body, "CurrentURIMetaData").unwrap_or_default();
            let mut st = state.lock().unwrap_or_else(PoisonError::into_inner);
            match st.set_uri(uri, meta) {
                Ok(()) => {
                    let notifies = st.prepare_notifies(EventService::AvTransport);
                    (soap_ok(AVT_TYPE, "SetAVTransportURI", ""), notifies)
                }
                Err(msg) => (soap_fault(714, &msg), Vec::new()),
            }
        }
        "SetNextAVTransportURI" => (soap_ok(AVT_TYPE, "SetNextAVTransportURI", ""), Vec::new()),
        "Play" => {
            let mut st = state.lock().unwrap_or_else(PoisonError::into_inner);
            match st.play() {
                Ok(()) => {
                    let notifies = st.prepare_notifies(EventService::AvTransport);
                    (soap_ok(AVT_TYPE, "Play", ""), notifies)
                }
                Err(msg) => (soap_fault(704, &msg), Vec::new()),
            }
        }
        "Pause" => {
            let mut st = state.lock().unwrap_or_else(PoisonError::into_inner);
            st.pause();
            let notifies = st.prepare_notifies(EventService::AvTransport);
            (soap_ok(AVT_TYPE, "Pause", ""), notifies)
        }
        "Stop" => {
            let (notifies, token) = {
                let mut st = state.lock().unwrap_or_else(PoisonError::into_inner);
                let token = st.soft_stop();
                let notifies = st.prepare_notifies(EventService::AvTransport);
                (notifies, token)
            };
            let close_state = Arc::clone(state);
            let _ = thread::Builder::new()
                .name("rusttv-dlna-close".to_owned())
                .spawn(move || {
                    thread::sleep(Duration::from_secs(2));
                    let mut st = close_state.lock().unwrap_or_else(PoisonError::into_inner);
                    if st.playback_gen == token && st.transport == TransportState::Stopped {
                        st.stop_playback();
                    }
                });
            (soap_ok(AVT_TYPE, "Stop", ""), notifies)
        }
        "Seek" => {
            if let Some(target) = xml_tag_text(&req.body, "Target")
                && let Some(secs) = parse_hms(&target)
            {
                let mut st = state.lock().unwrap_or_else(PoisonError::into_inner);
                st.seek_seconds(secs);
            }
            (soap_ok(AVT_TYPE, "Seek", ""), Vec::new())
        }
        "GetTransportInfo" => {
            let mut st = state.lock().unwrap_or_else(PoisonError::into_inner);
            let ts = st.current_transport_state().as_upnp();
            (
                soap_ok(
                    AVT_TYPE,
                    "GetTransportInfo",
                    &format!(
                        "<CurrentTransportState>{ts}</CurrentTransportState>\
                         <CurrentTransportStatus>OK</CurrentTransportStatus>\
                         <CurrentSpeed>1</CurrentSpeed>"
                    ),
                ),
                Vec::new(),
            )
        }
        "GetPositionInfo" => {
            let mut st = state.lock().unwrap_or_else(PoisonError::into_inner);
            let (rel, dur) = st.position_info();
            let uri = xml_escape(&st.current_uri);
            let meta = xml_escape(&st.current_metadata);
            (
                soap_ok(
                    AVT_TYPE,
                    "GetPositionInfo",
                    &format!(
                        "<Track>1</Track><TrackDuration>{dur}</TrackDuration>\
                         <TrackMetaData>{meta}</TrackMetaData><TrackURI>{uri}</TrackURI>\
                         <RelTime>{rel}</RelTime><AbsTime>{rel}</AbsTime>\
                         <RelCount>0</RelCount><AbsCount>0</AbsCount>"
                    ),
                ),
                Vec::new(),
            )
        }
        "GetMediaInfo" => {
            let mut st = state.lock().unwrap_or_else(PoisonError::into_inner);
            let (_, dur) = st.position_info();
            let tracks = if st.current_uri.is_empty() { 0 } else { 1 };
            let uri = xml_escape(&st.current_uri);
            let meta = xml_escape(&st.current_metadata);
            (
                soap_ok(
                    AVT_TYPE,
                    "GetMediaInfo",
                    &format!(
                        "<NrTracks>{tracks}</NrTracks><MediaDuration>{dur}</MediaDuration>\
                         <CurrentURI>{uri}</CurrentURI><CurrentURIMetaData>{meta}</CurrentURIMetaData>\
                         <NextURI></NextURI><NextURIMetaData></NextURIMetaData>\
                         <PlayMedium>NETWORK</PlayMedium><RecordMedium>NOT_IMPLEMENTED</RecordMedium>\
                         <WriteStatus>NOT_IMPLEMENTED</WriteStatus>"
                    ),
                ),
                Vec::new(),
            )
        }
        "GetDeviceCapabilities" => (
            soap_ok(
                AVT_TYPE,
                "GetDeviceCapabilities",
                "<PlayMedia>NETWORK</PlayMedia><RecMedia>NOT_IMPLEMENTED</RecMedia>\
                 <RecQualityModes>NOT_IMPLEMENTED</RecQualityModes>",
            ),
            Vec::new(),
        ),
        "GetTransportSettings" => (
            soap_ok(
                AVT_TYPE,
                "GetTransportSettings",
                "<PlayMode>NORMAL</PlayMode><RecQualityMode>NOT_IMPLEMENTED</RecQualityMode>",
            ),
            Vec::new(),
        ),
        "GetCurrentTransportActions" => (
            soap_ok(
                AVT_TYPE,
                "GetCurrentTransportActions",
                "<Actions>Play,Pause,Stop,Seek</Actions>",
            ),
            Vec::new(),
        ),
        _ => (soap_fault(401, "Invalid Action"), Vec::new()),
    }
}

fn handle_renderingcontrol(
    req: &HttpRequest,
    state: &Mutex<RendererState>,
) -> (SoapResult, Vec<PendingNotify>) {
    match req.soap_action().unwrap_or("") {
        "GetVolume" => {
            let vol = state.lock().unwrap_or_else(PoisonError::into_inner).volume;
            (
                soap_ok(
                    RC_TYPE,
                    "GetVolume",
                    &format!("<CurrentVolume>{vol}</CurrentVolume>"),
                ),
                Vec::new(),
            )
        }
        "SetVolume" => {
            let notifies = if let Some(raw) = xml_tag_text(&req.body, "DesiredVolume")
                && let Ok(vol) = raw.trim().parse::<u8>()
            {
                let mut st = state.lock().unwrap_or_else(PoisonError::into_inner);
                st.set_volume(vol);
                st.prepare_notifies(EventService::RenderingControl)
            } else {
                Vec::new()
            };
            (soap_ok(RC_TYPE, "SetVolume", ""), notifies)
        }
        "GetMute" => {
            let muted = state.lock().unwrap_or_else(PoisonError::into_inner).muted;
            (
                soap_ok(
                    RC_TYPE,
                    "GetMute",
                    &format!("<CurrentMute>{}</CurrentMute>", u8::from(muted)),
                ),
                Vec::new(),
            )
        }
        "SetMute" => {
            let notifies = if let Some(raw) = xml_tag_text(&req.body, "DesiredMute") {
                let muted = matches!(raw.trim(), "1" | "true" | "TRUE");
                let mut st = state.lock().unwrap_or_else(PoisonError::into_inner);
                st.set_mute(muted);
                st.prepare_notifies(EventService::RenderingControl)
            } else {
                Vec::new()
            };
            (soap_ok(RC_TYPE, "SetMute", ""), notifies)
        }
        "ListPresets" => (
            soap_ok(
                RC_TYPE,
                "ListPresets",
                "<CurrentPresetNameList>FactoryDefaults</CurrentPresetNameList>",
            ),
            Vec::new(),
        ),
        "SelectPreset" => (soap_ok(RC_TYPE, "SelectPreset", ""), Vec::new()),
        _ => (soap_fault(401, "Invalid Action"), Vec::new()),
    }
}

fn dispatch_notifies(notifies: Vec<PendingNotify>, state: &Mutex<RendererState>) {
    let mut dead_sids = Vec::new();
    for item in notifies {
        // Give the subscriber a brief beat to process the SUBSCRIBE 200 OK
        // before sending the initial SEQ:0 state snapshot.
        if item.seq == 0 {
            thread::sleep(Duration::from_millis(25));
        }
        if !send_gena_notify(&item) {
            dead_sids.push(item.sid);
        }
    }
    if !dead_sids.is_empty() {
        let mut st = state.lock().unwrap_or_else(PoisonError::into_inner);
        st.subscriptions
            .retain(|sub| !dead_sids.iter().any(|dead| dead == &sub.sid));
    }
}

fn send_gena_notify(item: &PendingNotify) -> bool {
    let addr = format!("{}:{}", item.target.host, item.target.port);
    let Ok(mut stream) = TcpStream::connect(&addr) else {
        return false;
    };
    let timeout = Some(Duration::from_secs(2));
    let _ = stream.set_read_timeout(timeout);
    let _ = stream.set_write_timeout(timeout);
    let req = format!(
        "NOTIFY {} HTTP/1.1\r\n\
         HOST: {}\r\n\
         CONTENT-TYPE: text/xml; charset=\"utf-8\"\r\n\
         CONTENT-LENGTH: {}\r\n\
         NT: upnp:event\r\n\
         NTS: upnp:propchange\r\n\
         SID: {}\r\n\
         SEQ: {}\r\n\
         CONNECTION: close\r\n\r\n\
         {}",
        item.target.path,
        addr,
        item.body.len(),
        item.sid,
        item.seq,
        item.body
    );
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let _ = stream.flush();
    // Drain the subscriber's status line so it doesn't hit ECONNRESET.
    let mut reply = [0u8; 256];
    let _ = stream.read(&mut reply);
    true
}

/// Parses the first `<http://host[:port][/path]>` callback URL from a UPnP
/// GENA `CALLBACK` header.
fn parse_callback_url(header: &str) -> Option<CallbackTarget> {
    let start = header.find('<')?;
    let rest = &header[start + 1..];
    let end = rest.find('>')?;
    let url = rest[..end].trim();
    if url.chars().any(char::is_control) {
        return None;
    }
    let stripped = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("HTTP://"))?;
    let (authority, path) = match stripped.find('/') {
        Some(idx) => (&stripped[..idx], &stripped[idx..]),
        None => (stripped, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().ok()?),
        None => (authority, 80),
    };
    if host.is_empty() {
        return None;
    }
    Some(CallbackTarget {
        host: host.to_owned(),
        port,
        path: path.to_owned(),
    })
}

fn avtransport_event_xml(state: TransportState, uri: &str, metadata: &str) -> String {
    let tracks = if uri.is_empty() { 0 } else { 1 };
    let actions = match state {
        TransportState::Playing => "Pause,Stop,Seek",
        TransportState::Paused => "Play,Stop,Seek",
        TransportState::Stopped => "Play,Stop",
    };
    let inner = format!(
        "<Event xmlns=\"urn:schemas-upnp-org:metadata-1-0/AVT/\">\
         <InstanceID val=\"0\">\
         <TransportState val=\"{}\"/>\
         <TransportStatus val=\"OK\"/>\
         <CurrentPlayMode val=\"NORMAL\"/>\
         <CurrentTransportActions val=\"{actions}\"/>\
         <NumberOfTracks val=\"{tracks}\"/>\
         <CurrentTrack val=\"{tracks}\"/>\
         <AVTransportURI val=\"{}\"/>\
         <CurrentTrackURI val=\"{}\"/>\
         <AVTransportURIMetaData val=\"{}\"/>\
         <CurrentTrackMetaData val=\"{}\"/>\
         </InstanceID></Event>",
        state.as_upnp(),
        xml_escape(uri),
        xml_escape(uri),
        xml_escape(metadata),
        xml_escape(metadata),
    );
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\">\
         <e:property><LastChange>{}</LastChange></e:property>\
         </e:propertyset>",
        xml_escape(&inner)
    )
}

fn renderingcontrol_event_xml(volume: u8, muted: bool) -> String {
    let inner = format!(
        "<Event xmlns=\"urn:schemas-upnp-org:metadata-1-0/RCS/\">\
         <InstanceID val=\"0\">\
         <Volume channel=\"Master\" val=\"{volume}\"/>\
         <Mute channel=\"Master\" val=\"{}\"/>\
         </InstanceID></Event>",
        u8::from(muted)
    );
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\">\
         <e:property><LastChange>{}</LastChange></e:property>\
         </e:propertyset>",
        xml_escape(&inner)
    )
}

fn connmgr_event_xml() -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\">\
         <e:property><SourceProtocolInfo></SourceProtocolInfo></e:property>\
         <e:property><SinkProtocolInfo>{PROTOCOL_INFO_SINK}</SinkProtocolInfo></e:property>\
         <e:property><CurrentConnectionIDs>0</CurrentConnectionIDs></e:property>\
         </e:propertyset>"
    )
}

fn run_ssdp(http_port: u16, udn: &str, stop: Arc<AtomicBool>) {
    let socket = match open_ssdp_socket() {
        Ok(sock) => sock,
        Err(error) => {
            log::warn!("DLNA SSDP socket failed: {error}");
            return;
        }
    };
    let _ = socket.set_read_timeout(Some(Duration::from_millis(500)));

    let send_sock = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).ok();
    if let Some(ref sender) = send_sock {
        send_ssdp_notify(sender, http_port, udn, "ssdp:alive");
    }
    let mut last_notify = Instant::now();
    let mut buf = [0u8; 2048];

    while !stop.load(Ordering::Relaxed) {
        if last_notify.elapsed() >= SSDP_NOTIFY_INTERVAL {
            if let Some(ref sender) = send_sock {
                send_ssdp_notify(sender, http_port, udn, "ssdp:alive");
            }
            last_notify = Instant::now();
        }
        let Ok((n, peer)) = socket.recv_from(&mut buf) else {
            continue;
        };
        let Ok(text) = std::str::from_utf8(&buf[..n]) else {
            continue;
        };
        if !text.starts_with("M-SEARCH ") {
            continue;
        }
        let Some(st) = extract_header(text, "ST") else {
            continue;
        };
        let targets = matching_targets(st, udn);
        if targets.is_empty() {
            continue;
        }
        let local_ip = local_ip_for(peer).unwrap_or_else(default_lan_ip);
        let location = format!("http://{local_ip}:{http_port}/description.xml");
        for nt in targets {
            let usn = usn_for(&nt, udn);
            let reply = format!(
                "HTTP/1.1 200 OK\r\n\
                 CACHE-CONTROL: max-age=1800\r\n\
                 EXT:\r\n\
                 LOCATION: {location}\r\n\
                 SERVER: Linux/6.0 UPnP/1.0 RustTV/1.0\r\n\
                 ST: {nt}\r\n\
                 USN: {usn}\r\n\r\n"
            );
            let _ = socket.send_to(reply.as_bytes(), peer);
        }
    }

    if let Some(ref sender) = send_sock {
        send_ssdp_notify(sender, http_port, udn, "ssdp:byebye");
    }
}

fn open_ssdp_socket() -> std::io::Result<UdpSocket> {
    let fd = socket_with(
        AddressFamily::INET,
        SocketType::DGRAM,
        SocketFlags::CLOEXEC,
        None,
    )?;
    let _ = sockopt::set_socket_reuseaddr(&fd, true);
    let _ = sockopt::set_socket_reuseport(&fd, true);
    bind(&fd, &SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, SSDP_PORT))?;
    let sock = UdpSocket::from(fd);
    let _ = sock.join_multicast_v4(&SSDP_MCAST, &Ipv4Addr::UNSPECIFIED);
    Ok(sock)
}

fn send_ssdp_notify(sender: &UdpSocket, http_port: u16, udn: &str, nts: &str) {
    let ip = default_lan_ip();
    let location = format!("http://{ip}:{http_port}/description.xml");
    let dest = SocketAddrV4::new(SSDP_MCAST, SSDP_PORT);
    let types = [
        "upnp:rootdevice".to_owned(),
        format!("uuid:{udn}"),
        DEVICE_TYPE.to_owned(),
        AVT_TYPE.to_owned(),
        CM_TYPE.to_owned(),
        RC_TYPE.to_owned(),
    ];
    for nt in types {
        let usn = usn_for(&nt, udn);
        let msg = if nts == "ssdp:alive" {
            format!(
                "NOTIFY * HTTP/1.1\r\n\
                 HOST: {SSDP_MCAST}:{SSDP_PORT}\r\n\
                 CACHE-CONTROL: max-age=1800\r\n\
                 LOCATION: {location}\r\n\
                 NT: {nt}\r\n\
                 NTS: ssdp:alive\r\n\
                 SERVER: Linux/6.0 UPnP/1.0 RustTV/1.0\r\n\
                 USN: {usn}\r\n\r\n"
            )
        } else {
            format!(
                "NOTIFY * HTTP/1.1\r\n\
                 HOST: {SSDP_MCAST}:{SSDP_PORT}\r\n\
                 NT: {nt}\r\n\
                 NTS: ssdp:byebye\r\n\
                 USN: {usn}\r\n\r\n"
            )
        };
        let _ = sender.send_to(msg.as_bytes(), dest);
    }
}

fn matching_targets(st: &str, udn: &str) -> Vec<String> {
    let uuid_target = format!("uuid:{udn}");
    if st.eq_ignore_ascii_case("ssdp:all") {
        return vec![
            "upnp:rootdevice".to_owned(),
            uuid_target,
            DEVICE_TYPE.to_owned(),
            AVT_TYPE.to_owned(),
            CM_TYPE.to_owned(),
            RC_TYPE.to_owned(),
        ];
    }
    if st.eq_ignore_ascii_case("upnp:rootdevice")
        || st == uuid_target
        || st == DEVICE_TYPE
        || st == AVT_TYPE
        || st == CM_TYPE
        || st == RC_TYPE
    {
        return vec![st.to_owned()];
    }
    Vec::new()
}

fn usn_for(nt: &str, udn: &str) -> String {
    if nt == format!("uuid:{udn}") {
        nt.to_owned()
    } else {
        format!("uuid:{udn}::{nt}")
    }
}

fn extract_header<'a>(packet: &'a str, name: &str) -> Option<&'a str> {
    for line in packet.lines().skip(1) {
        if let Some((k, v)) = line.split_once(':')
            && k.trim().eq_ignore_ascii_case(name)
        {
            return Some(v.trim());
        }
    }
    None
}

fn local_ip_for(peer: SocketAddr) -> Option<Ipv4Addr> {
    let sock = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    sock.connect(peer).ok()?;
    match sock.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() => Some(ip),
        _ => None,
    }
}

fn default_lan_ip() -> Ipv4Addr {
    // UDP connect never sends a packet; it asks the kernel routing table which
    // local IPv4 address reaches the LAN/default route.
    local_ip_for(SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::new(192, 0, 2, 1),
        80,
    )))
    .unwrap_or(Ipv4Addr::LOCALHOST)
}

/// Deterministic UUIDv4-formatted UDN derived from `/etc/machine-id` + device
/// name so Samsung Smart View / Gallery recognizes the same device across boots.
fn stable_udn(device_name: &str) -> String {
    let seed = fs::read_to_string("/etc/machine-id")
        .unwrap_or_default()
        .trim()
        .to_owned();
    let mut h1: u64 = 0xcbf2_9ce4_8422_2325;
    let mut h2: u64 = 0x1000_0000_01b3_9999;
    for byte in seed
        .bytes()
        .chain(b":".iter().copied())
        .chain(device_name.bytes())
    {
        h1 ^= u64::from(byte);
        h1 = h1.wrapping_mul(0x0000_0100_0000_01b3);
        h2 = h2.wrapping_mul(0x0000_0100_0000_01b3) ^ u64::from(byte);
    }
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        (h1 >> 32) as u32,
        ((h1 >> 16) & 0xffff) as u16,
        (h1 & 0x0fff) as u16,
        (((h2 >> 48) as u16) & 0x3fff) | 0x8000,
        h2 & 0xffff_ffff_ffff
    )
}

fn device_description_xml(friendly_name: &str, udn: &str, url_base: &str) -> String {
    let name = xml_escape(friendly_name);
    let base = xml_escape(url_base);
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <root xmlns=\"urn:schemas-upnp-org:device-1-0\">\
         <specVersion><major>1</major><minor>0</minor></specVersion>\
         <URLBase>{base}/</URLBase>\
         <device>\
         <deviceType>{DEVICE_TYPE}</deviceType>\
         <dlna:X_DLNADOC xmlns:dlna=\"urn:schemas-dlna-org:device-1-0\">DMR-1.50</dlna:X_DLNADOC>\
         <friendlyName>{name}</friendlyName>\
         <manufacturer>RustTV</manufacturer>\
         <modelDescription>RustTV Media Renderer</modelDescription>\
         <modelName>RustTV</modelName>\
         <modelNumber>1.0</modelNumber>\
         <UDN>uuid:{udn}</UDN>\
         <serviceList>\
         <service>\
         <serviceType>{AVT_TYPE}</serviceType>\
         <serviceId>urn:upnp-org:serviceId:AVTransport</serviceId>\
         <SCPDURL>/scpd/avtransport.xml</SCPDURL>\
         <controlURL>/control/avtransport</controlURL>\
         <eventSubURL>/event/avtransport</eventSubURL>\
         </service>\
         <service>\
         <serviceType>{CM_TYPE}</serviceType>\
         <serviceId>urn:upnp-org:serviceId:ConnectionManager</serviceId>\
         <SCPDURL>/scpd/connmgr.xml</SCPDURL>\
         <controlURL>/control/connmgr</controlURL>\
         <eventSubURL>/event/connmgr</eventSubURL>\
         </service>\
         <service>\
         <serviceType>{RC_TYPE}</serviceType>\
         <serviceId>urn:upnp-org:serviceId:RenderingControl</serviceId>\
         <SCPDURL>/scpd/renderingcontrol.xml</SCPDURL>\
         <controlURL>/control/renderingcontrol</controlURL>\
         <eventSubURL>/event/renderingcontrol</eventSubURL>\
         </service>\
         </serviceList>\
         </device>\
         </root>"
    )
}

fn xml_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

fn xml_unescape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let Some(semi) = after.find(';') else {
            out.push('&');
            rest = after;
            continue;
        };
        let entity = &after[..semi];
        match entity {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                if let Some(ch) = u32::from_str_radix(&entity[2..], 16)
                    .ok()
                    .and_then(char::from_u32)
                {
                    out.push(ch);
                }
            }
            _ if entity.starts_with('#') => {
                if let Some(ch) = entity[1..].parse::<u32>().ok().and_then(char::from_u32) {
                    out.push(ch);
                }
            }
            _ => {
                out.push('&');
                out.push_str(entity);
                out.push(';');
            }
        }
        rest = &after[semi + 1..];
    }
    out.push_str(rest);
    out
}

/// Extracts the text inside `<Tag>...</Tag>` (also matching namespace prefixes
/// like `<u:Tag>...</u:Tag>`).
fn xml_tag_text(xml: &str, tag: &str) -> Option<String> {
    let open_end = find_open_tag_end(xml, tag)?;
    let rest = &xml[open_end..];
    let close_start = find_close_tag_start(rest, tag)?;
    Some(xml_unescape(rest[..close_start].trim()))
}

fn find_open_tag_end(xml: &str, tag: &str) -> Option<usize> {
    let mut offset = 0;
    while let Some(lt) = xml[offset..].find('<') {
        let start = offset + lt;
        let after_lt = &xml[start + 1..];
        let gt = after_lt.find('>')?;
        let inside = &after_lt[..gt];
        if !inside.starts_with('/') && !inside.starts_with('?') && !inside.starts_with('!') {
            let name = inside.split_whitespace().next().unwrap_or("");
            let local = name.rsplit_once(':').map_or(name, |(_, l)| l);
            if local.eq_ignore_ascii_case(tag) {
                if inside.ends_with('/') {
                    return None;
                }
                return Some(start + 1 + gt + 1);
            }
        }
        offset = start + 1 + gt + 1;
    }
    None
}

fn find_close_tag_start(xml: &str, tag: &str) -> Option<usize> {
    let mut offset = 0;
    while let Some(lt) = xml[offset..].find("</") {
        let start = offset + lt;
        let after = &xml[start + 2..];
        let gt = after.find('>')?;
        let name = after[..gt].trim();
        let local = name.rsplit_once(':').map_or(name, |(_, l)| l);
        if local.eq_ignore_ascii_case(tag) {
            return Some(start);
        }
        offset = start + 2 + gt + 1;
    }
    None
}

fn format_hms(seconds: f64) -> String {
    let total = seconds.max(0.0).floor() as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

fn parse_hms(text: &str) -> Option<u64> {
    let parts: Vec<&str> = text.trim().split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let h: u64 = parts[0].parse().ok()?;
    let m: u64 = parts[1].parse().ok()?;
    let s_str = parts[2].split('.').next().unwrap_or(parts[2]);
    let s: u64 = s_str.parse().ok()?;
    Some(
        h.saturating_mul(3600)
            .saturating_add(m.saturating_mul(60))
            .saturating_add(s),
    )
}

const AVT_SCPD_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <actionList>
    <action><name>SetAVTransportURI</name><argumentList>
      <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
      <argument><name>CurrentURI</name><direction>in</direction><relatedStateVariable>AVTransportURI</relatedStateVariable></argument>
      <argument><name>CurrentURIMetaData</name><direction>in</direction><relatedStateVariable>AVTransportURIMetaData</relatedStateVariable></argument>
    </argumentList></action>
    <action><name>Play</name><argumentList>
      <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
      <argument><name>Speed</name><direction>in</direction><relatedStateVariable>TransportPlaySpeed</relatedStateVariable></argument>
    </argumentList></action>
    <action><name>Pause</name><argumentList>
      <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
    </argumentList></action>
    <action><name>Stop</name><argumentList>
      <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
    </argumentList></action>
    <action><name>Seek</name><argumentList>
      <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
      <argument><name>Unit</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_SeekMode</relatedStateVariable></argument>
      <argument><name>Target</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_SeekTarget</relatedStateVariable></argument>
    </argumentList></action>
    <action><name>GetTransportInfo</name><argumentList>
      <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
      <argument><name>CurrentTransportState</name><direction>out</direction><relatedStateVariable>TransportState</relatedStateVariable></argument>
      <argument><name>CurrentTransportStatus</name><direction>out</direction><relatedStateVariable>TransportStatus</relatedStateVariable></argument>
      <argument><name>CurrentSpeed</name><direction>out</direction><relatedStateVariable>TransportPlaySpeed</relatedStateVariable></argument>
    </argumentList></action>
    <action><name>GetPositionInfo</name><argumentList>
      <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
      <argument><name>Track</name><direction>out</direction><relatedStateVariable>CurrentTrack</relatedStateVariable></argument>
      <argument><name>TrackDuration</name><direction>out</direction><relatedStateVariable>CurrentTrackDuration</relatedStateVariable></argument>
      <argument><name>TrackMetaData</name><direction>out</direction><relatedStateVariable>CurrentTrackMetaData</relatedStateVariable></argument>
      <argument><name>TrackURI</name><direction>out</direction><relatedStateVariable>CurrentTrackURI</relatedStateVariable></argument>
      <argument><name>RelTime</name><direction>out</direction><relatedStateVariable>RelativeTimePosition</relatedStateVariable></argument>
      <argument><name>AbsTime</name><direction>out</direction><relatedStateVariable>AbsoluteTimePosition</relatedStateVariable></argument>
      <argument><name>RelCount</name><direction>out</direction><relatedStateVariable>RelativeCounterPosition</relatedStateVariable></argument>
      <argument><name>AbsCount</name><direction>out</direction><relatedStateVariable>AbsoluteCounterPosition</relatedStateVariable></argument>
    </argumentList></action>
    <action><name>GetMediaInfo</name><argumentList>
      <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
      <argument><name>NrTracks</name><direction>out</direction><relatedStateVariable>NumberOfTracks</relatedStateVariable></argument>
      <argument><name>MediaDuration</name><direction>out</direction><relatedStateVariable>CurrentMediaDuration</relatedStateVariable></argument>
      <argument><name>CurrentURI</name><direction>out</direction><relatedStateVariable>AVTransportURI</relatedStateVariable></argument>
      <argument><name>CurrentURIMetaData</name><direction>out</direction><relatedStateVariable>AVTransportURIMetaData</relatedStateVariable></argument>
      <argument><name>NextURI</name><direction>out</direction><relatedStateVariable>NextAVTransportURI</relatedStateVariable></argument>
      <argument><name>NextURIMetaData</name><direction>out</direction><relatedStateVariable>NextAVTransportURIMetaData</relatedStateVariable></argument>
      <argument><name>PlayMedium</name><direction>out</direction><relatedStateVariable>PlaybackStorageMedium</relatedStateVariable></argument>
      <argument><name>RecordMedium</name><direction>out</direction><relatedStateVariable>RecordStorageMedium</relatedStateVariable></argument>
      <argument><name>WriteStatus</name><direction>out</direction><relatedStateVariable>RecordMediumWriteStatus</relatedStateVariable></argument>
    </argumentList></action>
  </actionList>
  <serviceStateTable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_InstanceID</name><dataType>ui4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_SeekMode</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_SeekTarget</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="yes"><name>LastChange</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>TransportState</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>TransportStatus</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>TransportPlaySpeed</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>AVTransportURI</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>AVTransportURIMetaData</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>NextAVTransportURI</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>NextAVTransportURIMetaData</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>NumberOfTracks</name><dataType>ui4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>CurrentTrack</name><dataType>ui4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>CurrentTrackDuration</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>CurrentMediaDuration</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>CurrentTrackMetaData</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>CurrentTrackURI</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>RelativeTimePosition</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>AbsoluteTimePosition</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>RelativeCounterPosition</name><dataType>i4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>AbsoluteCounterPosition</name><dataType>i4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>PlaybackStorageMedium</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>RecordStorageMedium</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>RecordMediumWriteStatus</name><dataType>string</dataType></stateVariable>
  </serviceStateTable>
</scpd>"#;

const CM_SCPD_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <actionList>
    <action><name>GetProtocolInfo</name><argumentList>
      <argument><name>Source</name><direction>out</direction><relatedStateVariable>SourceProtocolInfo</relatedStateVariable></argument>
      <argument><name>Sink</name><direction>out</direction><relatedStateVariable>SinkProtocolInfo</relatedStateVariable></argument>
    </argumentList></action>
    <action><name>GetCurrentConnectionIDs</name><argumentList>
      <argument><name>ConnectionIDs</name><direction>out</direction><relatedStateVariable>CurrentConnectionIDs</relatedStateVariable></argument>
    </argumentList></action>
  </actionList>
  <serviceStateTable>
    <stateVariable sendEvents="yes"><name>SourceProtocolInfo</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="yes"><name>SinkProtocolInfo</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="yes"><name>CurrentConnectionIDs</name><dataType>string</dataType></stateVariable>
  </serviceStateTable>
</scpd>"#;

const RC_SCPD_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <actionList>
    <action><name>GetVolume</name><argumentList>
      <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
      <argument><name>Channel</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Channel</relatedStateVariable></argument>
      <argument><name>CurrentVolume</name><direction>out</direction><relatedStateVariable>Volume</relatedStateVariable></argument>
    </argumentList></action>
    <action><name>SetVolume</name><argumentList>
      <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
      <argument><name>Channel</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Channel</relatedStateVariable></argument>
      <argument><name>DesiredVolume</name><direction>in</direction><relatedStateVariable>Volume</relatedStateVariable></argument>
    </argumentList></action>
    <action><name>GetMute</name><argumentList>
      <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
      <argument><name>Channel</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Channel</relatedStateVariable></argument>
      <argument><name>CurrentMute</name><direction>out</direction><relatedStateVariable>Mute</relatedStateVariable></argument>
    </argumentList></action>
    <action><name>SetMute</name><argumentList>
      <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
      <argument><name>Channel</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Channel</relatedStateVariable></argument>
      <argument><name>DesiredMute</name><direction>in</direction><relatedStateVariable>Mute</relatedStateVariable></argument>
    </argumentList></action>
  </actionList>
  <serviceStateTable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_InstanceID</name><dataType>ui4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_Channel</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="yes"><name>LastChange</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>Volume</name><dataType>ui2</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>Mute</name><dataType>boolean</dataType></stateVariable>
  </serviceStateTable>
</scpd>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_only_http_and_https_media_uris() {
        assert!(is_allowed_media_uri("http://192.168.1.165:3434/photo.heic"));
        assert!(is_allowed_media_uri(
            "HTTPS://example.local/video.mp4?a=1&b=2"
        ));
        assert!(!is_allowed_media_uri("file:///etc/passwd"));
        assert!(!is_allowed_media_uri("--script=/tmp/evil.lua"));
        assert!(!is_allowed_media_uri("http://192.168.1.165/a\nb"));
    }

    #[test]
    fn extracts_and_unescapes_soap_xml_tags() {
        let soap = r#"<s:Envelope><s:Body><u:SetAVTransportURI xmlns:u="urn:x">
            <InstanceID>0</InstanceID>
            <CurrentURI>http://192.168.1.165:3434/img.heic?id=42&amp;token=a&amp;b=1</CurrentURI>
            <CurrentURIMetaData>&lt;DIDL-Lite&gt;</CurrentURIMetaData>
        </u:SetAVTransportURI></s:Body></s:Envelope>"#;
        assert_eq!(
            xml_tag_text(soap, "CurrentURI").as_deref(),
            Some("http://192.168.1.165:3434/img.heic?id=42&token=a&b=1")
        );
        assert_eq!(
            xml_tag_text(soap, "CurrentURIMetaData").as_deref(),
            Some("<DIDL-Lite>")
        );
    }

    #[test]
    fn protocol_info_advertises_heic_and_mp4() {
        assert!(PROTOCOL_INFO_SINK.contains("image/heic"));
        assert!(PROTOCOL_INFO_SINK.contains("image/heif"));
        assert!(PROTOCOL_INFO_SINK.contains("image/avif"));
        assert!(PROTOCOL_INFO_SINK.contains("video/mp4"));
    }

    #[test]
    fn formats_and_parses_upnp_timestamps() {
        assert_eq!(format_hms(3665.9), "01:01:05");
        assert_eq!(parse_hms("01:01:05"), Some(3665));
        assert_eq!(parse_hms("00:00:12.500"), Some(12));
        assert_eq!(parse_hms("bad"), None);
    }

    #[test]
    fn matches_ssdp_search_targets() {
        let udn = "12345678-1234-4123-8123-123456789abc";
        assert_eq!(matching_targets("ssdp:all", udn).len(), 6);
        assert_eq!(matching_targets(DEVICE_TYPE, udn), vec![DEVICE_TYPE]);
        assert!(matching_targets("urn:other:device:1", udn).is_empty());
    }

    #[test]
    fn builds_mpv_arguments_with_separator_before_uri() {
        let args = mpv_args(
            "http://192.168.1.165:3434/photo.heic",
            true,
            Path::new("/run/user/1000/dlna.sock"),
            Path::new("/run/user/1000/dlna.conf"),
            85,
            false,
        );
        assert!(args.contains(&"--vo=gpu".to_owned()));
        assert!(args.contains(&"--gpu-api=opengl".to_owned()));
        assert!(args.contains(&"--image-display-duration=inf".to_owned()));
        let len = args.len();
        assert_eq!(args[len - 2], "--");
        assert_eq!(args[len - 1], "http://192.168.1.165:3434/photo.heic");
    }

    #[test]
    fn parses_gena_callback_urls() {
        assert_eq!(
            parse_callback_url("<http://192.168.1.165:54321/upnp/event/cb>"),
            Some(CallbackTarget {
                host: "192.168.1.165".to_owned(),
                port: 54321,
                path: "/upnp/event/cb".to_owned(),
            })
        );
        assert_eq!(
            parse_callback_url("<http://192.168.1.10>"),
            Some(CallbackTarget {
                host: "192.168.1.10".to_owned(),
                port: 80,
                path: "/".to_owned(),
            })
        );
        assert_eq!(parse_callback_url("<file:///etc/passwd>"), None);
        assert_eq!(parse_callback_url("not-bracketed"), None);
    }

    #[test]
    fn builds_avtransport_last_change_event_xml() {
        let xml = avtransport_event_xml(
            TransportState::Playing,
            "http://192.168.1.165:3434/img.heic?a=1&b=2",
            "",
        );
        assert!(xml.contains("e:propertyset"));
        assert!(xml.contains("&lt;TransportState val=&quot;PLAYING&quot;/&gt;"));
        assert!(xml.contains("&amp;amp;b=2"));
    }
}
