/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::capture::{CaptureConfig, ExternalCaptureImage, PlainExternalImage};
use crate::renderer::{PlainExternalResources, PlainRenderer, PlainTexture};
use crate::render_api::CaptureBits;
use std::{fs, path::Path};

#[cfg_attr(feature = "capture", derive(serde::Serialize))]
#[cfg_attr(feature = "replay", derive(serde::Deserialize))]
struct NativeCapture {
    operations: Vec<crate::composite::NativeSurfaceOperation>,
    tiles: Vec<(NativeTileId, DeviceIntSize, PlainTexture)>,
}

fn image_format(format: wgt::TextureFormat) -> Result<api::ImageFormat> {
    Ok(match format {
        wgt::TextureFormat::Rgba8Unorm => api::ImageFormat::RGBA8,
        wgt::TextureFormat::Bgra8Unorm => api::ImageFormat::BGRA8,
        wgt::TextureFormat::R8Unorm => api::ImageFormat::R8,
        wgt::TextureFormat::Rg8Unorm => api::ImageFormat::RG8,
        wgt::TextureFormat::R16Unorm => api::ImageFormat::R16,
        wgt::TextureFormat::Rg16Unorm => api::ImageFormat::RG16,
        wgt::TextureFormat::Rgba32Float => api::ImageFormat::RGBAF32,
        wgt::TextureFormat::Rgba32Sint => api::ImageFormat::RGBAI32,
        _ => return Err("Unsupported capture texture format".into()),
    })
}

#[cfg(feature = "capture")]
fn write_ron<T: serde::Serialize>(path: impl AsRef<Path>, value: &T) -> Result<()> {
    let data = ron::ser::to_string(value).map_err(|error| format!("Encoding capture: {error}"))?;
    fs::write(path, data).map_err(|error| format!("Writing capture: {error}"))
}

#[cfg(feature = "replay")]
struct ReplayImages {
    images: HashMap<(api::ExternalImageId, u8), (ExternalImageSource, api::ImageDescriptor, TexelRect)>,
}

#[cfg(feature = "replay")]
impl ExternalImageProvider for ReplayImages {
    fn acquire(&mut self, id: api::ExternalImageId, channel: u8, _: bool) -> Result<ExternalImageLease> {
        let (source, descriptor, uv) = self.images.get(&(id, channel)).ok_or("Missing replay external image")?;
        ExternalImageLease::new(*descriptor, *uv, 0, source.clone(), |_| {})
    }
}

impl<A: hal::Api> FrameRenderer<A> {
    fn capture_texture(&self, texture: &Rc<Texture<A>>) -> Result<Vec<u8>> {
        let format = image_format(texture.format)?;
        let layout = ReadbackLayout::with_pixel_size(texture.size.width, texture.size.height,
            self.owner.capabilities.alignments.buffer_copy_pitch.get(), format.bytes_per_pixel() as u32)?;
        if !texture.initialized() {
            return Ok(vec![0; layout.row_bytes as usize * texture.size.height as usize]);
        }
        let buffer = self.readback_buffer(&layout)?;
        let previous = texture.current_usage();
        let mut commands = self.submissions.recording()?;
        texture.transition(&mut commands, wgt::TextureUses::COPY_SRC);
        buffer.transition(&mut commands, wgt::BufferUses::COPY_DST);
        unsafe { commands.encoder().copy_texture_to_buffer(&texture.raw, wgt::TextureUses::COPY_SRC, &buffer.raw,
            std::iter::once(hal::BufferTextureCopy {
                buffer_layout: wgt::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(layout.pitch), rows_per_image: Some(texture.size.height) },
                texture_base: hal::TextureCopyBase { mip_level: texture.base_mip, array_layer: 0, origin: wgt::Origin3d::ZERO, aspect: hal::FormatAspects::COLOR },
                size: texture.size.into(),
            })) }
        texture.transition(&mut commands, previous);
        buffer.transition(&mut commands, wgt::BufferUses::MAP_READ);
        drop(commands);
        self.submissions.wait()?;
        let pixels = self.owner.map_readback(&buffer.raw, &layout)?;
        dispatch_releases(&self.releases);
        Ok(pixels)
    }

    #[cfg(feature = "capture")]
    pub fn save_capture(&mut self, config: CaptureConfig, mut externals: Vec<ExternalCaptureImage>, device_size: Option<DeviceIntSize>) -> Result<()> {
        if self.is_failed() { return Err("Cannot capture a failed HAL renderer".into()); }
        let root = config.resource_root();
        fs::create_dir_all(&root).map_err(|error| error.to_string())?;
        if config.bits.contains(CaptureBits::EXTERNAL_RESOURCES) && !externals.is_empty() {
            fs::create_dir_all(root.join("externals")).map_err(|error| error.to_string())?;
            for (index, external) in externals.iter_mut().enumerate() {
                let lease = self.acquire_external(external.external.id, external.external.channel_index, false)?;
                let data = match &lease.source {
                    ExternalImageSource::Buffer(data) => { lease.complete_cpu_copy(); data.as_ref().clone() }
                    ExternalImageSource::Native(image) => {
                        external.descriptor = image.descriptor();
                        let texture = image.texture(&self.owner)?.with_lease(lease.state.clone(), TextureFilter::Linear)?;
                        self.capture_texture(&texture)?
                    }
                };
                let short_path = format!("externals/hal-{index}.raw");
                fs::write(root.join(&short_path), data).map_err(|error| error.to_string())?;
                let uv = if external.external.normalized_uvs {
                    TexelRect::new(lease.uv.uv0.x / external.descriptor.size.width as f32,
                        lease.uv.uv0.y / external.descriptor.size.height as f32,
                        lease.uv.uv1.x / external.descriptor.size.width as f32,
                        lease.uv.uv1.y / external.descriptor.size.height as f32)
                } else { lease.uv };
                write_ron(root.join(&external.short_path).with_extension("ron"), &PlainExternalImage {
                    data: short_path, external: external.external, uv,
                })?;
            }
            write_ron(root.join("external_resources.ron"), &PlainExternalResources { images: externals })?;
        }
        if config.bits.contains(CaptureBits::FRAME) {
            fs::create_dir_all(root.join("textures")).map_err(|error| error.to_string())?;
            if matches!(self.compositor, CompositorConfig::Native { .. }) {
                let mut native = NativeCapture { operations: self.native_operations.clone(), tiles: Vec::new() };
                for (id, size) in self.native_sizes.clone() {
                    let target = match &mut self.compositor {
                        CompositorConfig::Native { compositor, .. } => compositor.read_tile(id)?,
                        _ => unreachable!(),
                    };
                    let target = self.acquired_target(target, size)?;
                    let bytes = self.capture_texture(&target.texture)?;
                    let pitch = target.texture.size.width as usize * 4;
                    let mut pixels = Vec::with_capacity(size.width as usize * size.height as usize * 4);
                    for row in 0..size.height {
                        let start = (target.origin.y + row) as usize * pitch + target.origin.x as usize * 4;
                        pixels.extend_from_slice(&bytes[start..start + size.width as usize * 4]);
                    }
                    let data = format!("textures/hal-native-{}.raw", native.tiles.len());
                    fs::write(root.join(&data), pixels).map_err(|error| error.to_string())?;
                    native.tiles.push((id, size, PlainTexture { data, size,
                        format: image_format(target.texture.format)?, filter: TextureFilter::Linear, has_depth: false, category: None }));
                }
                write_ron(root.join("hal-native.ron"), &native)?;
            } else if root.join("hal-native.ron").exists() {
                fs::remove_file(root.join("hal-native.ron")).map_err(|error| error.to_string())?;
            }
            let mut renderer = PlainRenderer { device_size, textures: Default::default() };
            for (index, (id, texture)) in self.textures.iter().enumerate() {
                let data = format!("textures/hal-cache-{index}.raw");
                fs::write(root.join(&data), self.capture_texture(texture)?).map_err(|error| error.to_string())?;
                renderer.textures.insert(*id, PlainTexture {
                    data, size: DeviceIntSize::new(texture.size.width as i32, texture.size.height as i32),
                    format: image_format(texture.format)?, filter: texture.filter, has_depth: false, category: None,
                });
            }
            write_ron(root.join("renderer.ron"), &renderer)?;
        }
        dispatch_releases(&self.releases);
        Ok(())
    }

    #[cfg(feature = "replay")]
    pub fn load_capture(&mut self, config: CaptureConfig, externals: Vec<PlainExternalImage>) -> Result<()> {
        self.submissions.wait()?;
        self.descriptors.borrow_mut().clear();
        self.textures.clear();
        self.depths.clear();
        self.external_images.clear();
        let root = config.resource_root();
        let native = config.deserialize_for_resource::<NativeCapture, _>("hal-native");
        if native.is_some() && !matches!(self.compositor, CompositorConfig::Native { .. }) {
            return Err("HAL native frame replay requires a native compositor; rebuild the scene for another backend".into());
        }
        let device = self.external_image_device();
        if let CompositorConfig::Native { compositor, .. } = &mut self.compositor {
            use crate::composite::{NativeSurfaceOperation, NativeSurfaceOperationDetails as Op};
            let destroy: Vec<_> = self.native_operations.iter().filter_map(|op| match op.details {
                Op::CreateSurface { id, .. } | Op::CreateExternalSurface { id, .. } | Op::CreateBackdropSurface { id, .. } =>
                    Some(NativeSurfaceOperation { details: Op::DestroySurface { id } }),
                _ => None,
            }).collect();
            compositor.update_surfaces(&device, &destroy)?;
            if let Some(native) = &native { compositor.update_surfaces(&device, &native.operations)?; }
        }
        self.native_operations.clear();
        self.native_sizes.clear();
        if let Some(native) = native {
            self.native_operations = native.operations;
            for (id, size, plain) in native.tiles {
                let bytes = fs::read(root.join(&plain.data)).map_err(|error| error.to_string())?;
                let image = Texture::new(&self.owner, size.width as u32, size.height as u32,
                    texture_format(plain.format)?, plain.filter, false)?;
                let rect = DeviceIntRect::from_size(size);
                image.upload_recorded(&self.owner, &self.submissions, rect, &bytes, None, 0, None)?;
                self.bind_native_tile(id, size, rect, rect)?;
                let target = &self.native_targets[&id];
                let texture = target.texture.clone();
                let destination = rect.translate(target.origin.to_vector());
                self.copy(&image, &texture, rect, destination)?;
            }
            self.restore_external_images()?;
            self.submissions.wait()?;
            self.native_targets.clear();
            dispatch_releases(&self.releases);
        }
        let descriptions = config.deserialize_for_resource::<PlainExternalResources, _>("external_resources")
            .map(|resources| resources.images).unwrap_or_default();
        let factory = self.external_image_device();
        let mut images = HashMap::new();
        for external in externals {
            let description = descriptions.iter().find(|description| description.external.id == external.external.id
                && description.external.channel_index == external.external.channel_index)
                .ok_or("Capture has no descriptor for an external image")?;
            let bytes = fs::read(root.join(&external.data)).map_err(|error| error.to_string())?;
            let source = match external.external.image_type {
                api::ExternalImageType::Buffer => ExternalImageSource::Buffer(std::sync::Arc::new(bytes)),
                api::ExternalImageType::TextureHandle(_) => ExternalImageSource::Native(factory.create_image(description.descriptor, &bytes)?),
            };
            let uv = if external.external.normalized_uvs {
                TexelRect::new(external.uv.uv0.x * description.descriptor.size.width as f32,
                    external.uv.uv0.y * description.descriptor.size.height as f32,
                    external.uv.uv1.x * description.descriptor.size.width as f32,
                    external.uv.uv1.y * description.descriptor.size.height as f32)
            } else { external.uv };
            images.insert((external.external.id, external.external.channel_index), (source, description.descriptor, uv));
        }
        if let Some(renderer) = config.deserialize_for_resource::<PlainRenderer, _>("renderer") {
            for (id, plain) in renderer.textures {
                let bytes = fs::read(root.join(&plain.data)).map_err(|error| error.to_string())?;
                let texture = Texture::new(&self.owner, plain.size.width as u32, plain.size.height as u32,
                    texture_format(plain.format)?, plain.filter,
                    matches!(plain.format, api::ImageFormat::RGBA8 | api::ImageFormat::BGRA8 | api::ImageFormat::R8))?;
                texture.upload_recorded(&self.owner, &self.submissions, DeviceIntRect::from_size(plain.size), &bytes, None, 0, None)?;
                self.generate_mips(&texture)?;
                self.textures.insert(id, texture);
            }
        }
        self.submissions.submit()?;
        self.set_external_image_provider(Box::new(ReplayImages { images }));
        Ok(())
    }
}
