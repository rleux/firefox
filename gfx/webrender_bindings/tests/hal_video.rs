/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

const FOURCC_NV12: u32 = u32::from_le_bytes(*b"NV12");
const FOURCC_P010: u32 = u32::from_le_bytes(*b"P010");

extern "C" {
    fn wr_vulkan_query_video(major: u64, minor: u64, output: &mut WrHalVideoCapabilities) -> bool;
}

thread_local! {
    static REGISTERED_VIDEO: RefCell<Option<WrHalVideoCapabilities>> = RefCell::new(None);
}

#[no_mangle]
unsafe extern "C" fn wr_vulkan_register_dmabuf_device(
    device: *const u8,
    driver: *const u8,
    _: *const u64,
    _: usize,
    _: *const u64,
    _: usize,
    video: &WrHalVideoCapabilities,
) -> *mut c_void {
    assert_eq!(std::slice::from_raw_parts(device, 16), video.device_uuid);
    assert_eq!(std::slice::from_raw_parts(driver, 16), video.driver_uuid);
    REGISTERED_VIDEO.with(|slot| {
        assert!(slot.borrow_mut().replace(*video).is_none());
    });
    Box::into_raw(Box::new(())) as *mut c_void
}

#[no_mangle]
unsafe extern "C" fn wr_vulkan_unregister_dmabuf_device(registration: *mut c_void) {
    drop(Box::from_raw(registration as *mut ()));
    REGISTERED_VIDEO.with(|slot| assert!(slot.borrow_mut().take().is_some()));
}

#[test]
#[ignore = "Requires ExportVAAPIFrame and native Vulkan validation"]
fn vaapi_nv12_live_device_registration_carries_capabilities() {
    let (_, data) = fixture();
    let live_device = device();
    let registration = DeviceRegistration::new(&live_device).unwrap();
    REGISTERED_VIDEO.with(|slot| {
        let caps = slot.borrow();
        let caps = caps.as_ref().unwrap();
        assert_eq!(caps.drm_node, data.drm_node);
        assert!(caps.formats[..caps.format_count].iter().any(|format| {
            format.fourcc == data.fourcc
                && format.modifier == data.modifier
                && format.max_width >= data.allocation_width
                && format.max_height >= data.allocation_height
                && format.max_allocation_size >= data.allocation_size
        }));
    });
    drop(registration);
    REGISTERED_VIDEO.with(|slot| assert!(slot.borrow().is_none()));
}

#[test]
#[ignore = "Requires ExportVAAPIFrame and native Vulkan validation"]
fn vaapi_nv12_capability_probe_matches_decoder_and_clears_failed_query() {
    let (_, data) = fixture();
    let mut capabilities = WrHalVideoCapabilities::default();
    assert!(unsafe { wr_vulkan_query_video(data.drm_node[0], data.drm_node[1], &mut capabilities) });
    assert!((1..=4).contains(&capabilities.format_count));
    assert_eq!(capabilities.drm_node, data.drm_node);
    let formats = &capabilities.formats[..capabilities.format_count];
    assert!(formats.iter().any(|format| {
        format.fourcc == data.fourcc
            && format.modifier == data.modifier
            && format.max_width >= data.allocation_width
            && format.max_height >= data.allocation_height
            && format.max_allocation_size >= data.allocation_size
    }));
    let identity = device().dmabuf_capabilities().unwrap();
    assert_eq!(capabilities.device_uuid, identity.device_uuid());
    assert_eq!(capabilities.driver_uuid, identity.driver_uuid());
    assert!(!unsafe { wr_vulkan_query_video(u64::MAX, u64::MAX, &mut capabilities) });
    assert_eq!(capabilities.format_count, 0);
    assert_eq!(capabilities.drm_node, [0; 2]);
    assert_eq!(capabilities.device_uuid, [0; 16]);
    assert_eq!(capabilities.driver_uuid, [0; 16]);
    for format in capabilities.formats {
        assert_eq!(
            (
                format.fourcc,
                format.modifier,
                format.max_width,
                format.max_height,
                format.max_allocation_size
            ),
            (0, 0, 0, 0, 0)
        );
    }
}

#[test]
#[ignore = "Requires P010 ExportVAAPIFrame and native Vulkan validation"]
fn vaapi_p010_capability_probe_and_registration_match_exact_format() {
    let (_, data) = p010_fixture();
    let live_device = device();
    let registration = DeviceRegistration::new(&live_device).unwrap();
    REGISTERED_VIDEO.with(|slot| {
        let caps = slot.borrow();
        let caps = caps.as_ref().unwrap();
        assert!(caps.formats[..caps.format_count].iter().any(|format| {
            format.fourcc == FOURCC_P010
                && format.modifier == data.modifier
                && format.max_width >= data.allocation_width
                && format.max_height >= data.allocation_height
                && format.max_allocation_size >= data.allocation_size
        }));
    });
    drop(registration);
    let mut capabilities = WrHalVideoCapabilities::default();
    assert!(unsafe { wr_vulkan_query_video(data.drm_node[0], data.drm_node[1], &mut capabilities) });
    let formats = &capabilities.formats[..capabilities.format_count];
    assert!(formats.iter().any(|format| {
        format.fourcc == FOURCC_P010
            && format.modifier == data.modifier
            && format.max_width >= data.allocation_width
            && format.max_height >= data.allocation_height
            && format.max_allocation_size >= data.allocation_size
    }));
    assert!(formats.iter().any(|format| format.fourcc == FOURCC_NV12));
}

fn fixture() -> (Rc<Fixture>, WrHalVideo) {
    video_fixture("WR_NV12", FOURCC_NV12)
}

fn p010_fixture() -> (Rc<Fixture>, WrHalVideo) {
    assert_eq!(std::env::var("WR_VIDEO_FORMAT").as_deref(), Ok("P010"));
    video_fixture("WR_P010", FOURCC_P010)
}

fn expected_p010_bt709_limited(reference: &[u8], width: u32, point: [u32; 2]) -> [u8; 4] {
    let height = (reference.len() as u32 / 3) / width;
    let word = |offset| {
        let value = u16::from_le_bytes([reference[offset], reference[offset + 1]]);
        assert_eq!(value & 0x3f, 0);
        (value >> 6) as f32
    };
    let y_offset = ((point[1] * width + point[0]) * 2) as usize;
    let uv_offset = (width * height * 2 + ((point[1] / 2) * (width / 2) + point[0] / 2) * 4) as usize;
    let y = (word(y_offset) - 64.0) / 876.0;
    let cb = (word(uv_offset) - 512.0) / 896.0;
    let cr = (word(uv_offset + 2) - 512.0) / 896.0;
    let kr = 0.2126;
    let kb = 0.0722;
    let kg = 1.0 - kr - kb;
    let rgb = [
        y + 2.0 * (1.0 - kr) * cr,
        y - 2.0 * kb * (1.0 - kb) / kg * cb - 2.0 * kr * (1.0 - kr) / kg * cr,
        y + 2.0 * (1.0 - kb) * cb,
    ];
    let convert = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    [convert(rgb[0]), convert(rgb[1]), convert(rgb[2]), 255]
}

fn video_fixture(prefix: &str, fourcc: u32) -> (Rc<Fixture>, WrHalVideo) {
    init_log();
    let number = |suffix| {
        std::env::var(format!("{prefix}_{suffix}"))
            .expect("Run through ExportVAAPIFrame")
            .parse::<u64>()
            .unwrap()
    };
    let data = WrHalVideo {
        fd: number("FD") as i32,
        access_lock_fd: number("ACCESS_LOCK_FD") as i32,
        fourcc,
        width: number("WIDTH") as u32,
        height: number("HEIGHT") as u32,
        allocation_width: number("ALLOC_WIDTH") as u32,
        allocation_height: number("ALLOC_HEIGHT") as u32,
        allocation_size: number("BYTES"),
        modifier: number("MODIFIER"),
        strides: [number("Y_PITCH"), number("UV_PITCH")],
        offsets: [number("Y_OFFSET"), number("UV_OFFSET")],
        allocation_id: 45,
        producer_epoch: 3,
        drm_node: [number("DRM_MAJOR"), number("DRM_MINOR")],
    };
    (
        Rc::new(Fixture {
            image: RefCell::new(None),
            export: RefCell::new(None),
            releases: Default::default(),
            pixels: Vec::new(),
            video_access: Default::default(),
        }),
        data,
    )
}

fn device() -> hal::ExternalImageDevice {
    hal::create_vulkan_image_device(&hal::Options {
        validation: true,
        ..Default::default()
    })
    .unwrap()
}

fn acquire(
    fixture: &Rc<Fixture>,
    provider: &mut ExternalImages,
    data: WrHalVideo,
    generation: u64,
    channel: u8,
) -> Result<hal::ExternalImageLease, String> {
    *fixture.image.borrow_mut() = Some(WrHalImage {
        generation,
        source: WrHalImageSource::Video(data),
    });
    provider.acquire(ExternalImageId(77), channel, false)
}

#[test]
#[ignore = "Requires ExportVAAPIFrame and native Vulkan validation"]
fn vaapi_nv12_bridge_channels_share_lock_and_retire_cache() {
    let synchronous = std::env::var("WR_VIDEO_FORCE_SYNC").as_deref() == Ok("1");
    let (fixture, data) = fixture();
    let device = device();
    let mut provider = provider(&fixture, device.clone());
    let uv = acquire(&fixture, &mut provider, data, 7, 1).unwrap();
    let y = acquire(&fixture, &mut provider, data, 7, 0).unwrap();
    let duplicate = acquire(&fixture, &mut provider, data, 7, 1).unwrap();
    assert_eq!(uv.descriptor().format, ImageFormat::RG8);
    assert_eq!(
        uv.descriptor().size,
        DeviceIntSize::new((data.width / 2) as i32, (data.height / 2) as i32)
    );
    assert_eq!(y.descriptor().format, ImageFormat::R8);
    assert_eq!(
        y.descriptor().size,
        DeviceIntSize::new(data.width as i32, data.height as i32)
    );
    assert_eq!(fixture.video_access.borrow().locks, 1);
    drop((y, duplicate));
    assert!(fixture.video_access.borrow().locked);
    assert_eq!(fixture.video_access.borrow().unlocks, 0);
    drop(uv);
    assert_eq!(fixture.video_access.borrow().locked, !synchronous);
    device.finish().unwrap();
    assert!(!fixture.video_access.borrow().locked);
    assert_eq!(fixture.video_access.borrow().unlocks, 1);
    assert_eq!(Rc::strong_count(&fixture), 1);
    drop(acquire(&fixture, &mut provider, data, 8, 0).unwrap());
    device.finish().unwrap();
    assert_eq!(fixture.video_access.borrow().locks, 2);
    assert_eq!(fixture.video_access.borrow().unlocks, 2);
    assert!(!fixture.video_access.borrow().poisoned);
}

#[test]
#[ignore = "Requires P010 ExportVAAPIFrame and native Vulkan validation"]
fn vaapi_p010_bridge_channels_share_lock_and_require_exact_format() {
    let (fixture, data) = p010_fixture();
    let device = device();
    let mut provider = provider(&fixture, device.clone());
    let uv = acquire(&fixture, &mut provider, data, 7, 1).unwrap();
    let y = acquire(&fixture, &mut provider, data, 7, 0).unwrap();
    assert_eq!(uv.descriptor().format, ImageFormat::RG16);
    assert_eq!(
        uv.descriptor().size,
        DeviceIntSize::new((data.width / 2) as i32, (data.height / 2) as i32)
    );
    assert_eq!(y.descriptor().format, ImageFormat::R16);
    assert_eq!(
        y.descriptor().size,
        DeviceIntSize::new(data.width as i32, data.height as i32)
    );
    assert_eq!(fixture.video_access.borrow().locks, 1);
    let mut changed = data;
    changed.fourcc = FOURCC_NV12;
    assert!(acquire(&fixture, &mut provider, changed, 7, 0).is_err());
    drop(y);
    assert!(fixture.video_access.borrow().locked);
    drop(uv);
    device.finish().unwrap();
    assert!(!fixture.video_access.borrow().locked);
    assert_eq!(fixture.video_access.borrow().unlocks, 1);
}

#[test]
#[ignore = "Requires ExportVAAPIFrame and native Vulkan validation"]
fn vaapi_nv12_bridge_rejects_live_identity_and_device_changes() {
    let synchronous = std::env::var("WR_VIDEO_FORCE_SYNC").as_deref() == Ok("1");
    let (fixture, data) = fixture();
    let first = device();
    let mut provider = provider(&fixture, first.clone());
    let keep = acquire(&fixture, &mut provider, data, 7, 1).unwrap();
    assert!(acquire(&fixture, &mut provider, data, 8, 0).is_err());
    let different_lock = std::fs::File::open("/dev/null").unwrap();
    for change in 0..5 {
        let mut changed = data;
        match change {
            0 => changed.allocation_id += 1,
            1 => changed.producer_epoch += 1,
            2 => changed.width -= 2,
            3 => changed.drm_node[1] += 1,
            _ => changed.access_lock_fd = different_lock.as_raw_fd(),
        }
        assert!(acquire(&fixture, &mut provider, changed, 7, 0).is_err());
    }
    let recreated_device = device();
    let mut recreated = super::provider(&fixture, recreated_device.clone());
    assert!(acquire(&fixture, &mut recreated, data, 7, 0).is_err());
    assert_eq!(fixture.video_access.borrow().locks, 1);
    assert!(fixture.video_access.borrow().locked);
    drop(keep);
    assert_eq!(fixture.video_access.borrow().locked, !synchronous);
    let resumed = acquire(&fixture, &mut recreated, data, 7, 0).unwrap();
    assert_eq!(fixture.video_access.borrow().unlocks, 1);
    assert_eq!(fixture.video_access.borrow().locks, 2);
    drop(resumed);
    recreated_device.finish().unwrap();
    assert_eq!(fixture.video_access.borrow().locks, 2);
    assert_eq!(fixture.video_access.borrow().unlocks, 2);
}

#[test]
#[ignore = "Requires ExportVAAPIFrame and native Vulkan validation"]
fn vaapi_nv12_bridge_busy_lock_is_rejected_without_poisoning() {
    let (fixture, data) = fixture();
    let device = device();
    let mut provider = provider(&fixture, device.clone());
    fixture.video_access.borrow_mut().locked = true;
    assert!(acquire(&fixture, &mut provider, data, 7, 0).is_err());
    assert_eq!(fixture.video_access.borrow().locks, 0);
    assert!(fixture.video_access.borrow().locked);
    assert!(!fixture.video_access.borrow().poisoned);
    fixture.video_access.borrow_mut().locked = false;
    assert!(acquire(&fixture, &mut provider, data, 7, 2).is_err());
    drop(acquire(&fixture, &mut provider, data, 7, 1).unwrap());
    device.finish().unwrap();
    assert_eq!(fixture.video_access.borrow().locks, 1);
    assert_eq!(fixture.video_access.borrow().unlocks, 1);
}

struct VideoProvider {
    fixture: Box<Rc<Fixture>>,
    data: WrHalVideo,
    images: ExternalImages,
}

fn submit_video_frame(
    renderer: &mut hal::Renderer,
    api: &mut webrender::render_api::RenderApi,
    fixture: &Rc<Fixture>,
    data: WrHalVideo,
    epoch: u32,
) -> hal::FrameCompletion {
    let p010 = data.fourcc == FOURCC_P010;
    let stable = Box::new(fixture.clone());
    let images = provider(&stable, renderer.external_image_device());
    renderer
        .set_external_image_provider(Box::new(VideoProvider {
            fixture: stable,
            data,
            images,
        }))
        .unwrap();
    let document = api.add_document(DeviceIntSize::new(data.width as i32, data.height as i32));
    let pipeline = PipelineId(0, 0);
    let keys = [api.generate_image_key(), api.generate_image_key()];
    let mut transaction = webrender::render_api::Transaction::new();
    for channel in 0..2 {
        transaction.add_image(
            keys[channel],
            ImageDescriptor::new(
                (data.width >> channel) as i32,
                (data.height >> channel) as i32,
                match (p010, channel) {
                    (false, 0) => ImageFormat::R8,
                    (false, _) => ImageFormat::RG8,
                    (true, 0) => ImageFormat::R16,
                    (true, _) => ImageFormat::RG16,
                },
                ImageDescriptorFlags::empty(),
            ),
            ImageData::External(ExternalImageData {
                id: ExternalImageId(77),
                channel_index: channel as u8,
                image_type: ExternalImageType::TextureHandle(ImageBufferKind::Texture2D),
                normalized_uvs: false,
            }),
            None,
        );
    }
    let mut builder = DisplayListBuilder::new(pipeline);
    builder.begin(60.0);
    let bounds = LayoutRect::from_size(LayoutSize::new(data.width as f32, data.height as f32));
    let info = CommonItemProperties {
        clip_rect: bounds,
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
    builder.push_yuv_image(
        &info,
        bounds,
        if p010 {
            YuvData::P010(keys[0], keys[1])
        } else {
            YuvData::NV12(keys[0], keys[1])
        },
        if p010 { ColorDepth::Color10 } else { ColorDepth::Color8 },
        if p010 {
            YuvColorSpace::Rec709
        } else {
            YuvColorSpace::Rec601
        },
        ColorRange::Limited,
        ImageRendering::Auto,
    );
    builder.pop_stacking_context();
    transaction.set_root_pipeline(pipeline);
    transaction.set_display_list(Epoch(epoch), api.get_namespace_id(), builder.end());
    transaction.generate_frame(epoch as u64, true, false, RenderReasons::TESTING);
    api.send_transaction(document, transaction);
    renderer.prepare_frame(document).unwrap();
    renderer.render().unwrap();
    renderer.submit_work().unwrap()
}

#[test]
#[ignore = "Requires ExportVAAPIFrame and native Vulkan validation"]
fn async_nv12_cross_consumer_progresses_previous_renderer() {
    assert_ne!(std::env::var("WR_VIDEO_FORCE_SYNC").as_deref(), Ok("1"));
    let (fixture, data) = fixture();
    let (mut first_renderer, first_sender) = renderer();
    let first_device = first_renderer.external_image_device();
    let first_poller = first_device.consumer_poller();
    let mut first_api = first_sender.create_api();
    let first_completion = submit_video_frame(&mut first_renderer, &mut first_api, &fixture, data, 1);
    assert_eq!(fixture.video_access.borrow().locks, 1);
    assert_eq!(fixture.video_access.borrow().unlocks, 0);

    let (mut second_renderer, second_sender) = renderer();
    let second_device = second_renderer.external_image_device();
    let mut second_provider = provider(&fixture, second_device.clone());
    let keep = acquire(&fixture, &mut second_provider, data, 7, 1).unwrap();
    assert!(first_renderer.poll_completion(first_completion).unwrap());
    assert_eq!(fixture.video_access.borrow().unlocks, 1);
    assert_eq!(fixture.video_access.borrow().locks, 2);
    assert!(fixture.video_access.borrow().locked);

    let mut second_api = second_sender.create_api();
    let second_completion = submit_video_frame(&mut second_renderer, &mut second_api, &fixture, data, 2);
    let second_pixels = second_renderer
        .read_pixels_rgba8(FramebufferIntRect::from_size(FramebufferIntSize::new(
            data.width as i32,
            data.height as i32,
        )))
        .unwrap();
    assert!(second_renderer.poll_completion(second_completion).unwrap());
    let first_pixels = first_renderer
        .read_pixels_rgba8(FramebufferIntRect::from_size(FramebufferIntSize::new(
            data.width as i32,
            data.height as i32,
        )))
        .unwrap();
    assert_eq!(second_pixels, first_pixels);
    assert!(second_pixels.chunks_exact(4).all(|pixel| pixel[3] == 255));

    drop(keep);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        second_renderer.poll().unwrap();
        second_device.poll().unwrap();
        if !fixture.video_access.borrow().locked {
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(!fixture.video_access.borrow().locked);
    assert_eq!(fixture.video_access.borrow().unlocks, 2);
    first_api.shut_down(true);
    second_api.shut_down(true);
    drop(first_device);
    drop(first_renderer);
    assert!(first_poller().is_err());
}

#[test]
#[ignore = "Requires P010 ExportVAAPIFrame and native Vulkan validation"]
fn vaapi_p010_bridge_render_matches_independent_reference() {
    let (fixture, data) = p010_fixture();
    let (mut renderer, sender) = renderer();
    let device = renderer.external_image_device();
    let mut api = sender.create_api();
    let completion = submit_video_frame(&mut renderer, &mut api, &fixture, data, 1);
    let pixels = renderer
        .read_pixels_rgba8(FramebufferIntRect::from_size(FramebufferIntSize::new(
            data.width as i32,
            data.height as i32,
        )))
        .unwrap();
    assert!(renderer.poll_completion(completion).unwrap());
    let reference = std::fs::read(std::env::var("WR_P010_REFERENCE").unwrap()).unwrap();
    assert_eq!(reference.len(), (data.width * data.height * 3) as usize);
    let point = [data.width / 4, data.height / 4];
    let framebuffer_y = data.height - 1 - point[1];
    let offset = ((framebuffer_y * data.width + point[0]) * 4) as usize;
    let expected = expected_p010_bt709_limited(&reference, data.width, point);
    for (actual, expected) in pixels[offset..offset + 4].iter().zip(expected) {
        assert!(
            actual.abs_diff(expected) <= 2,
            "{:?} != {:?}",
            &pixels[offset..offset + 4],
            expected
        );
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while fixture.video_access.borrow().locked {
        renderer.poll().unwrap();
        device.poll().unwrap();
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert_eq!(fixture.video_access.borrow().locks, 1);
    assert_eq!(fixture.video_access.borrow().unlocks, 1);
    api.shut_down(true);
}
impl hal::ExternalImageProvider for VideoProvider {
    fn acquire(&mut self, _: ExternalImageId, channel: u8, _: bool) -> Result<hal::ExternalImageLease, String> {
        acquire(&self.fixture, &mut self.images, self.data, 7, channel)
    }
}

#[test]
#[ignore = "Requires ExportVAAPIFrame and native Vulkan validation"]
fn vaapi_nv12_bridge_rendering_completes_one_frame_lease() {
    check_rendering_completion(true);
}

#[test]
#[ignore = "Requires ExportVAAPIFrame and native Vulkan validation"]
fn vaapi_nv12_frame_completion_releases_before_readback() {
    check_rendering_completion(false);
}

fn check_rendering_completion(retain_extra_lease: bool) {
    let (fixture, data) = fixture();
    let (mut renderer, sender) = renderer();
    let device = renderer.external_image_device();
    let mut first = provider(&fixture, device.clone());
    let keep = retain_extra_lease.then(|| acquire(&fixture, &mut first, data, 7, 1).unwrap());
    let stable = Box::new(fixture.clone());
    let images = provider(&stable, device.clone());
    renderer
        .set_external_image_provider(Box::new(VideoProvider {
            fixture: stable,
            data,
            images,
        }))
        .unwrap();
    let mut api = sender.create_api();
    let document = api.add_document(DeviceIntSize::new(data.width as i32, data.height as i32));
    let pipeline = PipelineId(0, 0);
    let keys = [api.generate_image_key(), api.generate_image_key()];
    let mut transaction = webrender::render_api::Transaction::new();
    for channel in 0..2 {
        transaction.add_image(
            keys[channel],
            ImageDescriptor::new(
                (data.width >> channel) as i32,
                (data.height >> channel) as i32,
                if channel == 0 {
                    ImageFormat::R8
                } else {
                    ImageFormat::RG8
                },
                ImageDescriptorFlags::empty(),
            ),
            ImageData::External(ExternalImageData {
                id: ExternalImageId(77),
                channel_index: channel as u8,
                image_type: ExternalImageType::TextureHandle(ImageBufferKind::Texture2D),
                normalized_uvs: false,
            }),
            None,
        );
    }
    let mut builder = DisplayListBuilder::new(pipeline);
    builder.begin(60.0);
    let bounds = LayoutRect::from_size(LayoutSize::new(data.width as f32, data.height as f32));
    let info = CommonItemProperties {
        clip_rect: bounds,
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
    builder.push_yuv_image(
        &info,
        bounds,
        YuvData::NV12(keys[0], keys[1]),
        ColorDepth::Color8,
        YuvColorSpace::Rec601,
        ColorRange::Limited,
        ImageRendering::Auto,
    );
    builder.pop_stacking_context();
    transaction.set_root_pipeline(pipeline);
    transaction.set_display_list(Epoch(1), api.get_namespace_id(), builder.end());
    transaction.generate_frame(1, true, false, RenderReasons::TESTING);
    api.send_transaction(document, transaction);
    renderer.prepare_frame(document).unwrap();
    renderer.render().unwrap();
    let completion = renderer.submit_work().unwrap();
    let synchronous = std::env::var("WR_VIDEO_FORCE_SYNC").as_deref() == Ok("1");
    if synchronous && !retain_extra_lease {
        finish_video_images(&device, || renderer.poll_completion(completion)).unwrap();
    } else if !synchronous {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            renderer.poll().unwrap();
            let draw_complete = renderer.poll_completion(completion).unwrap();
            device.poll().unwrap();
            if draw_complete && (retain_extra_lease || !fixture.video_access.borrow().locked) {
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    if !retain_extra_lease {
        assert!(!fixture.video_access.borrow().locked);
        assert_eq!(fixture.video_access.borrow().unlocks, 1);
    }
    let pixels = renderer
        .read_pixels_rgba8(FramebufferIntRect::from_size(FramebufferIntSize::new(
            data.width as i32,
            data.height as i32,
        )))
        .unwrap();
    assert_eq!(pixels.len(), data.width as usize * data.height as usize * 4);
    assert!(pixels.chunks_exact(4).all(|pixel| pixel[3] == 255));
    renderer.poll().unwrap();
    assert_eq!(fixture.video_access.borrow().locks, 1);
    if retain_extra_lease {
        assert!(fixture.video_access.borrow().locked);
        if synchronous {
            assert!(finish_video_images(&device, || Ok(true)).is_err());
        }
    }
    drop(keep);
    if synchronous {
        finish_video_images(&device, || panic!("No video publication needs polling")).unwrap();
    } else {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            renderer.poll().unwrap();
            device.poll().unwrap();
            if !fixture.video_access.borrow().locked {
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    assert_eq!(fixture.video_access.borrow().unlocks, 1);
    assert_eq!(
        fixture
            .releases
            .borrow()
            .iter()
            .filter(|status| matches!(status, WrHalImageRelease::Complete))
            .count(),
        1
    );
    api.shut_down(true);
}
