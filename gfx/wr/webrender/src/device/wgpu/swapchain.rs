/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::window_surface::WindowSurface;
use super::{hal, InitialSurface, SubmissionQueue, SurfaceOptions, SurfaceWindow};
use crate::device::PresentResult;
use std::rc::Rc;

#[path = "swapchain_target.rs"]
mod target;
pub(super) use self::target::{SurfacePassTarget, SurfaceView};

pub(super) struct Swapchain {
    surface: Option<WindowSurface>,
    queue: Rc<SubmissionQueue>,
    config: Option<hal::SurfaceConfiguration>,
    acquired: Option<Rc<hal::DynAcquiredSurfaceTexture>>,
    // Keep views owned here even if a target guard is forgotten.
    target: Option<Rc<target::AttachmentResources>>,
    reconfigure: bool,
    paused: bool,
    present_result: Option<PresentResult>,
    readback_enabled: bool,
}

pub(super) struct AcquiredImage<'a> {
    swapchain: &'a mut Swapchain,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum PresentationStatus {
    Presented { suboptimal: bool },
    Timeout,
    Occluded,
    Outdated,
    Lost,
}

impl Swapchain {
    pub fn new(queue: &Rc<SubmissionQueue>) -> Option<Self> {
        queue.owner().initial_surface.take().map(|surface| Self {
            surface: match surface {
                InitialSurface::Detached => None,
                InitialSurface::Attached(surface) => Some(surface),
            },
            queue: queue.clone(),
            config: None,
            acquired: None,
            target: None,
            reconfigure: false,
            paused: false,
            present_result: None,
            readback_enabled: false,
        })
    }

    pub fn configuration(&self) -> Option<&hal::SurfaceConfiguration> {
        self.config.as_ref()
    }

    pub fn configure(&mut self, size: [u32; 2], options: SurfaceOptions) -> Result<(), String> {
        if self.acquired.is_some() {
            return Err("Cannot reconfigure a Vulkan swapchain with an acquired image".into());
        }
        let Some(surface) = self.surface.as_mut() else {
            return if size.contains(&0) {
                Ok(())
            } else {
                Err("No Vulkan window is attached".into())
            };
        };
        let owner = self.queue.owner();
        if owner.is_lost() {
            return Err("Cannot configure a Vulkan swapchain on a lost device".into());
        }
        let config = if size.contains(&0) {
            None
        } else {
            let caps = unsafe { owner.adapter.surface_capabilities(surface.raw.as_ref()) }
                .ok_or("Vulkan adapter no longer supports the window surface")?;
            let mut config = owner.surface_configuration(&caps, size, options)?;
            if !self.readback_enabled {
                config.usage.remove(super::wgt::TextureUses::COPY_SRC);
            }
            Some(config)
        };
        if self.config.is_some() {
            self.queue.wait()?;
            unsafe { owner.open.queue.wait_for_idle() }.map_err(|error| {
                owner.lost.set(true);
                format!("Waiting to reconfigure Vulkan swapchain: {error:?}")
            })?;
            unsafe { surface.raw.unconfigure(owner.open.device.as_ref()) };
            self.config = None;
        }
        if let Some(config) = config {
            unsafe { surface.raw.configure(owner.open.device.as_ref(), &config) }.map_err(
                |error| {
                    if matches!(error, hal::SurfaceError::Device(hal::DeviceError::Lost)) {
                        owner.lost.set(true);
                    }
                    format!("Configuring Vulkan swapchain: {error}")
                },
            )?;
            self.config = Some(config);
        }
        self.reconfigure = false;
        surface.options = options;
        Ok(())
    }

    pub fn acquire(&mut self) -> Result<Option<AcquiredImage<'_>>, hal::SurfaceError> {
        Ok(if self.acquire_image()? {
            Some(AcquiredImage { swapchain: self })
        } else {
            None
        })
    }

    fn acquire_image(&mut self) -> Result<bool, hal::SurfaceError> {
        if self.acquired.is_some() {
            return Err(hal::SurfaceError::Other(
                "A Vulkan swapchain image is already acquired",
            ));
        }
        if self.queue.owner().is_lost() {
            return Err(hal::SurfaceError::Device(hal::DeviceError::Lost));
        }
        if self.config.is_none() {
            return Ok(false);
        }
        #[cfg(test)]
        if let Some(error) = testing::acquire_error() {
            return Err(error);
        }
        let surface = &self.surface.as_ref().unwrap().raw;
        let image = unsafe { self.queue.acquire_surface(surface.as_ref()) }.map_err(|error| {
            if matches!(error, hal::SurfaceError::Device(hal::DeviceError::Lost)) {
                self.queue.owner().lost.set(true);
            }
            error
        })?;
        self.acquired = Some(image);
        Ok(true)
    }

    pub fn set_window(
        &mut self,
        window: Option<Rc<dyn SurfaceWindow>>,
        options: SurfaceOptions,
    ) -> Result<(), String> {
        if self.acquired.is_some() {
            return Err("Cannot replace a Vulkan window with an acquired image".into());
        }
        let surface = window
            .as_ref()
            .map(|window| self.queue.owner().create_surface(window, options))
            .transpose()?;
        self.queue.wait()?;
        self.configure([0, 0], options)?;
        self.surface = surface;
        self.reconfigure = false;
        self.present_result = None;
        Ok(())
    }

    pub fn set_paused(&mut self, paused: bool) -> Result<(), String> {
        if paused {
            if let Some(options) = self.surface.as_ref().map(|surface| surface.options) {
                self.configure([0, 0], options)?;
            }
        }
        self.paused = paused;
        self.present_result = None;
        Ok(())
    }

    pub fn prepare_target(&mut self, size: [u32; 2]) -> Result<bool, String> {
        if self.paused || self.surface.is_none() {
            self.present_result = Some(PresentResult::Occluded);
            return Ok(false);
        }
        if self.target.is_some() {
            let extent = self.config.as_ref().unwrap().extent;
            if [extent.width, extent.height] != size {
                return Err("Cannot resize an acquired Vulkan frame".into());
            }
            return Ok(true);
        }
        if self.present_result.is_some() {
            return Ok(false);
        }
        if self.reconfigure
            || self.config.as_ref().map_or(true, |config| {
                [config.extent.width, config.extent.height] != size
            })
        {
            self.configure(size, self.surface.as_ref().unwrap().options)?;
        }
        if self.config.as_ref().map_or(true, |config| {
            [config.extent.width, config.extent.height] != size
        }) {
            self.present_result = Some(PresentResult::SizeMismatch);
            return Ok(false);
        }
        match self.acquire_image() {
            Ok(true) => self.create_target().map(|_| true),
            Ok(false) | Err(hal::SurfaceError::Occluded) => {
                self.present_result = Some(PresentResult::Occluded);
                Ok(false)
            }
            Err(hal::SurfaceError::Timeout) => {
                self.present_result = Some(PresentResult::Retry);
                Ok(false)
            }
            Err(hal::SurfaceError::Outdated) => {
                self.reconfigure = true;
                self.present_result = Some(PresentResult::Retry);
                Ok(false)
            }
            Err(error) => Err(format!("Acquiring Vulkan output: {error}")),
        }
    }

    pub fn begin_frame(&mut self) {
        self.present_result = None;
    }

    pub fn enable_readback(&mut self) {
        if !self.readback_enabled {
            self.readback_enabled = true;
            self.reconfigure = true;
        }
    }

    pub fn present_result(&self) -> Option<PresentResult> {
        self.present_result
    }

    pub fn finish_target(&mut self) -> Result<(), String> {
        if self.target.is_none() {
            return Ok(());
        }
        self.present_result = Some(match self.present_target()? {
            PresentationStatus::Presented { suboptimal } => {
                self.reconfigure |= suboptimal;
                PresentResult::Presented
            }
            PresentationStatus::Outdated => {
                self.reconfigure = true;
                PresentResult::Retry
            }
            PresentationStatus::Lost => return Err("Vulkan window surface was lost".into()),
            PresentationStatus::Timeout => PresentResult::Retry,
            PresentationStatus::Occluded => PresentResult::Occluded,
        });
        Ok(())
    }

    pub fn discard_acquired(&mut self) -> Result<(), String> {
        self.target.take();
        let Some(image) = self.acquired.take() else {
            return Ok(());
        };
        let queue = &self.queue;
        // Even an unused image needs a submission to consume its acquire semaphore.
        let result = queue
            .submit_surface()
            .and_then(|serial| queue.wait_for(serial));
        if result.is_err() {
            queue.owner().lost.set(true);
            let _ = unsafe { queue.owner().open.queue.wait_for_idle() };
        }
        let image =
            Rc::try_unwrap(image).unwrap_or_else(|_| unreachable!("Acquired image still borrowed"));
        unsafe { self.surface.as_ref().unwrap().raw.discard_texture(image.texture) };
        result?;
        // Vulkan discard does not return the image to the swapchain.
        self.configure([0, 0], self.surface.as_ref().unwrap().options)
    }
}

impl AcquiredImage<'_> {
    pub fn texture(&self) -> &dyn hal::DynTexture {
        self.swapchain.acquired.as_ref().unwrap().texture.as_ref().borrow()
    }

    pub fn configuration(&self) -> &hal::SurfaceConfiguration {
        self.swapchain.config.as_ref().unwrap()
    }

    pub fn discard(self) -> Result<(), String> {
        self.swapchain.discard_acquired()
    }

    /// # Safety
    /// Queued commands must initialize the image and leave it in PRESENT usage.
    pub unsafe fn present(self) -> Result<PresentationStatus, String> {
        self.swapchain.queue.submit_surface()?;
        self.swapchain.target.take();
        let image = self.swapchain.acquired.take().unwrap();
        let image =
            Rc::try_unwrap(image).unwrap_or_else(|_| unreachable!("Acquired image still borrowed"));
        let owner = self.swapchain.queue.owner();
        let result = owner
            .open
            .queue
            .present(self.swapchain.surface.as_ref().unwrap().raw.as_ref(), image.texture);
        let suboptimal = image.suboptimal;
        // Inject after presenting so the real image and semaphores are consumed.
        #[cfg(test)]
        let (result, suboptimal) = testing::present_result(result, suboptimal);
        match result {
            Ok(()) => Ok(PresentationStatus::Presented {
                suboptimal,
            }),
            Err(hal::SurfaceError::Timeout) => Ok(PresentationStatus::Timeout),
            Err(hal::SurfaceError::Occluded) => Ok(PresentationStatus::Occluded),
            Err(hal::SurfaceError::Outdated) => Ok(PresentationStatus::Outdated),
            Err(hal::SurfaceError::Lost) => Ok(PresentationStatus::Lost),
            Err(error) => {
                if matches!(error, hal::SurfaceError::Device(hal::DeviceError::Lost)) {
                    owner.lost.set(true);
                }
                Err(format!("Presenting Vulkan swapchain image: {error}"))
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::hal;
    use std::cell::RefCell;

    thread_local! {
        static ACQUIRE_ERROR: RefCell<Option<hal::SurfaceError>> = const { RefCell::new(None) };
        static PRESENT_RESULT: RefCell<Option<Result<bool, hal::SurfaceError>>> = const { RefCell::new(None) };
    }

    pub fn fail_acquire(error: hal::SurfaceError) {
        ACQUIRE_ERROR.with(|value| assert!(value.replace(Some(error)).is_none()));
    }

    pub fn override_present(result: Result<bool, hal::SurfaceError>) {
        PRESENT_RESULT.with(|value| assert!(value.replace(Some(result)).is_none()));
    }

    pub(super) fn acquire_error() -> Option<hal::SurfaceError> {
        ACQUIRE_ERROR.with(|value| value.borrow_mut().take())
    }

    pub(super) fn present_result(
        result: Result<(), hal::SurfaceError>,
        suboptimal: bool,
    ) -> (Result<(), hal::SurfaceError>, bool) {
        let injected = PRESENT_RESULT.with(|value| value.borrow_mut().take());
        if injected.is_some() {
            result.as_ref().unwrap();
        }
        match injected {
            Some(Ok(suboptimal)) => (result, suboptimal),
            Some(Err(error)) => (Err(error), suboptimal),
            None => (result, suboptimal),
        }
    }
}

impl Drop for AcquiredImage<'_> {
    fn drop(&mut self) {
        let _ = self.swapchain.discard_acquired();
    }
}

impl Drop for Swapchain {
    fn drop(&mut self) {
        let _ = self.discard_acquired();
        if self.config.is_some() {
            let _ = self.queue.wait();
            let owner = self.queue.owner();
            unsafe {
                if owner.open.queue.wait_for_idle().is_err() {
                    owner.lost.set(true);
                }
                self.surface.as_ref().unwrap().raw.unconfigure(owner.open.device.as_ref());
            }
        }
    }
}
