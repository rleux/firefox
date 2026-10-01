/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::window_surface::WindowSurface;
use super::{hal, Device, SurfaceOptions};
use std::rc::Rc;
use wgpu_hal::{Adapter as _, Queue as _, Surface as _};

pub(super) struct Swapchain {
    surface: WindowSurface,
    owner: Rc<Device>,
    config: Option<hal::SurfaceConfiguration>,
}

impl Swapchain {
    pub fn new(owner: &Rc<Device>) -> Option<Self> {
        owner.surface.take().map(|surface| Self {
            surface,
            owner: owner.clone(),
            config: None,
        })
    }

    pub fn configuration(&self) -> Option<&hal::SurfaceConfiguration> {
        self.config.as_ref()
    }

    pub fn configure(&mut self, size: [u32; 2], options: SurfaceOptions) -> Result<(), String> {
        if self.owner.is_lost() {
            return Err("Cannot configure a Vulkan swapchain on a lost device".into());
        }
        let config = if size.contains(&0) {
            None
        } else {
            let caps = unsafe { self.owner.adapter.surface_capabilities(&self.surface.raw) }
                .ok_or("Vulkan adapter no longer supports the window surface")?;
            Some(self.owner.surface_configuration(&caps, size, options)?)
        };
        if self.config.is_some() {
            unsafe { self.owner.open.queue.wait_for_idle() }.map_err(|error| {
                self.owner.lost.set(true);
                format!("Waiting to reconfigure Vulkan swapchain: {error:?}")
            })?;
            unsafe { self.surface.raw.unconfigure(&self.owner.open.device) };
            self.config = None;
        }
        if let Some(config) = config {
            unsafe { self.surface.raw.configure(&self.owner.open.device, &config) }.map_err(
                |error| {
                    if matches!(error, hal::SurfaceError::Device(hal::DeviceError::Lost)) {
                        self.owner.lost.set(true);
                    }
                    format!("Configuring Vulkan swapchain: {error}")
                },
            )?;
            self.config = Some(config);
        }
        self.surface.options = options;
        Ok(())
    }
}

impl Drop for Swapchain {
    fn drop(&mut self) {
        if self.config.is_some() {
            unsafe {
                if self.owner.open.queue.wait_for_idle().is_err() {
                    self.owner.lost.set(true);
                }
                self.surface.raw.unconfigure(&self.owner.open.device);
            }
        }
    }
}
