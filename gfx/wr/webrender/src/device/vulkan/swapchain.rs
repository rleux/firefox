/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::window_surface::WindowSurface;
use super::{hal, SubmissionQueue, SurfaceOptions};
use std::rc::Rc;
use std::borrow::Borrow;
use wgpu_hal::{Adapter as _, Queue as _, Surface as _};

pub(super) struct Swapchain {
    surface: WindowSurface,
    queue: Rc<SubmissionQueue>,
    config: Option<hal::SurfaceConfiguration>,
    acquired: Option<Rc<hal::AcquiredSurfaceTexture<hal::api::Vulkan>>>,
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
        queue.owner().surface.take().map(|surface| Self {
            surface,
            queue: queue.clone(),
            config: None,
            acquired: None,
        })
    }

    pub fn configuration(&self) -> Option<&hal::SurfaceConfiguration> {
        self.config.as_ref()
    }

    pub fn configure(&mut self, size: [u32; 2], options: SurfaceOptions) -> Result<(), String> {
        if self.acquired.is_some() {
            return Err("Cannot reconfigure a Vulkan swapchain with an acquired image".into());
        }
        let owner = self.queue.owner();
        if owner.is_lost() {
            return Err("Cannot configure a Vulkan swapchain on a lost device".into());
        }
        let config = if size.contains(&0) {
            None
        } else {
            let caps = unsafe { owner.adapter.surface_capabilities(&self.surface.raw) }
                .ok_or("Vulkan adapter no longer supports the window surface")?;
            Some(owner.surface_configuration(&caps, size, options)?)
        };
        if self.config.is_some() {
            self.queue.wait()?;
            unsafe { owner.open.queue.wait_for_idle() }.map_err(|error| {
                owner.lost.set(true);
                format!("Waiting to reconfigure Vulkan swapchain: {error:?}")
            })?;
            unsafe { self.surface.raw.unconfigure(&owner.open.device) };
            self.config = None;
        }
        if let Some(config) = config {
            unsafe { self.surface.raw.configure(&owner.open.device, &config) }.map_err(
                |error| {
                    if matches!(error, hal::SurfaceError::Device(hal::DeviceError::Lost)) {
                        owner.lost.set(true);
                    }
                    format!("Configuring Vulkan swapchain: {error}")
                },
            )?;
            self.config = Some(config);
        }
        self.surface.options = options;
        Ok(())
    }

    pub fn acquire(&mut self) -> Result<Option<AcquiredImage<'_>>, hal::SurfaceError> {
        if self.acquired.is_some() {
            return Err(hal::SurfaceError::Other(
                "A Vulkan swapchain image is already acquired",
            ));
        }
        if self.queue.owner().is_lost() {
            return Err(hal::SurfaceError::Device(hal::DeviceError::Lost));
        }
        if self.config.is_none() {
            return Ok(None);
        }
        let image = unsafe { self.queue.acquire_surface(&self.surface.raw) }.map_err(|error| {
            if matches!(error, hal::SurfaceError::Device(hal::DeviceError::Lost)) {
                self.queue.owner().lost.set(true);
            }
            error
        })?;
        self.acquired = Some(image);
        Ok(Some(AcquiredImage { swapchain: self }))
    }

    fn discard_acquired(&mut self) -> Result<(), String> {
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
        unsafe { self.surface.raw.discard_texture(image.texture) };
        result?;
        // Vulkan discard does not return the image to the swapchain.
        self.configure([0, 0], self.surface.options)
    }
}

impl AcquiredImage<'_> {
    pub fn texture(&self) -> &hal::vulkan::Texture {
        self.swapchain.acquired.as_ref().unwrap().texture.borrow()
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
        let image = self.swapchain.acquired.take().unwrap();
        let image =
            Rc::try_unwrap(image).unwrap_or_else(|_| unreachable!("Acquired image still borrowed"));
        let owner = self.swapchain.queue.owner();
        let result = owner
            .open
            .queue
            .present(&self.swapchain.surface.raw, image.texture);
        match result {
            Ok(()) => Ok(PresentationStatus::Presented {
                suboptimal: image.suboptimal,
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
                self.surface.raw.unconfigure(&owner.open.device);
            }
        }
    }
}
