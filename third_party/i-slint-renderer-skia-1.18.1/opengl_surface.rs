// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

// cSpell: ignore fboid
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use std::{cell::RefCell, sync::Arc};

use glutin::{
    config::GetGlConfig,
    context::{ContextApi, ContextAttributesBuilder},
    display::GetGlDisplay,
    prelude::*,
    surface::{SurfaceAttributesBuilder, WindowSurface},
};
use i_slint_core::api::{GraphicsAPI, PhysicalSize as PhysicalWindowSize, Window};
use i_slint_core::graphics::{BorrowedOpenGLTexture, RequestedGraphicsAPI, RequestedOpenGLVersion};
use i_slint_core::partial_renderer::DirtyRegion;
use i_slint_core::platform::PlatformError;
use i_slint_core::renderer::DrawOutcome;

use crate::SkiaSharedContext;

/// Wraps a [`glutin::context::PossiblyCurrentContext`] and makes it not-current on drop,
/// so that no stale thread-local state remains after the context is destroyed.
struct GlutinContext(Option<glutin::context::PossiblyCurrentContext>);

impl GlutinContext {
    fn new(context: glutin::context::PossiblyCurrentContext) -> Self {
        Self(Some(context))
    }
}

impl std::ops::Deref for GlutinContext {
    type Target = glutin::context::PossiblyCurrentContext;
    fn deref(&self) -> &Self::Target {
        self.0.as_ref().unwrap()
    }
}

impl Drop for GlutinContext {
    fn drop(&mut self) {
        if let Some(context) = self.0.take()
            && let Err(e) = context.make_not_current()
        {
            i_slint_core::debug_log!(
                "Skia OpenGL Renderer: Failed to make context not current: {e}"
            );
        }
    }
}

/// The GL context and Skia context of a dropped [`OpenGLSurface`], kept for the
/// next window on this thread. Creating a new context for every window leaks
/// driver memory on some GPUs (Mali: ~1.6 MB per window on Android, where the
/// window is destroyed and recreated each time the app leaves the screen).
struct ParkedContext {
    context: glutin::context::NotCurrentContext,
    gr_context: skia_safe::gpu::DirectContext,
    fb_info: skia_safe::gpu::gl::FramebufferInfo,
}

thread_local! {
    static PARKED_CONTEXT: RefCell<Option<ParkedContext>> = const { RefCell::new(None) };
}

/// This surface type renders into the given window with OpenGL, using glutin and glow libraries.
pub struct OpenGLSurface {
    fb_info: skia_safe::gpu::gl::FramebufferInfo,
    surface: RefCell<skia_safe::Surface>,
    gr_context: RefCell<skia_safe::gpu::DirectContext>,
    glutin_context: GlutinContext,
    glutin_surface: glutin::surface::Surface<glutin::surface::WindowSurface>,
}

impl super::Surface for OpenGLSurface {
    fn new(
        _shared_context: &SkiaSharedContext,
        window_handle: Arc<dyn raw_window_handle::HasWindowHandle + Send + Sync>,
        display_handle: Arc<dyn raw_window_handle::HasDisplayHandle + Send + Sync>,
        size: PhysicalWindowSize,
        requested_graphics_api: Option<RequestedGraphicsAPI>,
    ) -> Result<Self, PlatformError> {
        Self::new_with_config(
            window_handle,
            display_handle,
            size,
            requested_graphics_api.as_ref().map(TryInto::try_into).transpose()?,
            glutin::config::ConfigTemplateBuilder::new(),
            None,
        )
    }

    fn name(&self) -> &'static str {
        "opengl"
    }

    fn with_graphics_api(&self, callback: &mut dyn FnMut(GraphicsAPI<'_>)) {
        let api = GraphicsAPI::NativeOpenGL {
            get_proc_address: &|name| {
                self.glutin_context.display().get_proc_address(name) as *const _
            },
        };
        callback(api)
    }

    fn with_active_surface(&self, callback: &mut dyn FnMut()) -> Result<(), PlatformError> {
        self.ensure_context_current()?;
        callback();
        Ok(())
    }

    fn render(
        &self,
        _window: &Window,
        size: PhysicalWindowSize,
        callback: &dyn Fn(
            &skia_safe::Canvas,
            Option<&mut skia_safe::gpu::DirectContext>,
            u8,
        ) -> Option<DirtyRegion>,
        pre_present_callback: &RefCell<Option<Box<dyn FnMut()>>>,
    ) -> Result<DrawOutcome, PlatformError> {
        self.ensure_context_current()?;

        let current_context = &self.glutin_context;

        let gr_context = &mut self.gr_context.borrow_mut();

        let mut surface = self.surface.borrow_mut();

        let (render_w, render_h, scale_x, scale_y) = Self::scaled_dimensions(size);
        let mut resized = false;

        if let Some((width, height)) =
            i32::try_from(render_w.get()).ok().zip(i32::try_from(render_h.get()).ok())
            && (width != surface.width() || height != surface.height())
        {
            self.glutin_surface.resize(current_context, render_w, render_h);
            *surface = Self::create_internal_surface(
                self.fb_info,
                current_context,
                gr_context,
                width,
                height,
            )?;
            resized = true;
        }

        let skia_canvas = surface.canvas();

        skia_canvas.save();
        if scale_x != 1.0 || scale_y != 1.0 {
            skia_canvas.scale((scale_x, scale_y));
        }
        let buffer_age = if resized {
            0
        } else {
            u8::try_from(self.glutin_surface.buffer_age()).unwrap_or_default()
        };
        let dirty_region = callback(skia_canvas, Some(gr_context), buffer_age);
        skia_canvas.restore();

        if let Some(pre_present_callback) = pre_present_callback.borrow_mut().as_mut() {
            pre_present_callback();
        }

        let swap_size = PhysicalWindowSize::new(render_w.get(), render_h.get());
        let swap_result = Self::swap_with_optional_damage(
            &self.glutin_surface,
            current_context,
            _window.scale_factor() * scale_x,
            _window.scale_factor() * scale_y,
            swap_size,
            dirty_region.as_ref(),
        );
        swap_result.map(|_| DrawOutcome::Success).map_err(|glutin_error| {
            format!("Skia OpenGL Renderer: Error swapping buffers: {glutin_error}").into()
        })
    }

    fn resize_event(&self, size: PhysicalWindowSize) -> Result<(), PlatformError> {
        self.ensure_context_current()?;

        if size.width > 0 && size.height > 0 {
            let (render_w, render_h, _, _) = Self::scaled_dimensions(size);
            self.glutin_surface.resize(&self.glutin_context, render_w, render_h);
        }

        Ok(())
    }

    fn bits_per_pixel(&self) -> Result<u8, PlatformError> {
        let config = self.glutin_context.config();
        let rgb_bits = match config.color_buffer_type() {
            Some(glutin::config::ColorBufferType::Rgb { r_size, g_size, b_size }) => {
                r_size + g_size + b_size
            }
            other => {
                return Err(format!(
                    "Skia OpenGL Renderer: unsupported color buffer {other:?} encountered"
                )
                .into());
            }
        };
        Ok(rgb_bits + config.alpha_size())
    }

    fn import_opengl_texture(
        &self,
        canvas: &skia_safe::Canvas,
        BorrowedOpenGLTexture { texture_id, size, origin, .. }: &BorrowedOpenGLTexture,
    ) -> Option<skia_safe::Image> {
        unsafe {
            let mut texture_info = skia_safe::gpu::gl::TextureInfo::from_target_and_id(
                glow::TEXTURE_2D,
                texture_id.get(),
            );
            texture_info.format = glow::RGBA8;
            let backend_texture = skia_safe::gpu::backend_textures::make_gl(
                (size.width as _, size.height as _),
                skia_safe::gpu::Mipmapped::No,
                texture_info,
                "Borrowed GL texture",
            );
            skia_safe::image::Image::from_texture(
                canvas.recording_context().as_mut().unwrap(),
                &backend_texture,
                match origin {
                    i_slint_core::graphics::BorrowedOpenGLTextureOrigin::TopLeft => {
                        skia_safe::gpu::SurfaceOrigin::TopLeft
                    }
                    i_slint_core::graphics::BorrowedOpenGLTextureOrigin::BottomLeft => {
                        skia_safe::gpu::SurfaceOrigin::BottomLeft
                    }
                    _ => unimplemented!(
                        "internal error: missing implementation for BorrowedOpenGLTextureOrigin"
                    ),
                },
                skia_safe::ColorType::RGBA8888,
                skia_safe::AlphaType::Unpremul,
                crate::sampled_texture_color_space(crate::TextureEncoding::Unorm),
            )
        }
    }
}

impl OpenGLSurface {
    pub fn new_with_config(
        window_handle: Arc<dyn raw_window_handle::HasWindowHandle>,
        display_handle: Arc<dyn raw_window_handle::HasDisplayHandle>,
        size: PhysicalWindowSize,
        requested_opengl_version: Option<RequestedOpenGLVersion>,
        config_builder: glutin::config::ConfigTemplateBuilder,
        config_filter: Option<&dyn Fn(&glutin::config::Config) -> bool>,
    ) -> Result<Self, PlatformError> {
        let (width, height, _, _) = Self::scaled_dimensions(size);

        let window_handle = window_handle
            .window_handle()
            .map_err(|e| format!("error obtaining window handle for skia opengl renderer: {e}"))?;
        let display_handle = display_handle
            .display_handle()
            .map_err(|e| format!("error obtaining display handle for skia opengl renderer: {e}"))?;

        if let Some(parked) = PARKED_CONTEXT.with(|slot| slot.borrow_mut().take()) {
            return Self::from_parked(parked, window_handle, width, height);
        }

        let (current_glutin_context, glutin_surface) = Self::init_glutin(
            window_handle,
            display_handle,
            width,
            height,
            requested_opengl_version,
            config_builder,
            config_filter,
        )?;

        glutin_surface.resize(&current_glutin_context, width, height);

        let fb_info = {
            use glow::HasContext;

            let gl = unsafe {
                glow::Context::from_loader_function_cstr(|name| {
                    current_glutin_context.display().get_proc_address(name) as *const _
                })
            };
            let fboid = unsafe { gl.get_parameter_i32(glow::FRAMEBUFFER_BINDING) };

            skia_safe::gpu::gl::FramebufferInfo {
                fboid: fboid.try_into().map_err(|_| {
                    "Skia Renderer: Internal error, framebuffer binding returned signed id"
                        .to_string()
                })?,
                format: skia_safe::gpu::gl::Format::RGBA8.into(),
                ..Default::default()
            }
        };

        let gl_interface = skia_safe::gpu::gl::Interface::new_load_with_cstr(|name| {
            current_glutin_context.display().get_proc_address(name) as *const _
        })
        .ok_or_else(|| {
            "Skia Renderer: Internal Error: Could not create OpenGL Interface".to_string()
        })?;

        let mut gr_context =
            skia_safe::gpu::direct_contexts::make_gl(gl_interface, None).ok_or_else(|| {
                "Skia Renderer: Internal Error: Could not create Skia Direct Context from GL interface".to_string()
            })?;
        // Skia caches up to 256 MB of GPU resources (textures, glyph atlases) by
        // default and frees them only above that; on a TV that driver memory keeps
        // growing. A parked context keeps this limit when it is reused.
        gr_context.set_resource_cache_limit(24 * 1024 * 1024);

        let width_i32: i32 = width.get().try_into().map_err(|e| {
                format!("Attempting to create window surface with width that doesn't fit into non-zero i32: {e}")
            })?;
        let height_i32: i32 = height.get().try_into().map_err(|e| {
                format!(
                    "Attempting to create window surface with height that doesn't fit into non-zero i32: {e}"
                )
            })?;

        let surface = Self::create_internal_surface(
            fb_info,
            &current_glutin_context,
            &mut gr_context,
            width_i32,
            height_i32,
        )?
        .into();

        Ok(Self {
            fb_info,
            surface,
            gr_context: RefCell::new(gr_context),
            glutin_context: GlutinContext::new(current_glutin_context),
            glutin_surface,
        })
    }

    /// A surface for a new window that reuses a parked GL and Skia context.
    fn from_parked(
        parked: ParkedContext,
        window_handle: raw_window_handle::WindowHandle<'_>,
        width: NonZeroU32,
        height: NonZeroU32,
    ) -> Result<Self, PlatformError> {
        let ParkedContext { context, mut gr_context, fb_info } = parked;
        let config = context.config();
        let attrs = SurfaceAttributesBuilder::<WindowSurface>::new().build(
            window_handle.as_raw(),
            width,
            height,
        );
        let glutin_surface = unsafe {
            config
                .display()
                .create_window_surface(&config, &attrs)
                .map_err(|e| format!("Error creating OpenGL window surface: {e}"))?
        };
        let context = context.make_current(&glutin_surface).map_err(
            |glutin_error: glutin::error::Error| -> PlatformError {
                format!("Skia Renderer: Failed to make reused OpenGL context current: {glutin_error}")
                    .into()
            },
        )?;
        glutin_surface
            .set_swap_interval(
                &context,
                glutin::surface::SwapInterval::Wait(NonZeroU32::new(1).unwrap()),
            )
            .ok();
        glutin_surface.resize(&context, width, height);
        // The GL state Skia tracks belongs to the previous window's surface.
        gr_context.reset(None);
        let surface = Self::create_internal_surface(
            fb_info,
            &context,
            &mut gr_context,
            width.get() as i32,
            height.get() as i32,
        )?
        .into();
        Ok(Self {
            fb_info,
            surface,
            gr_context: RefCell::new(gr_context),
            glutin_context: GlutinContext::new(context),
            glutin_surface,
        })
    }

    fn init_glutin(
        _window_handle: raw_window_handle::WindowHandle<'_>,
        _display_handle: raw_window_handle::DisplayHandle<'_>,
        width: NonZeroU32,
        height: NonZeroU32,
        requested_opengl_version: Option<RequestedOpenGLVersion>,
        config_template_builder: glutin::config::ConfigTemplateBuilder,
        config_filter: Option<&dyn Fn(&glutin::config::Config) -> bool>,
    ) -> Result<
        (
            glutin::context::PossiblyCurrentContext,
            glutin::surface::Surface<glutin::surface::WindowSurface>,
        ),
        PlatformError,
    > {
        cfg_if::cfg_if! {
            if #[cfg(target_os = "macos")] {
                let display_api_preference = glutin::display::DisplayApiPreference::Cgl;
            } else if #[cfg(not(target_family = "windows"))] {
                let display_api_preference = glutin::display::DisplayApiPreference::Egl;
            } else {
                let display_api_preference = glutin::display::DisplayApiPreference::EglThenWgl(Some(_window_handle.as_raw()));
            }
        }

        let gl_display = unsafe {
            glutin::display::Display::new(_display_handle.as_raw(), display_api_preference)
                .map_err(|glutin_error| {
                    format!(
                        "Error creating glutin display for native display {:?}: {}",
                        _display_handle.as_raw(),
                        glutin_error
                    )
                })?
        };

        // On macOS, there's only one GL config and that's initialized based on the values in the config template
        // builder. So if that one has transparency enabled, it'll show up in the config, and will be set on the
        // context later. So we must enable it here, there's no way of enabling it later.
        // On EGL/GLX/WGL there are system provided configs that may or may not support transparency. Here in case
        // the system doesn't support transparency, we want to fall back to a config that doesn't - better than not
        // rendering anything at all. So we don't want to limit the configurations we get to see early on.
        // Commented out due to https://github.com/rust-windowing/glutin/issues/1640
        #[cfg(target_os = "macos")]
        let config_template_builder = config_template_builder.with_transparency(true);

        // Upstream advises to use this only on Windows.
        #[cfg(target_family = "windows")]
        let config_template_builder =
            config_template_builder.compatible_with_native_window(_window_handle.as_raw());

        let config_template = config_template_builder.build();

        let prefer_opaque =
            std::env::var_os("SLINT_SKIA_OPAQUE_SURFACE").is_some_and(|v| v != "0");

        let config = unsafe {
            gl_display
                .find_configs(config_template)
                .map_err(|e| format!("Could not find valid OpenGL display configurations: {e}"))?
                .filter(|config| config_filter.as_ref().is_none_or(|filter_fn| filter_fn(config)))
                .reduce(|accum, config| {
                    // Never prefer MSAA over non-MSAA (costly on tile-based embedded GPUs).
                    if config.num_samples() < accum.num_samples() {
                        return config;
                    }
                    if config.num_samples() > accum.num_samples() {
                        return accum;
                    }
                    let cfg_alpha = config.supports_transparency().unwrap_or(false);
                    let acc_alpha = accum.supports_transparency().unwrap_or(false);
                    let better_alpha = if prefer_opaque {
                        !cfg_alpha && acc_alpha
                    } else {
                        cfg_alpha && !acc_alpha
                    };
                    if better_alpha { config } else { accum }
                })
                .ok_or("Unable to find suitable GL config")?
        };

        let requested_opengl_version =
            requested_opengl_version.unwrap_or(RequestedOpenGLVersion::OpenGLES(Some((3, 0))));
        let preferred_context_attributes = match requested_opengl_version {
            RequestedOpenGLVersion::OpenGL(version) => {
                let version =
                    version.map(|(major, minor)| glutin::context::Version { major, minor });
                ContextAttributesBuilder::new()
                    .with_context_api(ContextApi::OpenGl(version))
                    .build(Some(_window_handle.as_raw()))
            }
            RequestedOpenGLVersion::OpenGLES(version) => {
                let version =
                    version.map(|(major, minor)| glutin::context::Version { major, minor });

                ContextAttributesBuilder::new()
                    .with_context_api(ContextApi::Gles(version))
                    .build(Some(_window_handle.as_raw()))
            }
        };

        let gles2_fallback_context_attributes = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::Gles(Some(glutin::context::Version {
                major: 2,
                minor: 0,
            })))
            .build(Some(_window_handle.as_raw()));

        let fallback_context_attributes =
            ContextAttributesBuilder::new().build(Some(_window_handle.as_raw()));

        let not_current_gl_context = unsafe {
            gl_display
                .create_context(&config, &preferred_context_attributes)
                .or_else(|_| gl_display.create_context(&config, &gles2_fallback_context_attributes))
                .or_else(|_| gl_display.create_context(&config, &fallback_context_attributes))
                .map_err(|e| format!("Error creating OpenGL context: {e}"))
        }?;

        let attrs = SurfaceAttributesBuilder::<WindowSurface>::new().build(
            _window_handle.as_raw(),
            width,
            height,
        );

        let surface = unsafe {
            config
                .display()
                .create_window_surface(&config, &attrs)
                .map_err(|e| format!("Error creating OpenGL window surface: {e}"))?
        };

        let context = not_current_gl_context.make_current(&surface)
            .map_err(|glutin_error: glutin::error::Error| -> PlatformError {
                format!("Skia Renderer: Failed to make newly created OpenGL context current: {glutin_error}")
                .into()
        })?;

        // Align the GL layer to the top-left, so that resizing only invalidates the bottom/right
        // part of the window.
        #[cfg(target_os = "macos")]
        if let raw_window_handle::RawWindowHandle::AppKit(raw_window_handle::AppKitWindowHandle {
            ns_view,
            ..
        }) = _window_handle.as_raw()
        {
            let ns_view: &objc2_app_kit::NSView = unsafe { ns_view.cast().as_ref() };
            ns_view.setLayerContentsPlacement(objc2_app_kit::NSViewLayerContentsPlacement::TopLeft);
        }

        // Sanity check, as all this might succeed on Windows without working GL drivers, but this will fail:
        if context
            .display()
            .get_proc_address(&std::ffi::CString::new("glCreateShader").unwrap())
            .is_null()
        {
            return Err(
                "Failed to initialize OpenGL driver: Could not locate glCreateShader symbol"
                    .to_string()
                    .into(),
            );
        }

        // Try to default to vsync and ignore if the driver doesn't support it.
        surface
            .set_swap_interval(
                &context,
                glutin::surface::SwapInterval::Wait(NonZeroU32::new(1).unwrap()),
            )
            .ok();

        Ok((context, surface))
    }

    fn create_internal_surface(
        fb_info: skia_safe::gpu::gl::FramebufferInfo,
        gl_context: &glutin::context::PossiblyCurrentContext,
        gr_context: &mut skia_safe::gpu::DirectContext,
        width: i32,
        height: i32,
    ) -> Result<skia_safe::Surface, PlatformError> {
        let config = gl_context.config();

        let backend_render_target = skia_safe::gpu::backend_render_targets::make_gl(
            (width, height),
            Some(config.num_samples() as _),
            config.stencil_size() as _,
            fb_info,
        );
        match skia_safe::gpu::surfaces::wrap_backend_render_target(
            gr_context,
            &backend_render_target,
            skia_safe::gpu::SurfaceOrigin::BottomLeft,
            skia_safe::ColorType::RGBA8888,
            crate::attachment_color_space(crate::TextureEncoding::Unorm),
            None,
        ) {
            Some(surface) => Ok(surface),
            None => {
                Err("Skia OpenGL Renderer: Failed to allocate internal backend rendering target"
                    .into())
            }
        }
    }

    /// Reads the live UI render height cap from `$XDG_CONFIG_HOME/rusttv/render-height`
    /// (written by Settings → Display) or falls back to `SLINT_SKIA_MAX_HEIGHT`.
    /// Cached for 250 ms so normal 60 fps rendering performs zero syscalls per frame
    /// while live resolution changes in Settings take effect immediately.
    fn current_max_height() -> Option<u32> {
        struct Cache {
            checked: Instant,
            path: Option<PathBuf>,
            env_fallback: Option<u32>,
            value: Option<u32>,
        }
        static CACHE: Mutex<Option<Cache>> = Mutex::new(None);
        let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let entry = guard.get_or_insert_with(|| {
            let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
            let path = var("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|| var("HOME").map(|h| PathBuf::from(h).join(".config")))
                .map(|dir| dir.join("rusttv").join("render-height"));
            let env_fallback = std::env::var("SLINT_SKIA_MAX_HEIGHT")
                .ok()
                .and_then(|v| v.trim().parse::<u32>().ok())
                .filter(|&h| h >= 240);
            let value = path
                .as_deref()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .and_then(|s| s.trim().parse::<u32>().ok())
                .map(|h| (h >= 240).then_some(h))
                .unwrap_or(env_fallback);
            Cache { checked: now, path, env_fallback, value }
        });
        if now.duration_since(entry.checked) >= Duration::from_millis(250) {
            entry.checked = now;
            entry.value = entry
                .path
                .as_deref()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .and_then(|s| s.trim().parse::<u32>().ok())
                .map(|h| (h >= 240).then_some(h))
                .unwrap_or(entry.env_fallback);
        }
        entry.value
    }

    /// Optionally caps the EGL render buffer height (e.g. `720` on a 1920×1080
    /// display), relying on Wayland `wp_viewporter` (already configured by
    /// `winit`) to hardware-scale the buffer back to full screen at 0 ms GPU cost.
    fn scaled_dimensions(size: PhysicalWindowSize) -> (NonZeroU32, NonZeroU32, f32, f32) {
        let max_h = Self::current_max_height();
        let w = size.width.max(1);
        let h = size.height.max(1);
        if let Some(cap) = max_h
            && h > cap
        {
            let sy = cap as f32 / h as f32;
            // Round width to the nearest even pixel (exact 1366×768, 854×480, etc.).
            let sw = (((w as f32 * sy) / 2.0).round() as u32 * 2).max(2);
            let sx = sw as f32 / w as f32;
            return (
                NonZeroU32::new(sw).unwrap(),
                NonZeroU32::new(cap).unwrap(),
                sx,
                sy,
            );
        }
        (NonZeroU32::new(w).unwrap(), NonZeroU32::new(h).unwrap(), 1.0, 1.0)
    }

    fn ensure_context_current(&self) -> Result<(), PlatformError> {
        if !self.glutin_context.is_current() {
            self.glutin_context.make_current(&self.glutin_surface).map_err(
                |glutin_error| -> PlatformError {
                    format!("Skia Renderer: Error making context current: {glutin_error}").into()
                },
            )?;
        }
        Ok(())
    }

    fn swap_with_optional_damage(
        surface: &glutin::surface::Surface<glutin::surface::WindowSurface>,
        context: &glutin::context::PossiblyCurrentContext,
        scale_x: f32,
        scale_y: f32,
        size: PhysicalWindowSize,
        dirty_region: Option<&DirtyRegion>,
    ) -> glutin::error::Result<()> {
        #[cfg(target_os = "linux")]
        if let (
            glutin::surface::Surface::Egl(egl_surface),
            glutin::context::PossiblyCurrentContext::Egl(egl_context),
            Some(region),
        ) = (surface, context, dirty_region)
        {
            let win_w = size.width as i32;
            let win_h = size.height as i32;
            if win_w > 0 && win_h > 0 {
                let mut rects = Vec::new();
                for rect in region.iter() {
                    let x0 = (rect.min.x * scale_x).floor() as i32;
                    let y0 = (rect.min.y * scale_y).floor() as i32;
                    let x1 = (rect.max.x * scale_x).ceil() as i32;
                    let y1 = (rect.max.y * scale_y).ceil() as i32;
                    let cx0 = x0.clamp(0, win_w);
                    let cy0 = y0.clamp(0, win_h);
                    let cx1 = x1.clamp(cx0, win_w);
                    let cy1 = y1.clamp(cy0, win_h);
                    let w = cx1 - cx0;
                    let h = cy1 - cy0;
                    if w > 0 && h > 0 {
                        // EGL surface damage origin is bottom-left.
                        rects.push(glutin::surface::Rect::new(cx0, win_h - cy1, w, h));
                    }
                }
                if !rects.is_empty() {
                    return egl_surface
                        .swap_buffers_with_damage(egl_context, &rects)
                        .or_else(|_| surface.swap_buffers(context));
                }
            }
        }
        let _ = (scale_x, scale_y, size, dirty_region);
        surface.swap_buffers(context)
    }
}

impl Drop for OpenGLSurface {
    fn drop(&mut self) {
        // Make sure that the context is current before Skia calls glDelete***
        // In the event that this fails for some reason (lost GL context), convey that to Skia so that it doesn't try to call
        // glDelete***
        if self.ensure_context_current().is_err() {
            i_slint_core::debug_log!(
                "Skia OpenGL Renderer warning: Failed to make context current for destruction - considering context abandoned."
            );
            self.gr_context.borrow_mut().abandon();
            return;
        }
        // Free the window's render target while the context is current, then
        // park the contexts for the next window instead of destroying them.
        if let Some(placeholder) = skia_safe::surfaces::raster_n32_premul((1, 1)) {
            *self.surface.borrow_mut() = placeholder;
        }
        let mut gr_context = self.gr_context.borrow().clone();
        gr_context.flush_and_submit();
        if let Some(context) = self.glutin_context.0.take() {
            match context.make_not_current() {
                Ok(context) => PARKED_CONTEXT.with(|slot| {
                    *slot.borrow_mut() = Some(ParkedContext { context, gr_context, fb_info: self.fb_info });
                }),
                Err(e) => i_slint_core::debug_log!(
                    "Skia OpenGL Renderer: Failed to make context not current: {e}"
                ),
            }
        }
    }
}
