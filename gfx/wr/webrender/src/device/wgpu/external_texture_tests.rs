/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::{ExternalTexture, TextureSlot};
use crate::device::wgpu::{wgt, Options, TextureFilter};
use crate::device::wgpu::program::ShaderResource;
use crate::device::wgpu::texture_store::TextureStore;
use crate::device::wgpu::tests::{validation_logging, ERRORS};
use api::{
    ImageBufferKind, ImageFormat, ImageRendering,
    units::{DeviceIntSize, TexelRect},
};
use std::sync::atomic::Ordering;

fn device() -> Rc<Device> {
    validation_logging();
    Rc::new(
        Device::new(&Options {
            validation: true,
            ..Default::default()
        })
        .unwrap(),
    )
}

fn texture(owner: &Rc<Device>) -> Rc<Texture> {
    Texture::new(
        owner,
        2,
        2,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        false,
    )
    .unwrap()
}

fn external(handle: ExternalTextureHandle, rendering: ImageRendering) -> ExternalTexture {
    ExternalTexture::new(
        handle,
        ImageBufferKind::Texture2D,
        TexelRect::new(0., 0., 2., 2.),
        rendering,
    )
}

#[test]
#[ignore = "Requires Vulkan and validation"]
fn external_texture_names_and_binding_snapshots_are_independent() {
    {
        let owner = device();
        let mut store = TextureStore::new(&owner);
        let registry = store.external_textures();
        let mut owned = store
            .create(
                ImageBufferKind::Texture2D,
                ImageFormat::RGBA8,
                DeviceIntSize::new(2, 2),
                TextureFilter::Nearest,
                None,
            )
            .unwrap();
        let image = texture(&owner);
        let handle = registry.register(&image).unwrap();
        assert_eq!(handle.0, u64::from(owned.id));
        store.bind(TextureSlot(0), &owned).unwrap();
        store
            .bind_external(TextureSlot(1), &external(handle, ImageRendering::Auto))
            .unwrap();
        let bindings = store.bindings();
        match (&bindings[0], &bindings[1]) {
            (
                Some(ShaderResource::Texture { texture: first, .. }),
                Some(ShaderResource::Texture {
                    texture: second,
                    filter,
                }),
            ) => {
                assert!(!Rc::ptr_eq(first, second));
                assert!(Rc::ptr_eq(second, &image));
                assert_eq!(*filter, Some(TextureFilter::Linear));
            }
            _ => panic!("Expected owned and external texture bindings"),
        }
        drop(bindings);
        let weak = Rc::downgrade(&image);
        registry.unregister(handle).unwrap();
        drop(image);
        store.delete(&mut owned).unwrap();
        assert!(store.bindings()[0].is_none());
        assert!(store.bindings()[1].is_some());
        assert!(store
            .bind_external(TextureSlot(1), &external(handle, ImageRendering::Auto))
            .is_err());
        let saved = store.bindings();
        store.reset_bindings();
        assert!(weak.upgrade().is_some());
        drop(saved);
        assert!(weak.upgrade().is_none());
        let replacement = texture(&owner);
        let next = registry.register(&replacement).unwrap();
        assert!(next.0 > handle.0);
        for (rendering, expected) in [
            (ImageRendering::CrispEdges, TextureFilter::Linear),
            (ImageRendering::Pixelated, TextureFilter::Nearest),
        ] {
            store
                .bind_external(TextureSlot(2), &external(next, rendering))
                .unwrap();
            match &store.bindings()[2] {
                Some(ShaderResource::Texture { filter, .. }) => assert_eq!(*filter, Some(expected)),
                _ => panic!("Expected external binding"),
            }
        }
        store.clear_color_bindings();
        assert!(store.bindings().iter().all(Option::is_none));
        registry.unregister(next).unwrap();
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and validation"]
fn external_texture_registry_rejects_wrong_devices_and_invalid_handles() {
    {
        let owner = device();
        let other = device();
        let mut store = TextureStore::new(&owner);
        let registry = store.external_textures();
        assert!(registry.register(&texture(&other)).is_err());
        let handle = registry.register(&texture(&owner)).unwrap();
        assert!(store
            .bind_external(TextureSlot(16), &external(handle, ImageRendering::Auto))
            .is_err());
        for kind in [
            ImageBufferKind::TextureRect,
            ImageBufferKind::TextureExternal,
        ] {
            let invalid = ExternalTexture::new(
                handle,
                kind,
                TexelRect::new(0., 0., 2., 2.),
                ImageRendering::Auto,
            );
            assert!(store.bind_external(TextureSlot(0), &invalid).is_err());
        }
        assert!(store
            .bind_external(
                TextureSlot(0),
                &external(ExternalTextureHandle(0), ImageRendering::Auto)
            )
            .is_err());
        assert!(registry
            .unregister(ExternalTextureHandle((1u64 << 32) | handle.0))
            .is_err());
        registry.last_id.set(u32::MAX);
        assert!(registry.register(&texture(&owner)).is_err());
        assert!(registry.get(handle.0 as u32).is_ok());
        registry.unregister(handle).unwrap();
        assert!(registry.unregister(handle).is_err());
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
