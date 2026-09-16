/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#![cfg(target_os = "linux")]

use hal::ExternalImageProvider;
use std::{cell::RefCell, os::fd::AsRawFd, os::raw::c_void, rc::Rc};
use webrender::{api::units::*, api::*, hal};

mod bindings {
    #[derive(Clone, Copy)]
    pub struct WrExternalImageHandler(pub *mut std::os::raw::c_void);
    impl WrExternalImageHandler {
        pub fn object(self) -> *mut std::os::raw::c_void {
            self.0
        }
    }
}

#[path = "../src/hal_image.rs"]
mod hal_image;
use hal_image::*;

extern "C" {
    fn wr_snapshot_vulkan_dmabuf(
        data: &WrHalDmaBuf,
        destination: *mut u8,
        length: usize,
        stride: usize,
    ) -> bool;
}

#[test]
#[ignore = "Requires Linux Vulkan DMA-BUF and sync-file sharing"]
fn native_snapshots_preserve_format_and_validate_destination() {
    init_log();
    let producer = hal::create_vulkan_image_device(&hal::Options {
        validation: true,
        ..Default::default()
    }).unwrap();
    for format in [ImageFormat::RGBA8, ImageFormat::BGRA8] {
        let desc = ImageDescriptor::new(4, 4, format, ImageDescriptorFlags::empty());
        let pixels = [23, 47, 89, 128].repeat(16);
        let original = producer.create_image(desc, &pixels).unwrap();
        assert_eq!(producer.read_image(&original).unwrap(), pixels);
        let export = producer.export_dmabuf_image(&original, 0).unwrap();
        let layout = export.plane().layout();
        let data = WrHalDmaBuf {
            fd: export.plane().as_fd().as_raw_fd(),
            ready_fd: export.ready().as_fd().map_or(-1, |fd| fd.as_raw_fd()),
            width: 4,
            height: 4,
            format,
            modifier: layout.modifier(),
            stride: layout.stride(),
            offset: layout.offset(),
            device_uuid: layout.device_uuid(),
            driver_uuid: layout.driver_uuid(),
        };
        let mut destination = [0xa5; 80];
        for (length, stride) in [(79, 20), (80, 15)] {
            assert!(!unsafe {
                wr_snapshot_vulkan_dmabuf(&data, destination.as_mut_ptr(), length, stride)
            });
            assert_eq!(destination, [0xa5; 80]);
        }
        assert!(unsafe {
            wr_snapshot_vulkan_dmabuf(&data, destination.as_mut_ptr(), destination.len(), 20)
        });
        for (src, dst) in pixels.chunks_exact(16).zip(destination.chunks_exact(20)) {
            assert_eq!(src, &dst[..16]);
            assert_eq!(&dst[16..], &[0; 4]);
        }
    }
}

struct Fixture {
    image: RefCell<Option<WrHalImage>>,
    export: RefCell<Option<hal::DmaBufExport>>,
    releases: Rc<RefCell<Vec<WrHalImageRelease>>>,
    pixels: Vec<u8>,
}

struct Token {
    fixture: Rc<Fixture>,
    _export: Option<hal::DmaBufExport>,
}

#[no_mangle]
unsafe extern "C" fn wr_renderer_acquire_hal_image(
    obj: *mut c_void,
    _: ExternalImageId,
    _: u8,
    image: *mut WrHalImage,
) -> *mut WrHalImageLease {
    let fixture = &*(obj as *const Rc<Fixture>);
    let Some(value) = fixture.image.borrow_mut().take() else {
        return std::ptr::null_mut();
    };
    image.write(value);
    Box::into_raw(Box::new(Token {
        fixture: fixture.clone(),
        _export: fixture.export.borrow_mut().take(),
    })) as *mut WrHalImageLease
}

#[no_mangle]
unsafe extern "C" fn wr_renderer_release_hal_image(raw: *mut WrHalImageLease, status: WrHalImageRelease) {
    let token = Box::from_raw(raw as *mut Token);
    token.fixture.releases.borrow_mut().push(status);
}

fn provider(fixture: &Rc<Fixture>, device: hal::ExternalImageDevice) -> ExternalImages {
    ExternalImages::new(
        bindings::WrExternalImageHandler(fixture as *const Rc<Fixture> as *mut c_void),
        device,
    )
}

struct Notifier;
impl RenderNotifier for Notifier {
    fn clone(&self) -> Box<dyn RenderNotifier> {
        Box::new(Self)
    }
    fn wake_up(&self, _: bool) {}
    fn new_frame_ready(&self, _: DocumentId, _: FramePublishId, _: &FrameReadyParams) {}
}

fn init_log() {
    struct Logger;
    impl log::Log for Logger {
        fn enabled(&self, metadata: &log::Metadata) -> bool {
            metadata.level() <= log::Level::Warn
        }
        fn log(&self, record: &log::Record) {
            if self.enabled(record.metadata()) {
                eprintln!("{}: {}", record.target(), record.args());
            }
        }
        fn flush(&self) {}
    }
    static LOGGER: Logger = Logger;
    static START: std::sync::Once = std::sync::Once::new();
    START.call_once(|| {
        log::set_logger(&LOGGER).unwrap();
        log::set_max_level(log::LevelFilter::Warn);
    });
}

fn renderer() -> (hal::Renderer, webrender::render_api::RenderApiSender) {
    init_log();
    hal::create_vulkan_renderer(
        &hal::Options {
            validation: true,
            ..Default::default()
        },
        webrender::WebRenderOptions::default(),
        Box::new(Notifier),
    )
    .unwrap()
}

#[test]
#[ignore = "Requires a Linux Vulkan adapter"]
fn buffer_acquisitions_release_once_on_success_and_error() {
    let (renderer, sender) = renderer();
    for valid in [false, true] {
        let fixture = Rc::new(Fixture {
            image: RefCell::new(None),
            export: RefCell::new(None),
            releases: Default::default(),
            pixels: vec![17; 16],
        });
        *fixture.image.borrow_mut() = Some(WrHalImage {
            generation: 19,
            source: WrHalImageSource::Buffer(WrHalBuffer {
                data: fixture.pixels.as_ptr(),
                length: 16,
                width: 2,
                height: 2,
                stride: if valid { 8 } else { 4 },
                format: ImageFormat::RGBA8,
                opaque: true,
            }),
        });
        let mut provider = provider(&fixture, renderer.external_image_device());
        let result = provider.acquire(ExternalImageId(1), 0, false);
        assert_eq!(result.is_ok(), valid);
        if let Ok(lease) = result {
            assert_eq!(lease.generation(), 19);
            assert_eq!(lease.descriptor().size, DeviceIntSize::new(2, 2));
            drop(lease);
        }
        assert_eq!(fixture.releases.borrow().len(), 1);
        assert!(matches!(
            (valid, fixture.releases.borrow()[0]),
            (true, WrHalImageRelease::Complete) | (false, WrHalImageRelease::Unused)
        ));
        assert!(provider.acquire(ExternalImageId(1), 0, false).is_err());
        assert_eq!(fixture.releases.borrow().len(), 1);
        assert_eq!(Rc::strong_count(&fixture), 1);
    }
    for (fd, generation) in [(-2, 1), (0, 0)] {
        let fixture = Rc::new(Fixture {
            image: RefCell::new(Some(WrHalImage {
                generation,
                source: WrHalImageSource::VulkanDmaBuf(WrHalDmaBuf {
                    fd,
                    ready_fd: -1,
                    width: 4,
                    height: 4,
                    format: ImageFormat::RGBA8,
                    modifier: 0,
                    stride: 16,
                    offset: 0,
                    device_uuid: [0; 16],
                    driver_uuid: [0; 16],
                }),
            })),
            export: RefCell::new(None),
            releases: Default::default(),
            pixels: Vec::new(),
        });
        assert!(provider(&fixture, renderer.external_image_device())
            .acquire(ExternalImageId(1), 0, false)
            .is_err());
        assert!(matches!(
            fixture.releases.borrow().as_slice(),
            [WrHalImageRelease::Unused]
        ));
    }
    sender.create_api().shut_down(true);
}

struct SingleLease(Option<hal::ExternalImageLease>);
impl hal::ExternalImageProvider for SingleLease {
    fn acquire(&mut self, _: ExternalImageId, _: u8, _: bool) -> Result<hal::ExternalImageLease, String> {
        self.0.take().ok_or("Image already acquired".into())
    }
}

#[test]
#[ignore = "Requires Linux Vulkan DMA-BUF and sync-file sharing"]
fn native_descriptor_copy_survives_producer_descriptor_release() {
    let (mut renderer, sender) = renderer();
    let device = renderer.external_image_device();
    assert!(device.dmabuf_capabilities().unwrap().supported());
    let producer = hal::create_vulkan_image_device(&hal::Options {
        validation: true,
        ..Default::default()
    })
    .unwrap();
    let desc = ImageDescriptor::new(4, 4, ImageFormat::RGBA8, ImageDescriptorFlags::IS_OPAQUE);
    let pixels = [23, 47, 89, 255].repeat(16);
    let original = producer.create_image(desc, &pixels).unwrap();
    let export = producer.export_dmabuf_image(&original, 0).unwrap();
    let layout = export.plane().layout();
    let fd = export.plane().as_fd().as_raw_fd();
    let ready_fd = export.ready().as_fd().map_or(-1, |fd| fd.as_raw_fd());
    let fixture = Rc::new(Fixture {
        image: RefCell::new(Some(WrHalImage {
            generation: 71,
            source: WrHalImageSource::VulkanDmaBuf(WrHalDmaBuf {
                fd,
                ready_fd,
                width: 4,
                height: 4,
                format: layout.format(),
                modifier: layout.modifier(),
                stride: layout.stride(),
                offset: layout.offset(),
                device_uuid: layout.device_uuid(),
                driver_uuid: layout.driver_uuid(),
            }),
        })),
        export: RefCell::new(Some(export)),
        releases: Default::default(),
        pixels: Vec::new(),
    });
    let lease = provider(&fixture, device)
        .acquire(ExternalImageId(1), 0, false)
        .unwrap();
    assert_eq!(lease.generation(), 71);
    assert!(matches!(
        fixture.releases.borrow().as_slice(),
        [WrHalImageRelease::Complete]
    ));
    assert!(fixture.export.borrow().is_none());
    assert!(!std::path::Path::new(&format!("/proc/self/fd/{fd}")).exists());
    drop(fixture);
    drop(original);
    drop(producer);

    renderer
        .set_external_image_provider(Box::new(SingleLease(Some(lease))))
        .unwrap();
    let mut api = sender.create_api();
    let document = api.add_document(DeviceIntSize::new(4, 4));
    let pipeline = PipelineId(0, 0);
    let key = api.generate_image_key();
    let mut transaction = webrender::render_api::Transaction::new();
    transaction.add_image(
        key,
        desc,
        ImageData::External(ExternalImageData {
            id: ExternalImageId(1),
            channel_index: 0,
            image_type: ExternalImageType::TextureHandle(ImageBufferKind::Texture2D),
            normalized_uvs: false,
        }),
        None,
    );
    let mut builder = DisplayListBuilder::new(pipeline);
    builder.begin(60.0);
    let info = CommonItemProperties {
        clip_rect: LayoutRect::from_size(LayoutSize::new(4.0, 4.0)),
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
    builder.push_image(
        &info,
        info.clip_rect,
        ImageRendering::Pixelated,
        AlphaType::PremultipliedAlpha,
        key,
        ColorF::WHITE,
    );
    builder.pop_stacking_context();
    transaction.set_root_pipeline(pipeline);
    transaction.set_display_list(Epoch(0), api.get_namespace_id(), builder.end());
    transaction.generate_frame(1, true, false, RenderReasons::TESTING);
    api.send_transaction(document, transaction);
    renderer.prepare_frame(document).unwrap();
    renderer.render().unwrap();
    assert_eq!(
        renderer
            .read_pixels_rgba8(FramebufferIntRect::from_size(FramebufferIntSize::new(4, 4)))
            .unwrap(),
        pixels
    );
    api.shut_down(true);
}
