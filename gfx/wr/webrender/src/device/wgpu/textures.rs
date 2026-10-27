/* This Source Code Form is subject to the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::resources::Owned;
use super::state::UsageState;
use super::{hal, wgt, Device, Recording};
use crate::device::TextureFilter;
use api::ImageFormat;
use std::{rc::Rc, sync::Arc};

#[path = "texture_access.rs"]
mod access;

#[path = "texture_upload.rs"]
mod upload;
#[path = "texture_to_texture_copy.rs"]
mod texture_to_texture_copy;
#[path = "buffer_to_texture_copy.rs"]
mod buffer_to_texture_copy;
#[path = "texture_readback.rs"]
mod readback;
pub use self::readback::PendingReadback;

#[cfg(all(test, feature = "capture"))]
#[path = "external_readback_tests.rs"]
mod external_readback_tests;

#[derive(Clone, Copy)]
pub(super) struct TextureState {
    pub(super) usage: wgt::TextureUses,
    pub(super) initialized: bool,
}

pub struct Texture {
    view: Option<Owned<dyn hal::DynTextureView>>,
    target: Option<Owned<dyn hal::DynTextureView>>,
    mips: std::cell::RefCell<Vec<Option<Rc<Texture>>>>,
    pub(super) raw: Rc<Owned<dyn hal::DynTexture>>,
    size: wgt::Extent3d,
    format: wgt::TextureFormat,
    filter: TextureFilter,
    base_mip: u32,
    mip_count: u32,
    usage: wgt::TextureUses,
    copy_src_matches_view: bool,
    states: Rc<Vec<UsageState<TextureState>>>,
    #[cfg(target_os = "linux")]
    external: Option<Rc<super::DmaBufImage>>,
    shared_payload: Option<hal::shared_image::ImagePayload>,
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

fn shared_image_view_descriptor(
    descriptor: &hal::TextureDescriptor<'_>,
    view_format: wgt::TextureFormat,
    filter: TextureFilter,
    force_opaque: bool,
) -> Result<hal::TextureViewDescriptor<'static>, String> {
    if !matches!(
        descriptor.format,
        wgt::TextureFormat::Rgba8Unorm | wgt::TextureFormat::Bgra8Unorm
    ) || descriptor.dimension != wgt::TextureDimension::D2
        || descriptor.size.width == 0
        || descriptor.size.height == 0
        || descriptor.size.depth_or_array_layers != 1
        || descriptor.mip_level_count != 1
        || descriptor.sample_count != 1
        || descriptor.usage.contains(wgt::TextureUses::TRANSIENT)
        || filter == TextureFilter::Trilinear
    {
        return Err("Invalid shared canvas sampling descriptor".into());
    }
    if (view_format != descriptor.format && !descriptor.view_formats.contains(&view_format))
        || view_format.remove_srgb_suffix() != descriptor.format.remove_srgb_suffix()
        || descriptor
            .view_formats
            .iter()
            .any(|format| format.remove_srgb_suffix() != descriptor.format.remove_srgb_suffix())
    {
        return Err("Shared canvas view format is not declared or compatible".into());
    }
    Ok(hal::TextureViewDescriptor {
        label: Some("WR shared canvas sampled view"),
        swizzle: wgt::TextureComponentSwizzle {
            a: if force_opaque {
                wgt::ComponentSwizzle::One
            } else {
                wgt::ComponentSwizzle::A
            },
            ..Default::default()
        },
        format: view_format,
        dimension: wgt::TextureViewDimension::D2,
        usage: wgt::TextureUses::RESOURCE,
        range: wgt::ImageSubresourceRange {
            mip_level_count: Some(1),
            array_layer_count: Some(1),
            ..Default::default()
        },
    })
}


impl Texture {
    /// Import finalized immutable storage on its exact graphics endpoint.
    ///
    /// # Safety
    /// Finalization must be accepted and the logical texture expired. Keep the
    /// epoch open and immutable while aliases can record or retain accepted work.
    pub unsafe fn from_shared_image(
        owner: &Rc<Device>,
        epoch: Arc<hal::shared_image::ImageEpoch>,
        view_format: wgt::TextureFormat,
        filter: TextureFilter,
        force_opaque: bool,
    ) -> Result<Rc<Self>, String> {
        if owner.shared.is_none() || owner.is_lost() {
            return Err("Shared canvas sampling requires a live shared graphics device".into());
        }
        epoch
            .validate_owner(owner.open.device.as_ref(), owner.open.queue.as_ref())
            .map_err(|error| format!("Validating shared canvas epoch: {error}"))?;
        let payload = epoch
            .acquire_payload()
            .map_err(|error| format!("Admitting shared canvas image: {error}"))?;
        let descriptor = epoch.descriptor();
        let lease = epoch.lease().clone();
        let view_descriptor =
            shared_image_view_descriptor(descriptor, view_format, filter, force_opaque)?;
        if descriptor.size.width > owner.capabilities.limits.max_texture_dimension_2d
            || descriptor.size.height > owner.capabilities.limits.max_texture_dimension_2d
        {
            return Err("Shared canvas dimensions exceed renderer limits".into());
        }
        let required = hal::TextureFormatCapabilities::SAMPLED
            | if filter == TextureFilter::Linear {
                hal::TextureFormatCapabilities::SAMPLED_LINEAR
            } else {
                hal::TextureFormatCapabilities::empty()
            };
        let capabilities = unsafe { owner.adapter.texture_format_capabilities(view_format) };
        if !supports_float_color_format(view_format, owner.features, capabilities, required) {
            return Err("Unsupported shared canvas sampling format or filter".into());
        }
        let mut alias_descriptor = descriptor.clone();
        alias_descriptor.usage =
            wgt::TextureUses::RESOURCE | (descriptor.usage & wgt::TextureUses::COPY_SRC);
        let alias = unsafe {
            hal::shared_image::create_alias(
                lease.clone(),
                owner.open.device.as_ref(),
                &alias_descriptor,
            )
        }
        .map_err(|error| {
            if let hal::shared_image::ImageLeaseError::Device(error) = &error {
                owner.check_native_error(error);
            }
            format!("Creating shared canvas alias: {error}")
        })?;
        let raw = Rc::new(Owned::new(
            owner,
            alias,
            <dyn hal::DynDevice>::destroy_texture,
        ));
        hal::shared_image::validate_alias(
            lease.as_ref(),
            owner.open.device.as_ref(),
            &**raw,
            owner.open.queue.as_ref(),
            descriptor,
        )
        .map_err(|error| format!("Validating shared canvas owner: {error}"))?;
        let view = unsafe {
            owner
                .open
                .device
                .create_texture_view(&**raw, &view_descriptor)
        }
        .map_err(|error| {
            owner.check_native_error(&error);
            format!("Creating shared canvas sampled view: {error:?}")
        })?;
        Ok(Rc::new(Self {
            view: Some(Owned::new(
                owner,
                view,
                <dyn hal::DynDevice>::destroy_texture_view,
            )),
            target: None,
            mips: Default::default(),
            raw,
            size: descriptor.size,
            format: descriptor.format,
            filter,
            base_mip: 0,
            mip_count: 1,
            usage: alias_descriptor.usage,
            copy_src_matches_view: view_format == descriptor.format && !force_opaque,
            states: Rc::new(vec![UsageState::new(TextureState {
                usage: wgt::TextureUses::RESOURCE,
                initialized: true,
            })]),
            #[cfg(target_os = "linux")]
            external: None,
            shared_payload: Some(payload),
        }))
    }

    #[cfg(target_os = "linux")]
    pub fn from_foreign_rgb(
        image: &super::ForeignRgbImage,
        filter: TextureFilter,
        force_opaque: bool,
    ) -> Result<Rc<Self>, String> {
        Self::from_dma_buf(&image.image, filter, force_opaque)
    }

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
        let raw = Rc::new(Owned::<dyn hal::DynTexture>::new(owner, image.hal_texture()?, <dyn hal::DynDevice>::destroy_texture));
        let view = unsafe {
            owner.open.device.create_texture_view(
                &**raw,
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
            view: Some(Owned::new(owner, view, <dyn hal::DynDevice>::destroy_texture_view)),
            target: None,
            mips: Default::default(),
            raw,
            size,
            format: descriptor.format,
            filter,
            base_mip: 0,
            mip_count: 1,
            usage: wgt::TextureUses::RESOURCE,
            copy_src_matches_view: !force_opaque,
            states: image.states.clone(),
            external: Some(image.clone()),
            shared_payload: None,
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
        let view = if depth {
            None
        } else {
            Some(make_view(wgt::TextureUses::RESOURCE, mip_count)?)
        };
        let target = if renderable {
            Some(make_view(target_usage, 1)?)
        } else {
            None
        };
        Ok(Rc::new(Self {
            view,
            target,
            mips: Default::default(),
            raw,
            size,
            format,
            filter,
            base_mip: 0,
            mip_count,
            usage,
            copy_src_matches_view: true,
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
            shared_payload: None,
        }))
    }

    pub fn mip_view(self: &Rc<Self>, level: u32) -> Result<Rc<Self>, String> {
        if self.raw.owner.is_lost() {
            return Err("Vulkan device requires recreation".into());
        }
        if self.target.is_none() || level >= self.mip_count {
            return Err("Invalid Vulkan renderable mip view".into());
        }
        if self.mip_count == 1 {
            return Ok(self.clone());
        }
        if let Some(Some(view)) = self.mips.borrow().get(level as usize) {
            return Ok(view.clone());
        }
        let base_mip = self.base_mip + level;
        let target_usage = wgt::TextureUses::COLOR_TARGET;
        let make_view = |usage| {
            let owner = &self.raw.owner;
            let raw = unsafe {
                owner.open.device.create_texture_view(
                    &**self.raw,
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
                <dyn hal::DynDevice>::destroy_texture_view,
            ))
        };
        let view = Rc::new(Self {
            view: Some(make_view(wgt::TextureUses::RESOURCE)?),
            target: Some(make_view(target_usage)?),
            mips: Default::default(),
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
            copy_src_matches_view: self.copy_src_matches_view,
            #[cfg(target_os = "linux")]
            external: self.external.clone(),
            shared_payload: None,
            states: self.states.clone(),
        });
        let mut mips = self.mips.borrow_mut();
        mips.resize_with(self.mip_count as usize, || None);
        mips[level as usize] = Some(view.clone());
        Ok(view)
    }

    fn states(&self) -> &[UsageState<TextureState>] {
        &self.states[self.base_mip as usize..(self.base_mip + self.mip_count) as usize]
    }

    pub(super) fn check_shared_access(&self) -> Result<(), String> {
        if let Some(payload) = &self.shared_payload {
            payload
                .epoch()
                .ensure_open()
                .map_err(|error| format!("Shared canvas access rejected: {error}"))?;
        }
        Ok(())
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
        self.check_shared_access()?;
        #[cfg(target_os = "linux")]
        if self.external.is_some() && !self.sample_initialized() {
            return Err("DMA-BUF image has not been acquired for sampling".into());
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
        commands.keep(self);
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
                                base_mip_level: self.base_mip + level as u32,
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
        self.view.as_deref().expect("Depth textures have no sampled view")
    }

    pub fn raw_texture(&self) -> &dyn hal::DynTexture {
        &**self.raw
    }

    pub(super) fn supports_copy_src(&self) -> bool {
        self.usage.contains(wgt::TextureUses::COPY_SRC)
    }

    #[cfg(feature = "capture")]
    pub(super) fn supports_view_copy_src(&self) -> bool {
        self.supports_copy_src() && self.copy_src_matches_view
    }

    pub fn target_view(&self) -> Option<&dyn hal::DynTextureView> {
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

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn depth_and_mip_views_reuse_native_storage_without_cycles() {
    use crate::device::wgpu::Options;
    use crate::device::wgpu::tests::{validation_logging, ERRORS};
    use std::sync::atomic::Ordering;
    validation_logging();
    let owner = Rc::new(Device::new(&Options { validation: true, ..Default::default() }).unwrap());
    let depth = Texture::new(&owner, 4, 4, wgt::TextureFormat::Depth32Float, TextureFilter::Nearest, true).unwrap();
    assert!(depth.view.is_none());
    assert!(Rc::ptr_eq(&depth, &depth.mip_view(0).unwrap()));
    let texture = Texture::new(&owner, 8, 8, wgt::TextureFormat::Rgba8Unorm, TextureFilter::Trilinear, false).unwrap();
    let mip = texture.mip_view(1).unwrap();
    assert!(Rc::ptr_eq(&mip, &texture.mip_view(1).unwrap()));
    assert!(Rc::ptr_eq(&mip, &mip.mip_view(0).unwrap()));
    let weak = Rc::downgrade(&mip);
    drop(mip);
    assert!(weak.upgrade().is_some());
    drop(texture);
    assert!(weak.upgrade().is_none());
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[cfg(test)]
#[path = "shared_image_tests.rs"]
mod shared_image_tests;
