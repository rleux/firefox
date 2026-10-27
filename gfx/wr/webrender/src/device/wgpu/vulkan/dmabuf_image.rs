/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */
use super::{wgt, Device};
use crate::device::wgpu::{hal, state::UsageState, textures::TextureState};
pub use hal::dma_buf::DmaBufImageDescriptor;
use std::{os::fd::BorrowedFd, rc::Rc, sync::Arc};
#[path = "dmabuf_access.rs"]
mod access;

pub struct DmaBufImage {
    pub(in crate::device::wgpu) owner: Rc<Device>,
    pub(in crate::device::wgpu) inner: Arc<hal::dma_buf::Image>,
    pub(in crate::device::wgpu) states: Rc<Vec<UsageState<TextureState>>>,
}
impl DmaBufImage {
    pub(in crate::device::wgpu) fn from_hal_image(
        owner: &Rc<Device>,
        inner: Arc<hal::dma_buf::Image>,
    ) -> Rc<Self> {
        Rc::new(Self {
            owner: owner.clone(),
            inner,
            states: Rc::new(vec![UsageState::new(TextureState {
                usage: wgt::TextureUses::UNINITIALIZED,
                initialized: false,
            })]),
        })
    }
    pub(in crate::device::wgpu) fn hal_texture(&self) -> Result<Box<dyn hal::DynTexture>, String> {
        self.inner.create_alias(&*self.owner.open.device)
    }
    pub fn descriptor(&self) -> &DmaBufImageDescriptor {
        self.inner.descriptor()
    }
}
impl Device {
    /// Bind a single-plane Vulkan DMA-BUF allocation without acquiring it for GPU use.
    /// # Safety
    /// The FD must be a real, unprotected DMA-BUF matching the descriptor and
    /// producer identities, exported from a compatible Vulkan image with zero
    /// creation flags, one mip/layer/sample, and memory bound at offset zero.
    /// Later GPU use must acquire external ownership and synchronize with the producer.
    pub unsafe fn import_dma_buf(
        self: &Rc<Self>,
        fd: BorrowedFd<'_>,
        descriptor: DmaBufImageDescriptor,
    ) -> Result<Rc<DmaBufImage>, String> {
        if self.is_lost() {
            return Err("Vulkan device requires recreation".into());
        }
        let inner = unsafe {
            self.dma_buf_capabilities()?
                .native
                .import_dma_buf(fd, descriptor)
        }?;
        Ok(DmaBufImage::from_hal_image(self, inner))
    }
}
#[cfg(test)]
#[path = "dmabuf_image_tests.rs"]
pub(super) mod tests;
