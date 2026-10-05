/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use webrender::api::units::{DeviceIntRect, TexelRect};
use webrender::api::*;
use webrender::vulkan::{
    BufferPool, ExternalTextureRegistry, SubmissionQueue, Texture, TextureFilter, TextureFormat,
};

pub(crate) struct VulkanImages {
    registry: Rc<ExternalTextureRegistry>,
    uploads: SubmissionQueue,
    images: RefCell<HashMap<ExternalImageId, ImageDescriptor>>,
}

impl VulkanImages {
    pub fn new(registry: Rc<ExternalTextureRegistry>) -> Result<Self, String> {
        let pool = Rc::new(BufferPool::new(registry.device()));
        let uploads = SubmissionQueue::new(&pool, 3)?;
        Ok(Self {
            registry,
            uploads,
            images: RefCell::new(HashMap::new()),
        })
    }

    pub fn uses_registry(&self, registry: &Rc<ExternalTextureRegistry>) -> bool {
        Rc::ptr_eq(&self.registry, registry)
    }

    pub fn add(
        &self,
        descriptor: ImageDescriptor,
        target: ImageBufferKind,
        data: ImageData,
    ) -> Result<ImageData, String> {
        if !matches!(
            target,
            ImageBufferKind::Texture2D | ImageBufferKind::TextureRect
        ) {
            return Err("Unsupported Vulkan external image target".into());
        }
        let ImageData::Raw(bytes) = data else {
            return Err("External image creation requires pixels".into());
        };
        let format = match descriptor.format {
            ImageFormat::R8 => TextureFormat::R8Unorm,
            ImageFormat::R16 => TextureFormat::R16Unorm,
            ImageFormat::RG8 => TextureFormat::Rg8Unorm,
            ImageFormat::RG16 => TextureFormat::Rg16Unorm,
            ImageFormat::BGRA8 => TextureFormat::Bgra8Unorm,
            ImageFormat::RGBA8 => TextureFormat::Rgba8Unorm,
            ImageFormat::RGBAF32 => TextureFormat::Rgba32Float,
            ImageFormat::RGBAI32 => TextureFormat::Rgba32Sint,
        };
        let texture = Texture::new(
            self.registry.device(),
            descriptor.size.width as u32,
            descriptor.size.height as u32,
            format,
            TextureFilter::Linear,
            false,
        )?;
        texture.upload(
            &self.uploads,
            DeviceIntRect::from_size(descriptor.size),
            &bytes,
            descriptor.stride,
            descriptor.offset,
            None,
        )?;
        self.uploads.submit()?;
        let handle = self.registry.register(&texture)?;
        let id = ExternalImageId(handle.0);
        self.images.borrow_mut().insert(id, descriptor);
        Ok(ImageData::External(ExternalImageData {
            id,
            channel_index: 0,
            // Rectangle inputs retain texel UVs on an ordinary Vulkan 2D texture.
            image_type: ExternalImageType::TextureHandle(ImageBufferKind::Texture2D),
            normalized_uvs: false,
        }))
    }

    pub fn lock(&self, id: ExternalImageId, channel: u8) -> ExternalImage<'static> {
        if let Some(descriptor) = self.images.borrow().get(&id).filter(|_| channel == 0) {
            return ExternalImage {
                uv: TexelRect::new(
                    0.0,
                    0.0,
                    descriptor.size.width as f32,
                    descriptor.size.height as f32,
                ),
                source: ExternalImageSource::NativeTexture(ExternalTextureHandle(id.0)),
            };
        }
        ExternalImage {
            uv: TexelRect::new(0.0, 0.0, 0.0, 0.0),
            source: ExternalImageSource::Invalid,
        }
    }
}

impl Drop for VulkanImages {
    fn drop(&mut self) {
        for id in self.images.get_mut().keys() {
            let _ = self.registry.unregister(ExternalTextureHandle(id.0));
        }
    }
}
