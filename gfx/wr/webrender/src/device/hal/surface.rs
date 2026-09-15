/* This Source Code Form is subject to the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::backend::BackendApi;
use wgpu_hal::Surface as _;
use std::rc::Rc;
pub(crate) mod platform;
pub use platform::SurfaceWindow;

#[derive(Clone, Copy, Debug)]
pub struct SurfaceOptions {
    pub vsync: bool,
    pub transparent: bool,
}

impl Default for SurfaceOptions {
    fn default() -> Self { Self { vsync: true, transparent: false } }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresentationStatus {
    Acquired,
    Presented { suboptimal: bool },
    Suspended,
    Timeout,
    Occluded,
    Outdated,
    Lost,
}

#[derive(Clone, Debug, Default)]
pub struct SurfaceInfo {
    pub size: [u32; 2],
    pub format: Option<wgt::TextureFormat>,
    pub present_mode: Option<wgt::PresentMode>,
    pub alpha_mode: Option<wgt::CompositeAlphaMode>,
    pub generation: u64,
    pub acquired: u64,
    pub present_attempts: u64,
    pub presented: u64,
    pub discarded: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PresentationMethod { Blit, Draw }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PresentationPreference { Auto, Blit, Draw }

impl PresentationPreference {
    fn from_env() -> Result<Self> {
        match std::env::var("WR_HAL_PRESENTATION") {
            Err(std::env::VarError::NotPresent) => Ok(Self::Auto),
            Ok(value) if value == "auto" => Ok(Self::Auto),
            Ok(value) if value == "blit" => Ok(Self::Blit),
            Ok(value) if value == "draw" => Ok(Self::Draw),
            value => Err(format!("Invalid HAL presentation route: {value:?}")),
        }
    }
}

pub(crate) struct SurfaceSetup<A: hal::Api> {
    pub raw: A::Surface,
    pub window: platform::WindowOwner,
}

pub(super) struct SurfaceState<A: BackendApi> {
    pub setup: SurfaceSetup<A>,
    pub owner: Rc<Device<A>>,
    pub options: SurfaceOptions,
    pub config: Option<hal::SurfaceConfiguration>,
    pub acquired: Option<hal::AcquiredSurfaceTexture<A>>,
    pub info: SurfaceInfo,
    preference: PresentationPreference,
    pub presentation: PresentationMethod,
    pub dirty: bool,
    pub lost: bool,
    #[cfg(feature = "hal-testing")]
    injected_loss: bool,
}

impl<A: BackendApi> SurfaceState<A> {
    pub fn new(owner: &Rc<Device<A>>, setup: SurfaceSetup<A>, options: SurfaceOptions) -> Result<Self> {
        Ok(Self { setup, owner: owner.clone(), options, config: None, acquired: None,
            info: SurfaceInfo::default(), dirty: true, lost: false,
            preference: PresentationPreference::from_env()?, presentation: PresentationMethod::Draw,
            #[cfg(feature = "hal-testing")]
            injected_loss: false })
    }

    pub fn configure(&mut self, size: [u32; 2]) -> Result<()> {
        if self.acquired.is_some() { return Err("Cannot configure an acquired surface".into()); }
        { let _guard = self.owner.lock_queue()?;
            unsafe { self.owner.open.queue.wait_for_idle() }.map_err(|error| format!("Retiring presentation: {error:?}"))?; }
        if self.config.take().is_some() { unsafe { self.setup.raw.unconfigure(&self.owner.open.device) }; }
        self.info.size = size;
        self.info.format = None;
        self.info.present_mode = None;
        self.info.alpha_mode = None;
        if self.lost {
            self.setup.raw = self.setup.window.create_surface::<A>(&self.owner._instance)?;
            self.lost = false;
            println!("HAL native surface recreated");
        }
        if size.contains(&0) { self.dirty = false; return Ok(()); }
        let caps = unsafe { self.owner.adapter.surface_capabilities(&self.setup.raw) }
            .ok_or("Selected adapter cannot present to this surface")?;
        let (config, presentation) = negotiate(&caps, size, self.options, self.owner.max_texture_size() as u32,
            self.preference, |format| A::supports_presentation_blit(&self.owner, format))?;
        #[cfg(any(test, feature = "hal-testing"))]
        self.owner.check_fault(FailurePoint::Configure)?;
        unsafe { self.setup.raw.configure(&self.owner.open.device, &config) }
            .map_err(|error| { self.owner.lost.set(true); format!("Configuring surface: {error}") })?;
        self.info.size = [config.extent.width, config.extent.height];
        self.info.format = Some(config.format);
        self.info.present_mode = Some(config.present_mode);
        self.info.alpha_mode = Some(config.composite_alpha_mode);
        self.info.generation += 1;
        println!("HAL presentation: {:?} format={:?} usage={:?}", presentation, config.format, config.usage);
        self.presentation = presentation;
        self.config = Some(config);
        self.dirty = false;
        Ok(())
    }

    pub fn acquire(&mut self, fence: &A::Fence) -> Result<PresentationStatus> {
        if self.acquired.is_some() { return Err("A surface image is already acquired".into()); }
        if self.config.is_none() { return Ok(PresentationStatus::Suspended); }
        #[cfg(feature = "hal-testing")]
        if !self.injected_loss && std::env::var_os("WR_HAL_SURFACE_LOST_ONCE").is_some() {
            self.injected_loss = true;
            println!("HAL injected recoverable surface loss");
            return self.error(hal::SurfaceError::Lost);
        }
        #[cfg(any(test, feature = "hal-testing"))]
        self.owner.check_fault(FailurePoint::Acquire)?;
        match unsafe { self.setup.raw.acquire_texture(Some(std::time::Duration::from_millis(100)), fence) } {
            Ok(acquired) => {
                self.info.acquired += 1;
                self.acquired = Some(acquired);
                Ok(PresentationStatus::Acquired)
            }
            Err(error) => self.error(error),
        }
    }

    pub fn error(&mut self, error: hal::SurfaceError) -> Result<PresentationStatus> {
        let status = classify_error(error).map_err(|error| { self.owner.lost.set(true); error })?;
        self.dirty |= matches!(status, PresentationStatus::Outdated | PresentationStatus::Lost);
        self.lost |= status == PresentationStatus::Lost;
        Ok(status)
    }

    pub fn present(&mut self) -> Result<PresentationStatus> {
        let acquired = self.acquired.take().ok_or("No acquired surface image")?;
        self.info.present_attempts += 1;
        self.dirty |= acquired.suboptimal;
        let result = { let _guard = self.owner.lock_queue()?; unsafe { self.owner.open.queue.present(&self.setup.raw, acquired.texture) } };
        match result {
            Ok(()) => {
                self.info.presented += 1;
                Ok(PresentationStatus::Presented { suboptimal: acquired.suboptimal })
            }
            Err(error) => self.error(error),
        }
    }

    pub fn discard(&mut self) {
        if let Some(acquired) = self.acquired.take() {
            unsafe { self.setup.raw.discard_texture(acquired.texture) };
            self.info.discarded += 1;
            // HAL's Vulkan discard does not return the image to the swapchain.
            self.dirty = true;
        }
    }
}

fn classify_error(error: hal::SurfaceError) -> Result<PresentationStatus> {
    Ok(match error {
        hal::SurfaceError::Timeout => PresentationStatus::Timeout,
        hal::SurfaceError::Occluded => PresentationStatus::Occluded,
        hal::SurfaceError::Outdated => PresentationStatus::Outdated,
        hal::SurfaceError::Lost => PresentationStatus::Lost,
        error => return Err(format!("Surface failure: {error}")),
    })
}

impl<A: BackendApi> Drop for SurfaceState<A> {
    fn drop(&mut self) {
        unsafe {
            { let _guard = self.owner.queue_gate.lock().unwrap_or_else(|error| error.into_inner());
                let _ = self.owner.open.queue.wait_for_idle(); }
            self.discard();
            if self.config.is_some() { self.setup.raw.unconfigure(&self.owner.open.device); }
        }
    }
}

fn negotiate(caps: &hal::SurfaceCapabilities, size: [u32; 2], options: SurfaceOptions, limit: u32,
    preference: PresentationPreference, supports_blit: impl Fn(wgt::TextureFormat) -> bool)
    -> Result<(hal::SurfaceConfiguration, PresentationMethod)> {
    if size.contains(&0) || size.iter().any(|size| *size > limit) { return Err("Invalid surface dimensions".into()); }
    let available = |format| caps.formats.iter().any(|entry| entry.format == format
        && entry.color_spaces.contains(wgt::SurfaceColorSpaces::SRGB));
    let blit = if preference != PresentationPreference::Draw && caps.usage.contains(wgt::TextureUses::COPY_DST) {
        [wgt::TextureFormat::Rgba8Unorm, wgt::TextureFormat::Bgra8Unorm].iter()
            .copied().find(|format| available(*format) && supports_blit(*format))
    } else { None };
    let (format, presentation, usage) = if let Some(format) = blit {
        (format, PresentationMethod::Blit, wgt::TextureUses::COPY_DST)
    } else {
        if preference == PresentationPreference::Blit { return Err("Requested presentation blit is unsupported".into()); }
        if !caps.usage.contains(wgt::TextureUses::COLOR_TARGET) { return Err("Presentation requires a color-attachment surface".into()); }
        let format = [wgt::TextureFormat::Rgba8Unorm, wgt::TextureFormat::Bgra8Unorm,
            wgt::TextureFormat::Rgba8UnormSrgb, wgt::TextureFormat::Bgra8UnormSrgb].iter()
            .copied().find(|format| available(*format)).ok_or("Surface has no supported sRGB presentation format")?;
        (format, PresentationMethod::Draw, wgt::TextureUses::COLOR_TARGET)
    };
    let alpha = if options.transparent {
        if caps.composite_alpha_modes.contains(&wgt::CompositeAlphaMode::PreMultiplied) {
            wgt::CompositeAlphaMode::PreMultiplied
        } else {
            wgt::CompositeAlphaMode::Inherit
        }
    } else { wgt::CompositeAlphaMode::Opaque };
    if !caps.composite_alpha_modes.contains(&alpha) { return Err("Requested surface alpha mode is unsupported".into()); }
    let preferred = if options.vsync { wgt::PresentMode::Fifo } else { wgt::PresentMode::Immediate };
    let present_mode = if caps.present_modes.contains(&preferred) { preferred } else { wgt::PresentMode::Fifo };
    if !caps.present_modes.contains(&present_mode) { return Err("Surface has no supported present mode".into()); }
    let extent = caps.current_extent.unwrap_or(wgt::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 });
    if extent.width == 0 || extent.height == 0 || extent.width > limit || extent.height > limit { return Err("Surface extent is unavailable or exceeds limits".into()); }
    Ok((hal::SurfaceConfiguration {
        maximum_frame_latency: 2u32.clamp(*caps.maximum_frame_latency.start(), *caps.maximum_frame_latency.end()),
        present_mode, composite_alpha_mode: alpha, format, color_space: wgt::SurfaceColorSpace::Srgb,
        extent, usage, view_formats: Vec::new(),
    }, presentation))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn negotiate(caps: &hal::SurfaceCapabilities, size: [u32; 2], options: SurfaceOptions, limit: u32) -> Result<hal::SurfaceConfiguration> {
        super::negotiate(caps, size, options, limit, PresentationPreference::Blit, |_| true).map(|v| v.0)
    }

    #[test]
    fn draw_presentation_negotiates_only_supported_usages() {
        let mut caps = capabilities();
        for format in [wgt::TextureFormat::Rgba8Unorm, wgt::TextureFormat::Bgra8UnormSrgb] {
            caps.formats[0].format = format;
            caps.usage = wgt::TextureUses::COLOR_TARGET;
            let (config, method) = super::negotiate(&caps, [19, 13], SurfaceOptions::default(), 4096,
                PresentationPreference::Auto, |_| true).unwrap();
            assert_eq!(config.format, format);
            assert_eq!(method, PresentationMethod::Draw);
            assert_eq!(config.usage, wgt::TextureUses::COLOR_TARGET);
            assert!(super::negotiate(&caps, [19, 13], SurfaceOptions::default(), 4096,
                PresentationPreference::Blit, |_| true).is_err());
        }
        caps = capabilities();
        let (config, method) = super::negotiate(&caps, [19, 13], SurfaceOptions::default(), 4096,
            PresentationPreference::Auto, |_| false).unwrap();
        assert_eq!(method, PresentationMethod::Draw);
        assert_eq!(config.usage, wgt::TextureUses::COLOR_TARGET);
        caps.usage = wgt::TextureUses::COPY_DST;
        let (config, method) = super::negotiate(&caps, [19, 13], SurfaceOptions::default(), 4096,
            PresentationPreference::Auto, |_| true).unwrap();
        assert_eq!(method, PresentationMethod::Blit);
        assert_eq!(config.usage, wgt::TextureUses::COPY_DST);
        assert!(super::negotiate(&caps, [19, 13], SurfaceOptions::default(), 4096,
            PresentationPreference::Draw, |_| true).is_err());
    }

    fn capabilities() -> hal::SurfaceCapabilities {
        hal::SurfaceCapabilities {
            formats: vec![wgt::SurfaceFormatCapabilities { format: wgt::TextureFormat::Bgra8Unorm,
                color_spaces: wgt::SurfaceColorSpaces::SRGB }],
            maximum_frame_latency: 1..=3, current_extent: None,
            usage: wgt::TextureUses::COLOR_TARGET | wgt::TextureUses::COPY_DST,
            present_modes: vec![wgt::PresentMode::Fifo],
            composite_alpha_modes: vec![wgt::CompositeAlphaMode::Opaque, wgt::CompositeAlphaMode::PreMultiplied],
        }
    }

    #[test]
    fn surface_configuration_preserves_encoding_and_capabilities() {
        let mut caps = capabilities();
        let config = negotiate(&caps, [137, 99], SurfaceOptions::default(), 4096).unwrap();
        assert_eq!(config.format, wgt::TextureFormat::Bgra8Unorm);
        assert_eq!(config.color_space, wgt::SurfaceColorSpace::Srgb);
        assert_eq!((config.extent.width, config.extent.height), (137, 99));
        assert_eq!(config.maximum_frame_latency, 2);
        let config = negotiate(&caps, [128, 96], SurfaceOptions { vsync: false, transparent: true }, 4096).unwrap();
        assert_eq!(config.present_mode, wgt::PresentMode::Fifo);
        assert_eq!(config.composite_alpha_mode, wgt::CompositeAlphaMode::PreMultiplied);
        caps.current_extent = Some(wgt::Extent3d { width: 71, height: 59, depth_or_array_layers: 1 });
        let config = negotiate(&caps, [128, 96], SurfaceOptions::default(), 4096).unwrap();
        assert_eq!((config.extent.width, config.extent.height), (71, 59));
        assert!(negotiate(&caps, [0, 96], SurfaceOptions::default(), 4096).is_err());
        assert!(negotiate(&caps, [4097, 96], SurfaceOptions::default(), 4096).is_err());
        caps.formats[0].format = wgt::TextureFormat::Bgra8UnormSrgb;
        assert!(negotiate(&caps, [128, 96], SurfaceOptions::default(), 4096).is_err());
        caps = capabilities();
        caps.usage = wgt::TextureUses::COLOR_TARGET;
        assert!(negotiate(&caps, [128, 96], SurfaceOptions::default(), 4096).is_err());
        caps = capabilities();
        caps.composite_alpha_modes = vec![wgt::CompositeAlphaMode::Inherit];
        let config = negotiate(&caps, [128, 96], SurfaceOptions { transparent: true, ..Default::default() }, 4096).unwrap();
        assert_eq!(config.composite_alpha_mode, wgt::CompositeAlphaMode::Inherit);
        caps.composite_alpha_modes = vec![wgt::CompositeAlphaMode::Opaque];
        assert!(negotiate(&caps, [128, 96], SurfaceOptions { transparent: true, ..Default::default() }, 4096).is_err());
    }

    #[test]
    fn surface_failures_are_distinct_from_success() {
        for (error, expected) in [(hal::SurfaceError::Timeout, PresentationStatus::Timeout),
            (hal::SurfaceError::Occluded, PresentationStatus::Occluded),
            (hal::SurfaceError::Outdated, PresentationStatus::Outdated),
            (hal::SurfaceError::Lost, PresentationStatus::Lost)] {
            assert_eq!(classify_error(error).unwrap(), expected);
        }
        assert!(classify_error(hal::SurfaceError::Device(hal::DeviceError::Lost)).is_err());
        assert!(classify_error(hal::SurfaceError::Other("probe")).is_err());
    }
}
