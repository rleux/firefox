/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#![cfg(all(feature = "vulkan", target_os = "linux"))]

extern crate log;
extern crate webrender;
extern crate wgpu_hal;

#[path = "../../../webrender_bindings/src/vulkan_external.rs"]
mod ffi;

use self::ffi::*;
use std::os::fd::{AsRawFd, OwnedFd};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use webrender::api::*;
use webrender::vulkan::{Device, Options, SharedTimeline, Submission};

struct ValidationLog;
static ERRORS: AtomicUsize = AtomicUsize::new(0);
impl log::Log for ValidationLog {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, entry: &log::Record) {
        if entry.target().starts_with("wgpu_hal") && entry.level() == log::Level::Error {
            ERRORS.fetch_add(1, Ordering::Relaxed);
        }
        eprintln!("{}", entry.args());
    }
    fn flush(&self) {}
}

struct ExportedImage {
    image: Arc<wgpu_hal::dma_buf::Image>,
    fd: OwnedFd,
    descriptor: WrVulkanDmaBufDescriptor,
}

fn export_image(owner: &Rc<Device>, identity: &WrVulkanTimelineDescriptor) -> Rc<ExportedImage> {
    let (image, fd) = wgpu_hal::dma_buf::testing::export_image(
        owner.hal_owner(),
        webrender::vulkan::TextureFormat::Rgba8Unorm,
        0,
        [2, 2],
    )
    .unwrap();
    let native = *image.descriptor();
    Rc::new(ExportedImage {
        image,
        descriptor: WrVulkanDmaBufDescriptor {
            fd: fd.as_raw_fd(),
            width: 2,
            height: 2,
            format: ImageFormat::RGBA8,
            modifier: 0,
            offset: native.offset,
            stride: native.row_pitch,
            device_uuid: identity.device_uuid,
            driver_uuid: identity.driver_uuid,
            copy_src: false,
            copy_dst: true,
            color_target: false,
        },
        fd,
    })
}

#[test]
#[ignore = "Requires Vulkan DMA-BUF, timelines and validation"]
fn dmabuf_bindings_retain_images_until_renderer_submission() {
    let _ = log::set_logger(&ValidationLog);
    log::set_max_level(log::LevelFilter::Warn);
    struct Notice;
    impl RenderNotifier for Notice {
        fn clone(&self) -> Box<dyn RenderNotifier> {
            Box::new(Self)
        }
        fn wake_up(&self, _: bool) {}
        fn new_frame_ready(&self, _: DocumentId, _: FramePublishId, _: &FrameReadyParams) {}
    }
    let (renderer, _) = webrender::create_webrender_instance(
        webrender::GpuBackendConfig::Vulkan(Options {
            validation: true,
            ..Default::default()
        }),
        Box::new(Notice),
        webrender::WebRenderOptions {
            enable_debugger: false,
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let context = wr_vulkan_external_images_new(&renderer);
    assert!(!context.is_null());
    let context = unsafe { Box::from_raw(context) };
    let producer = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Default::default()
        })
        .unwrap(),
    );
    let ready = SharedTimeline::new(&producer).unwrap();
    let (fd, device_uuid, driver_uuid) = ready.export().unwrap().into_parts();
    let ready_descriptor = WrVulkanTimelineDescriptor {
        fd: fd.as_raw_fd(),
        device_uuid,
        driver_uuid,
    };
    let ready_import = unsafe { wr_vulkan_timeline_import(&context, &ready_descriptor) };
    assert!(!ready_import.is_null());
    let ready_import = unsafe { Box::from_raw(ready_import) };
    drop(fd);
    let source = export_image(&producer, &ready_descriptor);
    let mut send = Submission::new(&producer).unwrap();
    {
        let mut commands = send.recording().unwrap();
        commands.keep(&source);
        unsafe {
            wgpu_hal::dma_buf::testing::publish(
                &source.image,
                commands.encoder(),
                [1., 0., 0., 1.],
                false,
            )
        }
        .unwrap();
        commands.signal_timeline(&ready, 1).unwrap();
    }
    send.submit().unwrap();
    let imported = unsafe { wr_vulkan_dmabuf_import(&context, &source.descriptor) };
    assert!(!imported.is_null());
    let imported = unsafe { Box::from_raw(imported) };
    assert!(wr_vulkan_dmabuf_matches_context(&imported, &context));
    let same_context = wr_vulkan_external_images_new(&renderer);
    assert!(!same_context.is_null());
    assert!(wr_vulkan_dmabuf_matches_context(&imported, unsafe {
        &*same_context
    }));
    unsafe { wr_vulkan_external_images_delete(same_context) };
    let (other, _) = webrender::create_webrender_instance(
        webrender::GpuBackendConfig::Vulkan(Options {
            validation: true,
            ..Default::default()
        }),
        Box::new(Notice),
        webrender::WebRenderOptions {
            enable_debugger: false,
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let other_context = wr_vulkan_external_images_new(&other);
    assert!(!other_context.is_null());
    assert!(!wr_vulkan_dmabuf_matches_context(&imported, unsafe {
        &*other_context
    }));
    unsafe { wr_vulkan_external_images_delete(other_context) };
    other.deinit();
    assert_eq!(source.fd.as_raw_fd(), source.descriptor.fd);
    let released = wr_vulkan_timeline_new(&context);
    assert!(!released.is_null());
    let released = unsafe { Box::from_raw(released) };
    let mut handle = ExternalTextureHandle(0);
    assert!(unsafe { wr_vulkan_dmabuf_acquire(&imported, &ready_import, 1, &mut handle) });
    assert_ne!(handle.0, 0);
    let receipt = wr_vulkan_dmabuf_release(&imported, &released, 1);
    assert!(!receipt.is_null());
    let receipt = unsafe { Box::from_raw(receipt) };
    assert_eq!(
        wr_vulkan_release_status(Some(&receipt)),
        WrVulkanReleaseStatus::Pending
    );
    assert!(wr_vulkan_dmabuf_release(&imported, &released, 2).is_null());
    unsafe { wr_vulkan_dmabuf_delete(Box::into_raw(imported)) };
    unsafe { wr_vulkan_timeline_delete(Box::into_raw(ready_import)) };
    unsafe { wr_vulkan_timeline_delete(Box::into_raw(released)) };
    unsafe { wr_vulkan_external_images_delete(Box::into_raw(context)) };
    renderer.deinit();
    assert_eq!(
        wr_vulkan_release_status(Some(&receipt)),
        WrVulkanReleaseStatus::Submitted
    );
    unsafe { wr_vulkan_release_delete(Box::into_raw(receipt)) };
    assert!(send.wait(Some(Duration::from_secs(5))).unwrap());
    drop(send);
    drop(source);
    drop(ready);
    drop(producer);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
