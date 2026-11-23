# AirPlay & Smart View Screen Mirroring on Linux

RustTV integrates screen-mirroring receivers in **Settings → Screen Mirroring** (`crates/rusttv-launcher/src/linux/device/casting.rs`), letting you toggle receivers on/off, customize the advertised device name, and keep them running across reboots.

Preferences are stored in `$XDG_CONFIG_HOME/rusttv/casting.toml` (usually `~/.config/rusttv/casting.toml`).

## 1. AirPlay (iPhone, iPad, macOS) — via `UxPlay`

[UxPlay](https://github.com/FDH2/UxPlay) provides AirPlay 2 screen mirroring, video streaming, and audio on ARM64 Linux (including the Magcubic HY320F projector) using GStreamer, Wayland, PipeWire, and Avahi.

### Install on Debian / Armbian

```bash
sudo apt update
sudo apt install -y \
  uxplay \
  avahi-daemon \
  gstreamer1.0-plugins-base \
  gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-bad \
  gstreamer1.0-libav
sudo systemctl enable --now avahi-daemon
```

> If your distro does not package `uxplay`, build it from source (`cmake` + `libplist-dev` + `libssl-dev` + `libavahi-compat-libdnssd-dev` + `libgstreamer1.0-dev`) and install the binary into `/usr/local/bin/uxplay`.

Once installed, open **Settings → Screen Mirroring** in RustTV and turn on **AirPlay**. Your iPhone, iPad, or Mac will see the projector under **Screen Mirroring** and AirPlay pickers. When mirroring stops, UxPlay closes its fullscreen window automatically and returns to RustTV Home.

## 2. Samsung Smart View (Miracast / Wi-Fi Display)

Samsung Smart View uses **Miracast (Wi-Fi Display over Wi-Fi Direct / P2P)** for full screen mirroring, and **DLNA / UPnP** for casting videos and photos over normal Wi-Fi.

### Full Screen Mirroring (Miracast)

RustTV supports any of these Miracast sink binaries on `PATH` (checked in order):

1. **`miraclecast`** (`miracle-sinkctl` + `miracle-wifid`) — standard Linux Wi-Fi Display sink.
2. **`wfd-sink`** — standalone Wi-Fi Display sink.
3. **`lazycast`** — lightweight ARM Linux Miracast receiver.

Check **Settings → Screen Mirroring → Readiness → Smart View Wi-Fi Direct (P2P)** to verify whether your Wi-Fi driver reports `P2P-client` and `P2P-GO` modes (via `iw list`).

> Note: Many low-cost USB/SDIO projector Wi-Fi chips (e.g. some `aic8800` / `rtl8821cs` drivers) either lack Wi-Fi P2P support or cannot share P2P simultaneously with NetworkManager. If your Wi-Fi adapter does not support P2P, use **Smart View / DLNA media cast** below over normal Wi-Fi.

### Media Casting over Standard Wi-Fi (DLNA / UPnP)

For casting photos (including Samsung / iPhone **HEIC/HEIF**, AVIF, JPEG, PNG), videos, and music from Samsung Gallery / Smart View and Android/iOS media apps without needing Wi-Fi Direct, RustTV includes a built-in UPnP MediaRenderer backed by `mpv` (with `gmediarender` as fallback if `mpv` is not installed):

```bash
sudo apt update
sudo apt install -y mpv
```

Then enable **Smart View / DLNA media cast** in **Settings → Screen Mirroring**.

> **Mainline Allwinner Linux (HY320F):** If legacy BSP packages `gstreamer1.0-omx-allwinner` / `gstreamer1.0-omx-allwinner-config` are installed on a mainline kernel without `/dev/cedar_dev`, remove them (`sudo apt remove gstreamer1.0-omx-allwinner gstreamer1.0-omx-allwinner-config`) so GStreamer (`uxplay` / fallback `gmediarender`) uses the mainline V4L2 stateless (`v4l2codecs`) and software decoders instead of crashing inside `libVE.so`. RustTV also automatically disables legacy `omx*` decoders via `GST_PLUGIN_FEATURE_RANK` when `/dev/cedar_dev` is absent.
