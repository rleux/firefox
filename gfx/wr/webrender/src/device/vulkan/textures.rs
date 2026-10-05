/* This Source Code Form is subject to the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::resources::Owned;
use super::state::UsageState;
use super::{hal, wgt, Device, Recording};
use crate::device::TextureFilter;
use api::ImageFormat;
use std::rc::Rc;
use wgpu_hal::{Adapter as _, CommandEncoder as _, Device as _};

#[path = "texture_upload.rs"]
mod upload;
#[path = "texture_to_texture_copy.rs"]
mod texture_to_texture_copy;
#[path = "buffer_to_texture_copy.rs"]
mod buffer_to_texture_copy;
#[path = "texture_readback.rs"]
mod readback;
pub use self::readback::PendingReadback;

#[derive(Clone, Copy)]
pub(super) struct TextureState {
    pub(super) usage: wgt::TextureUses,
    pub(super) initialized: bool,
}

pub struct Texture {
    view: Owned<hal::vulkan::TextureView>,
    target: Option<Owned<hal::vulkan::TextureView>>,
    pub(super) raw: Rc<Owned<hal::vulkan::Texture>>,
    size: wgt::Extent3d,
    format: wgt::TextureFormat,
    filter: TextureFilter,
    base_mip: u32,
    mip_count: u32,
    usage: wgt::TextureUses,
    states: Rc<Vec<UsageState<TextureState>>>,
    #[cfg(target_os = "linux")]
    external: Option<Rc<super::DmaBufImage>>,
}

pub(super) fn texture_format(format: ImageFormat) -> wgt::TextureFormat {
    match format {
        ImageFormat::RGBA8 => wgt::TextureFormat::Rgba8Unorm,
        ImageFormat::BGRA8 => wgt::TextureFormat::Bgra8Unorm,
        ImageFormat::R8 => wgt::TextureFormat::R8Unorm,
        ImageFormat::RG8 => wgt::TextureFormat::Rg8Unorm,
        ImageFormat::R16 => wgt::TextureFormat::R16Unorm,
        ImageFormat::RG16 => wgt::TextureFormat::Rg16Unorm,
        ImageFormat::RGBAF32 => wgt::TextureFormat::Rgba32Float,
        ImageFormat::RGBAI32 => wgt::TextureFormat::Rgba32Sint,
    }
}

impl Texture {
    #[cfg(target_os = "linux")]
    pub fn from_dma_buf(
        image: &Rc<super::DmaBufImage>,
        filter: TextureFilter,
        force_opaque: bool,
    ) -> Result<Rc<Self>, String> {
        if filter == TextureFilter::Trilinear || image.owner.is_lost() {
            return Err("Imported textures require a live device and no mipmap filtering".into());
        }
        let owner = &image.owner;
        let descriptor = image.descriptor();
        let size = wgt::Extent3d {
            width: descriptor.size[0],
            height: descriptor.size[1],
            depth_or_array_layers: 1,
        };
        let borrowed = unsafe {
            owner.open.device.texture_from_raw(
                image.image,
                &hal::TextureDescriptor {
                    label: Some("WR imported DMA-BUF texture"),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgt::TextureDimension::D2,
                    format: descriptor.format,
                    usage: descriptor.usage,
                    memory_flags: hal::MemoryFlags::empty(),
                    view_formats: Vec::new(),
                },
                Some(Box::new(|| {})),
                hal::vulkan::TextureMemory::External,
            )
        };
        // DmaBufImage owns the native image and memory; only drop the HAL wrapper.
        let raw = Rc::new(Owned::new(owner, borrowed, |_, texture| drop(texture)));
        let view = unsafe {
            owner.open.device.create_texture_view(
                &raw,
                &hal::TextureViewDescriptor {
                    label: Some("WR imported DMA-BUF view"),
                    swizzle: wgt::TextureComponentSwizzle {
                        a: if force_opaque {
                            wgt::ComponentSwizzle::One
                        } else {
                            wgt::ComponentSwizzle::A
                        },
                        ..Default::default()
                    },
                    format: descriptor.format,
                    dimension: wgt::TextureViewDimension::D2,
                    usage: wgt::TextureUses::RESOURCE,
                    range: wgt::ImageSubresourceRange {
                        mip_level_count: Some(1),
                        array_layer_count: Some(1),
                        ..Default::default()
                    },
                },
            )
        }
        .map_err(|error| format!("Creating DMA-BUF texture view: {error:?}"))?;
        Ok(Rc::new(Self {
            view: Owned::new(owner, view, hal::vulkan::Device::destroy_texture_view),
            target: None,
            raw,
            size,
            format: descriptor.format,
            filter,
            base_mip: 0,
            mip_count: 1,
            usage: wgt::TextureUses::RESOURCE,
            states: image.states.clone(),
            external: Some(image.clone()),
        }))
    }

    pub fn new(
        owner: &Rc<Device>,
        width: u32,
        height: u32,
        format: wgt::TextureFormat,
        filter: TextureFilter,
        renderable: bool,
    ) -> Result<Rc<Self>, String> {
        if width == 0
            || height == 0
            || width > owner.capabilities.limits.max_texture_dimension_2d
            || height > owner.capabilities.limits.max_texture_dimension_2d
        {
            return Err("Invalid Vulkan texture dimensions".into());
        }
        match format {
            wgt::TextureFormat::Rgba8Unorm
            | wgt::TextureFormat::Bgra8Unorm
            | wgt::TextureFormat::R8Unorm
            | wgt::TextureFormat::Rg8Unorm
            | wgt::TextureFormat::R16Unorm
            | wgt::TextureFormat::Rg16Unorm
            | wgt::TextureFormat::Rgba32Float
            | wgt::TextureFormat::Rgba32Sint
            | wgt::TextureFormat::Depth32Float => {}
            _ => return Err(format!("Unsupported Vulkan texture format: {format:?}")),
        }
        if !owner.features.contains(format.required_features()) {
            return Err(format!("Vulkan device lacks features for {format:?}"));
        }
        let depth = format == wgt::TextureFormat::Depth32Float;
        if depth && (!renderable || filter != TextureFilter::Nearest) {
            return Err("Depth textures require a nearest-filtered render target".into());
        }
        let mip_count = if filter == TextureFilter::Trilinear {
            32 - width.max(height).leading_zeros()
        } else {
            1
        };
        let renderable = renderable || mip_count > 1;
        let target_usage = if depth {
            wgt::TextureUses::DEPTH_WRITE
        } else {
            wgt::TextureUses::COLOR_TARGET
        };
        let mut usage = wgt::TextureUses::COPY_SRC | wgt::TextureUses::COPY_DST;
        let mut required =
            hal::TextureFormatCapabilities::COPY_SRC | hal::TextureFormatCapabilities::COPY_DST;
        if !depth {
            usage |= wgt::TextureUses::RESOURCE;
            required |= hal::TextureFormatCapabilities::SAMPLED;
        }
        if renderable {
            usage |= target_usage;
            required |= if depth {
                hal::TextureFormatCapabilities::DEPTH_STENCIL_ATTACHMENT
            } else {
                hal::TextureFormatCapabilities::COLOR_ATTACHMENT
                    | hal::TextureFormatCapabilities::COLOR_ATTACHMENT_BLEND
            };
        }
        if filter != TextureFilter::Nearest {
            required |= hal::TextureFormatCapabilities::SAMPLED_LINEAR;
        }
        let capabilities = unsafe { owner.adapter.texture_format_capabilities(format) };
        if !capabilities.contains(required) {
            return Err(format!(
                "Unsupported Vulkan format usages {format:?}: {required:?}"
            ));
        }
        let size = wgt::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let device = &owner.open.device;
        let raw = unsafe {
            device.create_texture(&hal::TextureDescriptor {
                label: Some("WR Vulkan texture"),
                size,
                mip_level_count: mip_count,
                sample_count: 1,
                dimension: wgt::TextureDimension::D2,
                format,
                usage,
                memory_flags: hal::MemoryFlags::empty(),
                view_formats: Vec::new(),
            })
        }
        .map_err(|error| format!("Creating {format:?} texture: {error:?}"))?;
        let raw = Rc::new(Owned::new(owner, raw, hal::vulkan::Device::destroy_texture));
        let make_view = |usage, levels| -> Result<_, String> {
            let view = unsafe {
                device.create_texture_view(
                    &raw,
                    &hal::TextureViewDescriptor {
                        label: Some("WR Vulkan texture view"),
                        swizzle: Default::default(),
                        format,
                        dimension: wgt::TextureViewDimension::D2,
                        usage,
                        range: wgt::ImageSubresourceRange {
                            mip_level_count: Some(levels),
                            array_layer_count: Some(1),
                            ..Default::default()
                        },
                    },
                )
            }
            .map_err(|error| format!("Creating texture view: {error:?}"))?;
            Ok(Owned::new(
                owner,
                view,
                hal::vulkan::Device::destroy_texture_view,
            ))
        };
        let view = make_view(
            if depth {
                target_usage
            } else {
                wgt::TextureUses::RESOURCE
            },
            mip_count,
        )?;
        let target = if renderable {
            Some(make_view(target_usage, 1)?)
        } else {
            None
        };
        Ok(Rc::new(Self {
            view,
            target,
            raw,
            size,
            format,
            filter,
            base_mip: 0,
            mip_count,
            usage,
            states: Rc::new(
                (0..mip_count)
                    .map(|_| {
                        UsageState::new(TextureState {
                            usage: wgt::TextureUses::UNINITIALIZED,
                            initialized: false,
                        })
                    })
                    .collect(),
            ),
            #[cfg(target_os = "linux")]
            external: None,
        }))
    }

    pub fn mip_view(self: &Rc<Self>, level: u32) -> Result<Rc<Self>, String> {
        if self.raw.owner.is_lost() {
            return Err("Vulkan device requires recreation".into());
        }
        if self.target.is_none() || level >= self.mip_count {
            return Err("Invalid Vulkan renderable mip view".into());
        }
        let base_mip = self.base_mip + level;
        let depth = self.format == wgt::TextureFormat::Depth32Float;
        let target_usage = if depth {
            wgt::TextureUses::DEPTH_WRITE
        } else {
            wgt::TextureUses::COLOR_TARGET
        };
        let make_view = |usage| {
            let owner = &self.raw.owner;
            let raw = unsafe {
                owner.open.device.create_texture_view(
                    &self.raw,
                    &hal::TextureViewDescriptor {
                        label: Some("WR Vulkan mip view"),
                        swizzle: Default::default(),
                        format: self.format,
                        dimension: wgt::TextureViewDimension::D2,
                        usage,
                        range: wgt::ImageSubresourceRange {
                            base_mip_level: base_mip,
                            mip_level_count: Some(1),
                            array_layer_count: Some(1),
                            ..Default::default()
                        },
                    },
                )
            }
            .map_err(|error| format!("Creating Vulkan mip view: {error:?}"))?;
            Ok::<_, String>(Owned::new(
                owner,
                raw,
                hal::vulkan::Device::destroy_texture_view,
            ))
        };
        Ok(Rc::new(Self {
            view: make_view(if depth {
                target_usage
            } else {
                wgt::TextureUses::RESOURCE
            })?,
            target: Some(make_view(target_usage)?),
            raw: self.raw.clone(),
            size: wgt::Extent3d {
                width: (self.size.width >> level).max(1),
                height: (self.size.height >> level).max(1),
                depth_or_array_layers: 1,
            },
            format: self.format,
            filter: if self.filter == TextureFilter::Trilinear {
                TextureFilter::Linear
            } else {
                self.filter
            },
            base_mip,
            mip_count: 1,
            usage: self.usage,
            #[cfg(target_os = "linux")]
            external: self.external.clone(),
            states: self.states.clone(),
        }))
    }

    fn states(&self) -> &[UsageState<TextureState>] {
        &self.states[self.base_mip as usize..(self.base_mip + self.mip_count) as usize]
    }

    pub fn current_usage(&self) -> wgt::TextureUses {
        self.states()[0].current().usage
    }

    pub fn initialized(&self) -> bool {
        self.states()[0].current().initialized
    }

    pub fn sample_initialized(&self) -> bool {
        self.states()
            .iter()
            .all(|state| state.current().initialized)
    }

    pub fn invalidate(self: &Rc<Self>, commands: &mut Recording<'_>) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        if self.external.is_some() {
            return Err("Imported textures are read-only".into());
        }
        let recording = commands.recording_id(&self.raw.owner)?;
        for state in self.states() {
            state.check_recording(&recording)?;
        }
        for (level, state) in self.states().iter().enumerate() {
            let (_, first) = state.prepare(
                &recording,
                TextureState {
                    initialized: false,
                    ..state.current()
                },
            )?;
            if first {
                let resource = self.clone();
                commands.commit(move || resource.states()[level].commit());
            }
        }
        commands.keep(self.clone());
        Ok(())
    }

    pub(super) fn initialize(self: &Rc<Self>, commands: &mut Recording<'_>) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        if self.external.is_some() {
            return Err("Imported textures are read-only".into());
        }
        let recording = commands.recording_id(&self.raw.owner)?;
        let (_, first) = self.states()[0].prepare(
            &recording,
            TextureState {
                initialized: true,
                ..self.states()[0].current()
            },
        )?;
        if first {
            let resource = self.clone();
            commands.commit(move || resource.states()[0].commit());
        }
        commands.keep(self.clone());
        Ok(())
    }

    pub fn transition(
        self: &Rc<Self>,
        commands: &mut Recording<'_>,
        to: wgt::TextureUses,
    ) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        if self.external.is_some() && !self.sample_initialized() {
            return Err("DMA-BUF image has not been acquired for sampling".into());
        }
        if !self.usage.contains(to)
            || ![
                wgt::TextureUses::COPY_SRC,
                wgt::TextureUses::COPY_DST,
                wgt::TextureUses::RESOURCE,
                wgt::TextureUses::COLOR_TARGET,
                wgt::TextureUses::DEPTH_WRITE,
            ]
            .contains(&to)
        {
            return Err("Invalid Vulkan texture usage transition".into());
        }
        let recording = commands.recording_id(&self.raw.owner)?;
        let count = if to == wgt::TextureUses::RESOURCE {
            self.mip_count
        } else {
            1
        };
        for state in self.states().iter().take(count as usize) {
            state.check_recording(&recording)?;
        }
        commands.keep(self.clone());
        for level in 0..count as usize {
            let (from, first) = self.states()[level].prepare(
                &recording,
                TextureState {
                    usage: to,
                    ..self.states()[level].current()
                },
            )?;
            if first {
                let resource = self.clone();
                commands.commit(move || resource.states()[level].commit());
            }
            // Separate render passes need dependencies even without a layout change.
            if from.usage != to
                || to.intersects(wgt::TextureUses::COLOR_TARGET | wgt::TextureUses::DEPTH_WRITE)
            {
                unsafe {
                    commands
                        .encoder()
                        .transition_textures(std::iter::once(hal::TextureBarrier {
                            queue_family_ownership_transfer: None,
                            texture: &**self.raw,
                            range: wgt::ImageSubresourceRange {
                                base_mip_level: self.base_mip + level as u32,
                                mip_level_count: Some(1),
                                array_layer_count: Some(1),
                                ..Default::default()
                            },
                            usage: hal::StateTransition {
                                from: from.usage,
                                to,
                            },
                        }));
                }
            }
        }
        Ok(())
    }

    pub fn view(&self) -> &hal::vulkan::TextureView {
        &self.view
    }

    pub fn raw_texture(&self) -> &hal::vulkan::Texture {
        &self.raw
    }

    pub fn target_view(&self) -> Option<&hal::vulkan::TextureView> {
        self.target.as_deref()
    }

    pub(super) fn samples_attachment(&self, attachment: &Self) -> bool {
        Rc::ptr_eq(&self.raw, &attachment.raw)
            && (self.base_mip..self.base_mip + self.mip_count).contains(&attachment.base_mip)
    }

    pub fn size(&self) -> wgt::Extent3d {
        self.size
    }

    pub fn format(&self) -> wgt::TextureFormat {
        self.format
    }

    pub fn filter(&self) -> TextureFilter {
        self.filter
    }

    pub fn mip_count(&self) -> u32 {
        self.mip_count
    }
}
