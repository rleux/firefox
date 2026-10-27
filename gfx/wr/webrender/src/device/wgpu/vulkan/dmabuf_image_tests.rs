/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::wgpu::{hal, Options, SharedTimeline, Submission};
use crate::device::wgpu::resources::Owned;
use crate::device::wgpu::tests::{map_upload, validation_logging, ERRORS};
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::Ordering;
use std::time::Duration;

#[path = "dmabuf_access_tests.rs"]
mod access;

fn descriptor() -> DmaBufImageDescriptor {
    DmaBufImageDescriptor {
        size: [3, 2],
        format: wgt::TextureFormat::Rgba8Unorm,
        usage: wgt::TextureUses::RESOURCE,
        modifier: 0,
        offset: 128,
        row_pitch: 64,
        device_uuid: [0; 16],
        driver_uuid: [0; 16],
    }
}

#[test]
fn dma_buf_linear_bounds_include_offset_and_last_pixel() {
    let mut desc = descriptor();
    assert!(desc.validate_plane(203).is_err());
    assert!(desc.validate_plane(204).is_ok());
    desc.size = [1, 1];
    desc.row_pitch = 4096;
    assert!(desc.validate_plane(132).is_ok());
    desc.offset = u64::MAX - 3;
    assert!(desc.validate_plane(u64::MAX).is_err());
    desc.offset = 0;
    desc.size = [1, 3];
    desc.row_pitch = u64::MAX - 3;
    assert!(desc.validate_plane(u64::MAX).is_err());
}

#[test]
fn dma_buf_plane_checks_preserve_opaque_modifier_layouts() {
    let mut desc = descriptor();
    for (pitch, offset) in [(0, 0), (8, 0), (13, 0), (16, 1)] {
        desc.row_pitch = pitch;
        desc.offset = offset;
        assert!(desc.validate_plane(4096).is_err());
    }
    desc.modifier = 1;
    desc.row_pitch = 1;
    desc.offset = 7;
    assert!(desc.validate_plane(8).is_ok());
    assert!(desc.validate_plane(7).is_err());
    desc.size[0] = 0;
    assert!(desc.validate_plane(8).is_err());
}

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

pub(in crate::device::wgpu) fn consumer_device(
    shared: bool,
) -> (
    Rc<Device>,
    Option<std::sync::Arc<dyn hal::shared::SharedDeviceContext>>,
) {
    if !shared {
        return (device(), None);
    }
    let original = device();
    let pair = unsafe {
        hal::shared::open_shared_device(
            &*original.adapter,
            original.features,
            &original.capabilities().limits,
            &wgt::MemoryHints::MemoryUsage,
        )
    }
    .unwrap();
    let context = pair.context;
    let graphics = std::sync::Arc::new(pair.graphics);
    drop(pair.compute);
    let (_, instance) = context.clone_instance();
    let adapter = unsafe { instance.enumerate_adapters(None) }.pop().unwrap();
    unsafe extern "C" fn fatal() -> ! {
        std::process::exit(88)
    }
    let owner = Rc::new(
        unsafe {
            Device::from_shared_endpoint(
                graphics.clone(),
                instance,
                adapter,
                crate::device::wgpu::SharedDeviceOptions {
                    context: context.clone(),
                    recover: Rc::new(|_| std::process::exit(88)),
                    fatal_process: fatal,
                },
            )
        }
        .unwrap(),
    );
    assert!(std::sync::Arc::ptr_eq(&owner.open, &graphics));
    let first = owner.dma_buf_capabilities().unwrap();
    let second = owner.dma_buf_capabilities().unwrap();
    assert!(std::sync::Arc::ptr_eq(first.native.formats(), second.native.formats()));
    assert!(!first.native.formats().is_empty());
    (owner, Some(context))
}

fn export_image(
    owner: &Rc<Device>,
    format: wgt::TextureFormat,
    modifier: u64,
    size: [u32; 2],
) -> (Rc<DmaBufImage>, OwnedFd) {
    let (image, fd) =
        hal::dma_buf::testing::export_image(owner.open.clone(), format, modifier, size).unwrap();
    (DmaBufImage::from_hal_image(owner, image), fd)
}

#[test]
#[ignore = "Requires Vulkan DMA-BUF images, shared timelines and validation"]
fn dma_buf_import_reuses_shared_pixels_and_retains_pending_images() {
    for shared in [false, true] {
        let producer = device();
        let (consumer, _context) = consumer_device(shared);
        let ready = SharedTimeline::new(&producer).unwrap();
        let ready_import = SharedTimeline::import(&consumer, &ready.export().unwrap()).unwrap();
        let released = SharedTimeline::new(&consumer).unwrap();
        let release_import =
            SharedTimeline::import(&producer, &released.export().unwrap()).unwrap();
        let mut value = 0;
        for format in [
            wgt::TextureFormat::Rgba8Unorm,
            wgt::TextureFormat::Bgra8Unorm,
        ] {
            let usages = wgt::TextureUses::RESOURCE
                | wgt::TextureUses::COPY_SRC
                | wgt::TextureUses::COPY_DST;
            let formats = producer.dma_buf_formats(format, usages).unwrap();
            for caps in formats.iter().filter(|entry| entry.exportable()) {
                let (source, fd) = export_image(&producer, format, caps.modifier(), [17, 9]);
                let mut imported = Some(
                    unsafe { consumer.import_dma_buf(fd.as_fd(), *source.descriptor()) }.unwrap(),
                );
                assert!(fd.try_clone().is_ok());
                drop(fd);
                let weak = Rc::downgrade(imported.as_ref().unwrap());
                let size = 17 * 9 * 4;
                for channel in 0..3 {
                    value += 1;
                    let mut color = [0.0, 0.0, 0.0, 1.0];
                    color[channel] = 1.0;
                    let mut send = access::publish(
                        &source,
                        &ready,
                        value,
                        if channel == 0 {
                            None
                        } else {
                            Some((&release_import, value - 1))
                        },
                        color,
                    );
                    let output = Rc::new(Owned::new(
                        &consumer,
                        unsafe {
                            consumer.open.device.create_buffer(&hal::BufferDescriptor {
                                label: Some("DMA-BUF import readback"),
                                size,
                                usage: wgt::BufferUses::COPY_DST | wgt::BufferUses::MAP_READ,
                                memory_flags: hal::MemoryFlags::PREFER_COHERENT,
                            })
                        }
                        .unwrap()
                        .0,
                        <dyn hal::DynDevice>::destroy_buffer,
                    ));
                    let mut receive = Submission::new(&consumer).unwrap();
                    {
                        let image = imported.as_ref().unwrap();
                        let mut recording = receive.recording().unwrap();
                        let alias = Rc::new(Owned::new(
                            &consumer,
                            image.hal_texture().unwrap(),
                            <dyn hal::DynDevice>::destroy_texture,
                        ));
                        unsafe { image.acquire(&mut recording, &ready_import, value) }.unwrap();
                        unsafe {
                            recording
                                .encoder()
                                .transition_textures(&[hal::TextureBarrier {
                                    texture: &**alias,
                                    range: wgt::ImageSubresourceRange::default(),
                                    queue_family_ownership_transfer: None,
                                    usage: hal::StateTransition {
                                        from: wgt::TextureUses::RESOURCE,
                                        to: wgt::TextureUses::COPY_SRC,
                                    },
                                }]);
                            recording
                                .encoder()
                                .transition_buffers(&[hal::BufferBarrier {
                                    buffer: &**output,
                                    usage: hal::StateTransition {
                                        from: wgt::BufferUses::empty(),
                                        to: wgt::BufferUses::COPY_DST,
                                    },
                                }]);
                            recording.encoder().copy_texture_to_buffer(
                                &**alias,
                                wgt::TextureUses::COPY_SRC,
                                &**output,
                                &[hal::BufferTextureCopy {
                                    buffer_layout: wgt::TexelCopyBufferLayout {
                                        offset: 0,
                                        bytes_per_row: Some(17 * 4),
                                        rows_per_image: None,
                                    },
                                    texture_base: hal::TextureCopyBase {
                                        mip_level: 0,
                                        array_layer: 0,
                                        origin: wgt::Origin3d::ZERO,
                                        aspect: hal::FormatAspects::COLOR,
                                    },
                                    size: wgt::Extent3d {
                                        width: 17,
                                        height: 9,
                                        depth_or_array_layers: 1,
                                    }
                                    .into(),
                                }],
                            );
                            recording
                                .encoder()
                                .transition_buffers(&[hal::BufferBarrier {
                                    buffer: &**output,
                                    usage: hal::StateTransition {
                                        from: wgt::BufferUses::COPY_DST,
                                        to: wgt::BufferUses::MAP_READ,
                                    },
                                }]);
                            recording
                                .encoder()
                                .transition_textures(&[hal::TextureBarrier {
                                    texture: &**alias,
                                    range: wgt::ImageSubresourceRange::default(),
                                    queue_family_ownership_transfer: None,
                                    usage: hal::StateTransition {
                                        from: wgt::TextureUses::COPY_SRC,
                                        to: wgt::TextureUses::RESOURCE,
                                    },
                                }]);
                        }
                        recording.keep(&alias);
                        recording.keep(&output);
                        image.release(&mut recording, &released, value).unwrap();
                    }
                    receive.submit().unwrap();
                    if channel == 2 {
                        drop(imported.take());
                        assert!(weak.upgrade().is_some());
                    }
                    assert!(receive.wait(Some(Duration::from_secs(5))).unwrap());
                    assert!(send.wait(Some(Duration::from_secs(5))).unwrap());
                    let mut pixel = [0, 0, 0, 255];
                    pixel[channel] = 255;
                    if format == wgt::TextureFormat::Bgra8Unorm {
                        pixel.swap(0, 2);
                    }
                    assert_eq!(map_upload(&consumer, &**output, size), pixel.repeat(17 * 9));
                }
                assert!(weak.upgrade().is_none());
            }
        }
        assert!(value > 0, "No supported exportable DMA-BUF modifier was exercised");
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan DMA-BUF images and validation"]
fn dma_buf_import_rejects_bad_metadata_and_preserves_descriptors() {
    for shared in [false, true] {
        let producer = device();
        let mut exercised = 0;
        for format in [wgt::TextureFormat::Rgba8Unorm, wgt::TextureFormat::Bgra8Unorm] {
            let usages = wgt::TextureUses::RESOURCE
                | wgt::TextureUses::COPY_SRC | wgt::TextureUses::COPY_DST;
            let formats = producer.dma_buf_formats(format, usages).unwrap();
            for caps in formats.iter().filter(|entry| entry.exportable()) {
                let (consumer, _context) = consumer_device(shared);
                let (source, fd) = export_image(&producer, format, caps.modifier(), [17, 9]);
                for which in 0..5 {
                    let mut descriptor = *source.descriptor();
                    match which {
                        0 => descriptor.size = [0, 9],
                        1 => descriptor.modifier = u64::MAX,
                        2 => descriptor.device_uuid[0] ^= 1,
                        3 => descriptor.driver_uuid[0] ^= 1,
                        _ => descriptor.row_pitch = 0,
                    }
                    assert!(unsafe { consumer.import_dma_buf(fd.as_fd(), descriptor) }.is_err(),
                        "shared={shared}, format={format:?}, modifier={:#x}, variant={which}, original_pitch={}, invalid_pitch={}", caps.modifier(), source.descriptor().row_pitch, descriptor.row_pitch);
                    assert!(fd.try_clone().is_ok());
                }
                if caps.modifier() == 0 {
                    let mut descriptor = *source.descriptor();
                    descriptor.row_pitch = 1;
                    assert!(unsafe { consumer.import_dma_buf(fd.as_fd(), descriptor) }.is_err());
                    assert!(fd.try_clone().is_ok());
                }
                let (large, large_fd) = export_image(&producer, format, caps.modifier(), [1024, 1024]);
                let error = unsafe { consumer.import_dma_buf(fd.as_fd(), *large.descriptor()) }
                    .err().unwrap();
                let expected = if caps.modifier() == 0 {
                    "Linear DMA-BUF pixels exceed the allocation"
                } else {
                    "requirements"
                };
                assert!(error.contains(expected), "{}", error);
                assert!(fd.try_clone().is_ok());
                assert!(!consumer.is_lost());
                let imported = unsafe {
                    consumer.import_dma_buf(large_fd.as_fd(), *large.descriptor())
                }.unwrap();
                let weak = Rc::downgrade(&consumer);
                drop(consumer);
                assert!(weak.upgrade().is_some());
                drop(imported);
                assert!(weak.upgrade().is_none());
                exercised += 1;
                assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
            }
        }
        assert!(exercised > 0, "No supported exportable DMA-BUF modifier was exercised");
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires shared Vulkan DMA-BUF images and validation"]
fn shared_dma_buf_import_preserves_owner_counters_and_admission() {
    use crate::device::wgpu::{ExternalTextureRegistry, ForeignRgbLayout, Texture, TextureFilter};
    use crate::device::wgpu::render_device::RenderDevice;

    let producer = device();
    let (consumer, context) = consumer_device(true);
    let context = context.unwrap();
    let renderer = RenderDevice::new(&consumer).unwrap();
    let registry = renderer.textures.external_textures();
    let capabilities = registry.device().dma_buf_capabilities().unwrap();
    let (source, fd) = export_image(&producer, wgt::TextureFormat::Rgba8Unorm, 0, [2, 2]);
    assert!(capabilities.supports(source.descriptor()));
    let counts = || {
        let counters = consumer.open.device.get_internal_counters();
        (counters.textures.read(), counters.texture_memory.read())
    };
    let before = counts();
    let image = unsafe { consumer.import_dma_buf(fd.as_fd(), *source.descriptor()) }.unwrap();
    assert!(Rc::ptr_eq(&image.owner, &consumer));
    assert_eq!(counts(), before);
    let weak = std::sync::Arc::downgrade(&image.inner);
    let texture = Texture::from_dma_buf(&image, TextureFilter::Nearest, false).unwrap();
    let aliased = counts();
    assert_eq!(aliased, (before.0 + 1, before.1));
    let handle = registry.register(&texture).unwrap();
    let foreign_registry = ExternalTextureRegistry::new(&producer);
    assert!(foreign_registry.register(&texture).is_err());
    assert_eq!(counts(), aliased);
    registry.unregister(handle).unwrap();
    drop(texture);
    assert!(weak.upgrade().is_some());
    assert_eq!(counts(), before);
    let alias_image = unsafe { consumer.import_dma_buf(fd.as_fd(), *source.descriptor()) }.unwrap();
    let alias_weak = std::sync::Arc::downgrade(&alias_image.inner);
    let alias = alias_image.hal_texture().unwrap();
    assert_eq!(counts(), (before.0 + 1, before.1));
    drop(alias_image);
    assert!(alias_weak.upgrade().is_some());
    unsafe { consumer.open.device.destroy_texture(alias) };
    assert!(alias_weak.upgrade().is_none());
    assert_eq!(counts(), before);
    let descriptor = source.descriptor();
    let layout = ForeignRgbLayout::new(
        descriptor.size,
        0x34324241,
        descriptor.modifier,
        descriptor.row_pitch,
        descriptor.offset,
    )
    .unwrap();
    if let Some(node) = capabilities.foreign_drm_node() {
        if capabilities.supports_foreign_rgb(node, layout) {
            let foreign = unsafe { consumer.import_foreign_rgb(fd.as_fd(), layout) }.unwrap();
            assert_eq!(foreign.layout().size(), [2, 2]);
            drop(foreign);
            assert_eq!(counts(), before);
        }
    }
    context.drain().unwrap();
    assert!(unsafe { consumer.import_dma_buf(fd.as_fd(), *descriptor) }.is_err());
    assert!(Texture::from_dma_buf(&image, TextureFilter::Nearest, false).is_err());
    assert_eq!(counts(), before);
    drop(image);
    assert!(weak.upgrade().is_none());
    assert!(fd.try_clone().is_ok());
    drop((registry, renderer, consumer));
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
