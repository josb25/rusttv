//! The test pattern: a Siemens star in the middle, smaller ones near the
//! corners, a crosshair, two rings and a frame along the screen's edges,
//! in white lines on dark outlines, so they stand out over any picture.
//! Sharp edges and spokes that stay apart towards the star's centre show
//! when a projector's lens is in focus, the corners how even it is.
//!
//! It shows over everything while a device control that asks for it (its
//! device file's `pattern = true`, e.g. the lens focus) moves: as its own
//! full-screen `wlr-layer-shell` overlay that takes neither keys nor clicks
//! ([`PatternOverlay`], for the remote's keys and Settings), or drawn by the
//! side menu while that control's row has the focus ([`paint`]).

use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

use resvg::tiny_skia::{FillRule, LineCap, Path, PathBuilder, Pixmap, Stroke, Transform};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer::{self, WlBuffer},
    wl_compositor::WlCompositor,
    wl_region::WlRegion,
    wl_registry::WlRegistry,
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{
        self as layer_surface, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1,
    },
};

use crate::linux::keyboard::draw::paint as ink;

/// How long the pattern stays after the last step.
const SHOWN_FOR: Duration = Duration::from_millis(2500);
/// The design width the sizes below are for (a 1080p screen).
const DESIGN_WIDTH: f32 = 1920.0;
/// The middle star's radius; the corner stars', and how far in they are.
const STAR_R: f32 = 180.0;
const CORNER_R: f32 = 64.0;
const CORNER_IN: f32 = 150.0;
/// White and black wedges each of the stars has.
const SPOKES: usize = 18;
/// Rings around the middle star.
const RINGS: [f32; 2] = [300.0, 420.0];
/// The frame's distance from the edges.
const FRAME_IN: f32 = 8.0;
/// Lines: white, on a dark outline this wide.
const LINE: f32 = 2.0;
const OUTLINE: f32 = 4.0;
const WHITE: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xFF];
const DARK: [u8; 4] = [0x00, 0x00, 0x00, 0xFF];

/// Paints the test pattern over `pixmap` (its own size: the screen's).
pub fn paint(pixmap: &mut Pixmap) {
    let (w, h) = (pixmap.width() as f32, pixmap.height() as f32);
    let u = w / DESIGN_WIDTH;
    let (cx, cy) = (w / 2.0, h / 2.0);
    // The frame along the edges.
    let f = FRAME_IN * u;
    lines(
        pixmap,
        &[(f, f), (w - f, f), (w - f, h - f), (f, h - f), (f, f)],
        u,
    );
    // The crosshair, stopping short of the star.
    let gap = (STAR_R + 24.0) * u;
    for (from, to) in [
        ((0.0, cy), (cx - gap, cy)),
        ((cx + gap, cy), (w, cy)),
        ((cx, 0.0), (cx, cy - gap)),
        ((cx, cy + gap), (cx, h)),
    ] {
        lines(pixmap, &[from, to], u);
    }
    for r in RINGS {
        if let Some(ring) = PathBuilder::from_circle(cx, cy, r * u) {
            outlined(pixmap, &ring, u);
        }
    }
    star(pixmap, cx, cy, STAR_R * u, u);
    let i = CORNER_IN * u;
    for (x, y) in [(i, i), (w - i, i), (i, h - i), (w - i, h - i)] {
        star(pixmap, x, y, CORNER_R * u, u);
    }
}

/// A polyline through `points`.
fn lines(pixmap: &mut Pixmap, points: &[(f32, f32)], u: f32) {
    let mut path = PathBuilder::new();
    for (index, &(x, y)) in points.iter().enumerate() {
        if index == 0 {
            path.move_to(x, y);
        } else {
            path.line_to(x, y);
        }
    }
    if let Some(path) = path.finish() {
        outlined(pixmap, &path, u);
    }
}

/// `path` stroked white on a dark outline.
fn outlined(pixmap: &mut Pixmap, path: &Path, u: f32) {
    for (width, colour) in [(OUTLINE, DARK), (LINE, WHITE)] {
        let stroke = Stroke {
            width: width * u,
            line_cap: LineCap::Butt,
            ..Stroke::default()
        };
        pixmap.stroke_path(path, &ink(colour), &stroke, Transform::identity(), None);
    }
}

/// A Siemens star: a dark disc with [`SPOKES`] white wedges, and a white
/// rim.
fn star(pixmap: &mut Pixmap, cx: f32, cy: f32, r: f32, u: f32) {
    if let Some(disc) = PathBuilder::from_circle(cx, cy, r) {
        pixmap.fill_path(
            &disc,
            &ink(DARK),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
    let wedge = std::f32::consts::PI / SPOKES as f32;
    let mut wedges = PathBuilder::new();
    for spoke in 0..SPOKES {
        let from = 2.0 * wedge * spoke as f32;
        wedges.move_to(cx, cy);
        const ARC_STEPS: usize = 6;
        for step in 0..=ARC_STEPS {
            let angle = from + wedge * step as f32 / ARC_STEPS as f32;
            wedges.line_to(cx + r * angle.cos(), cy + r * angle.sin());
        }
        wedges.close();
    }
    if let Some(wedges) = wedges.finish() {
        pixmap.fill_path(
            &wedges,
            &ink(WHITE),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
    if let Some(rim) = PathBuilder::from_circle(cx, cy, r) {
        outlined(pixmap, &rim, u);
    }
}

/// The pattern over everything for a moment; the thread behind it lives as
/// long as this.
#[derive(Debug)]
pub struct PatternOverlay {
    wake: OwnedFd,
}

impl PatternOverlay {
    /// Starts the pattern's thread; `None` without a pipe or thread.
    pub fn start() -> Option<Self> {
        let (wake_read, wake) = rustix::pipe::pipe_with(
            rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK,
        )
        .ok()?;
        let spawned = std::thread::Builder::new()
            .name("rusttv-pattern".into())
            .spawn(move || {
                if let Err(error) = run(&wake_read) {
                    log::warn!("test pattern: {error}");
                }
            });
        if let Err(error) = spawned {
            log::warn!("no thread for the test pattern: {error}");
            return None;
        }
        Some(Self { wake })
    }

    /// Shows the pattern, or keeps it [`SHOWN_FOR`] longer.
    pub fn show(&self) {
        let _ = rustix::io::write(&self.wake, &[1]);
    }
}

fn run(wake: &OwnedFd) -> Result<(), String> {
    let connection = Connection::connect_to_env().map_err(|error| error.to_string())?;
    let (globals, mut queue) =
        registry_queue_init::<State>(&connection).map_err(|error| error.to_string())?;
    let qh = queue.handle();
    let bind = |name: &'static str| move |error| format!("no {name}: {error}");
    let mut state = State {
        compositor: globals.bind(&qh, 1..=4, ()).map_err(bind("compositor"))?,
        layer_shell: globals.bind(&qh, 1..=4, ()).map_err(bind("layer shell"))?,
        shm: globals.bind(&qh, 1..=1, ()).map_err(bind("shm"))?,
        shown: None,
    };
    let mut hide_at: Option<Instant> = None;
    loop {
        let timeout = hide_at.map(|at| at.saturating_duration_since(Instant::now()));
        if super::wait(&mut queue, &mut state, wake, timeout)? {
            state.show(&qh);
            hide_at = Some(Instant::now() + SHOWN_FOR);
        } else if hide_at.is_some_and(|at| Instant::now() >= at) {
            hide_at = None;
            state.hide();
        }
    }
}

struct Shown {
    surface: WlSurface,
    layer: ZwlrLayerSurfaceV1,
    /// Drawn once the compositor said the size.
    drawn: bool,
}

struct State {
    compositor: WlCompositor,
    layer_shell: ZwlrLayerShellV1,
    shm: WlShm,
    shown: Option<Shown>,
}

impl State {
    fn show(&mut self, qh: &QueueHandle<Self>) {
        if self.shown.is_some() {
            return;
        }
        let surface = self.compositor.create_surface(qh, ());
        // Clicks go through to what is under it.
        let region = self.compositor.create_region(qh, ());
        surface.set_input_region(Some(&region));
        region.destroy();
        let layer = self.layer_shell.get_layer_surface(
            &surface,
            None,
            Layer::Overlay,
            "rusttv-pattern".into(),
            qh,
            (),
        );
        layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        layer.set_size(0, 0);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        surface.commit();
        self.shown = Some(Shown {
            surface,
            layer,
            drawn: false,
        });
    }

    fn hide(&mut self) {
        if let Some(shown) = self.shown.take() {
            shown.layer.destroy();
            shown.surface.destroy();
        }
    }

    fn draw(&mut self, width: u32, height: u32, qh: &QueueHandle<Self>) {
        let Some(shown) = self.shown.as_mut().filter(|shown| !shown.drawn) else {
            return;
        };
        let Some(pixels) = pixels(width, height) else {
            return;
        };
        match super::shm_buffer(&self.shm, &pixels, width, height, qh) {
            Ok(buffer) => {
                shown.surface.attach(Some(&buffer), 0, 0);
                shown.surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
                shown.surface.commit();
                shown.drawn = true;
            }
            Err(error) => log::warn!("drawing the test pattern: {error}"),
        }
    }
}

/// The pattern alone as `wl_shm` ARGB8888 bytes, `width` × `height`.
fn pixels(width: u32, height: u32) -> Option<Vec<u8>> {
    let mut pixmap = Pixmap::new(width, height)?;
    paint(&mut pixmap);
    let mut bytes = pixmap.take();
    for pixel in bytes.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Some(bytes)
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        state: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        event: layer_surface::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            layer_surface::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer.ack_configure(serial);
                if width > 0 && height > 0 {
                    state.draw(width, height, qh);
                }
            }
            layer_surface::Event::Closed => state.hide(),
            _ => {}
        }
    }
}

impl Dispatch<WlBuffer, ()> for State {
    fn event(
        _: &mut Self,
        buffer: &WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            buffer.destroy();
        }
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as wayland_client::Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: WlCompositor);
delegate_noop!(State: ignore WlSurface);
delegate_noop!(State: WlRegion);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: WlShmPool);
delegate_noop!(State: ZwlrLayerShellV1);

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba(pixmap: &Pixmap, x: u32, y: u32) -> [u8; 4] {
        let index = ((y * pixmap.width() + x) * 4) as usize;
        let data = pixmap.data();
        [
            data[index],
            data[index + 1],
            data[index + 2],
            data[index + 3],
        ]
    }

    #[test]
    fn draws_a_star_crosshair_and_frame_and_leaves_the_rest_clear() {
        let mut pixmap = Pixmap::new(1920, 1080).unwrap();
        paint(&mut pixmap);
        // The star's disc is opaque; its first wedge (from 0°) is white.
        assert_eq!(rgba(&pixmap, 960 + 120, 540 + 8), WHITE);
        // Between wedges: dark.
        let between = std::f32::consts::PI / SPOKES as f32 * 1.5;
        let (x, y) = (960.0 + 120.0 * between.cos(), 540.0 + 120.0 * between.sin());
        assert_eq!(rgba(&pixmap, x as u32, y as u32), DARK);
        // The crosshair's white line, away from the star.
        assert_eq!(rgba(&pixmap, 100, 540), WHITE);
        assert_eq!(rgba(&pixmap, 960, 60), WHITE);
        // The frame.
        assert_eq!(rgba(&pixmap, 500, 8), WHITE);
        // Elsewhere the picture shows through.
        assert_eq!(rgba(&pixmap, 500, 300)[3], 0);
        // A corner star.
        assert_eq!(rgba(&pixmap, 150 + 40, 150 + 3)[3], 0xFF);
    }

    #[test]
    fn scales_with_the_screen() {
        let mut pixmap = Pixmap::new(960, 540).unwrap();
        paint(&mut pixmap);
        assert_eq!(rgba(&pixmap, 480 + 60, 270 + 4), WHITE);
        assert_eq!(rgba(&pixmap, 50, 270)[3], 0xFF);
    }
}
