/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::rc::Rc;
use std::sync::{mpsc, Arc};
use std::time::Duration;
use webrender::api::{units::*, *};
use webrender::render_api::{RenderApi, RenderApiSender, Transaction};
use webrender::vulkan::{Device, Options, SharedTimeline, Submission, TextureFormat, TimelineHandle};
use wgpu_hal::dma_buf::{testing, Image};

#[repr(C)]
pub struct TestVulkanImage {
    pub(super) memory_fd: i32,
    pub(super) ready_fd: i32,
    pub(super) offset: u64,
    pub(super) stride: u64,
    pub(super) device_uuid: [u8; 16],
    pub(super) driver_uuid: [u8; 16],
    pub(super) copy_src: bool,
}

pub struct Fixture {
    renderer: Option<webrender::Renderer>,
    next: Option<MigrationRenderer>,
    producer: Rc<Device>,
    submission: Submission,
    _image: Rc<Arc<Image>>,
    _memory_fd: OwnedFd,
    _ready_fd: OwnedFd,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        drop(self.next.take());
        if let Some(renderer) = self.renderer.take() {
            renderer.deinit();
        }
        assert!(self.submission.wait(Some(Duration::from_secs(5))).unwrap());
    }
}

struct Notice(Option<mpsc::Sender<()>>);
impl RenderNotifier for Notice {
    fn clone(&self) -> Box<dyn RenderNotifier> {
        Box::new(Self(self.0.clone()))
    }
    fn wake_up(&self, _: bool) {}
    fn new_frame_ready(&self, _: DocumentId, _: FramePublishId, _: &FrameReadyParams) {
        if let Some(ready) = &self.0 { let _ = ready.send(()); }
    }
}

pub(super) fn renderer() -> webrender::Renderer {
    renderer_pair(None).0
}

fn renderer_pair(ready: Option<mpsc::Sender<()>>) -> (webrender::Renderer, RenderApiSender) {
    webrender::create_webrender_instance(
        webrender::GpuBackendConfig::Vulkan(Options {
            validation: true,
            ..Default::default()
        }),
        Box::new(Notice(ready)),
        webrender::WebRenderOptions {
            enable_debugger: false,
            ..Default::default()
        },
        None,
    )
    .unwrap()
}

struct MigrationRenderer {
    renderer: Option<webrender::Renderer>,
    api: RenderApi,
    document: DocumentId,
    ready: mpsc::Receiver<()>,
}

impl MigrationRenderer {
    fn new() -> Self {
        let (notify, ready) = mpsc::channel();
        let (renderer, sender) = renderer_pair(Some(notify));
        let mut api = sender.create_api();
        let document = api.add_document(DeviceIntSize::new(2, 2));
        Self { renderer: Some(renderer), api, document, ready }
    }

    fn sample(&mut self, handle: ExternalTextureHandle) {
        struct Handle(ExternalTextureHandle);
        impl ExternalImageHandler for Handle {
            fn lock(&mut self, _: ExternalImageId, _: u8, _: bool) -> ExternalImage<'_> {
                ExternalImage {
                    uv: TexelRect::new(0., 0., 2., 2.),
                    source: ExternalImageSource::NativeTexture(self.0),
                }
            }
            fn unlock(&mut self, _: ExternalImageId, _: u8) {}
        }
        let renderer = self.renderer.as_mut().unwrap();
        renderer.set_external_image_handler(Box::new(Handle(handle)));
        let pipeline = PipelineId(0, 0);
        let mut transaction = Transaction::new();
        let key = self.api.generate_image_key();
        transaction.add_image(
            key,
            ImageDescriptor::new(2, 2, ImageFormat::RGBA8, ImageDescriptorFlags::empty()),
            ImageData::External(ExternalImageData {
                id: ExternalImageId(1),
                channel_index: 0,
                image_type: ExternalImageType::TextureHandle(ImageBufferKind::Texture2D),
                normalized_uvs: false,
            }),
            None,
        );
        let mut builder = DisplayListBuilder::new(pipeline);
        builder.begin(60.);
        let rect = LayoutRect::from_size(LayoutSize::new(2., 2.));
        builder.push_image(
            &CommonItemProperties::new(rect, SpaceAndClipInfo::root_scroll(pipeline)),
            rect, ImageRendering::Pixelated, AlphaType::PremultipliedAlpha,
            key, ColorF::WHITE,
        );
        transaction.set_root_pipeline(pipeline);
        transaction.set_display_list(Epoch(0), self.api.get_namespace_id(), builder.end());
        transaction.generate_frame(1, true, false, RenderReasons::TESTING);
        self.api.send_transaction(self.document, transaction);
        self.ready.recv_timeout(Duration::from_secs(15)).unwrap();
        renderer.update();
        renderer.render(DeviceIntSize::new(2, 2), 0).unwrap();
        assert_eq!(
            renderer.read_pixels_rgba8(FramebufferIntRect::from_size(FramebufferIntSize::new(2, 2))),
            [255, 0, 0, 255].repeat(4),
        );
    }
}

impl Drop for MigrationRenderer {
    fn drop(&mut self) {
        self.api.delete_document(self.document);
        if let Some(renderer) = self.renderer.take() {
            renderer.deinit();
        }
    }
}

#[no_mangle]
pub extern "C" fn wr_test_vulkan_renderer_new() -> *mut webrender::Renderer {
    Box::into_raw(Box::new(renderer()))
}

#[no_mangle]
pub unsafe extern "C" fn wr_test_vulkan_renderer_delete(renderer: *mut webrender::Renderer) {
    if !renderer.is_null() {
        (*Box::from_raw(renderer)).deinit();
    }
}

#[no_mangle]
pub extern "C" fn wr_test_vulkan_image_new(output: &mut TestVulkanImage) -> *mut Fixture {
    let renderer = renderer();
    let producer = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Default::default()
        })
        .unwrap(),
    );
    let ready = SharedTimeline::new(&producer).unwrap();
    let (ready_fd, device_uuid, driver_uuid) = ready.export().unwrap().into_parts();
    let (image, memory_fd) = testing::export_image(producer.hal_owner(), TextureFormat::Rgba8Unorm, 0, [2, 2]).unwrap();
    let image = Rc::new(image);
    let descriptor = image.descriptor();
    assert_eq!(descriptor.device_uuid, device_uuid);
    assert_eq!(descriptor.driver_uuid, driver_uuid);
    let mut submission = Submission::new(&producer).unwrap();
    {
        let mut commands = submission.recording().unwrap();
        commands.keep(&image);
        unsafe { testing::publish(&image, commands.encoder(), [1., 0., 0., 1.], false) }.unwrap();
        commands.signal_timeline(&ready, 1).unwrap();
    }
    submission.submit().unwrap();
    *output = TestVulkanImage {
        memory_fd: memory_fd.as_raw_fd(),
        ready_fd: ready_fd.as_raw_fd(),
        offset: descriptor.offset,
        stride: descriptor.row_pitch,
        device_uuid,
        driver_uuid,
        copy_src: false,
    };
    Box::into_raw(Box::new(Fixture {
        renderer: Some(renderer),
        next: None,
        producer,
        submission,
        _image: image,
        _memory_fd: memory_fd,
        _ready_fd: ready_fd,
    }))
}

#[no_mangle]
pub extern "C" fn wr_test_vulkan_image_renderer(fixture: &mut Fixture) -> &mut webrender::Renderer {
    fixture.renderer.as_mut().unwrap()
}

#[no_mangle]
pub extern "C" fn wr_test_vulkan_image_submit(fixture: &mut Fixture) {
    fixture.renderer.take().unwrap().deinit();
}

#[no_mangle]
pub unsafe extern "C" fn wr_test_vulkan_image_wait(
    fixture: &Fixture,
    fd: i32,
    device_uuid: &[u8; 16],
    driver_uuid: &[u8; 16],
    value: u64,
) -> bool {
    let fd = BorrowedFd::borrow_raw(fd).try_clone_to_owned().unwrap();
    let handle = TimelineHandle::from_fd(fd, *device_uuid, *driver_uuid);
    let timeline = SharedTimeline::import(&fixture.producer, &handle).unwrap();
    let mut wait = Submission::new(&fixture.producer).unwrap();
    wait.recording().unwrap().wait_timeline(&timeline, value).unwrap();
    wait.submit().unwrap();
    wait.wait(Some(Duration::from_secs(5))).unwrap()
}

#[no_mangle]
pub unsafe extern "C" fn wr_test_vulkan_image_delete(fixture: *mut Fixture) {
    drop(Box::from_raw(fixture));
}

#[no_mangle]
pub extern "C" fn wr_test_vulkan_image_next_renderer(fixture: &mut Fixture) -> &mut webrender::Renderer {
    fixture.next.get_or_insert_with(MigrationRenderer::new).renderer.as_mut().unwrap()
}

#[no_mangle]
pub extern "C" fn wr_test_vulkan_image_sample(fixture: &mut Fixture, handle: u64) {
    fixture.next.as_mut().unwrap().sample(ExternalTextureHandle(handle));
}

#[no_mangle]
pub extern "C" fn wr_test_vulkan_image_submit_next(fixture: &mut Fixture) {
    drop(fixture.next.take());
}

#[no_mangle]
pub unsafe extern "C" fn wr_test_vulkan_image_value(
    fixture: &Fixture,
    fd: i32,
    device_uuid: &[u8; 16],
    driver_uuid: &[u8; 16],
) -> u64 {
    let fd = BorrowedFd::borrow_raw(fd).try_clone_to_owned().unwrap();
    let handle = TimelineHandle::from_fd(fd, *device_uuid, *driver_uuid);
    let owner = fixture.producer.hal_owner();
    let timeline = wgpu_hal::external_sync::import_timeline(&*owner.device, &handle).unwrap();
    wgpu_hal::external_sync::current_value(&*timeline).unwrap()
}
