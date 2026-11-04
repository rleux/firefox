/* This Source Code Form is subject to the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::wgpu::{surface_testing, X11Display, X11Window};
use crate::{GpuSubmissionStatus, PresentResult};
use std::time::{Duration, Instant};
use wgpu_types::SurfaceStatus;

fn window_renderer(
    detached: bool,
) -> (
    Renderer,
    RenderApi,
    mpsc::Receiver<()>,
    Rc<X11Window>,
    Rc<X11Display>,
) {
    validation_logging();
    let display = Rc::new(unsafe { X11Display::new() });
    let window = Rc::new(unsafe { X11Window::with_display(display.clone()) });
    let (tx, rx) = mpsc::channel();
    let (renderer, sender) = crate::create_webrender_instance(
        GpuBackendConfig::Vulkan(Options {
            window: (!detached).then(|| window.clone() as Rc<dyn crate::wgpu::SurfaceWindow>),
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
    (renderer, sender.create_api(), rx, window, display)
}
fn publish(
    renderer: &mut Renderer,
    api: &mut RenderApi,
    rx: &mpsc::Receiver<()>,
    document: DocumentId,
    epoch: u32,
) {
    let pipeline = PipelineId(0, 0);
    let rect = LayoutRect::from_size(LayoutSize::new(64., 48.));
    let info = CommonItemProperties {
        clip_rect: rect,
        clip_chain_id: ClipChainId::INVALID,
        spatial_id: SpatialId::root_scroll_node(pipeline),
        flags: PrimitiveFlags::default(),
    };
    let mut builder = DisplayListBuilder::new(pipeline);
    builder.begin(60.);
    builder.push_rect(&info, rect, ColorF::new(1., 0., 0., 1.));
    builder.push_rect(
        &info,
        LayoutRect::from_origin_and_size(LayoutPoint::new(0., 24.), LayoutSize::new(64., 24.)),
        ColorF::new(0., 0., 1., 1.),
    );
    let mut txn = Transaction::new();
    txn.set_root_pipeline(pipeline);
    txn.set_display_list(Epoch(epoch), api.get_namespace_id(), builder.end());
    txn.generate_frame(epoch as u64 + 1, true, false, RenderReasons::TESTING);
    api.send_transaction(document, txn);
    rx.recv_timeout(Duration::from_secs(15)).unwrap();
    renderer.update();
}
fn render(renderer: &mut Renderer) -> Option<PresentResult> {
    renderer
        .render(DeviceIntSize::new(64, 48), 0)
        .unwrap()
        .present_result
}
fn idle(renderer: &mut Renderer) -> GpuSubmissionStatus {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = renderer.gpu_submission_status().unwrap().unwrap();
        assert!(status.completed <= status.submitted);
        if status.completed == status.submitted {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "renderer completion did not progress while idle"
        );
        std::thread::yield_now();
    }
}
fn observe(window: &X11Window) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let pixels = window.pixels([64, 48]);
        if pixels
            .iter()
            .enumerate()
            .all(|(i, &p)| p == if i / 64 < 24 { 0xff0000 } else { 0x0000ff })
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "native renderer pixels did not match"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
#[ignore = "Requires native X11, Vulkan presentation and Khronos validation"]
fn vulkan_renderer_presents_directly_and_snapshots_only_on_request() {
    let (mut renderer, mut api, rx, window, display) = window_renderer(false);
    let before = ERRORS.load(Ordering::Relaxed);
    let initial = renderer.gpu_submission_status().unwrap().unwrap();
    assert_eq!(
        renderer
            .render(DeviceIntSize::new(64, 48), 0)
            .unwrap()
            .present_result,
        None
    );
    assert_eq!(
        renderer.gpu_submission_status().unwrap().unwrap().submitted,
        initial.submitted
    );
    let document = api.add_document(DeviceIntSize::new(64, 48));
    publish(&mut renderer, &mut api, &rx, document, 0);
    assert_eq!(render(&mut renderer), Some(PresentResult::Presented));
    observe(&window);
    assert!(!renderer.device.readback_ready());
    assert!(renderer.record_frame(ImageFormat::RGBA8).is_none());
    assert!(renderer.device.failure().is_none());
    assert!(renderer.prepare_readback().unwrap());
    assert_eq!(render(&mut renderer), Some(PresentResult::Presented));
    let bytes = renderer.read_pixels_rgba8(FramebufferIntRect::from_size(FramebufferIntSize::new(
        64, 48,
    )));
    assert!(bytes.chunks_exact(4).enumerate().all(|(i, p)| p
        == if i / 64 < 24 {
            [255, 0, 0, 255]
        } else {
            [0, 0, 255, 255]
        }));
    let mut cropped = [0; 24];
    renderer.read_pixels_into(
        FramebufferIntRect::from_origin_and_size(
            FramebufferIntPoint::new(5, 23), FramebufferIntSize::new(3, 2)),
        ImageFormat::BGRA8,
        &mut cropped,
    );
    assert!(cropped[..12].chunks_exact(4).all(|p| p == [0, 0, 255, 255]));
    assert!(cropped[12..].chunks_exact(4).all(|p| p == [255, 0, 0, 255]));
    let (frame, size) = renderer.record_frame(ImageFormat::RGBA8).unwrap();
    assert_eq!(size, DeviceIntSize::new(64, 48));
    let mut recorded = vec![0; bytes.len()];
    assert!(renderer.map_recorded_frame(frame, &mut recorded, 64 * 4));
    assert_eq!(recorded, bytes);
    let (shot, size) = renderer.get_screenshot_async(
        DeviceIntRect::from_size(DeviceIntSize::new(64, 48)),
        DeviceIntSize::new(32, 24),
        ImageFormat::RGBA8,
    );
    assert_eq!(size, DeviceIntSize::new(32, 24));
    let mut scaled = vec![0; 32 * 24 * 4];
    assert!(renderer.map_and_recycle_screenshot(shot, &mut scaled, 32 * 4, ImageFormat::RGBA8));
    assert_eq!(&scaled[..4], &[255, 0, 0, 255]);
    assert_eq!(&scaled[scaled.len() - 4..], &[0, 0, 255, 255]);
    let (shot, size) = renderer.get_screenshot_async(
        DeviceIntRect::from_origin_and_size(DeviceIntPoint::new(7, 24), DeviceIntSize::new(48, 24)),
        DeviceIntSize::new(24, 12),
        ImageFormat::BGRA8,
    );
    assert_eq!(size, DeviceIntSize::new(24, 12));
    let mut cropped_scaled = vec![0; 24 * 12 * 4];
    assert!(renderer.map_and_recycle_screenshot(
        shot, &mut cropped_scaled, 24 * 4, ImageFormat::RGBA8));
    assert!(cropped_scaled.chunks_exact(4).all(|p| p == [0, 0, 255, 255]));
    assert!(renderer.device.readback_ready());
    let mut last = idle(&mut renderer).submitted;
    for _ in 0..8 {
        assert_eq!(render(&mut renderer), Some(PresentResult::Presented));
        assert!(!renderer.device.readback_ready());
        assert!(renderer.record_frame(ImageFormat::RGBA8).is_none());
        let status = renderer.gpu_submission_status().unwrap().unwrap();
        assert!(status.submitted > last);
        last = status.submitted;
    }
    assert_eq!(idle(&mut renderer).submitted, last);
    assert!(renderer.prepare_readback().unwrap());
    let replacement = Rc::new(unsafe { X11Window::with_display(display) });
    renderer
        .set_wgpu_surface(
            Some(replacement.clone()),
            crate::wgpu::SurfaceOptions::default(),
        )
        .unwrap();
    assert!(!renderer.device.readback_ready());
    assert!(renderer.record_frame(ImageFormat::RGBA8).is_none());
    assert_eq!(render(&mut renderer), Some(PresentResult::Presented));
    observe(&replacement);
    assert!(!renderer.device.readback_ready());
    assert!(renderer.prepare_readback().unwrap());
    assert_eq!(render(&mut renderer), Some(PresentResult::Presented));
    assert_eq!(
        renderer.read_pixels_rgba8(FramebufferIntRect::from_size(FramebufferIntSize::new(
            64, 48
        ))),
        bytes
    );
    api.delete_document(document);
    renderer.deinit();
    assert_eq!(ERRORS.load(Ordering::Relaxed), before);
}

#[test]
#[ignore = "Requires native X11, Vulkan presentation and Khronos validation"]
fn vulkan_renderer_recovers_surface_statuses_without_a_new_scene() {
    let (mut renderer, mut api, rx, window, _display) = window_renderer(false);
    let before = ERRORS.load(Ordering::Relaxed);
    let document = api.add_document(DeviceIntSize::new(64, 48));
    publish(&mut renderer, &mut api, &rx, document, 0);
    for acquire in [true, false] {
        for (status, expected) in [
            (SurfaceStatus::Timeout, PresentResult::Retry),
            (SurfaceStatus::Occluded, PresentResult::Occluded),
            (SurfaceStatus::Outdated, PresentResult::Retry),
            (SurfaceStatus::Lost, PresentResult::Retry),
            (SurfaceStatus::Suboptimal, PresentResult::Presented),
        ] {
            if acquire {
                surface_testing::ACQUIRE.with(|s| s.set(Some(status)));
            } else {
                surface_testing::PRESENT.with(|s| s.set(Some(status)));
            }
            assert_eq!(render(&mut renderer), Some(expected));
            assert!(renderer.device.failure().is_none());
            if expected != PresentResult::Presented {
                assert!(renderer.force_redraw);
            }
            assert_eq!(
                renderer
                    .render(DeviceIntSize::zero(), 0)
                    .unwrap()
                    .present_result,
                None
            );
            assert_eq!(render(&mut renderer), Some(PresentResult::Presented));
            observe(&window);
        }
    }
    window.resize([40, 30]);
    surface_testing::ACQUIRE.with(|s| s.set(Some(SurfaceStatus::Outdated)));
    assert_eq!(render(&mut renderer), Some(PresentResult::Retry));
    assert_eq!(render(&mut renderer), Some(PresentResult::SizeMismatch));
    assert_eq!(
        renderer
            .render(DeviceIntSize::new(40, 30), 0)
            .unwrap()
            .present_result,
        Some(PresentResult::Presented)
    );
    renderer.set_surface_paused(true).unwrap();
    let status = renderer.gpu_submission_status().unwrap().unwrap();
    assert_eq!(status.submitted, status.completed);
    publish(&mut renderer, &mut api, &rx, document, 1);
    assert_eq!(render(&mut renderer), Some(PresentResult::Occluded));
    window.resize([64, 48]);
    renderer.set_surface_paused(false).unwrap();
    assert_eq!(render(&mut renderer), Some(PresentResult::Presented));
    observe(&window);
    api.delete_document(document);
    renderer.deinit();
    assert_eq!(ERRORS.load(Ordering::Relaxed), before);
}

#[test]
#[ignore = "Requires native X11, Vulkan presentation and Khronos validation"]
fn vulkan_renderer_detach_replace_preserves_resources_and_drains_after_loss() {
    let (mut renderer, mut api, rx, window, display) = window_renderer(true);
    let before = ERRORS.load(Ordering::Relaxed);
    let document = api.add_document(DeviceIntSize::new(64, 48));
    publish(&mut renderer, &mut api, &rx, document, 0);
    assert_eq!(render(&mut renderer), Some(PresentResult::Occluded));
    let first = idle(&mut renderer).submitted;
    renderer
        .set_wgpu_surface(Some(window.clone()), crate::wgpu::SurfaceOptions::default())
        .unwrap();
    assert_eq!(render(&mut renderer), Some(PresentResult::Presented));
    observe(&window);
    let unrelated = Rc::new(unsafe { X11Window::new() });
    assert!(renderer
        .set_wgpu_surface(
            Some(unrelated.clone()),
            crate::wgpu::SurfaceOptions::default()
        )
        .is_err());
    drop(unrelated);
    assert!(renderer.device.failure().is_none());
    assert_eq!(render(&mut renderer), Some(PresentResult::Presented));
    observe(&window);
    let weak = Rc::downgrade(&window);
    drop(window);
    renderer
        .set_wgpu_surface(None, crate::wgpu::SurfaceOptions::default())
        .unwrap();
    assert!(weak.upgrade().is_none());
    assert_eq!(render(&mut renderer), Some(PresentResult::Occluded));
    let window = Rc::new(unsafe { X11Window::with_display(display) });
    let weak = Rc::downgrade(&window);
    renderer
        .set_wgpu_surface(Some(window.clone()), crate::wgpu::SurfaceOptions::default())
        .unwrap();
    drop(window);
    assert_eq!(render(&mut renderer), Some(PresentResult::Presented));
    assert!(idle(&mut renderer).submitted > first);
    renderer.device.wgpu_test_lose_device();
    assert!(renderer.gpu_submission_status().is_err());
    assert!(renderer
        .set_wgpu_surface(None, crate::wgpu::SurfaceOptions::default())
        .is_err());
    assert!(weak.upgrade().is_none());
    api.delete_document(document);
    renderer.deinit();
    assert_eq!(ERRORS.load(Ordering::Relaxed), before);
}

#[test]
#[ignore = "Requires native X11, Vulkan presentation and Khronos validation"]
fn vulkan_renderer_never_publishes_snapshots_from_failed_presentations() {
    let (mut renderer, mut api, rx, _window, _display) = window_renderer(false);
    let before = ERRORS.load(Ordering::Relaxed);
    let document = api.add_document(DeviceIntSize::new(64, 48));
    publish(&mut renderer, &mut api, &rx, document, 0);
    for acquire in [false, true] {
        for (status, expected) in [
            (SurfaceStatus::Timeout, PresentResult::Retry),
            (SurfaceStatus::Outdated, PresentResult::Retry),
            (SurfaceStatus::Lost, PresentResult::Retry),
            (SurfaceStatus::Occluded, PresentResult::Occluded),
        ] {
            assert!(renderer.prepare_readback().unwrap());
            assert!(renderer.prepare_readback().unwrap());
            assert_eq!(render(&mut renderer), Some(PresentResult::Presented));
            assert!(renderer.device.readback_ready());
            assert!(renderer.prepare_readback().unwrap());
            if acquire {
                surface_testing::ACQUIRE.with(|s| s.set(Some(status)));
            } else {
                surface_testing::PRESENT.with(|s| s.set(Some(status)));
            }
            assert_eq!(render(&mut renderer), Some(expected));
            assert!(!renderer.device.readback_ready());
            assert!(renderer.record_frame(ImageFormat::RGBA8).is_none());
            let (_, size) = renderer.get_screenshot_async(
                DeviceIntRect::from_size(DeviceIntSize::new(64, 48)),
                DeviceIntSize::new(64, 48),
                ImageFormat::RGBA8,
            );
            assert!(size.is_empty());
            let mut pixel = [17; 4];
            renderer.read_pixels_into(
                FramebufferIntRect::from_size(FramebufferIntSize::new(1, 1)),
                ImageFormat::RGBA8,
                &mut pixel,
            );
            assert_eq!(pixel, [17; 4]);
            assert!(renderer.device.failure().is_none());
            assert_eq!(render(&mut renderer), Some(PresentResult::Presented));
            assert!(!renderer.device.readback_ready());
        }
    }
    assert!(renderer.prepare_readback().unwrap());
    surface_testing::PRESENT.with(|s| s.set(Some(SurfaceStatus::Suboptimal)));
    assert_eq!(render(&mut renderer), Some(PresentResult::Presented));
    assert!(renderer.device.readback_ready());
    assert_eq!(
        renderer.read_pixels_rgba8(FramebufferIntRect::from_size(FramebufferIntSize::new(1, 1))),
        [255, 0, 0, 255]
    );
    api.delete_document(document);
    renderer.deinit();
    assert_eq!(ERRORS.load(Ordering::Relaxed), before);
}
