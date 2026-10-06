/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::super::textures::texture_format;

#[derive(Default)]
pub(super) struct ReadbackState {
    requested: bool,
    window: Option<Rc<Texture>>,
    error: Option<String>,
}

#[cfg(test)]
#[path = "readback_device_tests.rs"]
mod tests;

impl RenderDevice {
    pub fn prepare_readback(&mut self, enabled: bool) {
        if enabled {
            if let Some(swapchain) = &mut self.swapchain {
                swapchain.enable_readback();
            }
        }
        self.readback = ReadbackState {
            requested: enabled,
            ..Default::default()
        };
    }

    #[cfg(feature = "capture")]
    pub(super) fn external_readback_source(
        &self,
        handle: api::ExternalTextureHandle,
        target: api::ImageBufferKind,
    ) -> Result<Rc<Texture>, String> {
        if target == api::ImageBufferKind::Texture2D {
            u32::try_from(handle.0)
                .map_err(|_| "Invalid Vulkan external readback handle".to_owned())
                .and_then(|id| self.textures.external_textures().get(id))
        } else {
            Err("Unsupported Vulkan external readback target".into())
        }
    }

    pub(super) fn capture_window(&mut self) {
        self.readback.window = None;
        self.readback.error = None;
        if !self.readback.requested {
            return;
        }
        let Some(swapchain) = &self.swapchain else {
            return;
        };
        let Some(target) = swapchain.current_target() else {
            return;
        };
        self.readback.requested = false;
        let capture = self
            .submissions
            .recording()
            .and_then(|mut commands| target.snapshot(&mut commands));
        match capture {
            Ok(texture) => {
                self.readback.window = Some(texture);
                self.readback.error = None;
            }
            Err(error) => {
                self.readback.window = None;
                self.readback.error = Some(error);
            }
        }
    }

    pub(super) fn capture_source(&self, target: ReadTarget) -> Result<Rc<Texture>, String> {
        if !matches!(target, ReadTarget::Default) {
            return self.textures.read_target(target);
        }
        if self.swapchain.is_some() {
            return self.readback.window.clone().ok_or_else(|| {
                self.readback
                    .error
                    .clone()
                    .unwrap_or_else(|| "No Vulkan window readback was prepared".into())
            });
        }
        self.textures
            .output()
            .ok_or_else(|| "No Vulkan output is available for readback".into())
    }

    pub fn capture_pixels(
        &mut self,
        source: Rc<Texture>,
        rect: DeviceIntRect,
        format: ImageFormat,
    ) -> Result<Vec<u8>, String> {
        if let Some(error) = self.failure() {
            return Err(error.to_owned());
        }
        let native = source.format();
        let desired = texture_format(format);
        let swizzle = native != desired;
        if swizzle
            && !matches!(
                (native, desired),
                (
                    wgt::TextureFormat::Rgba8Unorm,
                    wgt::TextureFormat::Bgra8Unorm
                ) | (
                    wgt::TextureFormat::Bgra8Unorm,
                    wgt::TextureFormat::Rgba8Unorm
                )
            )
        {
            return Err("Unsupported Vulkan readback format conversion".into());
        }
        self.submissions.submit()?;
        let mut pixels = source.readback(rect)?.wait()?;
        if swizzle {
            for pixel in pixels.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
        }
        Ok(pixels)
    }

    pub fn capture_into(
        &mut self,
        source: Rc<Texture>,
        rect: DeviceIntRect,
        format: ImageFormat,
        output: &mut [u8],
    ) -> Result<(), String> {
        if rect.min.x < 0 || rect.min.y < 0 || rect.max.x <= rect.min.x || rect.max.y <= rect.min.y
        {
            return Err("Invalid Vulkan readback rectangle".into());
        }
        let length = (rect.width() as usize)
            .checked_mul(rect.height() as usize)
            .and_then(|area| area.checked_mul(format.bytes_per_pixel() as usize));
        if length != Some(output.len()) {
            return Err("Vulkan readback output size does not match the rectangle".into());
        }
        let pixels = self.capture_pixels(source, rect, format)?;
        output.copy_from_slice(&pixels);
        Ok(())
    }
}
