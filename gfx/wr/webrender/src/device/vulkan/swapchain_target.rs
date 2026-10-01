/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{AcquiredImage, PresentationStatus, Swapchain};
use std::borrow::Borrow;
use super::super::draw::ColorAttachment;
use super::super::resources::Owned;
use super::super::state::UsageState;
use super::super::{hal, wgt, Device, Recording, SubmissionQueue, Texture};
use std::rc::Rc;
use wgpu_hal::{CommandEncoder as _, Device as _};

#[derive(Clone, Copy)]
struct AttachmentState {
    usage: wgt::TextureUses,
    initialized: bool,
}

pub(super) struct AttachmentResources {
    view: Owned<hal::vulkan::TextureView>,
    size: wgt::Extent3d,
    format: wgt::TextureFormat,
    state: UsageState<AttachmentState>,
}

pub(in crate::device::vulkan) struct SurfaceTarget<'a> {
    image: AcquiredImage<'a>,
}

impl Swapchain {
    pub(super) fn create_target(&mut self) -> Result<(), String> {
        let owner = self.queue.owner();
        let config = self.config.as_ref().ok_or("Swapchain is not configured")?;
        let image = self
            .acquired
            .as_ref()
            .ok_or("No swapchain image is acquired")?;
        let view = unsafe {
            owner.open.device.create_texture_view(
                image.texture.borrow(),
                &hal::TextureViewDescriptor {
                    label: Some("WR swapchain attachment"),
                    swizzle: Default::default(),
                    format: config.format,
                    dimension: wgt::TextureViewDimension::D2,
                    usage: wgt::TextureUses::COLOR_TARGET,
                    range: wgt::ImageSubresourceRange {
                        mip_level_count: Some(1),
                        array_layer_count: Some(1),
                        ..Default::default()
                    },
                },
            )
        }
        .map_err(|error| format!("Creating swapchain attachment view: {error:?}"))?;
        let resources = Rc::new(AttachmentResources {
            view: Owned::new(owner, view, hal::vulkan::Device::destroy_texture_view),
            size: config.extent,
            format: config.format,
            state: UsageState::new(AttachmentState {
                usage: wgt::TextureUses::UNINITIALIZED,
                initialized: false,
            }),
        });
        self.target = Some(resources);
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub(in crate::device::vulkan) struct SurfaceView<'a> {
    resources: &'a Rc<AttachmentResources>,
    texture: &'a hal::vulkan::Texture,
    queue: &'a SubmissionQueue,
}

impl<'a> AcquiredImage<'a> {
    pub fn into_target(self) -> Result<SurfaceTarget<'a>, String> {
        self.swapchain.create_target()?;
        Ok(SurfaceTarget { image: self })
    }
}

impl Swapchain {
    pub fn current_target(&self) -> Option<SurfaceView<'_>> {
        Some(SurfaceView {
            resources: self.target.as_ref()?,
            texture: self.acquired.as_ref()?.texture.borrow(),
            queue: &self.queue,
        })
    }

    pub fn present_target(&mut self) -> Result<PresentationStatus, String> {
        let target = self
            .current_target()
            .ok_or("No swapchain target is acquired")?;
        if !target.initialized() {
            return Err("Cannot present an uninitialized swapchain attachment".into());
        }
        target.transition(&mut self.queue.recording()?, wgt::TextureUses::PRESENT)?;
        unsafe { AcquiredImage { swapchain: self }.present() }
    }
}

impl SurfaceTarget<'_> {
    fn resources(&self) -> &Rc<AttachmentResources> {
        self.image.swapchain.target.as_ref().unwrap()
    }

    pub fn present(self) -> Result<PresentationStatus, String> {
        self.image.swapchain.present_target()
    }
}

impl SurfaceView<'_> {
    pub fn invalidate(&self, commands: &mut Recording<'_>) -> Result<(), String> {
        self.update(
            commands,
            AttachmentState {
                initialized: false,
                ..self.resources.state.current()
            },
        )
        .map(|_| ())
    }
    fn update(
        &self,
        commands: &mut Recording<'_>,
        state: AttachmentState,
    ) -> Result<AttachmentState, String> {
        self.queue.check_recording(commands)?;
        let recording = commands.recording_id(&self.resources.view.owner)?;
        let (previous, first) = self.resources.state.prepare(&recording, state)?;
        if first {
            let resources = self.resources.clone();
            commands.commit(move || resources.state.commit());
            commands.keep(self.resources.clone());
        }
        Ok(previous)
    }

    fn transition(
        &self,
        commands: &mut Recording<'_>,
        usage: wgt::TextureUses,
    ) -> Result<(), String> {
        let previous = self.update(
            commands,
            AttachmentState {
                usage,
                ..self.resources.state.current()
            },
        )?;
        if previous.usage != usage || usage == wgt::TextureUses::COLOR_TARGET {
            unsafe {
                commands
                    .encoder()
                    .transition_textures(std::iter::once(hal::TextureBarrier {
                        queue_family_ownership_transfer: None,
                        texture: self.texture,
                        range: wgt::ImageSubresourceRange {
                            mip_level_count: Some(1),
                            array_layer_count: Some(1),
                            ..Default::default()
                        },
                        usage: hal::StateTransition {
                            from: previous.usage,
                            to: usage,
                        },
                    }));
            }
        }
        Ok(())
    }
}

impl ColorAttachment for &SurfaceTarget<'_> {
    fn owner(&self) -> &Rc<Device> {
        &self.resources().view.owner
    }
    fn size(&self) -> wgt::Extent3d {
        self.resources().size
    }
    fn format(&self) -> wgt::TextureFormat {
        self.resources().format
    }
    fn target_view(&self) -> Option<&hal::vulkan::TextureView> {
        Some(&self.resources().view)
    }
    fn initialized(&self) -> bool {
        self.resources().state.current().initialized
    }
    fn validate_recording(&self, commands: &Recording<'_>) -> Result<(), String> {
        self.image.swapchain.queue.check_recording(commands)
    }
    fn prepare(&self, commands: &mut Recording<'_>) -> Result<(), String> {
        self.image
            .swapchain
            .current_target()
            .unwrap()
            .prepare(commands)
    }
    fn initialize(&self, commands: &mut Recording<'_>) -> Result<(), String> {
        self.image
            .swapchain
            .current_target()
            .unwrap()
            .initialize(commands)
    }
    fn is_sampled_by(&self, _: &Texture) -> bool {
        false
    }
}

impl ColorAttachment for SurfaceView<'_> {
    fn owner(&self) -> &Rc<Device> {
        &self.resources.view.owner
    }
    fn size(&self) -> wgt::Extent3d {
        self.resources.size
    }
    fn format(&self) -> wgt::TextureFormat {
        self.resources.format
    }
    fn target_view(&self) -> Option<&hal::vulkan::TextureView> {
        Some(&self.resources.view)
    }
    fn initialized(&self) -> bool {
        self.resources.state.current().initialized
    }
    fn validate_recording(&self, commands: &Recording<'_>) -> Result<(), String> {
        self.queue.check_recording(commands)
    }
    fn prepare(&self, commands: &mut Recording<'_>) -> Result<(), String> {
        self.transition(commands, wgt::TextureUses::COLOR_TARGET)
    }
    fn initialize(&self, commands: &mut Recording<'_>) -> Result<(), String> {
        self.update(
            commands,
            AttachmentState {
                initialized: true,
                ..self.resources.state.current()
            },
        )
        .map(|_| ())
    }
    fn is_sampled_by(&self, _: &Texture) -> bool {
        false
    }
}
