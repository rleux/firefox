/* This Source Code Form is subject to the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::resources::Owned;
use super::state::UsageState;
use super::{hal, wgt, Device, Recording};
use crate::device::TextureFilter;
use std::rc::Rc;

#[path = "texture_access.rs"]
mod access;

#[path = "texture_upload.rs"]
mod upload;
#[path = "texture_to_texture_copy.rs"]
mod texture_to_texture_copy;
#[path = "texture_readback.rs"]
mod readback;
pub use self::readback::PendingReadback;

#[derive(Clone, Copy)]
struct TextureState {
    usage: wgt::TextureUses,
    initialized: bool,
}

pub struct Texture {
    view: Owned<dyn hal::DynTextureView>,
    target: Option<Owned<dyn hal::DynTextureView>>,
    pub(super) raw: Rc<Owned<dyn hal::DynTexture>>,
    size: wgt::Extent3d,
    format: wgt::TextureFormat,
    filter: TextureFilter,
    mip_count: u32,
    usage: wgt::TextureUses,
    states: Vec<UsageState<TextureState>>,
}

pub(super) fn supports_float_color_format(
    format: wgt::TextureFormat,
    features: wgt::Features,
    capabilities: hal::TextureFormatCapabilities,
    required: hal::TextureFormatCapabilities,
) -> bool {
    features.contains(format.required_features())
        && matches!(
            format.sample_type(None, Some(features)),
            Some(wgt::TextureSampleType::Float { .. })
        )
        && capabilities.contains(required)
}

impl Texture {
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
            };
        }
        if filter != TextureFilter::Nearest {
            required |= hal::TextureFormatCapabilities::SAMPLED_LINEAR;
        }
        let capabilities = unsafe { owner.adapter.texture_format_capabilities(format) };
        let supported = if filter == TextureFilter::Trilinear {
            supports_float_color_format(format, owner.features, capabilities, required)
        } else {
            owner.features.contains(format.required_features()) && capabilities.contains(required)
        };
        if !supported {
            return Err(format!(
                "Unsupported Vulkan format usages {format:?}: {required:?}"
            ));
        }
        let size = wgt::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let device = owner.open.device.as_ref();
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
        let raw = Rc::new(Owned::new(owner, raw, <dyn hal::DynDevice>::destroy_texture));
        let make_view = |usage, levels| -> Result<_, String> {
            let view = unsafe {
                device.create_texture_view(
                    &**raw,
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
                <dyn hal::DynDevice>::destroy_texture_view,
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
            mip_count,
            usage,
            states: (0..mip_count)
                .map(|_| {
                    UsageState::new(TextureState {
                        usage: wgt::TextureUses::UNINITIALIZED,
                        initialized: false,
                    })
                })
                .collect(),
        }))
    }

    pub fn current_usage(&self) -> wgt::TextureUses {
        self.states[0].current().usage
    }

    pub fn initialized(&self) -> bool {
        self.states[0].current().initialized
    }

    pub fn sample_initialized(&self) -> bool {
        self.states.iter().all(|state| state.current().initialized)
    }

    pub fn invalidate(self: &Rc<Self>, commands: &mut Recording<'_>) -> Result<(), String> {
        self.writable()?.invalidate(commands)
    }

    pub(super) fn initialize(self: &Rc<Self>, commands: &mut Recording<'_>) -> Result<(), String> {
        self.writable()?.initialize(commands)
    }

    pub fn transition(
        self: &Rc<Self>,
        commands: &mut Recording<'_>,
        to: wgt::TextureUses,
    ) -> Result<(), String> {
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
        self.transition_validated(commands, to)
    }

    fn transition_validated(
        self: &Rc<Self>,
        commands: &mut Recording<'_>,
        to: wgt::TextureUses,
    ) -> Result<(), String> {
        let recording = commands.recording_id(&self.raw.owner)?;
        let count = if to == wgt::TextureUses::RESOURCE {
            self.mip_count
        } else {
            1
        };
        for state in self.states.iter().take(count as usize) {
            state.check_recording(&recording)?;
        }
        commands.keep(self);
        for level in 0..count as usize {
            let (from, first) = self.states[level].prepare(
                &recording,
                TextureState {
                    usage: to,
                    ..self.states[level].current()
                },
            )?;
            if first {
                let resource = self.clone();
                commands.commit(move || resource.states[level].commit());
            }
            // Separate render passes need dependencies even without a layout change.
            if from.usage != to
                || to.intersects(wgt::TextureUses::COLOR_TARGET | wgt::TextureUses::DEPTH_WRITE | wgt::TextureUses::COPY_DST)
            {
                #[cfg(test)]
                self.raw.owner.trace.borrow_mut().push(super::tests::Command::TextureBarrier(from.usage, to));
                unsafe {
                    commands
                        .encoder()
                        .transition_textures(&[hal::TextureBarrier {
                            queue_family_ownership_transfer: None,
                            texture: &**self.raw,
                            range: wgt::ImageSubresourceRange {
                                base_mip_level: level as u32,
                                mip_level_count: Some(1),
                                array_layer_count: Some(1),
                                ..Default::default()
                            },
                            usage: hal::StateTransition {
                                from: from.usage,
                                to,
                            },
                        }]);
                }
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn view(&self) -> &dyn hal::DynTextureView {
        &*self.view
    }

    pub fn raw_texture(&self) -> &dyn hal::DynTexture {
        &**self.raw
    }

    pub fn target_view(&self) -> Option<&dyn hal::DynTextureView> {
        self.target.as_deref()
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
