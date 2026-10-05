/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{wgt, Device, DmaBufImage, DmaBufImageDescriptor, Recording, SharedTimeline, SyncFileWait};
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

    /// # Safety
    /// The fence must cover submitted producer writes to the initialized allocation.
    /// Hold exclusive publication access until GPU completion of its ownership return.
    pub unsafe fn acquire(
        &self,
        commands: &mut Recording<'_>,
        ready: SyncFileWait,
    ) -> Result<(), String> {
        let recording = commands.recording_id(&self.image.owner)?;
        self.image.states[0].check_recording(&recording)?;
        if self.image.states[0].current().initialized {
            return Err("Foreign RGB image is already acquired".into());
        }
        commands.wait_sync_file(ready)?;
        self.image.record_access(
            commands,
            &recording,
            true,
            ash::vk::QUEUE_FAMILY_FOREIGN_EXT,
        )
    }

    /// The foreign producer may reuse the allocation only after this signal completes.
    pub fn release(
        &self,
        commands: &mut Recording<'_>,
        released: &Rc<SharedTimeline>,
        value: u64,
    ) -> Result<(), String> {
        let recording = commands.recording_id(&self.image.owner)?;
        self.image.states[0].check_recording(&recording)?;
        if !self.image.states[0].current().initialized {
            return Err("Foreign RGB image is not acquired".into());
        }
        commands.signal_timeline(released, value)?;
        self.image.record_access(
            commands,
            &recording,
            false,
            ash::vk::QUEUE_FAMILY_FOREIGN_EXT,
        )
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
            .open
            .device
            .enabled_device_extensions()
            .contains(&ash::ext::queue_family_foreign::NAME)
        {
            return Err("Foreign queue ownership is unavailable".into());
        }
        let image = self.import_dma_buf_impl(fd, layout.descriptor, false)?;
        Ok(ForeignRgbImage { image, layout })
    }
}
