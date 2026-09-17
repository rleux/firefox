/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use api::units::*;
use api::*;
use crate::device::hal::{ExternalImageProvider, Options};
use crate::render_api::Transaction;
use crate::WebRenderOptions;
use std::cell::RefCell;

#[test]
fn nv12_layout_rejects_invalid_storage() {
    let make = |allocation, visible, modifier, strides, offsets, bytes| {
        Nv12DmaBufLayout::new(allocation, visible, modifier, strides, offsets, bytes)
    };
    assert!(make(
        [256, 128],
        [248, 120],
        INTEL_Y_TILED,
        [256; 2],
        [0, 32768],
        49152
    )
    .is_ok());
    assert!(make([256, 128], [256, 128], 0, [256; 2], [0, 32768], 49152).is_ok());
    for (allocation, visible, modifier, strides, offsets, bytes) in [
        ([0, 128], [256, 128], 0, [256; 2], [0, 32768], 49152),
        ([256, 128], [258, 128], 0, [256; 2], [0, 32768], 49152),
        ([256, 128], [255, 128], 0, [256; 2], [0, 32768], 49152),
        ([256, 128], [256, 128], 1, [256; 2], [0, 32768], 49152),
        ([256, 128], [256, 128], 0, [128; 2], [0, 32768], 49152),
        ([256, 128], [256, 128], 0, [256; 2], [0, 32768], 49151),
        ([256, 128], [256, 128], 0, [256; 2], [0, 16384], 49152),
        (
            [256, 128],
            [256, 128],
            0,
            [u64::MAX; 2],
            [0, 32768],
            u64::MAX,
        ),
        (
            [256, 128],
            [256, 128],
            INTEL_Y_TILED,
            [257; 2],
            [0, 32768],
            49152,
        ),
        (
            [256, 128],
            [256, 128],
            INTEL_Y_TILED,
            [256; 2],
            [1, 32768],
            49152,
        ),
    ] {
        assert!(make(allocation, visible, modifier, strides, offsets, bytes).is_err());
    }
}

#[test]
fn nv12_abandonment_survives_later_plane_completion() {
    let observed = Rc::new(Cell::new(None));
    let result = observed.clone();
    let release = Release {
        access: None,
        callback: Some(Box::new(move |status| result.set(Some(status)))),
        status: Cell::new(ExternalImageRelease::Unused),
    };
    release.finish(ExternalImageRelease::Abandoned);
    release.finish(ExternalImageRelease::Complete);
    release.finish(ExternalImageRelease::Unused);
    drop(release);
    assert_eq!(observed.get(), Some(ExternalImageRelease::Abandoned));
}

#[test]
fn nv12_capabilities_check_modifier_extent_and_bytes() {
    let layout =
        Nv12DmaBufLayout::new([256, 128], [248, 120], 0, [256; 2], [0, 32768], 49152).unwrap();
    let capabilities = Nv12DmaBufCapabilities {
        modifier: 0,
        max_size: [256, 128],
        max_allocation_size: 49152,
    };
    assert!(capabilities.supports(&layout));
    for changed in [
        Nv12DmaBufCapabilities {
            modifier: INTEL_Y_TILED,
            ..capabilities
        },
        Nv12DmaBufCapabilities {
            max_size: [254, 128],
            ..capabilities
        },
        Nv12DmaBufCapabilities {
            max_size: [256, 126],
            ..capabilities
        },
        Nv12DmaBufCapabilities {
            max_allocation_size: 49151,
            ..capabilities
        },
    ] {
        assert!(!changed.supports(&layout));
    }
}

fn fixture() -> (OwnedFd, Nv12DmaBufLayout, [u64; 2], Vec<u8>) {
    let number = |name| {
        std::env::var(name)
            .expect("Run through ExportVAAPIFrame")
            .parse::<u64>()
            .unwrap()
    };
    let fd = unsafe { BorrowedFd::borrow_raw(number("WR_NV12_FD") as i32) };
    let layout = Nv12DmaBufLayout::new(
        [
            number("WR_NV12_ALLOC_WIDTH") as u32,
            number("WR_NV12_ALLOC_HEIGHT") as u32,
        ],
        [
            number("WR_NV12_WIDTH") as u32,
            number("WR_NV12_HEIGHT") as u32,
        ],
        number("WR_NV12_MODIFIER"),
        [number("WR_NV12_Y_PITCH"), number("WR_NV12_UV_PITCH")],
        [number("WR_NV12_Y_OFFSET"), number("WR_NV12_UV_OFFSET")],
        number("WR_NV12_BYTES"),
    )
    .unwrap();
    let bytes = std::fs::read(std::env::var("WR_NV12_REFERENCE").unwrap()).unwrap();
    (
        fd.try_clone_to_owned().unwrap(),
        layout,
        [number("WR_NV12_DRM_MAJOR"), number("WR_NV12_DRM_MINOR")],
        bytes,
    )
}

fn read_plane(device: &ExternalImageDevice, image: &ExternalNativeImage) -> Vec<u8> {
    use super::super::super::super::resources::Buffer;
    let producer = device.dmabuf_producer().unwrap();
    let owner = &producer.owner;
    let texture = image.texture(owner).unwrap();
    let layout = ReadbackLayout::with_pixel_size(
        texture.size.width,
        texture.size.height,
        owner.capabilities.alignments.buffer_copy_pitch.get(),
        bytes_per_pixel(texture.format) as u32,
    )
    .unwrap();
    let buffer = Buffer::readback(owner, &layout).unwrap();
    {
        let mut commands = producer.submissions.recording().unwrap();
        texture.transition(&mut commands, wgt::TextureUses::COPY_SRC);
        buffer.transition(&mut commands, wgt::BufferUses::COPY_DST);
        unsafe {
            copy_readback::<V>(
                commands.encoder(),
                &texture.raw,
                &buffer.raw,
                &layout,
                texture.size,
                texture.copy_aspect(),
            );
        }
        texture.transition(&mut commands, wgt::TextureUses::RESOURCE);
        buffer.transition(&mut commands, wgt::BufferUses::MAP_READ);
    }
    producer.submissions.wait().unwrap();
    owner.map_readback(&buffer.raw, &layout).unwrap()
}

struct Notice;
impl RenderNotifier for Notice {
    fn clone(&self) -> Box<dyn RenderNotifier> {
        Box::new(Self)
    }
    fn wake_up(&self, _: bool) {}
    fn new_frame_ready(&self, _: DocumentId, _: FramePublishId, _: &FrameReadyParams) {}
    fn shut_down(&self) {}
}
struct Provider {
    image: WeakForeignNv12Image,
    size: [u32; 2],
}
impl ExternalImageProvider for Provider {
    fn acquire(&mut self, id: ExternalImageId, channel: u8, _: bool) -> Result<ExternalImageLease> {
        assert_eq!(id.0, 77);
        self.image
            .upgrade()
            .ok_or("NV12 publication released too early")?
            .lease(
                channel,
                TexelRect::new(
                    0.0,
                    0.0,
                    (self.size[0] >> channel) as f32,
                    (self.size[1] >> channel) as f32,
                ),
            )
    }
}

#[test]
#[ignore = "Requires ExportVAAPIFrame and native Vulkan validation"]
fn vaapi_nv12_direct_sampling_and_shared_release() {
    check_sampling(0);
    check_sampling(8);
}

fn check_sampling(crop: u32) {
    let (fd, mut layout, node, mut reference) = fixture();
    if crop != 0 {
        let original = layout.visible;
        assert!(original.iter().all(|&size| size > crop));
        layout.visible = [original[0] - crop, original[1] - crop];
        let mut cropped = Vec::new();
        for channel in 0..2 {
            let offset = if channel == 0 {
                0
            } else {
                (original[0] * original[1]) as usize
            };
            for row in 0..(layout.visible[1] >> channel) as usize {
                let start = offset + row * original[0] as usize;
                cropped.extend_from_slice(&reference[start..start + layout.visible[0] as usize]);
            }
        }
        reference = cropped;
    }
    let (width, height) = (layout.visible[0], layout.visible[1]);
    let (mut renderer, sender) = crate::hal::create_vulkan_renderer(
        &Options {
            validation: true,
            ..Default::default()
        },
        WebRenderOptions::default(),
        Box::new(Notice),
    )
    .unwrap();
    let device = renderer.external_image_device();
    assert!(device
        .vaapi_nv12_capabilities()
        .unwrap()
        .iter()
        .any(|caps| caps.supports(&layout)));
    let released = Rc::new(RefCell::new(Vec::new()));
    let log = released.clone();
    let image = unsafe {
        device.import_vaapi_nv12(fd.as_fd(), layout, node, 1, move |status| {
            log.borrow_mut().push(status)
        })
    }
    .unwrap();
    let owner = &device.dmabuf_producer().unwrap().owner;
    for channel in 0..2 {
        let captured = read_plane(&device, &image.0.planes[channel]);
        let offset = if channel == 0 {
            0
        } else {
            (width * height) as usize
        };
        for row in 0..(height >> channel) as usize {
            let actual = row * layout.allocation[0] as usize;
            let expected = offset + row * width as usize;
            assert_eq!(
                &captured[actual..actual + width as usize],
                &reference[expected..expected + width as usize]
            );
        }
    }
    let y = image.0.planes[0].texture(owner).unwrap();
    let uv = image.0.planes[1].texture(owner).unwrap();
    assert!(
        Rc::ptr_eq(&y.raw, &uv.raw),
        "Both views must use the same imported image"
    );
    assert_eq!(y.allocation_id, uv.allocation_id);
    assert_ne!(y.copy_aspect(), uv.copy_aspect());
    drop((y, uv));
    let hold_y = image
        .lease(0, TexelRect::new(0.0, 0.0, width as f32, height as f32))
        .unwrap();
    let hold_uv = image
        .lease(
            1,
            TexelRect::new(0.0, 0.0, (width / 2) as f32, (height / 2) as f32),
        )
        .unwrap();
    let weak = image.downgrade();
    renderer
        .set_external_image_provider(Box::new(Provider {
            image: image.downgrade(),
            size: layout.visible,
        }))
        .unwrap();
    drop(image);
    let mut api = sender.create_api();
    let document = api.add_document(DeviceIntSize::new((width * 2 + 2) as i32, height as i32));
    let pipeline = PipelineId(0, 0);
    let native_keys = [api.generate_image_key(), api.generate_image_key()];
    let control_keys = [api.generate_image_key(), api.generate_image_key()];
    assert_eq!(reference.len(), (width * height * 3 / 2) as usize);
    for (iteration, filter) in [
        ImageRendering::Pixelated,
        ImageRendering::Auto,
        ImageRendering::Auto,
    ]
    .iter()
    .copied()
    .enumerate()
    {
        let mut transaction = Transaction::new();
        if iteration == 0 {
            for channel in 0..2 {
                transaction.add_image(
                    native_keys[channel],
                    layout.descriptor(channel),
                    ImageData::External(ExternalImageData {
                        id: ExternalImageId(77),
                        channel_index: channel as u8,
                        image_type: ExternalImageType::TextureHandle(ImageBufferKind::Texture2D),
                        normalized_uvs: false,
                    }),
                    None,
                );
                let range = if channel == 0 {
                    0..(width * height) as usize
                } else {
                    (width * height) as usize..reference.len()
                };
                transaction.add_image(
                    control_keys[channel],
                    layout.descriptor(channel),
                    ImageData::new(reference[range].to_vec()),
                    None,
                );
            }
        }
        let mut builder = DisplayListBuilder::new(pipeline);
        builder.begin(60.0);
        let info = CommonItemProperties {
            clip_rect: LayoutRect::from_size(LayoutSize::new(
                (width * 2 + 2) as f32,
                height as f32,
            )),
            clip_chain_id: ClipChainId::INVALID,
            spatial_id: SpatialId::root_scroll_node(pipeline),
            flags: PrimitiveFlags::default(),
        };
        builder.push_stacking_context(
            info.spatial_id,
            info.flags,
            None,
            TransformStyle::Flat,
            MixBlendMode::Normal,
            &[],
            &[],
            RasterSpace::Screen,
            StackingContextFlags::empty(),
            None,
        );
        for (x, keys) in [(0, native_keys), (width + 2, control_keys)] {
            let scale = if iteration == 2 { 0.5 } else { 1.0 };
            builder.push_yuv_image(
                &info,
                LayoutRect::from_origin_and_size(
                    LayoutPoint::new(x as f32, 0.0),
                    LayoutSize::new(width as f32 * scale, height as f32 * scale),
                ),
                YuvData::NV12(keys[0], keys[1]),
                ColorDepth::Color8,
                YuvColorSpace::Rec601,
                ColorRange::Limited,
                filter,
            );
        }
        builder.pop_stacking_context();
        transaction.set_root_pipeline(pipeline);
        transaction.set_display_list(
            Epoch(iteration as u32),
            api.get_namespace_id(),
            builder.end(),
        );
        transaction.generate_frame(iteration as u64, true, false, RenderReasons::TESTING);
        api.send_transaction(document, transaction);
        renderer.prepare_frame(document).unwrap();
        renderer.render().unwrap();
        let pixels = renderer
            .read_pixels_rgba8(FramebufferIntRect::from_size(FramebufferIntSize::new(
                (width * 2 + 2) as i32,
                height as i32,
            )))
            .unwrap();
        for row in pixels.chunks_exact((width * 2 + 2) as usize * 4) {
            assert_eq!(
                &row[..width as usize * 4],
                &row[(width + 2) as usize * 4..],
                "Direct plane sampling differs from uploaded control"
            );
        }
        renderer.poll().unwrap();
        assert!(released.borrow().is_empty());
    }
    drop(hold_y);
    assert!(
        released.borrow().is_empty(),
        "UV lease must retain the producer"
    );
    drop(hold_uv);
    assert_eq!(*released.borrow(), [ExternalImageRelease::Complete]);
    assert!(weak.upgrade().is_none());
    api.shut_down(true);
}

#[test]
#[ignore = "Requires ExportVAAPIFrame and native Vulkan validation"]
fn vaapi_nv12_rejects_device_mismatch_without_acquire() {
    let (fd, layout, mut node, _) = fixture();
    node[1] += 1;
    let device = ExternalImageDevice::new(&Rc::new(
        create_vulkan_device(&Options {
            validation: true,
            ..Default::default()
        })
        .unwrap(),
    ));
    let status = Rc::new(Cell::new(None));
    let result = status.clone();
    assert!(unsafe {
        device.import_vaapi_nv12(fd.as_fd(), layout, node, 1, move |s| result.set(Some(s)))
    }
    .is_err());
    assert_eq!(status.get(), Some(ExternalImageRelease::Unused));
}

#[test]
#[ignore = "Requires ExportVAAPIFrame and native Vulkan validation"]
fn vaapi_nv12_submit_failure_abandons_publication() {
    let (fd, layout, node, _) = fixture();
    let device = ExternalImageDevice::new(&Rc::new(
        create_vulkan_device(&Options {
            validation: true,
            ..Default::default()
        })
        .unwrap(),
    ));
    device
        .dmabuf_producer()
        .unwrap()
        .owner
        .fault
        .set(Some(FailurePoint::Submit));
    let status = Rc::new(Cell::new(None));
    let result = status.clone();
    assert!(unsafe {
        device.import_vaapi_nv12(fd.as_fd(), layout, node, 1, move |s| result.set(Some(s)))
    }
    .is_err());
    assert_eq!(status.get(), Some(ExternalImageRelease::Abandoned));
    assert!(device.dmabuf_producer().is_err());
}

#[test]
#[ignore = "Requires ExportVAAPIFrame and native Vulkan validation"]
fn vaapi_nv12_partial_view_failure_cleans_up() {
    let (fd, layout, node, _) = fixture();
    let device = ExternalImageDevice::new(&Rc::new(
        create_vulkan_device(&Options {
            validation: true,
            ..Default::default()
        })
        .unwrap(),
    ));
    let owner = &device.dmabuf_producer().unwrap().owner;
    let baseline = owner.memory.get();
    owner.fault.set(Some(FailurePoint::VideoPlaneView));
    let status = Rc::new(Cell::new(None));
    let result = status.clone();
    assert!(unsafe {
        device.import_vaapi_nv12(fd.as_fd(), layout, node, 1, move |s| result.set(Some(s)))
    }
    .is_err());
    assert_eq!(status.get(), Some(ExternalImageRelease::Unused));
    assert_eq!(owner.memory.get().textures, baseline.textures);
    assert_eq!(owner.memory.get().texture_bytes, baseline.texture_bytes);
    assert!(!owner.lost.get());
}
