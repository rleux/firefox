/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use super::super::{Device, Options, Samplers, Submission, TextureFilter};
use super::super::pipeline::DrawPipeline;
use super::super::tests::{validation_logging, ERRORS};
use api::units::{DeviceIntPoint, DeviceIntSize};
use crate::device::RenderState;
use std::sync::atomic::Ordering;

#[path = "draw_upload_tests.rs"]
mod uploads;

#[path = "draw_projection_tests.rs"]
mod projection;

#[path = "attachment_sync_tests.rs"]
pub(in crate::device::wgpu) mod synchronization;

fn device() -> Rc<Device> {
    validation_logging();
    let device = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    eprintln!("Vulkan adapter: {:?}", device.info());
    device
}

fn target(device: &Rc<Device>) -> Rc<Texture> {
    Texture::new(
        device,
        2,
        2,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        true,
    )
    .unwrap()
}

fn quad(device: &Rc<Device>) -> Rc<Buffer> {
    Buffer::new(
        device,
        &[0, 0, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 255, 255, 0, 0],
        wgt::BufferUses::VERTEX,
    )
    .unwrap()
}

fn pass(target: &Rc<Texture>) -> DrawPass<'_> {
    DrawPass {
            viewport: None,
        target,
        origin: DeviceIntPoint::zero(),
        depth: None,
        clear_color: None,
        clear_depth: None,
        depth_range: 0.0..1.0,
    }
}

fn floats(values: &[f32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_ne_bytes())
        .collect()
}

fn draw(device: &Rc<Device>, source: Option<Rc<Texture>>) -> Draw {
    let shader = super::super::shader::select_draw_shader(
        if source.is_some() { "cs_scale" } else { "ps_clear" },
        if source.is_some() { &["TEXTURE_2D"] } else { &[] },
        false,
    )
    .unwrap();
    let pipeline = DrawPipeline::new(
        device,
        shader,
        wgt::TextureFormat::Rgba8Unorm,
        false,
        RenderState::default(),
    )
    .unwrap();
    let projection = Buffer::new(
        device,
        &floats(&[
            1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
        ]),
        wgt::BufferUses::UNIFORM,
    )
    .unwrap();
    let instances = if source.is_some() {
        floats(&[-1.0, -1.0, 1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0])
    } else {
        floats(&[
            -1.0, -1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 1.0, -1.0, -1.0, 1.0, 1.0, 0.0, 1.0, 0.0, 1.0,
        ])
    };
    Draw {
        bindings: DrawBindings::new(
            &pipeline,
            Some(projection),
            source
                .into_iter()
                .map(|texture| (texture, TextureFilter::Nearest))
                .collect(),
            Vec::new(),
            Some(Rc::new(Samplers::new(device).unwrap())),
        )
        .unwrap(),
        instances: Buffer::new(device, &instances, wgt::BufferUses::VERTEX).unwrap(),
        instance_offset: 0,
        instance_count: 1,
        scissor: DeviceIntRect::from_size(DeviceIntSize::new(2, 2)),
    }
}

fn pixels(target: &Rc<Texture>) -> Vec<u8> {
    let size = target.size();
    target
        .readback(DeviceIntRect::from_size(DeviceIntSize::new(
            size.width as i32,
            size.height as i32,
        )))
        .unwrap()
        .wait()
        .unwrap()
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn draw_pass_preserves_pixels_clips_scissors_and_retains_resources() {
    let device = device();
    let target = target(&device);
    let quad = quad(&device);
    let mut draws = [draw(&device, None)];
    let pending = Rc::downgrade(&draws[0].instances);
    let pending_quad = Rc::downgrade(&quad);
    let mut submission = Submission::new(&device).unwrap();
    {
        let mut recording = submission.recording().unwrap();
        pass(&target).record(&mut recording, &quad, &[]).unwrap();
        draws[0].scissor = DeviceIntRect::from_origin_and_size(
            DeviceIntPoint::new(-1, -1),
            DeviceIntSize::new(2, 4),
        );
        pass(&target).record(&mut recording, &quad, &draws).unwrap();
        draws[0].instance_offset = 32;
        draws[0].scissor = DeviceIntRect::from_origin_and_size(
            DeviceIntPoint::new(1, 0),
            DeviceIntSize::new(3, 3),
        );
        pass(&target).record(&mut recording, &quad, &draws).unwrap();
        draws[0].scissor = DeviceIntRect::from_origin_and_size(
            DeviceIntPoint::new(5, 5),
            DeviceIntSize::new(2, 2),
        );
        pass(&target).record(&mut recording, &quad, &draws).unwrap();
        draws[0].instance_count = 0;
        draws[0].scissor = DeviceIntRect::from_size(DeviceIntSize::new(2, 2));
        pass(&target).record(&mut recording, &quad, &draws).unwrap();
    }
    drop(draws);
    drop(quad);
    assert!(pending.upgrade().is_some());
    assert!(pending_quad.upgrade().is_some());
    submission.submit().unwrap();
    submission.wait(None).unwrap();
    assert!(pending.upgrade().is_none());
    assert!(pending_quad.upgrade().is_none());
    assert_eq!(pixels(&target), [255, 0, 0, 255, 0, 255, 0, 255].repeat(2));
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn draw_pass_initializes_attachments_and_rolls_back_abandoned_recording() {
    let device = device();
    let target = target(&device);
    let depth = Texture::new(
        &device,
        2,
        2,
        wgt::TextureFormat::Depth32Float,
        TextureFilter::Nearest,
        true,
    )
    .unwrap();
    let quad = quad(&device);
    {
        let mut submission = Submission::new(&device).unwrap();
        let mut recording = submission.recording().unwrap();
        let mut pass = pass(&target);
        pass.depth = Some(&depth);
        pass.record(&mut recording, &quad, &[]).unwrap();
        assert!(target.initialized() && depth.initialized());
    }
    assert!(!target.initialized() && !depth.initialized());
    let mut submission = Submission::new(&device).unwrap();
    {
        let mut recording = submission.recording().unwrap();
        let mut pass = pass(&target);
        pass.depth = Some(&depth);
        pass.record(&mut recording, &quad, &[]).unwrap();
    }
    submission.submit().unwrap();
    submission.wait(None).unwrap();
    assert_eq!(pixels(&target), [0; 16]);
    let mut depth_draws = [draw(&device, None)];
    let shader = depth_draws[0].bindings.pipeline.shader;
    let pipeline = DrawPipeline::new(
        &device,
        shader,
        target.format(),
        true,
        RenderState {
            depth_test: Some(crate::device::DepthFunction::Less),
            depth_write: true,
            ..RenderState::default()
        },
    )
    .unwrap();
    let projection = depth_draws[0]
        .bindings
        .resources
        .buffer_uses()
        .next()
        .unwrap()
        .0
        .clone();
    depth_draws[0].bindings =
        DrawBindings::new(&pipeline, Some(projection), Vec::new(), Vec::new(), None).unwrap();
    let mut submission = Submission::new(&device).unwrap();
    {
        let mut recording = submission.recording().unwrap();
        let mut pass = pass(&target);
        pass.depth = Some(&depth);
        pass.depth_range = 0.0..0.5;
        pass.record(&mut recording, &quad, &depth_draws).unwrap();
        depth_draws[0].instance_offset = 32;
        pass.record(&mut recording, &quad, &depth_draws).unwrap();
    }
    submission.submit().unwrap();
    submission.wait(None).unwrap();
    assert_eq!(pixels(&target), [255, 0, 0, 255].repeat(4));
    let mut submission = Submission::new(&device).unwrap();
    {
        let mut recording = submission.recording().unwrap();
        let mut draws = [draw(&device, None)];
        draws[0].instance_count = 2;
        pass(&target).record(&mut recording, &quad, &draws).unwrap();
    }
    submission.submit().unwrap();
    submission.wait(None).unwrap();
    assert_eq!(pixels(&target), [0, 255, 0, 255].repeat(4));
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn draw_pass_rejects_incompatible_resources_before_recording() {
    let device = device();
    let target = target(&device);
    let quad = quad(&device);
    let mut draws = [draw(&device, None)];
    let mut submission = Submission::new(&device).unwrap();
    let mut recording = submission.recording().unwrap();
    for (offset, count) in [(4, 2), (0, 3), (u64::MAX, 1), (1, 1)] {
        draws[0].instance_offset = offset;
        draws[0].instance_count = count;
        assert!(pass(&target).record(&mut recording, &quad, &draws).is_err());
    }
    draws[0].instance_offset = 0;
    draws[0].instance_count = 1;
    let wrong_format = Texture::new(
        &device,
        2,
        2,
        wgt::TextureFormat::R8Unorm,
        TextureFilter::Nearest,
        true,
    )
    .unwrap();
    assert!(pass(&wrong_format)
        .record(&mut recording, &quad, &draws)
        .is_err());
    let depth = Texture::new(
        &device,
        2,
        2,
        wgt::TextureFormat::Depth32Float,
        TextureFilter::Nearest,
        true,
    )
    .unwrap();
    assert!(pass(&depth).record(&mut recording, &quad, &[]).is_err());
    let mut invalid = pass(&target);
    invalid.depth = Some(&depth);
    assert!(invalid.record(&mut recording, &quad, &draws).is_err());
    invalid.depth = Some(&target);
    assert!(invalid.record(&mut recording, &quad, &[]).is_err());
    invalid.depth = None;
    invalid.clear_depth = Some(0.5);
    assert!(invalid.record(&mut recording, &quad, &[]).is_err());
    invalid.clear_depth = None;
    invalid.depth_range = 0.0..f32::NAN;
    assert!(invalid.record(&mut recording, &quad, &[]).is_err());
    let foreign = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    let foreign_quad = Buffer::new(&foreign, &[0; 16], wgt::BufferUses::VERTEX).unwrap();
    assert!(pass(&target)
        .record(&mut recording, &foreign_quad, &draws)
        .is_err());
    draws[0].instances = Buffer::new(&foreign, &[0; 64], wgt::BufferUses::VERTEX).unwrap();
    assert!(pass(&target).record(&mut recording, &quad, &draws).is_err());
    assert!(!target.initialized());
    assert_eq!(target.current_usage(), wgt::TextureUses::UNINITIALIZED);
    assert_eq!(quad.current_usage(), wgt::BufferUses::MAP_WRITE);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn draw_pass_rejects_attachment_feedback_but_allows_disjoint_mips() {
    let device = device();
    let texture = Texture::new(
        &device,
        2,
        2,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Trilinear,
        true,
    )
    .unwrap();
    let base = texture.mip_view(0).unwrap();
    let mip = texture.mip_view(1).unwrap();
    let quad = quad(&device);
    let mut submission = Submission::new(&device).unwrap();
    {
        let mut recording = submission.recording().unwrap();
        let all_mips = [draw(&device, Some(texture.clone()))];
        let error = pass(&mip)
            .record(&mut recording, &quad, &all_mips)
            .unwrap_err();
        assert!(error.contains("feedback"), "{}", error);
        let source = [draw(&device, Some(base.clone()))];
        assert!(pass(&base)
            .record(&mut recording, &quad, &source)
            .unwrap_err()
            .contains("feedback"));
        assert!(pass(&mip)
            .record(&mut recording, &quad, &source)
            .unwrap_err()
            .contains("uninitialized"));
        let mut clear = pass(&base);
        clear.clear_color = Some(wgt::Color::RED);
        clear.record(&mut recording, &quad, &[]).unwrap();
        pass(&mip).record(&mut recording, &quad, &source).unwrap();
    }
    submission.submit().unwrap();
    submission.wait(None).unwrap();
    assert_eq!(pixels(&mip), [255, 0, 0, 255]);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn draw_pass_combines_vertex_and_shader_buffer_reads() {
    let device = device();
    let target = target(&device);
    let mut draw = draw(&device, None);
    let shared = Buffer::new(
        &device,
        &[0; 64],
        wgt::BufferUses::VERTEX | wgt::BufferUses::UNIFORM,
    )
    .unwrap();
    draw.bindings = DrawBindings::new(
        &draw.bindings.pipeline,
        Some(shared.clone()),
        Vec::new(),
        Vec::new(),
        None,
    )
    .unwrap();
    draw.instances = shared.clone();
    let mut submission = Submission::new(&device).unwrap();
    {
        let mut recording = submission.recording().unwrap();
        pass(&target)
            .record(&mut recording, &shared, &[draw])
            .unwrap();
        assert_eq!(
            shared.current_usage(),
            wgt::BufferUses::VERTEX | wgt::BufferUses::UNIFORM
        );
    }
    submission.submit().unwrap();
    submission.wait(None).unwrap();
    assert_eq!(pixels(&target), [0; 16]);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn repeated_draws_prepare_a_sampled_view_once_per_pass() {
    let device = device();
    let target = target(&device);
    let quad = quad(&device);
    let queue = super::super::SubmissionQueue::new(&Rc::new(super::super::BufferPool::new(&device)), 2).unwrap();
    let source = Texture::new(&device, 1, 1, target.format(), TextureFilter::Nearest, false).unwrap();
    source.upload(&queue, DeviceIntRect::from_size(DeviceIntSize::new(1, 1)), &[255, 0, 0, 255], None, 0, None).unwrap();
    let draw = draw(&device, Some(source));
    device.trace.borrow_mut().clear();
    pass(&target).record(&mut queue.recording().unwrap(), &quad, &vec![draw; 8]).unwrap();
    queue.wait().unwrap();
    assert_eq!(pixels(&target), [255, 0, 0, 255].repeat(4));
    assert_eq!(device.trace.borrow().iter().filter(|c| matches!(c, super::super::tests::Command::PrepareSampledView)).count(), 1);
    for predicate in [
        (|c: &super::super::tests::Command| matches!(c, super::super::tests::Command::DrawInvariant)) as fn(&_) -> bool,
        |c| matches!(c, super::super::tests::Command::DrawPipeline),
        |c| matches!(c, super::super::tests::Command::DrawScissor),
        |c| matches!(c, super::super::tests::Command::DrawInstances),
    ] {
        assert_eq!(device.trace.borrow().iter().filter(|c| predicate(c)).count(), 1);
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
