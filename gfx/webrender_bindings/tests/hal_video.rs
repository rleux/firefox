/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

extern "C" {
    fn wr_vulkan_query_nv12(major: u64, minor: u64, output: &mut WrHalNv12Capabilities) -> bool;
}

thread_local! {
    static REGISTERED_VIDEO: RefCell<Option<WrHalNv12Capabilities>> = RefCell::new(None);
}

#[no_mangle]
unsafe extern "C" fn wr_vulkan_register_dmabuf_device(
    device: *const u8,
    driver: *const u8,
    _: *const u64,
    _: usize,
    _: *const u64,
    _: usize,
    video: &WrHalNv12Capabilities,
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
            format.modifier == data.modifier
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
    let mut capabilities = WrHalNv12Capabilities::default();
    assert!(unsafe { wr_vulkan_query_nv12(data.drm_node[0], data.drm_node[1], &mut capabilities) });
    assert!((1..=2).contains(&capabilities.format_count));
    assert_eq!(capabilities.drm_node, data.drm_node);
    let formats = &capabilities.formats[..capabilities.format_count];
    assert!(formats.iter().any(|format| {
        format.modifier == data.modifier
            && format.max_width >= data.allocation_width
            && format.max_height >= data.allocation_height
            && format.max_allocation_size >= data.allocation_size
    }));
    let identity = device().dmabuf_capabilities().unwrap();
    assert_eq!(capabilities.device_uuid, identity.device_uuid());
    assert_eq!(capabilities.driver_uuid, identity.driver_uuid());
    assert!(!unsafe { wr_vulkan_query_nv12(u64::MAX, u64::MAX, &mut capabilities) });
    assert_eq!(capabilities.format_count, 0);
    assert_eq!(capabilities.drm_node, [0; 2]);
    assert_eq!(capabilities.device_uuid, [0; 16]);
    assert_eq!(capabilities.driver_uuid, [0; 16]);
    for format in capabilities.formats {
        assert_eq!(
            (
                format.modifier,
                format.max_width,
                format.max_height,
                format.max_allocation_size
            ),
            (0, 0, 0, 0)
        );
    }
}

fn fixture() -> (Rc<Fixture>, WrHalNv12) {
    init_log();
    let number = |name| {
        std::env::var(name)
            .expect("Run through ExportVAAPIFrame")
            .parse::<u64>()
            .unwrap()
    };
    let data = WrHalNv12 {
        fd: number("WR_NV12_FD") as i32,
        access_lock_fd: number("WR_NV12_ACCESS_LOCK_FD") as i32,
        width: number("WR_NV12_WIDTH") as u32,
        height: number("WR_NV12_HEIGHT") as u32,
        allocation_width: number("WR_NV12_ALLOC_WIDTH") as u32,
        allocation_height: number("WR_NV12_ALLOC_HEIGHT") as u32,
        allocation_size: number("WR_NV12_BYTES"),
        modifier: number("WR_NV12_MODIFIER"),
        strides: [number("WR_NV12_Y_PITCH"), number("WR_NV12_UV_PITCH")],
        offsets: [number("WR_NV12_Y_OFFSET"), number("WR_NV12_UV_OFFSET")],
        allocation_id: 45,
        producer_epoch: 3,
        drm_node: [number("WR_NV12_DRM_MAJOR"), number("WR_NV12_DRM_MINOR")],
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
    data: WrHalNv12,
    generation: u64,
    channel: u8,
) -> Result<hal::ExternalImageLease, String> {
    *fixture.image.borrow_mut() = Some(WrHalImage {
        generation,
        source: WrHalImageSource::Nv12(data),
    });
    provider.acquire(ExternalImageId(77), channel, false)
}

#[test]
#[ignore = "Requires ExportVAAPIFrame and native Vulkan validation"]
fn vaapi_nv12_bridge_channels_share_lock_and_retire_cache() {
    let (fixture, data) = fixture();
    let mut provider = provider(&fixture, device());
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
    assert!(!fixture.video_access.borrow().locked);
    assert_eq!(fixture.video_access.borrow().unlocks, 1);
    assert_eq!(Rc::strong_count(&fixture), 1);
    drop(acquire(&fixture, &mut provider, data, 8, 0).unwrap());
    assert_eq!(fixture.video_access.borrow().locks, 2);
    assert_eq!(fixture.video_access.borrow().unlocks, 2);
    assert!(!fixture.video_access.borrow().poisoned);
}

#[test]
#[ignore = "Requires ExportVAAPIFrame and native Vulkan validation"]
fn vaapi_nv12_bridge_rejects_live_identity_and_device_changes() {
    let (fixture, data) = fixture();
    let first = device();
    let mut provider = provider(&fixture, first);
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
    let mut recreated = super::provider(&fixture, device());
    assert!(acquire(&fixture, &mut recreated, data, 7, 0).is_err());
    assert_eq!(fixture.video_access.borrow().locks, 1);
    assert!(fixture.video_access.borrow().locked);
    drop(keep);
    assert_eq!(fixture.video_access.borrow().unlocks, 1);
    drop(acquire(&fixture, &mut recreated, data, 7, 0).unwrap());
    assert_eq!(fixture.video_access.borrow().locks, 2);
    assert_eq!(fixture.video_access.borrow().unlocks, 2);
}

#[test]
#[ignore = "Requires ExportVAAPIFrame and native Vulkan validation"]
fn vaapi_nv12_bridge_busy_lock_is_rejected_without_poisoning() {
    let (fixture, data) = fixture();
    let mut provider = provider(&fixture, device());
    fixture.video_access.borrow_mut().locked = true;
    assert!(acquire(&fixture, &mut provider, data, 7, 0).is_err());
    assert_eq!(fixture.video_access.borrow().locks, 0);
    assert!(fixture.video_access.borrow().locked);
    assert!(!fixture.video_access.borrow().poisoned);
    fixture.video_access.borrow_mut().locked = false;
    assert!(acquire(&fixture, &mut provider, data, 7, 2).is_err());
    drop(acquire(&fixture, &mut provider, data, 7, 1).unwrap());
    assert_eq!(fixture.video_access.borrow().locks, 1);
    assert_eq!(fixture.video_access.borrow().unlocks, 1);
}

struct VideoProvider {
    fixture: Box<Rc<Fixture>>,
    data: WrHalNv12,
    images: ExternalImages,
}
impl hal::ExternalImageProvider for VideoProvider {
    fn acquire(
        &mut self,
        _: ExternalImageId,
        channel: u8,
        _: bool,
    ) -> Result<hal::ExternalImageLease, String> {
        acquire(&self.fixture, &mut self.images, self.data, 7, channel)
    }
}

#[test]
#[ignore = "Requires ExportVAAPIFrame and native Vulkan validation"]
fn vaapi_nv12_bridge_rendering_completes_one_frame_lease() {
    let (fixture, data) = fixture();
    let (mut renderer, sender) = renderer();
    let device = renderer.external_image_device();
    let mut first = provider(&fixture, device.clone());
    let keep = acquire(&fixture, &mut first, data, 7, 1).unwrap();
    let stable = Box::new(fixture.clone());
    let images = provider(&stable, device);
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
    assert!(fixture.video_access.borrow().locked);
    drop(keep);
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
