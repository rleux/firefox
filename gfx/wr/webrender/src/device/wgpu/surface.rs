/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{core, images::Image, wgt, Device, Options};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use std::{rc::Rc, sync::Arc};

/// An owner that pins its native window and display handles.
///
/// # Safety
/// Every successfully returned handle must identify the same live native object
/// until this owner is dropped. Implementations must prevent native destruction
/// or replacement during that time, including through interior mutability or
/// other owners; retaining an `Rc` alone does not establish this guarantee.
pub unsafe trait SurfaceWindow: HasDisplayHandle + HasWindowHandle {}

#[derive(Clone, Copy, Debug)]
pub struct SurfaceOptions {
    pub vsync: bool,
    pub transparent: bool,
}
impl Default for SurfaceOptions {
    fn default() -> Self {
        Self {
            vsync: true,
            transparent: false,
        }
    }
}

pub(super) fn validate_display(
    options: &Options,
    owner: Option<&Rc<dyn HasDisplayHandle>>,
) -> Result<(), String> {
    let display = owner
        .map(|owner| {
            owner
                .display_handle()
                .map(|handle| handle.as_raw())
                .map_err(|e| format!("Getting Vulkan display handle: {e}"))
        })
        .transpose()?;
    if let Some(window) = &options.window {
        let actual = window
            .display_handle()
            .map_err(|e| format!("Getting Vulkan window display: {e}"))?
            .as_raw();
        if Some(actual) != display {
            return Err("Vulkan window and display owner must use the same display".into());
        }
    }
    Ok(())
}

pub(super) struct WindowSurface {
    raw: Arc<core::instance::Surface>,
    window: Rc<dyn SurfaceWindow>,
}
impl WindowSurface {
    pub fn supports_adapter(&self, adapter: &core::instance::Adapter) -> bool {
        self.raw.get_capabilities(adapter).is_ok()
    }

    pub fn new(
        instance: &core::instance::Instance,
        window: Rc<dyn SurfaceWindow>,
    ) -> Result<Self, String> {
        let display = window
            .display_handle()
            .map_err(|e| format!("Getting Vulkan display handle: {e}"))?
            .as_raw();
        let handle = window
            .window_handle()
            .map_err(|e| format!("Getting Vulkan window handle: {e}"))?
            .as_raw();
        validate_handles(display, handle)?;
        // SurfaceWindow pins both handles; fields drop the core surface before its owner.
        let raw = unsafe { instance.create_surface(Some(display), handle) }
            .map_err(|e| format!("Creating Vulkan window surface: {e}"))?;
        Ok(Self { raw, window })
    }
}

fn validate_handles(display: RawDisplayHandle, window: RawWindowHandle) -> Result<(), String> {
    use raw_window_handle::{RawDisplayHandle as D, RawWindowHandle as W};
    let valid = match (display, window) {
        #[cfg(target_os = "linux")]
        (D::Xlib(d), W::Xlib(w)) => d.display.is_some() && w.window != 0,
        #[cfg(target_os = "linux")]
        (D::Xcb(d), W::Xcb(_)) => d.connection.is_some(),
        #[cfg(target_os = "linux")]
        (D::Wayland(_), W::Wayland(_)) => true,
        #[cfg(target_os = "windows")]
        (D::Windows(_), W::Win32(w)) => w.hinstance.is_some(),
        #[cfg(target_os = "android")]
        (D::Android(_), W::AndroidNdk(_)) => true,
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err("Vulkan surface requires matching live native display and window handles".into())
    }
}

pub(super) fn configuration(
    caps: &wgpu_hal::SurfaceCapabilities,
    size: [u32; 2],
    options: SurfaceOptions,
    limit: u32,
    readback: bool,
) -> Result<wgt::SurfaceConfiguration<Vec<wgt::TextureFormat>>, String> {
    if size.contains(&0) || size.iter().any(|&n| n > limit) {
        return Err("Invalid Vulkan surface dimensions".into());
    }
    if !caps.usage.contains(wgt::TextureUses::COLOR_TARGET) {
        return Err("Vulkan surface does not support direct rendering".into());
    }
    let format = [
        wgt::TextureFormat::Rgba8Unorm,
        wgt::TextureFormat::Bgra8Unorm,
    ]
    .iter()
    .copied()
    .find(|format| {
        caps.formats.iter().any(|entry| {
            entry.format == *format && entry.color_spaces.contains(wgt::SurfaceColorSpaces::SRGB)
        })
    })
    .ok_or("Direct Vulkan rendering requires a UNORM surface in the sRGB color space")?;
    let alpha_mode = if options.transparent {
        if caps
            .composite_alpha_modes
            .contains(&wgt::CompositeAlphaMode::PreMultiplied)
        {
            wgt::CompositeAlphaMode::PreMultiplied
        } else {
            wgt::CompositeAlphaMode::Inherit
        }
    } else {
        wgt::CompositeAlphaMode::Opaque
    };
    if !caps.composite_alpha_modes.contains(&alpha_mode) {
        return Err("Requested Vulkan surface alpha mode is unsupported".into());
    }
    let preferred = if options.vsync {
        wgt::PresentMode::Fifo
    } else {
        wgt::PresentMode::Immediate
    };
    let present_mode = if caps.present_modes.contains(&preferred) {
        preferred
    } else {
        wgt::PresentMode::Fifo
    };
    if !caps.present_modes.contains(&present_mode) {
        return Err("Vulkan surface has no supported present mode".into());
    }
    let extent = caps.current_extent.unwrap_or(wgt::Extent3d {
        width: size[0],
        height: size[1],
        depth_or_array_layers: 1,
    });
    if extent.width == 0 || extent.height == 0 || extent.width > limit || extent.height > limit {
        return Err("Vulkan surface extent is unavailable or exceeds limits".into());
    }
    let mut usage = wgt::TextureUsages::RENDER_ATTACHMENT;
    if readback && caps.usage.contains(wgt::TextureUses::COPY_SRC) {
        usage |= wgt::TextureUsages::COPY_SRC;
    }
    Ok(wgt::SurfaceConfiguration {
        usage,
        format,
        width: extent.width,
        height: extent.height,
        present_mode,
        desired_maximum_frame_latency: 2,
        alpha_mode,
        view_formats: vec![],
        color_space: wgt::SurfaceColorSpace::Srgb,
    })
}

use crate::device::PresentResult as Outcome;

pub(super) struct InitialSurface {
    pub window: Option<WindowSurface>,
    pub display_owner: Rc<dyn HasDisplayHandle>,
    pub options: SurfaceOptions,
}

pub(super) struct Surface {
    acquired: Option<Arc<Image>>,
    window: Option<WindowSurface>,
    config: Option<wgt::SurfaceConfiguration<Vec<wgt::TextureFormat>>>,
    options: SurfaceOptions,
    paused: bool,
    reconfigure: bool,
    suboptimal: bool,
    readback: bool,
    pub outcome: Option<Outcome>,
    owner: Arc<Device>,
    display_owner: Rc<dyn HasDisplayHandle>,
}
impl Surface {
    pub fn new(owner: &Arc<Device>, initial: InitialSurface) -> Self {
        Self {
            acquired: None,
            window: initial.window,
            config: None,
            options: initial.options,
            paused: false,
            reconfigure: false,
            suboptimal: false,
            readback: false,
            outcome: None,
            owner: owner.clone(),
            display_owner: initial.display_owner,
        }
    }
    pub fn begin_frame(&mut self) {
        self.outcome = None;
    }
    pub fn configure(&mut self, size: [u32; 2]) -> Result<(), String> {
        if self.acquired.is_some() {
            return Err("Cannot resize an active Vulkan frame".into());
        }
        let Some(window) = &self.window else {
            return Err("No Vulkan window is attached".into());
        };
        self.owner.check().map_err(|e| e.to_string())?;
        if size.contains(&0) {
            window.raw.unconfigure();
            self.config = None;
            return Ok(());
        }
        let caps = window
            .raw
            .get_hal_capabilities(&self.owner.adapter)
            .map_err(|e| e.to_string())?;
        let config = configuration(
            &caps,
            size,
            self.options,
            self.owner.limits().max_texture_dimension_2d,
            self.readback,
        )?;
        window
            .raw
            .configure(&self.owner.device, &config)
            .map_err(|e| e.to_string())?;
        self.owner.check().map_err(|e| e.to_string())?;
        self.config = Some(config);
        self.reconfigure = false;
        Ok(())
    }
    fn recover(&mut self, status: wgt::SurfaceStatus) -> Result<(), String> {
        use wgt::SurfaceStatus as S;
        self.outcome = Some(match status {
            S::Good | S::Suboptimal => Outcome::Presented,
            S::Validation => return Err("Vulkan surface validation failed".into()),
            S::Timeout => Outcome::Retry,
            S::Occluded => Outcome::Occluded,
            S::Outdated => {
                self.reconfigure = true;
                Outcome::Retry
            }
            S::Lost => {
                let window = self
                    .window
                    .as_ref()
                    .ok_or("No Vulkan window is attached")?
                    .window
                    .clone();
                self.window = None;
                self.config = None;
                self.window = Some(WindowSurface::new(&self.owner.instance, window)?);
                self.window
                    .as_ref()
                    .unwrap()
                    .raw
                    .get_capabilities(&self.owner.adapter)
                    .map_err(|e| e.to_string())?;
                self.reconfigure = true;
                Outcome::Retry
            }
        });
        Ok(())
    }
    pub fn acquire(&mut self, size: [u32; 2]) -> Result<Option<Arc<Image>>, String> {
        if let Some(image) = &self.acquired {
            if [image.size().width, image.size().height] != size {
                return Err("Cannot resize an active Vulkan frame".into());
            }
            return Ok(Some(image.clone()));
        }
        if self.outcome.is_some() {
            return Ok(None);
        }
        if self.paused || self.window.is_none() || size.contains(&0) {
            self.outcome = Some(Outcome::Occluded);
            return Ok(None);
        }
        if self.reconfigure
            || self
                .config
                .as_ref()
                .is_none_or(|c| [c.width, c.height] != size)
        {
            self.configure(size)?;
        }
        if self
            .config
            .as_ref()
            .is_some_and(|c| [c.width, c.height] != size)
        {
            self.outcome = Some(Outcome::SizeMismatch);
            return Ok(None);
        }
        #[cfg(test)]
        let injected_suboptimal = match testing::acquire_status() {
            Some(wgt::SurfaceStatus::Suboptimal) => true,
            Some(wgt::SurfaceStatus::Good) | None => false,
            Some(status) => {
                self.recover(status)?;
                return Ok(None);
            }
        };
        #[cfg(not(test))]
        let injected_suboptimal = false;
        let output = self
            .window
            .as_ref()
            .unwrap()
            .raw
            .get_current_texture()
            .map_err(|e| e.to_string())?;
        self.owner.check().map_err(|e| e.to_string())?;
        if let Some(texture) = output.texture {
            self.suboptimal =
                injected_suboptimal || matches!(output.status, wgt::SurfaceStatus::Suboptimal);
            let image = Image::from_surface(&self.owner, texture)?;
            self.acquired = Some(image.clone());
            Ok(Some(image))
        } else {
            self.recover(output.status)?;
            Ok(None)
        }
    }
    pub fn present(&mut self) -> Result<(), String> {
        let owner = self.owner.clone();
        let _queue_access = owner.queue_access();
        if self.acquired.take().is_none() {
            return Ok(());
        }
        let result = self
            .window
            .as_ref()
            .unwrap()
            .raw
            .present()
            .map_err(|e| e.to_string())?;
        self.owner.check().map_err(|e| e.to_string())?;
        #[cfg(test)]
        let result = testing::present_status().unwrap_or(result);
        let suboptimal = matches!(result, wgt::SurfaceStatus::Suboptimal);
        self.recover(result)?;
        self.reconfigure |= self.suboptimal || suboptimal;
        Ok(())
    }
    pub fn discard(&mut self) -> Result<(), String> {
        if self.acquired.take().is_some() {
            let window = self.window.as_ref().unwrap();
            let result = window.raw.discard().map_err(|e| e.to_string());
            window.raw.unconfigure();
            self.config = None;
            result?;
        }
        Ok(())
    }
    pub fn pause(&mut self, paused: bool) -> Result<(), String> {
        if self.acquired.is_some() {
            return Err("Cannot pause an active Vulkan frame".into());
        }
        if paused {
            if let Some(window) = &self.window {
                window.raw.unconfigure();
            }
            self.config = None;
        }
        self.owner.check().map_err(|e| e.to_string())?;
        self.paused = paused;
        self.outcome = None;
        Ok(())
    }
    pub fn replace(
        &mut self,
        window: Option<Rc<dyn SurfaceWindow>>,
        options: SurfaceOptions,
    ) -> Result<(), String> {
        if self.acquired.is_some() {
            return Err("Cannot replace an active Vulkan frame".into());
        }
        if window.is_none() {
            if let Some(window) = self.window.take() {
                window.raw.unconfigure();
            }
            self.config = None;
            self.options = options;
            self.outcome = None;
            return self.owner.check().map_err(|e| e.to_string());
        }
        self.owner.check().map_err(|e| e.to_string())?;
        let replacement = window
            .map(|window| -> Result<WindowSurface, String> {
                let expected = self
                    .display_owner
                    .display_handle()
                    .map_err(|e| e.to_string())?
                    .as_raw();
                if window.display_handle().map_err(|e| e.to_string())?.as_raw() != expected {
                    return Err("Vulkan window replacement cannot change displays".into());
                }
                let surface = WindowSurface::new(&self.owner.instance, window)?;
                surface
                    .raw
                    .get_capabilities(&self.owner.adapter)
                    .map_err(|e| e.to_string())?;
                Ok(surface)
            })
            .transpose()?;
        if let Some(window) = &self.window {
            window.raw.unconfigure();
        }
        self.owner.check().map_err(|e| e.to_string())?;
        self.window = replacement;
        self.config = None;
        self.options = options;
        self.outcome = None;
        Ok(())
    }
    pub fn has_output(&self) -> bool {
        self.acquired.is_some()
    }
    pub fn snapshot_enabled(&self) -> bool {
        self.readback
            && self.has_output()
            && self
                .config
                .as_ref()
                .is_some_and(|c| c.usage.contains(wgt::TextureUsages::COPY_SRC))
    }
    pub fn enable_readback(&mut self) -> Result<bool, String> {
        if !self.readback {
            self.readback = true;
            self.reconfigure = true;
        }
        self.owner.check().map_err(|e| e.to_string())?;
        self.window
            .as_ref()
            .map(|window| {
                window
                    .raw
                    .get_capabilities(&self.owner.adapter)
                    .map(|caps| caps.usages.contains(wgt::TextureUsages::COPY_SRC))
                    .map_err(|e| e.to_string())
            })
            .unwrap_or(Ok(false))
    }
    pub fn snapshot(
        &self,
        encoder: &Arc<core::command::CommandEncoder>,
    ) -> Result<Arc<Image>, String> {
        let image = self
            .acquired
            .as_ref()
            .ok_or("No Vulkan surface output is acquired")?;
        if !image
            .raw
            .descriptor()
            .usage
            .contains(wgt::TextureUsages::COPY_SRC)
        {
            return Err("Vulkan surface does not support readback".into());
        }
        let result = Image::new(
            &self.owner,
            image.size().width,
            image.size().height,
            image.format(),
            super::TextureFilter::Nearest,
            false,
        )?;
        let info = |image: &Image| wgt::TexelCopyTextureInfo {
            texture: image.raw.clone(),
            mip_level: 0,
            origin: wgt::Origin3d::ZERO,
            aspect: wgt::TextureAspect::All,
        };
        encoder.copy_texture_to_texture(&info(image), &info(&result), &image.size());
        self.owner.check().map_err(|e| e.to_string())?;
        Ok(result)
    }
}
impl Drop for Surface {
    fn drop(&mut self) {
        let _ = self.discard();
        if let Some(window) = &self.window {
            window.raw.unconfigure();
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::wgt;
    use std::cell::Cell;
    thread_local! { pub static ACQUIRE: Cell<Option<wgt::SurfaceStatus>> = const { Cell::new(None) }; pub static PRESENT: Cell<Option<wgt::SurfaceStatus>> = const { Cell::new(None) }; }
    pub fn acquire_status() -> Option<wgt::SurfaceStatus> {
        ACQUIRE.with(|s| s.take())
    }
    pub fn present_status() -> Option<wgt::SurfaceStatus> {
        PRESENT.with(|s| s.take())
    }
}
#[cfg(test)]
#[path = "surface_tests.rs"]
pub(super) mod tests;
