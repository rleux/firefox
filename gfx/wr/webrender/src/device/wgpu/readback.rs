/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::super::textures::texture_format;

#[derive(Default)]
pub(super) struct ReadbackState {
    requested: bool,
    completed: Option<super::super::PendingReadback>,
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
            completed: self.readback.completed.take(),
            ..Default::default()
        };
    }

    #[cfg(feature = "capture")]
    pub(super) fn external_readback_source(
        &mut self,
        handle: api::ExternalTextureHandle,
        target: api::ImageBufferKind,
    ) -> Result<Rc<Texture>, String> {
        if let Some(error) = self.failure() {
            return Err(error.to_owned());
        }
        if target != api::ImageBufferKind::Texture2D {
            return Err("Unsupported Vulkan external readback target".into());
        }
        let id = u32::try_from(handle.0)
            .map_err(|_| "Invalid Vulkan external readback handle".to_owned())?;
        let source = self.textures.external_textures().get(id)?;
        if source.supports_copy_src() {
            return Ok(source);
        }
        self.flush_pass()?;
        let size = source.size();
        let snapshot = self
            .scratch
            .acquire(size.width, size.height, source.format(), true)?;
        self.prepare_blitter(&snapshot)?;
        let rect =
            DeviceIntRect::from_size(DeviceIntSize::new(size.width as i32, size.height as i32));
        // Sample the view to preserve swizzling without requiring COPY_SRC on imports.
        self.blitter[&snapshot.format()].record(
            &mut self.submissions.recording()?,
            &self.submissions,
            &mut self.scratch,
            TextureBlit {
                source: &source,
                target: &snapshot,
                source_rect: rect,
                target_rect: rect,
                filter: TextureFilter::Nearest,
            },
        )?;
        Ok(snapshot)
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

    fn prepare_capture(
        &mut self,
        source: Rc<Texture>,
        rect: DeviceIntRect,
        format: ImageFormat,
    ) -> Result<(super::super::PendingReadback, bool), String> {
        self.flush_pass()?;
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
        let previous = self.readback.completed.take();
        Ok((source.readback_reusing(rect, previous)?, swizzle))
    }

    pub fn capture_pixels(
        &mut self,
        source: Rc<Texture>,
        rect: DeviceIntRect,
        format: ImageFormat,
    ) -> Result<Vec<u8>, String> {
        let (mut readback, swizzle) = self.prepare_capture(source, rect, format)?;
        let mut pixels = readback.wait()?;
        if swizzle {
            for pixel in pixels.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
        }
        self.readback.completed = Some(readback);
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
        let (mut readback, swizzle) = self.prepare_capture(source, rect, format)?;
        readback.wait_into(output)?;
        if swizzle {
            for pixel in output.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
        }
        self.readback.completed = Some(readback);
        Ok(())
    }
}
