/* This Source Code Form is subject to the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::GpuBackendConfig;
use crate::device::vulkan::{
    Options,
    tests::{validation_logging, ERRORS},
};
use crate::render_api::Transaction;
use api::*;
use std::sync::{mpsc, atomic::Ordering};

#[path = "vulkan_image_tests.rs"]
mod images;
#[path = "vulkan_external_image_tests.rs"]
mod external_images;
#[path = "vulkan_clip_tests.rs"]
mod clips;
#[path = "vulkan_filter_tests.rs"]
mod filters;
#[path = "vulkan_blend_tests.rs"]
mod blends;
#[path = "vulkan_glyph_tests.rs"]
mod glyphs;

struct Notice(mpsc::Sender<()>);

impl RenderNotifier for Notice {
    fn clone(&self) -> Box<dyn RenderNotifier> {
        Box::new(Self(self.0.clone()))
    }
    fn wake_up(&self, _: bool) {}
    fn new_frame_ready(&self, _: DocumentId, _: FramePublishId, _: &FrameReadyParams) {
        let _ = self.0.send(());
    }
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn vulkan_renderer_draws_a_display_list_through_device_construction() {
    render_display_list(false);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn vulkan_renderer_uses_pooled_uploads_when_shared_instances_are_requested() {
    render_display_list(true);
}

fn render_display_list(enable_shared_instance_buffer: bool) {
    validation_logging();
    let (tx, rx) = mpsc::channel();
    let options = crate::WebRenderOptions {
        enable_subpixel_aa: false,
        enable_debugger: false,
        enable_shared_instance_buffer,
        ..Default::default()
    };
    let (mut renderer, sender) = crate::create_webrender_instance(
        GpuBackendConfig::Vulkan(Options {
            validation: true,
            ..Default::default()
        }),
        Box::new(Notice(tx)),
        options,
        None,
    )
    .unwrap();
    assert!(!renderer.use_shared_instance_buffer);
    assert!(renderer.vaos.shared_instance_buffer.is_none());
    let initial = renderer.gpu_submission_status().unwrap().unwrap();
    renderer.render(DeviceIntSize::new(32, 32), 0).unwrap();
    assert_eq!(
        renderer.gpu_submission_status().unwrap().unwrap().submitted,
        initial.submitted,
    );
    let mut previous_submission = initial.submitted;
    let mut api = sender.create_api();
    let size = DeviceIntSize::new(32, 32);
    let document = api.add_document(size);
    let pipeline = PipelineId(0, 0);
    let rect = LayoutRect::from_size(LayoutSize::new(32.0, 32.0));
    let info = CommonItemProperties {
        clip_rect: rect,
        clip_chain_id: ClipChainId::INVALID,
        spatial_id: SpatialId::root_scroll_node(pipeline),
        flags: PrimitiveFlags::default(),
    };
    for epoch in 0..2 {
        let mut builder = DisplayListBuilder::new(pipeline);
        builder.begin(60.0);
        builder.push_rect(&info, rect, ColorF::new(1.0, 0.0, 0.0, 1.0));
        let inset = LayoutRect::from_origin_and_size(
            LayoutPoint::new(8.0, 8.0),
            LayoutSize::new(16.0, 16.0),
        );
        let alpha = if epoch == 0 { 0.5 } else { 1.0 };
        builder.push_rect(&info, inset, ColorF::new(0.0, 1.0, 0.0, alpha));
        let mut transaction = Transaction::new();
        transaction.set_root_pipeline(pipeline);
        transaction.set_display_list(Epoch(epoch), api.get_namespace_id(), builder.end());
        transaction.generate_frame(epoch as u64 + 1, true, false, RenderReasons::TESTING);
        api.send_transaction(document, transaction);
        rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap();
        renderer.update();
        assert_eq!(renderer.render(size, 0).unwrap().present_result, None);
        assert!(renderer.record_frame(ImageFormat::RGBA8).is_none());
        let (screenshot, screenshot_size) = renderer.get_screenshot_async(
            DeviceIntRect::from_size(size),
            size,
            ImageFormat::RGBA8,
        );
        assert!(screenshot_size.is_empty());
        assert!(!renderer.map_and_recycle_screenshot(
            screenshot, &mut [17; 4], 4, ImageFormat::RGBA8,
        ));
        assert!(renderer.device.failure().is_none());
        let status = renderer.gpu_submission_status().unwrap().unwrap();
        assert!(status.submitted > previous_submission);
        assert!(status.completed <= status.submitted);
        previous_submission = status.submitted;
        let output = renderer.device.vulkan_test_output().unwrap();
        let pixels = output
            .readback(DeviceIntRect::from_size(size))
            .unwrap()
            .wait()
            .unwrap();
        let complete = renderer.gpu_submission_status().unwrap().unwrap();
        assert_eq!(complete.submitted, status.submitted);
        assert_eq!(complete.completed, status.submitted);
        for (index, pixel) in pixels.chunks_exact(4).enumerate() {
            if (8..24).contains(&(index % 32)) && (8..24).contains(&(index / 32)) {
                let expected = if epoch == 0 {
                    [127u8, 128, 0, 255]
                } else {
                    [0, 255, 0, 255]
                };
                assert!(
                    pixel.iter().zip(expected).all(|(&a, b)| a.abs_diff(b) <= 1),
                    "pixel {}: {:?}",
                    index,
                    pixel
                );
            } else {
                assert_eq!(pixel, [255, 0, 0, 255], "pixel {}", index);
            }
        }
    }
    api.delete_document(document);
    renderer.deinit();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[cfg(all(target_os = "linux", feature = "debugger"))]
#[ignore = "Requires 24-bit Xvfb, a presentation-capable Vulkan adapter and validation"]
fn vulkan_window_renderer_presents_display_lists_without_an_output_texture() {
    use crate::device::vulkan::{surface_testing, X11Window};
    use crate::PresentResult;
    use wgpu_hal::SurfaceError;
    validation_logging();
    let window = Rc::new(unsafe { X11Window::new() });
    let (tx, rx) = mpsc::channel();
    let mut options = crate::WebRenderOptions {
        enable_subpixel_aa: false,
        enable_debugger: false,
        ..Default::default()
    };
    if let CompositorConfig::Draw {
        max_partial_present_rects,
        draw_previous_partial_present_regions,
        ..
    } = &mut options.compositor_config
    {
        *max_partial_present_rects = 1;
        *draw_previous_partial_present_regions = true;
    }
    let (mut renderer, sender) = crate::create_webrender_instance(
        GpuBackendConfig::Vulkan(Options {
            window: Some(window.clone()),
            validation: true,
            ..Default::default()
        }),
        Box::new(Notice(tx)),
        options,
        None,
    )
    .unwrap();
    assert!(matches!(
        renderer.current_compositor_kind,
        CompositorKind::Draw {
            max_partial_present_rects: 0,
            ..
        }
    ));
    let mut api = sender.create_api();
    let document = api.add_document(DeviceIntSize::new(64, 48));
    renderer.set_surface_paused(true).unwrap();
    renderer.set_surface_paused(true).unwrap();
    renderer.set_surface_paused(false).unwrap();
    assert_eq!(
        renderer.render(DeviceIntSize::new(64, 48), 0).unwrap().present_result,
        None,
    );
    let pipeline = PipelineId(0, 0);
    for (epoch, size) in [[64u32, 48], [64, 48], [32, 24], [32, 24]]
        .iter()
        .copied()
        .enumerate()
    {
        window.resize(size);
        let device_size = DeviceIntSize::new(size[0] as i32, size[1] as i32);
        let rect = LayoutRect::from_size(LayoutSize::new(size[0] as f32, size[1] as f32));
        let info = CommonItemProperties {
            clip_rect: rect,
            clip_chain_id: ClipChainId::INVALID,
            spatial_id: SpatialId::root_scroll_node(pipeline),
            flags: PrimitiveFlags::default(),
        };
        let mut builder = DisplayListBuilder::new(pipeline);
        builder.begin(60.0);
        builder.push_rect(&info, rect, ColorF::new(1.0, 0.0, 0.0, 1.0));
        let alpha = if epoch % 2 == 0 { 0.5 } else { 1.0 };
        builder.push_rect(
            &info,
            LayoutRect::from_origin_and_size(
                LayoutPoint::new(8.0, 8.0),
                LayoutSize::new((size[0] - 16) as f32, (size[1] - 16) as f32),
            ),
            ColorF::new(0.0, 1.0, 0.0, alpha),
        );
        let mut transaction = Transaction::new();
        transaction.set_document_view(DeviceIntRect::from_size(device_size));
        transaction.set_root_pipeline(pipeline);
        transaction.set_display_list(Epoch(epoch as u32), api.get_namespace_id(), builder.end());
        transaction.generate_frame(epoch as u64 + 1, true, false, RenderReasons::TESTING);
        api.send_transaction(document, transaction);
        rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap();
        renderer.update();
        assert_eq!(
            renderer.render(device_size, 0).unwrap().present_result,
            Some(PresentResult::Presented),
        );
        assert!(renderer.device.vulkan_test_output().is_none());
        let wait_for_pixels = || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let pixels = window.pixels(size);
                let matches = pixels.iter().enumerate().all(|(index, &pixel)| {
                    let x = index as u32 % size[0];
                    let y = index as u32 / size[0];
                    if (8..size[0] - 8).contains(&x) && (8..size[1] - 8).contains(&y) {
                        let expected = if alpha == 1.0 {
                            [0u8, 255, 0]
                        } else {
                            [127, 128, 0]
                        };
                        [16, 8, 0].iter().zip(expected).all(|(&shift, channel)| {
                            ((pixel >> shift) as u8).abs_diff(channel) <= 1
                        })
                    } else {
                        pixel == 0xff0000
                    }
                });
                if matches {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "Presented Renderer pixels did not match frame {epoch}"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };
        wait_for_pixels();
        window.clear();
        assert_eq!(
            renderer.render(device_size, 0).unwrap().present_result,
            Some(PresentResult::Presented),
        );
        assert!(renderer.device.vulkan_test_output().is_none());
        wait_for_pixels();

        renderer.set_surface_paused(true).unwrap();
        let status = renderer.gpu_submission_status().unwrap().unwrap();
        assert_eq!(status.submitted, status.completed);
        window.clear();
        for _ in 0..2 {
            assert_eq!(
                renderer.render(device_size, 0).unwrap().present_result,
                Some(PresentResult::Occluded),
            );
            assert!(window.pixels(size).iter().all(|&pixel| pixel == 0));
        }
        let resized = [size[0] + 4, size[1] + 4];
        window.resize(resized);
        let resized = DeviceIntSize::new(resized[0] as i32, resized[1] as i32);
        assert_eq!(
            renderer.render(resized, 0).unwrap().present_result,
            Some(PresentResult::Occluded),
        );
        renderer.set_surface_paused(true).unwrap();
        renderer.set_surface_paused(false).unwrap();
        assert_eq!(
            renderer.render(resized, 0).unwrap().present_result,
            Some(PresentResult::Presented),
        );
        window.resize(size);
        renderer.set_surface_paused(false).unwrap();
        assert!(renderer.force_redraw);
        assert_eq!(
            renderer.render(device_size, 0).unwrap().present_result,
            Some(PresentResult::Presented),
        );
        wait_for_pixels();

        if epoch == 3 {
            for acquire in [true, false] {
                for (error, expected) in [
                    (SurfaceError::Timeout, PresentResult::Retry),
                    (SurfaceError::Outdated, PresentResult::Retry),
                    (SurfaceError::Occluded, PresentResult::Occluded),
                ] {
                    if acquire {
                        surface_testing::fail_acquire(error);
                    } else {
                        surface_testing::override_present(Err(error));
                    }
                    assert_eq!(
                        renderer.render(device_size, 0).unwrap().present_result,
                        Some(expected),
                    );
                    assert!(renderer.force_redraw);
                    assert!(renderer.device.failure().is_none());
                    assert_eq!(
                        renderer.render(DeviceIntSize::zero(), 0).unwrap().present_result,
                        None,
                    );
                    window.clear();
                    assert_eq!(
                        renderer.render(device_size, 0).unwrap().present_result,
                        Some(PresentResult::Presented),
                    );
                    assert!(renderer.device.vulkan_test_output().is_none());
                    wait_for_pixels();
                }
            }
            surface_testing::override_present(Ok(true));
            assert_eq!(
                renderer.render(device_size, 0).unwrap().present_result,
                Some(PresentResult::Presented),
            );
            assert_eq!(
                renderer.render(device_size, 0).unwrap().present_result,
                Some(PresentResult::Presented),
            );
            wait_for_pixels();

            window.resize([40, 30]);
            surface_testing::fail_acquire(SurfaceError::Outdated);
            assert_eq!(
                renderer.render(device_size, 0).unwrap().present_result,
                Some(PresentResult::Retry),
            );
            assert_eq!(
                renderer.render(device_size, 0).unwrap().present_result,
                Some(PresentResult::SizeMismatch),
            );
            window.resize(size);
            assert_eq!(
                renderer.render(device_size, 0).unwrap().present_result,
                Some(PresentResult::Presented),
            );
            wait_for_pixels();

            surface_testing::override_present(Err(SurfaceError::Lost));
            let errors = renderer.render(device_size, 0).unwrap_err();
            assert!(errors.iter().any(|error| {
                matches!(error, RendererError::Device(message) if message.contains("surface was lost"))
            }));
            assert!(renderer.set_surface_paused(true).is_err());
            assert!(renderer.set_surface_paused(false).is_err());
        }
    }
    api.delete_document(document);
    renderer.deinit();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[cfg(all(target_os = "linux", feature = "debugger"))]
#[ignore = "Requires X11, a presentation-capable Vulkan adapter and validation"]
fn vulkan_window_renderer_replaces_surface_without_rebuilding_scene() {
    window_renderer_surface_lifecycle(false);
}

#[test]
#[cfg(all(target_os = "linux", feature = "debugger"))]
#[ignore = "Requires X11, a presentation-capable Vulkan adapter and validation"]
fn vulkan_window_renderer_starts_without_surface() {
    window_renderer_surface_lifecycle(true);
}

#[cfg(all(target_os = "linux", feature = "debugger"))]
fn window_renderer_surface_lifecycle(start_detached: bool) {
    use crate::device::vulkan::{SurfaceOptions, X11Display, X11Window};
    use crate::PresentResult;

    validation_logging();
    let display = Rc::new(unsafe { X11Display::new() });
    let display_weak = Rc::downgrade(&display);
    let mut window = Rc::new(unsafe { X11Window::with_display(display.clone()) });
    let (tx, rx) = mpsc::channel();
    let (mut renderer, sender) = crate::create_webrender_instance(
        GpuBackendConfig::Vulkan(Options {
            window: if start_detached { None } else { Some(window.clone()) },
            display_owner: Some(display.clone()),
            validation: true,
            ..Default::default()
        }),
        Box::new(Notice(tx)),
        crate::WebRenderOptions {
            enable_subpixel_aa: false,
            enable_debugger: false,
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let size = DeviceIntSize::new(64, 48);
    let mut api = sender.create_api();
    let document = api.add_document(size);
    let pipeline = PipelineId(0, 0);
    let rect = LayoutRect::from_size(LayoutSize::new(64.0, 48.0));
    let common = CommonItemProperties::new(rect, SpaceAndClipInfo::root_scroll(pipeline));
    let mut builder = DisplayListBuilder::new(pipeline);
    builder.begin(60.0);
    builder.push_rect(&common, rect, ColorF::new(0.0, 1.0, 0.0, 1.0));
    builder.push_rect(
        &common,
        LayoutRect::from_origin_and_size(LayoutPoint::new(8.0, 8.0), LayoutSize::new(48.0, 32.0)),
        ColorF::new(1.0, 0.0, 0.0, 1.0),
    );
    let mut transaction = Transaction::new();
    transaction.set_root_pipeline(pipeline);
    transaction.set_display_list(Epoch(0), api.get_namespace_id(), builder.end());
    transaction.generate_frame(1, true, false, RenderReasons::TESTING);
    api.send_transaction(document, transaction);
    rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap();
    renderer.update();
    assert_eq!(
        renderer.render(size, 0).unwrap().present_result,
        Some(if start_detached { PresentResult::Occluded } else { PresentResult::Presented })
    );
    assert!(renderer.device.vulkan_test_output().is_none());
    let cached_textures: FastHashMap<_, _> = renderer
        .texture_resolver
        .texture_cache_map
        .iter()
        .map(|(&id, entry)| (id, DrawTarget::from_texture(&entry.texture, false)))
        .collect();
    assert!(!cached_textures.is_empty());
    let mut submitted = renderer.gpu_submission_status().unwrap().unwrap().submitted;

    for paused in [false, true] {
        let old = Rc::downgrade(&window);
        if paused {
            renderer.set_surface_paused(true).unwrap();
            renderer
                .set_vulkan_surface(None, SurfaceOptions::default())
                .unwrap();
            let completed = renderer.gpu_submission_status().unwrap().unwrap();
            assert!(completed.completed >= submitted);
            assert_eq!(completed.submitted, completed.completed);
            window = Rc::new(unsafe { X11Window::with_display(display.clone()) });
            assert!(old.upgrade().is_none());
            for _ in 0..2 {
                assert_eq!(
                    renderer.render(size, 0).unwrap().present_result,
                    Some(PresentResult::Occluded)
                );
                assert!(renderer.device.vulkan_test_output().is_none());
            }
        } else {
            window = Rc::new(unsafe { X11Window::with_display(display.clone()) });
            assert_eq!(old.upgrade().is_some(), !start_detached);
        }
        renderer
            .set_vulkan_surface(Some(window.clone()), SurfaceOptions::default())
            .unwrap();
        assert!(old.upgrade().is_none());
        if paused {
            assert_eq!(
                renderer.render(size, 0).unwrap().present_result,
                Some(PresentResult::Occluded)
            );
            assert!(window.pixels([64, 48]).iter().all(|&pixel| pixel == 0));
            renderer.set_surface_paused(false).unwrap();
        }
        let result = renderer.render(size, 0).unwrap();
        assert_eq!(result.present_result, Some(PresentResult::Presented));
        assert_eq!(result.stats.color_target_count, 1);
        assert_eq!(
            renderer.texture_resolver.texture_cache_map.len(),
            cached_textures.len()
        );
        for (&id, target) in &cached_textures {
            assert_eq!(
                DrawTarget::from_texture(
                    &renderer.texture_resolver.texture_cache_map[&id].texture,
                    false
                ),
                *target
            );
        }
        assert!(renderer.device.vulkan_test_output().is_none());
        let status = renderer.gpu_submission_status().unwrap().unwrap();
        assert!(status.submitted > submitted);
        submitted = status.submitted;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !window
            .pixels([64, 48])
            .iter()
            .enumerate()
            .all(|(index, &pixel)| {
                let inside = (8..56).contains(&(index % 64)) && (8..40).contains(&(index / 64));
                pixel == if inside { 0xff0000 } else { 0x00ff00 }
            })
        {
            assert!(
                std::time::Instant::now() < deadline,
                "Cached scene was not presented on the replacement window"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    renderer
        .set_vulkan_surface(None, SurfaceOptions::default())
        .unwrap();
    let weak = Rc::downgrade(&window);
    drop(window);
    assert!(weak.upgrade().is_none());
    drop(display);
    assert!(display_weak.upgrade().is_some());
    api.delete_document(document);
    renderer.deinit();
    assert!(display_weak.upgrade().is_none());
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
