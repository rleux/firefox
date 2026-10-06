/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::wgpu::{
    render_device::RenderDevice,
    tests::{validation_logging, ERRORS},
    Options,
};
use crate::device::{DrawTarget, GpuBackend, LoadOp, RenderPassDescriptor, StoreOp};
use crate::internal_types::RenderTargetInfo;
use api::units::{DeviceIntRect, DeviceIntSize};
use api::{ExternalTextureHandle, ImageBufferKind, ImageDescriptor, ImageDescriptorFlags};
use std::sync::atomic::Ordering;

fn sampled_view(source: &Rc<Texture>, opaque: bool) -> Rc<Texture> {
    let owner = &source.raw.owner;
    let view = unsafe {
        owner.open.device.create_texture_view(
            source.raw_texture(),
            &hal::TextureViewDescriptor {
                label: Some("Sample-only readback fixture"),
                swizzle: wgt::TextureComponentSwizzle {
                    a: if opaque {
                        wgt::ComponentSwizzle::One
                    } else {
                        wgt::ComponentSwizzle::A
                    },
                    ..Default::default()
                },
                format: source.format,
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
    .unwrap();
    Rc::new(Texture {
        view: Some(Owned::new(owner, view, <dyn hal::DynDevice>::destroy_texture_view)),
        target: None,
        mips: Default::default(),
        raw: source.raw.clone(),
        size: source.size,
        format: source.format,
        filter: TextureFilter::Nearest,
        base_mip: 0,
        mip_count: 1,
        usage: wgt::TextureUses::RESOURCE,
        states: source.states.clone(),
        #[cfg(target_os = "linux")]
        external: None,
    })
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn external_readback_samples_views_and_flushes_pending_writes() {
    validation_logging();
    let owner = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Default::default()
        })
        .unwrap(),
    );
    let mut device = RenderDevice::new(&owner).unwrap();
    let registry = device.textures.external_textures();
    let size = DeviceIntSize::new(3, 2);
    let rect = DeviceIntRect::from_size(size);
    let colors = [
        255, 0, 0, 64, 0, 255, 0, 128, 0, 0, 255, 0, 32, 64, 96, 128, 255, 255, 0, 255, 0, 255,
        255, 64,
    ];
    for native_format in [ImageFormat::RGBA8, ImageFormat::BGRA8] {
        device.begin_frame().unwrap();
        let mut handle = device
            .textures
            .create(
                ImageBufferKind::Texture2D,
                native_format,
                size,
                TextureFilter::Nearest,
                Some(RenderTargetInfo { has_depth: false }),
            )
            .unwrap();
        let source = device.textures.image(&handle).unwrap();
        let mut native = colors;
        if native_format == ImageFormat::BGRA8 {
            for pixel in native.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
        }
        device.upload_texture_immediate(&handle, &native).unwrap();
        let mut registered = Vec::new();
        for opaque in [false, true] {
            let view = sampled_view(&source, opaque);
            assert!(view
                .transition(
                    &mut device.submissions.recording().unwrap(),
                    wgt::TextureUses::COPY_SRC
                )
                .is_err());
            let external = registry.register(&view).unwrap();
            registered.push(external);
            for format in [ImageFormat::RGBA8, ImageFormat::BGRA8] {
                let mut expected = colors;
                for pixel in expected.chunks_exact_mut(4) {
                    if opaque {
                        pixel[3] = 255;
                    }
                    if format == ImageFormat::BGRA8 {
                        pixel.swap(0, 2);
                    }
                }
                let descriptor = ImageDescriptor::new(3, 2, format, ImageDescriptorFlags::empty());
                assert_eq!(
                    GpuBackend::read_external_texture(
                        &mut device,
                        external,
                        ImageBufferKind::Texture2D,
                        &descriptor
                    ),
                    expected,
                    "{native_format:?}, {format:?}, opaque={opaque}",
                );
                assert_eq!(view.current_usage(), wgt::TextureUses::RESOURCE);
                assert!(view.sample_initialized());
            }
        }
        device
            .begin_render_pass(&RenderPassDescriptor {
                target: DrawTarget::from_texture(&handle, false),
                render_area: Some(rect),
                color_load: LoadOp::Clear([0.0, 0.0, 1.0, 0.0]),
                depth_load: LoadOp::Load,
            })
            .unwrap();
        let descriptor =
            ImageDescriptor::new(3, 2, ImageFormat::RGBA8, ImageDescriptorFlags::empty());
        for (index, alpha) in [0u8, 255].iter().copied().enumerate() {
            assert_eq!(
                GpuBackend::read_external_texture(
                    &mut device,
                    registered[index],
                    ImageBufferKind::Texture2D,
                    &descriptor
                ),
                [0, 0, 255, alpha].repeat(6),
            );
        }
        device.end_render_pass(StoreOp::Store).unwrap();
        assert!(GpuBackend::read_external_texture(
            &mut device,
            ExternalTextureHandle(u64::MAX),
            ImageBufferKind::Texture2D,
            &descriptor,
        )
        .is_empty());
        for external in registered {
            registry.unregister(external).unwrap();
        }
        device.textures.delete(&mut handle).unwrap();
        device.end_frame().unwrap();
        assert!(device.failure().is_none());
    }
    let integer = Texture::new(
        &owner,
        2,
        1,
        wgt::TextureFormat::Rgba32Sint,
        TextureFilter::Nearest,
        false,
    )
    .unwrap();
    let bytes: Vec<u8> = (0..32).collect();
    integer
        .upload(
            &device.submissions,
            DeviceIntRect::from_size(DeviceIntSize::new(2, 1)),
            &bytes,
            None,
            0,
            None,
        )
        .unwrap();
    let external = registry.register(&integer).unwrap();
    let descriptor =
        ImageDescriptor::new(2, 1, ImageFormat::RGBAI32, ImageDescriptorFlags::empty());
    assert_eq!(
        GpuBackend::read_external_texture(
            &mut device,
            external,
            ImageBufferKind::Texture2D,
            &descriptor
        ),
        bytes,
    );
    registry.unregister(external).unwrap();
    device.submissions.wait().unwrap();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
