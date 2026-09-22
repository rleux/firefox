/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::super::resources::{bytes_per_pixel, texture_format};
use super::*;

#[derive(Clone, Copy, Debug)]
pub struct Win32ImageLayout {
    descriptor: api::ImageDescriptor,
    allocation_size: u64,
    memory_type: u32,
    device_uuid: [u8; 16],
    driver_uuid: [u8; 16],
}

impl Win32ImageLayout {
    pub fn new(
        descriptor: api::ImageDescriptor,
        allocation_size: u64,
        memory_type: u32,
        device_uuid: [u8; 16],
        driver_uuid: [u8; 16],
    ) -> Result<Self> {
        validate_descriptor(descriptor)?;
        let format = plane_format(descriptor.format)?;
        if descriptor
            .flags
            .contains(api::ImageDescriptorFlags::ALLOW_MIPMAPS)
            || memory_type >= 32
        {
            return Err(
                "Win32 image sharing requires one mip and a valid Vulkan memory type".into(),
            );
        }
        let minimum = (descriptor.size.width as u64)
            .checked_mul(descriptor.size.height as u64)
            .and_then(|size| size.checked_mul(bytes_per_pixel(format) as u64))
            .ok_or("Win32 image size overflow")?;
        if allocation_size < minimum {
            return Err("Win32 allocation is smaller than its image".into());
        }
        Ok(Self {
            descriptor,
            allocation_size,
            memory_type,
            device_uuid,
            driver_uuid,
        })
    }
    pub fn descriptor(&self) -> api::ImageDescriptor {
        self.descriptor
    }
    pub fn allocation_size(&self) -> u64 {
        self.allocation_size
    }
    pub fn memory_type(&self) -> u32 {
        self.memory_type
    }
    pub fn device_uuid(&self) -> [u8; 16] {
        self.device_uuid
    }
    pub fn driver_uuid(&self) -> [u8; 16] {
        self.driver_uuid
    }
    pub(super) fn validate_device(&self, device: [u8; 16], driver: [u8; 16]) -> Result<()> {
        if device != self.device_uuid || driver != self.driver_uuid {
            return Err(
                "Opaque Win32 sharing requires the same Vulkan physical device and driver".into(),
            );
        }
        Ok(())
    }
}

pub(super) fn plane_format(format: api::ImageFormat) -> Result<wgt::TextureFormat> {
    if !matches!(
        format,
        api::ImageFormat::RGBA8
            | api::ImageFormat::BGRA8
            | api::ImageFormat::R8
            | api::ImageFormat::RG8
            | api::ImageFormat::R16
            | api::ImageFormat::RG16
    ) {
        return Err(
            "Win32 GPU copy requires RGBA8/BGRA8/R8/RG8/R16/RG16 single-plane storage".into(),
        );
    }
    texture_format(format)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn win32_layout_checks_storage_and_device_identity() {
        let descriptor = api::ImageDescriptor::new(
            17,
            13,
            api::ImageFormat::RGBA8,
            api::ImageDescriptorFlags::empty(),
        );
        assert!(Win32ImageLayout::new(descriptor, 17 * 13 * 4 - 1, 0, [1; 16], [2; 16]).is_err());
        assert!(Win32ImageLayout::new(descriptor, 4096, 32, [1; 16], [2; 16]).is_err());
        let layout = Win32ImageLayout::new(descriptor, 4096, 0, [1; 16], [2; 16]).unwrap();
        assert!(layout.validate_device([1; 16], [2; 16]).is_ok());
        assert!(layout.validate_device([3; 16], [2; 16]).is_err());
        assert!(layout.validate_device([1; 16], [3; 16]).is_err());
        let mip = api::ImageDescriptor::new(
            17,
            13,
            api::ImageFormat::RGBA8,
            api::ImageDescriptorFlags::ALLOW_MIPMAPS,
        );
        assert!(Win32ImageLayout::new(mip, 4096, 0, [1; 16], [2; 16]).is_err());
        assert!(plane_format(api::ImageFormat::RGBAF32).is_err());
        for format in [
            api::ImageFormat::R8,
            api::ImageFormat::RG8,
            api::ImageFormat::R16,
            api::ImageFormat::RG16,
        ] {
            let plane = api::ImageDescriptor::new(9, 7, format, api::ImageDescriptorFlags::empty());
            assert!(Win32ImageLayout::new(plane, 4096, 0, [1; 16], [2; 16]).is_ok());
        }
    }
}
