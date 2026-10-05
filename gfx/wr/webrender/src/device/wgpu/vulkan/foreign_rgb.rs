/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{wgt, Device, DmaBufImage, DmaBufImageDescriptor};
use std::os::fd::BorrowedFd;
use std::rc::Rc;

#[derive(Clone, Copy, Debug)]
pub struct ForeignRgbLayout {
    descriptor: DmaBufImageDescriptor,
}

impl ForeignRgbLayout {
    pub fn new(
        size: [u32; 2],
        fourcc: u32,
        modifier: u64,
        row_pitch: u64,
        offset: u64,
    ) -> Result<Self, String> {
        if !cfg!(target_endian = "little") || modifier != 0 {
            return Err("Foreign RGB requires little-endian linear storage".into());
        }
        let format = match fourcc {
            0x34324241 => wgt::TextureFormat::Rgba8Unorm,
            0x34325241 => wgt::TextureFormat::Bgra8Unorm,
            _ => return Err("Foreign RGB requires DRM ABGR8888 or ARGB8888".into()),
        };
        let descriptor = DmaBufImageDescriptor {
            size,
            format,
            usage: wgt::TextureUses::RESOURCE,
            modifier,
            offset,
            row_pitch,
            device_uuid: [0; 16],
            driver_uuid: [0; 16],
        };
        descriptor.validate_plane(u64::MAX)?;
        Ok(Self { descriptor })
    }

    pub fn size(&self) -> [u32; 2] {
        self.descriptor.size
    }
    pub fn format(&self) -> wgt::TextureFormat {
        self.descriptor.format
    }
    pub fn row_pitch(&self) -> u64 {
        self.descriptor.row_pitch
    }
    pub fn offset(&self) -> u64 {
        self.descriptor.offset
    }
    pub fn validate_allocation(&self, bytes: u64) -> Result<(), String> {
        self.descriptor.validate_plane(bytes)
    }
}

#[derive(Clone)]
pub struct ForeignRgbImage {
    pub(super) image: Rc<DmaBufImage>,
    layout: ForeignRgbLayout,
}

impl ForeignRgbImage {
    pub fn layout(&self) -> ForeignRgbLayout {
        self.layout
    }
}

impl Device {
    /// # Safety
    /// The FD must describe the negotiated unprotected, single-plane allocation.
    /// GPU use requires acquiring GENERAL/FOREIGN_EXT ownership after producer writes.
    pub unsafe fn import_foreign_rgb(
        self: &Rc<Self>,
        fd: BorrowedFd<'_>,
        layout: ForeignRgbLayout,
    ) -> Result<ForeignRgbImage, String> {
        if !self
            .raw_device()
            .enabled_device_extensions()
            .contains(&ash::ext::queue_family_foreign::NAME)
        {
            return Err("Foreign queue ownership is unavailable".into());
        }
        let image = self.import_dma_buf_impl(fd, layout.descriptor, false)?;
        Ok(ForeignRgbImage { image, layout })
    }
}
