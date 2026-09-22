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

thread_local! {
    static COMPLETION_GATE: Cell<bool> = const { Cell::new(false) };
}

fn gated_device() -> ExternalImageDevice {
    let mut device = create_vulkan_device(&Options {
        validation: true,
        ..Default::default()
    })
    .unwrap();
    device.completion_probe = Some(|_, _| {
        Ok(Box::new(|wait| {
            Ok(wait || COMPLETION_GATE.with(|gate| gate.get()))
        }))
    });
    ExternalImageDevice::new(&Rc::new(device))
}

fn set_completion_gate(open: bool) {
    COMPLETION_GATE.with(|gate| gate.set(open));
}

fn wait_for_hardware(device: &ExternalImageDevice) {
    let producer = device.dmabuf_producer().unwrap();
    unsafe { producer.owner.open.queue.wait_for_idle() }.unwrap();
}

#[test]
fn nv12_layout_rejects_invalid_storage() {
    let make = |allocation, visible, modifier, strides, offsets, bytes| {
        VideoDmaBufLayout::new(
            VideoDmaBufFormat::Nv12,
            allocation,
            visible,
            modifier,
            strides,
            offsets,
            bytes,
        )
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
fn p010_layout_checks_sample_storage_and_alignment() {
    let make = |allocation, visible, modifier, strides, offsets, bytes| {
        VideoDmaBufLayout::new(
            VideoDmaBufFormat::P010,
            allocation,
            visible,
            modifier,
            strides,
            offsets,
            bytes,
        )
    };
    assert!(make(
        [256, 128],
        [248, 120],
        INTEL_Y_TILED,
        [512; 2],
        [0, 65536],
        98304,
    )
    .is_ok());
    let linear = make(
        [256, 128],
        [256, 128],
        0,
        [512; 2],
        [0, 65536],
        98304,
    )
    .unwrap();
    assert_eq!(linear.descriptor(0).format, ImageFormat::R16);
    assert_eq!(linear.descriptor(1).format, ImageFormat::RG16);
    for (allocation, visible, modifier, strides, offsets, bytes) in [
        ([0, 128], [256, 128], 0, [512; 2], [0, 65536], 98304),
        ([256, 128], [258, 120], 0, [512; 2], [0, 65536], 98304),
        ([256, 128], [247, 120], 0, [512; 2], [0, 65536], 98304),
        ([255, 128], [248, 120], 0, [512; 2], [0, 65536], 98304),
        ([256, 128], [256, 128], 1, [512; 2], [0, 65536], 98304),
        ([256, 128], [256, 128], 0, [510, 512], [0, 65536], 98304),
        ([256, 128], [256, 128], 0, [513, 512], [0, 65536], 98304),
        ([256, 128], [256, 128], 0, [512; 2], [1, 65536], 98304),
        ([256, 128], [256, 128], 0, [512; 2], [0, 32768], 98304),
        ([256, 128], [256, 128], 0, [512; 2], [0, 65536], 98303),
        (
            [256, 128],
            [256, 128],
            0,
            [u64::MAX; 2],
            [0, 65536],
            u64::MAX,
        ),
        (
            [256, 128],
            [256, 128],
            INTEL_Y_TILED,
            [514, 512],
            [0, 65536],
            98304,
        ),
        (
            [256, 128],
            [256, 128],
            INTEL_Y_TILED,
            [512; 2],
            [0, 65538],
            98304,
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
    let layout = VideoDmaBufLayout::new(
        VideoDmaBufFormat::Nv12,
        [256, 128],
        [248, 120],
        0,
        [256; 2],
        [0, 32768],
        49152,
    )
    .unwrap();
    let capabilities = VideoDmaBufCapabilities {
        format: VideoDmaBufFormat::Nv12,
        modifier: 0,
        max_size: [256, 128],
        max_allocation_size: 49152,
    };
    assert!(capabilities.supports(&layout));
    for changed in [
        VideoDmaBufCapabilities {
            modifier: INTEL_Y_TILED,
            ..capabilities
        },
        VideoDmaBufCapabilities {
            max_size: [254, 128],
            ..capabilities
        },
        VideoDmaBufCapabilities {
            max_size: [256, 126],
            ..capabilities
        },
        VideoDmaBufCapabilities {
            max_allocation_size: 49151,
            ..capabilities
        },
    ] {
        assert!(!changed.supports(&layout));
    }
}

#[test]
fn p010_capabilities_require_exact_format_modifier_extent_and_bytes() {
    let layout = VideoDmaBufLayout::new(
        VideoDmaBufFormat::P010,
        [256, 128],
        [248, 120],
        0,
        [512; 2],
        [0, 65536],
        98304,
    )
    .unwrap();
    let capabilities = VideoDmaBufCapabilities {
        format: VideoDmaBufFormat::P010,
        modifier: 0,
        max_size: [256, 128],
        max_allocation_size: 98304,
    };
    assert!(capabilities.supports(&layout));
    for changed in [
        VideoDmaBufCapabilities {
            format: VideoDmaBufFormat::Nv12,
            ..capabilities
        },
        VideoDmaBufCapabilities {
            modifier: INTEL_Y_TILED,
            ..capabilities
        },
        VideoDmaBufCapabilities {
            max_size: [254, 128],
            ..capabilities
        },
        VideoDmaBufCapabilities {
            max_size: [256, 126],
            ..capabilities
        },
        VideoDmaBufCapabilities {
            max_allocation_size: 98303,
            ..capabilities
        },
    ] {
        assert!(!changed.supports(&layout));
    }
}

fn fixture() -> (OwnedFd, VideoDmaBufLayout, [u64; 2], Vec<u8>) {
    let number = |name| {
        std::env::var(name)
            .expect("Run through ExportVAAPIFrame")
            .parse::<u64>()
            .unwrap()
    };
    let fd = unsafe { BorrowedFd::borrow_raw(number("WR_NV12_FD") as i32) };
    let layout = VideoDmaBufLayout::new(
        VideoDmaBufFormat::Nv12,
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

fn p010_fixture() -> (OwnedFd, VideoDmaBufLayout, [u64; 2], Vec<u8>) {
    let number = |name| {
        std::env::var(name)
            .expect("Run through the P010 ExportVAAPIFrame fixture")
            .parse::<u64>()
            .unwrap()
    };
    assert_eq!(std::env::var("WR_VIDEO_FORMAT").as_deref(), Ok("P010"));
    let fd = unsafe { BorrowedFd::borrow_raw(number("WR_P010_FD") as i32) };
    let layout = VideoDmaBufLayout::new(
        VideoDmaBufFormat::P010,
        [
            number("WR_P010_ALLOC_WIDTH") as u32,
            number("WR_P010_ALLOC_HEIGHT") as u32,
        ],
        [
            number("WR_P010_WIDTH") as u32,
            number("WR_P010_HEIGHT") as u32,
        ],
        number("WR_P010_MODIFIER"),
        [number("WR_P010_Y_PITCH"), number("WR_P010_UV_PITCH")],
        [number("WR_P010_Y_OFFSET"), number("WR_P010_UV_OFFSET")],
        number("WR_P010_BYTES"),
    )
    .unwrap();
    let bytes = std::fs::read(std::env::var("WR_P010_REFERENCE").unwrap()).unwrap();
    (
        fd.try_clone_to_owned().unwrap(),
        layout,
        [number("WR_P010_DRM_MAJOR"), number("WR_P010_DRM_MINOR")],
        bytes,
    )
}

fn crop_p010_reference(
    reference: &[u8],
    original: [u32; 2],
    visible: [u32; 2],
) -> Vec<u8> {
    let mut cropped = Vec::new();
    let y_bytes = (original[0] * original[1] * 2) as usize;
    for channel in 0..2 {
        let plane = if channel == 0 { 0 } else { y_bytes };
        let rows = (visible[1] >> channel) as usize;
        let source_stride = (original[0] * 2) as usize;
        let row_bytes = (visible[0] * 2) as usize;
        for row in 0..rows {
            let start = plane + row * source_stride;
            cropped.extend_from_slice(&reference[start..start + row_bytes]);
        }
    }
    cropped
}

fn p010_word(reference: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([reference[offset], reference[offset + 1]])
}

fn expected_p010_pixel(
    reference: &[u8],
    size: [u32; 2],
    point: [u32; 2],
    color_space: YuvColorSpace,
    range: ColorRange,
) -> [u8; 4] {
    let [width, height] = size;
    let y_offset = ((point[1] * width + point[0]) * 2) as usize;
    let uv_offset = (width * height * 2
        + ((point[1] / 2) * (width / 2) + point[0] / 2) * 4)
        as usize;
    let code = |offset| {
        let word = p010_word(reference, offset);
        assert_eq!(word & 0x3f, 0);
        (word >> 6) as f32
    };
    let (y, cb, cr) = match range {
        ColorRange::Limited => (
            (code(y_offset) - 64.0) / 876.0,
            (code(uv_offset) - 512.0) / 896.0,
            (code(uv_offset + 2) - 512.0) / 896.0,
        ),
        ColorRange::Full => (
            code(y_offset) / 1023.0,
            (code(uv_offset) - 512.0) / 1023.0,
            (code(uv_offset + 2) - 512.0) / 1023.0,
        ),
    };
    let (kr, kb) = match color_space {
        YuvColorSpace::Rec601 => (0.299, 0.114),
        YuvColorSpace::Rec709 => (0.2126, 0.0722),
        _ => unreachable!(),
    };
    let kg = 1.0 - kr - kb;
    let r = y + 2.0 * (1.0 - kr) * cr;
    let g = y
        - 2.0 * kb * (1.0 - kb) / kg * cb
        - 2.0 * kr * (1.0 - kr) / kg * cr;
    let b = y + 2.0 * (1.0 - kb) * cb;
    let convert = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    [convert(r), convert(g), convert(b), 255]
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
    image: WeakForeignYuvImage,
    size: [u32; 2],
}
impl ExternalImageProvider for Provider {
    fn acquire(&mut self, id: ExternalImageId, channel: u8, _: bool) -> Result<ExternalImageLease> {
        assert_eq!(id.0, 77);
        self.image
            .upgrade()
            .ok_or("YUV publication released too early")?
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
    let asynchronous = std::env::var("WR_VIDEO_FORCE_SYNC").as_deref() != Ok("1");
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
        .vaapi_video_capabilities(VideoDmaBufFormat::Nv12)
        .unwrap()
        .iter()
        .any(|caps| caps.supports(&layout)));
    let released = Rc::new(RefCell::new(Vec::new()));
    let log = released.clone();
    let image = unsafe {
        device.import_vaapi_video(fd.as_fd(), layout, node, 1, move |status| {
            log.borrow_mut().push(status)
        })
    }
    .unwrap();
    let acquire = device.submitted();
    assert!(acquire > 0);
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
    let release = device.submitted();
    assert!(release > acquire);
    assert!(weak.upgrade().is_none());
    if asynchronous {
        assert!(released.borrow().is_empty());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            device.poll().unwrap();
            if !released.borrow().is_empty() {
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    assert_eq!(*released.borrow(), [ExternalImageRelease::Complete]);
    api.shut_down(true);
}

#[test]
#[ignore = "Requires a real P010 ExportVAAPIFrame and native Vulkan validation"]
fn vaapi_p010_plane_bytes_sampling_and_shared_release() {
    check_p010_sampling(0);
    check_p010_sampling(8);
}

fn check_p010_sampling(crop: u32) {
    let asynchronous = std::env::var("WR_VIDEO_FORCE_SYNC").as_deref() != Ok("1");
    let (fd, mut layout, node, original_reference) = p010_fixture();
    let reference = if crop == 0 {
        original_reference
    } else {
        let original = layout.visible;
        assert!(original.iter().all(|&size| size > crop));
        layout.visible = [original[0] - crop, original[1] - crop];
        crop_p010_reference(&original_reference, original, layout.visible)
    };
    let (width, height) = (layout.visible[0], layout.visible[1]);
    assert_eq!(reference.len(), (width * height * 3) as usize);
    assert!(reference
        .chunks_exact(2)
        .all(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]) & 0x3f == 0));
    assert!(reference
        .chunks_exact(2)
        .any(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]) >> 6 != 0));

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
    let p010_capabilities = device
        .vaapi_video_capabilities(VideoDmaBufFormat::P010)
        .unwrap();
    assert!(!p010_capabilities.is_empty());
    assert!(p010_capabilities
        .iter()
        .all(|capabilities| capabilities.format == VideoDmaBufFormat::P010));
    assert!(p010_capabilities
        .iter()
        .any(|capabilities| capabilities.supports(&layout)));
    assert!(device
        .vaapi_video_capabilities(VideoDmaBufFormat::Nv12)
        .unwrap()
        .iter()
        .all(|capabilities| {
            capabilities.format == VideoDmaBufFormat::Nv12
                && !capabilities.supports(&layout)
        }));

    let released = Rc::new(RefCell::new(Vec::new()));
    let log = released.clone();
    let image = unsafe {
        device.import_vaapi_video(fd.as_fd(), layout, node, 21, move |status| {
            log.borrow_mut().push(status)
        })
    }
    .unwrap();
    let acquire = device.submitted();
    assert!(acquire > 0);
    let owner = &device.dmabuf_producer().unwrap().owner;
    for channel in 0..2 {
        let captured = read_plane(&device, &image.0.planes[channel]);
        let expected_plane = if channel == 0 {
            0
        } else {
            (width * height * 2) as usize
        };
        let rows = (height >> channel) as usize;
        let actual_stride = (layout.allocation[0] * 2) as usize;
        let row_bytes = (width * 2) as usize;
        for row in 0..rows {
            let actual = row * actual_stride;
            let expected = expected_plane + row * row_bytes;
            assert_eq!(
                &captured[actual..actual + row_bytes],
                &reference[expected..expected + row_bytes]
            );
        }
    }
    let y = image.0.planes[0].texture(owner).unwrap();
    let uv = image.0.planes[1].texture(owner).unwrap();
    assert!(Rc::ptr_eq(&y.raw, &uv.raw));
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
    let frame_width = width * 2 + 2;
    let document = api.add_document(DeviceIntSize::new(frame_width as i32, height as i32));
    let pipeline = PipelineId(0, 0);
    let native_keys = [api.generate_image_key(), api.generate_image_key()];
    let control_keys = [api.generate_image_key(), api.generate_image_key()];
    let mut iteration = 0;
    for color_space in [YuvColorSpace::Rec601, YuvColorSpace::Rec709] {
        for range in [ColorRange::Limited, ColorRange::Full] {
            for (filter, scale) in [
                (ImageRendering::Pixelated, 1.0),
                (ImageRendering::Auto, 1.0),
                (ImageRendering::Auto, 0.5),
            ] {
                let mut transaction = Transaction::new();
                if iteration == 0 {
                    for channel in 0..2 {
                        transaction.add_image(
                            native_keys[channel],
                            layout.descriptor(channel),
                            ImageData::External(ExternalImageData {
                                id: ExternalImageId(77),
                                channel_index: channel as u8,
                                image_type: ExternalImageType::TextureHandle(
                                    ImageBufferKind::Texture2D,
                                ),
                                normalized_uvs: false,
                            }),
                            None,
                        );
                        let range = if channel == 0 {
                            0..(width * height * 2) as usize
                        } else {
                            (width * height * 2) as usize..reference.len()
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
                        frame_width as f32,
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
                    builder.push_yuv_image(
                        &info,
                        LayoutRect::from_origin_and_size(
                            LayoutPoint::new(x as f32, 0.0),
                            LayoutSize::new(width as f32 * scale, height as f32 * scale),
                        ),
                        YuvData::P010(keys[0], keys[1]),
                        ColorDepth::Color10,
                        color_space,
                        range,
                        filter,
                    );
                }
                builder.pop_stacking_context();
                transaction.set_root_pipeline(pipeline);
                transaction.set_display_list(
                    Epoch(iteration),
                    api.get_namespace_id(),
                    builder.end(),
                );
                transaction.generate_frame(iteration as u64, true, false, RenderReasons::TESTING);
                api.send_transaction(document, transaction);
                renderer.prepare_frame(document).unwrap();
                renderer.render().unwrap();
                let pixels = renderer
                    .read_pixels_rgba8(FramebufferIntRect::from_size(
                        FramebufferIntSize::new(frame_width as i32, height as i32),
                    ))
                    .unwrap();
                for row in pixels.chunks_exact(frame_width as usize * 4) {
                    assert_eq!(
                        &row[..width as usize * 4],
                        &row[(width + 2) as usize * 4..],
                        "Direct P010 sampling differs from uploaded control"
                    );
                }
                if filter == ImageRendering::Pixelated {
                    let point = [width / 4, height / 4];
                    let framebuffer_y = height - 1 - point[1];
                    let offset = ((framebuffer_y * frame_width + point[0]) * 4) as usize;
                    let expected =
                        expected_p010_pixel(&reference, [width, height], point, color_space, range);
                    for (actual, expected) in pixels[offset..offset + 4].iter().zip(expected) {
                        assert!(
                            actual.abs_diff(expected) <= 2,
                            "P010 {:?}/{:?} pixel {:?} differs from independent {:?}",
                            color_space,
                            range,
                            &pixels[offset..offset + 4],
                            expected_p010_pixel(
                                &reference,
                                [width, height],
                                point,
                                color_space,
                                range,
                            )
                        );
                    }
                }
                renderer.poll().unwrap();
                assert!(released.borrow().is_empty());
                iteration += 1;
            }
        }
    }
    drop(hold_y);
    assert!(released.borrow().is_empty());
    drop(hold_uv);
    let release = device.submitted();
    assert!(release > acquire);
    assert!(weak.upgrade().is_none());
    if asynchronous {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while released.borrow().is_empty() {
            device.poll().unwrap();
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    assert_eq!(*released.borrow(), [ExternalImageRelease::Complete]);
    api.shut_down(true);
}

#[test]
#[ignore = "Requires ExportVAAPIFrame and native Vulkan validation"]
fn vaapi_nv12_acquire_and_release_complete_asynchronously() {
    assert_ne!(std::env::var("WR_VIDEO_FORCE_SYNC").as_deref(), Ok("1"));
    set_completion_gate(false);
    let (fd, layout, node, _) = fixture();
    let device = gated_device();
    let released = Rc::new(RefCell::new(Vec::new()));
    let result = released.clone();
    let image = unsafe {
        device.import_vaapi_video(fd.as_fd(), layout, node, 9, move |status| {
            result.borrow_mut().push(status)
        })
    }
    .unwrap();
    let acquire = device.submitted();
    wait_for_hardware(&device);
    assert!(!device.poll_complete(acquire).unwrap());
    let y = image
        .lease(0, TexelRect::new(0.0, 0.0, layout.visible[0] as f32, layout.visible[1] as f32))
        .unwrap();
    let uv = image
        .lease(1, TexelRect::new(0.0, 0.0, (layout.visible[0] / 2) as f32, (layout.visible[1] / 2) as f32))
        .unwrap();
    drop(image);
    drop(y);
    assert!(released.borrow().is_empty());
    drop(uv);
    let release = device.submitted();
    wait_for_hardware(&device);
    assert!(!device.poll_complete(release).unwrap());
    assert!(released.borrow().is_empty());
    set_completion_gate(true);
    assert!(device.poll_complete(release).unwrap());
    assert_eq!(*released.borrow(), [ExternalImageRelease::Unused]);
    assert!(device.poll_complete(release).unwrap());
    assert_eq!(released.borrow().len(), 1);
    set_completion_gate(false);
}

#[test]
#[ignore = "Requires ExportVAAPIFrame and native Vulkan validation"]
fn vaapi_nv12_shutdown_drains_ownership_return() {
    assert_ne!(std::env::var("WR_VIDEO_FORCE_SYNC").as_deref(), Ok("1"));
    set_completion_gate(false);
    let (fd, layout, node, _) = fixture();
    let device = gated_device();
    let released = Rc::new(RefCell::new(Vec::new()));
    let result = released.clone();
    let image = unsafe {
        device.import_vaapi_video(fd.as_fd(), layout, node, 10, move |status| {
            result.borrow_mut().push(status)
        })
    }
    .unwrap();
    drop(image);
    assert!(released.borrow().is_empty());
    drop(device);
    assert_eq!(*released.borrow(), [ExternalImageRelease::Unused]);
}

#[test]
#[ignore = "Requires ExportVAAPIFrame and native Vulkan validation"]
fn vaapi_nv12_in_flight_ownership_is_bounded() {
    assert_ne!(std::env::var("WR_VIDEO_FORCE_SYNC").as_deref(), Ok("1"));
    set_completion_gate(false);
    let (fd, layout, node, _) = fixture();
    let device = gated_device();
    let descriptor = ImageDescriptor::new(4, 4, ImageFormat::RGBA8, ImageDescriptorFlags::empty());
    let owned = (0..4)
        .map(|value| device.create_image(descriptor, &[value; 64]).unwrap())
        .collect::<Vec<_>>();
    let released = Rc::new(RefCell::new(Vec::new()));
    let result = released.clone();
    let image = unsafe {
        device.import_vaapi_video(fd.as_fd(), layout, node, 11, move |status| {
            result.borrow_mut().push(status)
        })
    }
    .unwrap();
    let producer = device.dmabuf_producer().unwrap();
    for (index, owned) in owned.iter().enumerate() {
        let texture = owned.texture(&producer.owner).unwrap();
        texture
            .upload_recorded(
                &producer.owner,
                &producer.submissions,
                DeviceIntRect::from_size(DeviceIntSize::new(4, 4)),
                &[index as u8; 64],
                None,
                0,
                None,
            )
            .unwrap();
        producer.submissions.submit().unwrap();
        let mut stats = MemoryStats::default();
        producer.submissions.memory(&mut stats);
        assert!(stats.in_flight <= 3);
        assert!(released.borrow().is_empty());
    }
    drop(image);
    assert!(released.borrow().is_empty());
    set_completion_gate(true);
    device.finish().unwrap();
    assert_eq!(*released.borrow(), [ExternalImageRelease::Unused]);
    let mut stats = MemoryStats::default();
    producer.submissions.memory(&mut stats);
    assert_eq!(stats.in_flight, 0);
    set_completion_gate(false);
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
        device.import_vaapi_video(fd.as_fd(), layout, node, 1, move |s| result.set(Some(s)))
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
        device.import_vaapi_video(fd.as_fd(), layout, node, 1, move |s| result.set(Some(s)))
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
        device.import_vaapi_video(fd.as_fd(), layout, node, 1, move |s| result.set(Some(s)))
    }
    .is_err());
    assert_eq!(status.get(), Some(ExternalImageRelease::Unused));
    assert_eq!(owner.memory.get().textures, baseline.textures);
    assert_eq!(owner.memory.get().texture_bytes, baseline.texture_bytes);
    assert!(!owner.lost.get());
}

#[test]
#[ignore = "Requires a fresh ExportVAAPIFrame allocation and native Vulkan validation"]
fn nv12_ownership_return_failure_abandons() {
    assert_ne!(std::env::var("WR_VIDEO_FORCE_SYNC").as_deref(), Ok("1"));
    let (fd, layout, node, _) = fixture();
    let device = ExternalImageDevice::new(&Rc::new(
        create_vulkan_device(&Options {
            validation: true,
            ..Default::default()
        })
        .unwrap(),
    ));
    let released = Rc::new(RefCell::new(Vec::new()));
    let result = released.clone();
    let image = unsafe {
        device.import_vaapi_video(fd.as_fd(), layout, node, 12, move |status| {
            result.borrow_mut().push(status)
        })
    }
    .unwrap();
    device
        .dmabuf_producer()
        .unwrap()
        .owner
        .fault
        .set(Some(FailurePoint::Submit));
    drop(image);
    assert!(released.borrow().is_empty());
    assert!(device.poll().is_err());
    assert_eq!(*released.borrow(), [ExternalImageRelease::Abandoned]);
    assert!(device.poll().is_err());
    assert_eq!(released.borrow().len(), 1);
}
