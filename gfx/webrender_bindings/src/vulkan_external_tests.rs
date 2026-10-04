/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

#[test]
fn null_external_objects_can_be_deleted() {
    unsafe { wr_vulkan_external_images_delete(std::ptr::null_mut()) };
    unsafe { wr_vulkan_timeline_delete(std::ptr::null_mut()) };
    unsafe { wr_vulkan_dmabuf_delete(std::ptr::null_mut()) };
    unsafe { wr_vulkan_release_delete(std::ptr::null_mut()) };
    assert_eq!(wr_vulkan_release_status(None), WrVulkanReleaseStatus::Abandoned);
}

fn descriptor() -> WrVulkanDmaBufDescriptor {
    WrVulkanDmaBufDescriptor {
        fd: -1,
        width: 3,
        height: 2,
        format: ImageFormat::RGBA8,
        modifier: 19,
        offset: 64,
        stride: 128,
        device_uuid: [3; 16],
        driver_uuid: [5; 16],
        copy_src: false,
        copy_dst: false,
        color_target: false,
    }
}

#[cfg(all(feature = "vulkan", target_os = "linux"))]
#[test]
fn image_descriptor_preserves_format_usage_and_layout() {
    for (format, expected) in [
        (ImageFormat::RGBA8, TextureFormat::Rgba8Unorm),
        (ImageFormat::BGRA8, TextureFormat::Bgra8Unorm),
    ] {
        for bits in 0..8 {
            let mut desc = descriptor();
            desc.format = format;
            desc.copy_src = bits & 1 != 0;
            desc.copy_dst = bits & 2 != 0;
            desc.color_target = bits & 4 != 0;
            let raw = image_descriptor(&desc).unwrap();
            assert_eq!(raw.size, [3, 2]);
            assert_eq!(raw.format, expected);
            assert_eq!((raw.modifier, raw.offset, raw.row_pitch), (19, 64, 128));
            assert_eq!((raw.device_uuid, raw.driver_uuid), ([3; 16], [5; 16]));
            assert!(raw.usage.contains(TextureUses::RESOURCE));
            assert_eq!(raw.usage.contains(TextureUses::COPY_SRC), desc.copy_src);
            assert_eq!(raw.usage.contains(TextureUses::COPY_DST), desc.copy_dst);
            assert_eq!(raw.usage.contains(TextureUses::COLOR_TARGET), desc.color_target);
        }
    }
    let mut invalid = descriptor();
    invalid.format = ImageFormat::R8;
    assert!(image_descriptor(&invalid).is_err());
}

#[cfg(all(feature = "vulkan", target_os = "linux"))]
#[test]
#[ignore = "Requires Vulkan DMA-BUF and shared timelines"]
fn external_binding_timelines_preserve_fd_ownership_and_failure_outputs() {
    use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
    use webrender::api::*;
    struct Notice;
    impl RenderNotifier for Notice {
        fn clone(&self) -> Box<dyn RenderNotifier> {
            Box::new(Notice)
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
    let images_ptr = wr_vulkan_external_images_new(&renderer);
    assert!(!images_ptr.is_null());
    let images = unsafe { Box::from_raw(images_ptr) };
    let timeline_ptr = wr_vulkan_timeline_new(&images);
    assert!(!timeline_ptr.is_null());
    let timeline = unsafe { Box::from_raw(timeline_ptr) };
    let mut exported = WrVulkanTimelineDescriptor {
        fd: -1,
        device_uuid: [0; 16],
        driver_uuid: [0; 16],
    };
    assert!(wr_vulkan_timeline_export(&timeline, &mut exported));
    let fd = unsafe { OwnedFd::from_raw_fd(exported.fd) };
    let imported_ptr = unsafe { wr_vulkan_timeline_import(&images, &exported) };
    assert!(!imported_ptr.is_null());
    let imported = unsafe { Box::from_raw(imported_ptr) };
    assert!(fd.as_fd().try_clone_to_owned().is_ok());
    let mut unchanged = WrVulkanTimelineDescriptor {
        fd: -99,
        device_uuid: [7; 16],
        driver_uuid: [8; 16],
    };
    assert!(!wr_vulkan_timeline_export(&imported, &mut unchanged));
    assert_eq!(
        (unchanged.fd, unchanged.device_uuid, unchanged.driver_uuid),
        (-99, [7; 16], [8; 16])
    );
    for mismatch in 0..3 {
        let mut invalid = exported;
        match mismatch {
            0 => invalid.device_uuid[0] ^= 1,
            1 => invalid.driver_uuid[0] ^= 1,
            _ => invalid.fd = -1,
        }
        assert!(unsafe { wr_vulkan_timeline_import(&images, &invalid) }.is_null());
        assert!(fd.as_fd().try_clone_to_owned().is_ok());
    }
    let mut image = descriptor();
    assert!(unsafe { wr_vulkan_dmabuf_import(&images, &image) }.is_null());
    // Unsupported formats are rejected before passing this non-memory FD to Vulkan.
    image.fd = fd.as_raw_fd();
    image.format = ImageFormat::R8;
    assert!(unsafe { wr_vulkan_dmabuf_import(&images, &image) }.is_null());
    assert!(fd.as_fd().try_clone_to_owned().is_ok());
    unsafe { wr_vulkan_external_images_delete(Box::into_raw(images)) };
    renderer.deinit();
    unsafe { wr_vulkan_timeline_delete(Box::into_raw(imported)) };
    unsafe { wr_vulkan_timeline_delete(Box::into_raw(timeline)) };
    drop(fd);
}

#[cfg(not(all(feature = "vulkan", target_os = "linux")))]
#[test]
fn unsupported_external_bindings_fail_without_changing_outputs() {
    let images = WrVulkanExternalImages {};
    let timeline = WrVulkanTimeline {};
    let image = WrVulkanDmaBufImage {};
    assert!(!wr_vulkan_dmabuf_matches_context(&image, &images));
    let receipt = WrVulkanRelease {};
    let mut descriptor = WrVulkanTimelineDescriptor {
        fd: -99,
        device_uuid: [7; 16],
        driver_uuid: [8; 16],
    };
    assert!(wr_vulkan_timeline_new(&images).is_null());
    assert!(unsafe { wr_vulkan_timeline_import(&images, &descriptor) }.is_null());
    assert!(!wr_vulkan_timeline_export(&timeline, &mut descriptor));
    assert_eq!(
        (descriptor.fd, descriptor.device_uuid, descriptor.driver_uuid),
        (-99, [7; 16], [8; 16])
    );
    assert!(unsafe { wr_vulkan_dmabuf_import(&images, &super::tests::descriptor()) }.is_null());
    let mut handle = ExternalTextureHandle(99);
    assert!(!unsafe { wr_vulkan_dmabuf_acquire(&image, &timeline, 1, &mut handle) });
    assert_eq!(handle.0, 99);
    assert!(wr_vulkan_dmabuf_release(&image, &timeline, 1).is_null());
    assert_eq!(
        wr_vulkan_release_status(Some(&receipt)),
        WrVulkanReleaseStatus::Abandoned
    );
}
