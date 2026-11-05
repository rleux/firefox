/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{wgt, Device, DmaBufImage, DmaBufImageDescriptor};
use std::{os::fd::{BorrowedFd,AsRawFd},sync::Arc};

#[derive(Clone,Copy,Debug)]
pub struct ForeignRgbLayout { descriptor:DmaBufImageDescriptor }
impl ForeignRgbLayout {
    pub fn new(size:[u32;2],fourcc:u32,modifier:u64,row_pitch:u64,offset:u64)->Result<Self,String> {
        if !cfg!(target_endian="little") || modifier!=0 {return Err("Foreign RGB requires little-endian LINEAR storage".into());}
        let format=match fourcc{0x34324241=>wgt::TextureFormat::Rgba8Unorm,0x34325241=>wgt::TextureFormat::Bgra8Unorm,_=>return Err("Foreign RGB requires DRM ABGR8888 or ARGB8888".into())};
        let descriptor=DmaBufImageDescriptor{size,format,usage:wgt::TextureUses::RESOURCE,modifier,offset,row_pitch,device_uuid:[0;16],driver_uuid:[0;16]};
        descriptor.validate_plane(u64::MAX)?;Ok(Self{descriptor})
    }
    pub fn size(&self)->[u32;2]{self.descriptor.size}
    pub fn format(&self)->wgt::TextureFormat{self.descriptor.format}
}
impl Device {
    pub(in crate::device::wgpu) fn foreign_rgb_drm_node(&self)->Option<[u64;2]> {
        *self.native_cache.drm_node.get_or_init(|| {
            let hal=self.raw_device();
            let adapter=unsafe{self.adapter.clone().as_hal::<super::hal::api::Vulkan>()}?;
            if !hal.enabled_device_extensions().contains(&ash::ext::queue_family_foreign::NAME)||!adapter.physical_device_capabilities().supports_extension(ash::ext::physical_device_drm::NAME)||!self.supports_sync_file_import(){return None;}
            let mut drm=ash::vk::PhysicalDeviceDrmPropertiesEXT::default();
            unsafe{hal.shared_instance().raw_instance().get_physical_device_properties2(hal.raw_physical_device(),&mut ash::vk::PhysicalDeviceProperties2::default().push_next(&mut drm));}
            (drm.has_render!=0&&drm.render_major>=0&&drm.render_minor>=0).then_some([drm.render_major as u64,drm.render_minor as u64])
        })
    }
    /// The render-node FD declares producer identity; it does not authenticate DMA-BUF provenance.
    /// The image must be a compatible unprotected allocation fully initialized before ready.
    pub unsafe fn import_foreign_rgb(self:&Arc<Self>,fd:BorrowedFd<'_>,render_node:BorrowedFd<'_>,layout:ForeignRgbLayout)->Result<Arc<DmaBufImage>,String> {
        let mut stat=std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe{libc::fstat(render_node.as_raw_fd(),stat.as_mut_ptr())}!=0{return Err("Invalid producer render-node FD".into());}
        let stat=unsafe{stat.assume_init()};
        if stat.st_mode&libc::S_IFMT!=libc::S_IFCHR || self.foreign_rgb_drm_node()!=Some([libc::major(stat.st_rdev) as u64,libc::minor(stat.st_rdev) as u64]){return Err("Producer render-node identity mismatch".into());}
        unsafe{self.import_dma_buf_impl(fd,layout.descriptor,false)}
    }
}
