/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{DmaBufImage, TextureState};
use crate::device::wgpu::{wgt, Recording, SharedTimeline};
use ash::vk;
use std::rc::Rc;

impl DmaBufImage {
    /// # Safety
    /// The producer must release this initialized image in GENERAL layout to
    /// QUEUE_FAMILY_EXTERNAL before `ready` reaches `value`, and must not access
    /// it again until the consumer's release signal completes. Concurrent aliases
    /// must obey the same ownership protocol.
    pub unsafe fn acquire(
        self: &Rc<Self>,
        commands: &mut Recording<'_>,
        ready: &Rc<SharedTimeline>,
        value: u64,
    ) -> Result<(), String> {
        let recording = commands.recording_id(&self.owner)?;
        self.states[0].check_recording(&recording)?;
        if self.states[0].current().initialized {
            return Err("DMA-BUF image is already acquired".into());
        }
        commands.wait_timeline(ready, value)?;
        self.record_access(commands, &recording, true)
    }

    /// Publish the release value to the producer only after successful submission.
    pub fn release(
        self: &Rc<Self>,
        commands: &mut Recording<'_>,
        released: &Rc<SharedTimeline>,
        value: u64,
    ) -> Result<(), String> {
        let recording = commands.recording_id(&self.owner)?;
        self.states[0].check_recording(&recording)?;
        if !self.states[0].current().initialized {
            return Err("DMA-BUF image is not acquired".into());
        }
        commands.signal_timeline(released, value)?;
        self.record_access(commands, &recording, false)
    }

    fn record_access(
        self: &Rc<Self>,
        commands: &mut Recording<'_>,
        recording: &Rc<()>,
        acquire: bool,
    ) -> Result<(), String> {
        let (_, first) = self.states[0].prepare(
            recording,
            TextureState {
                usage: if acquire {
                    wgt::TextureUses::RESOURCE
                } else {
                    wgt::TextureUses::UNINITIALIZED
                },
                initialized: acquire,
            },
        )?;
        if first {
            let image = self.clone();
            commands.commit(move || image.states[0].commit());
        }
        commands.keep(self);
        let family = self.owner.raw_device().queue_family_index();
        if acquire {
            self.image_barrier(
                commands,
                vk::ImageLayout::GENERAL,
                vk::ImageLayout::GENERAL,
                vk::QUEUE_FAMILY_EXTERNAL,
                family,
                vk::AccessFlags::empty(),
                vk::AccessFlags::SHADER_READ,
            );
            self.image_barrier(
                commands,
                vk::ImageLayout::GENERAL,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::QUEUE_FAMILY_IGNORED,
                vk::QUEUE_FAMILY_IGNORED,
                vk::AccessFlags::empty(),
                vk::AccessFlags::SHADER_READ,
            );
        } else {
            self.image_barrier(
                commands,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::ImageLayout::GENERAL,
                vk::QUEUE_FAMILY_IGNORED,
                vk::QUEUE_FAMILY_IGNORED,
                vk::AccessFlags::SHADER_READ,
                vk::AccessFlags::empty(),
            );
            self.image_barrier(
                commands,
                vk::ImageLayout::GENERAL,
                vk::ImageLayout::GENERAL,
                family,
                vk::QUEUE_FAMILY_EXTERNAL,
                vk::AccessFlags::SHADER_READ,
                vk::AccessFlags::empty(),
            );
        }
        Ok(())
    }

    fn image_barrier(
        &self,
        commands: &mut Recording<'_>,
        old: vk::ImageLayout,
        new: vk::ImageLayout,
        src: u32,
        dst: u32,
        src_access: vk::AccessFlags,
        dst_access: vk::AccessFlags,
    ) {
        unsafe {
            self.owner.raw_device().raw_device().cmd_pipeline_barrier(
                commands.encoder().as_any().downcast_ref::<wgpu_hal::vulkan::CommandEncoder>().expect("Vulkan encoder").raw_handle(),
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[vk::ImageMemoryBarrier::default()
                    .image(self.image)
                    .old_layout(old)
                    .new_layout(new)
                    .src_queue_family_index(src)
                    .dst_queue_family_index(dst)
                    .src_access_mask(src_access)
                    .dst_access_mask(dst_access)
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .level_count(1)
                            .layer_count(1),
                    )],
            );
        }
    }
}
