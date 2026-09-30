/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::{BlendMode, DepthFunction};

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

fn pass(target: &Rc<Texture>) -> DrawPass<'_> {
    DrawPass {
        target,
        origin: DeviceIntPoint::zero(),
        viewport: None,
        depth: None,
        clear_color: None,
        clear_depth: None,
        depth_range: 0.0..1.0,
    }
}

fn program(store: &mut ProgramStore) -> Program {
    let mut program = store.create("ps_clear", &[], false).unwrap();
    store.link(&mut program, &desc::CLEAR).unwrap();
    program
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn pipeline_bindings_follow_state_and_attachment_compatibility() {
    let device = device();
    let mut store = ProgramStore::default();
    let mut program = program(&mut store);
    let a = Texture::new(
        &device,
        2,
        2,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        true,
    )
    .unwrap();
    let b = Texture::new(
        &device,
        3,
        1,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        true,
    )
    .unwrap();
    let gray = Texture::new(
        &device,
        2,
        2,
        wgt::TextureFormat::R8Unorm,
        TextureFilter::Nearest,
        true,
    )
    .unwrap();
    let depth = Texture::new(
        &device,
        2,
        2,
        wgt::TextureFormat::Depth32Float,
        TextureFilter::Nearest,
        true,
    )
    .unwrap();
    let normal = RenderState::default();
    assert!(store.bind_pipeline(&program, normal, &pass(&a)).unwrap());
    let first = store.pipeline.as_ref().unwrap().1.clone();
    assert!(!store.bind_pipeline(&program, normal, &pass(&b)).unwrap());
    assert!(Rc::ptr_eq(&first, &store.pipeline.as_ref().unwrap().1));
    store
        .state(&program)
        .unwrap()
        .set_transform(&Transform3D::scale(2.0, 2.0, 1.0));
    assert!(!store.bind_pipeline(&program, normal, &pass(&b)).unwrap());
    assert!(store.resolve_current(&pass(&b), &[], None).is_ok());
    assert!(store.resolve_current(&pass(&gray), &[], None).is_err());
    assert!(store.bind_pipeline(&program, normal, &pass(&gray)).unwrap());
    let gray_pipeline = store.pipeline.as_ref().unwrap().1.clone();
    assert_eq!(
        store.pipeline.as_ref().unwrap().1.format,
        wgt::TextureFormat::R8Unorm
    );
    let mut depth_pass = pass(&a);
    depth_pass.depth = Some(&depth);
    assert!(store.resolve_current(&depth_pass, &[], None).is_err());
    assert!(store.bind_pipeline(&program, normal, &depth_pass).unwrap());
    let depth_pipeline = store.pipeline.as_ref().unwrap().1.clone();
    assert!(store.pipeline.as_ref().unwrap().1.has_depth);
    let state = RenderState {
        color_write: false,
        ..normal
    };
    assert!(store.bind_pipeline(&program, state, &depth_pass).unwrap());
    assert!(!store.bind_pipeline(&program, state, &depth_pass).unwrap());
    let mut variants = Vec::new();
    for state in [
        state,
        RenderState {
            blend_mode: BlendMode::Alpha,
            ..normal
        },
        RenderState {
            depth_test: Some(DepthFunction::Less),
            ..normal
        },
        RenderState {
            depth_test: Some(DepthFunction::LessEqual),
            ..normal
        },
        RenderState {
            depth_test: Some(DepthFunction::Less),
            depth_write: true,
            ..normal
        },
    ] {
        store.bind_pipeline(&program, state, &depth_pass).unwrap();
        let pipeline = store.pipeline.as_ref().unwrap().1.clone();
        assert!(!Rc::ptr_eq(&depth_pipeline, &pipeline));
        assert!(variants
            .iter()
            .all(|(_, previous)| !Rc::ptr_eq(previous, &pipeline)));
        variants.push((state, pipeline));
    }
    for _ in 0..2 {
        store.unbind();
        assert!(store.bind_pipeline(&program, normal, &pass(&b)).unwrap());
        assert!(Rc::ptr_eq(&first, &store.pipeline.as_ref().unwrap().1));
        assert!(store.bind_pipeline(&program, normal, &pass(&gray)).unwrap());
        assert!(Rc::ptr_eq(
            &gray_pipeline,
            &store.pipeline.as_ref().unwrap().1
        ));
        assert!(store.bind_pipeline(&program, normal, &depth_pass).unwrap());
        assert!(Rc::ptr_eq(
            &depth_pipeline,
            &store.pipeline.as_ref().unwrap().1
        ));
        for (state, pipeline) in &variants {
            assert!(store.bind_pipeline(&program, *state, &depth_pass).unwrap());
            assert!(Rc::ptr_eq(pipeline, &store.pipeline.as_ref().unwrap().1));
            assert!(!store.bind_pipeline(&program, *state, &depth_pass).unwrap());
        }
    }
    store.delete(&mut program).unwrap();
    assert!(store.pipeline.is_none());
    assert!(store.resolve_current(&depth_pass, &[], None).is_err());
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn failed_pipeline_binds_preserve_the_previous_program() {
    let device = device();
    let target = Texture::new(
        &device,
        1,
        1,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        true,
    )
    .unwrap();
    let pass = pass(&target);
    let mut store = ProgramStore::default();
    let mut a = program(&mut store);
    let mut b = program(&mut store);
    let mut unlinked = store.create("ps_clear", &[], false).unwrap();
    let state = RenderState::default();
    assert!(store.bind_pipeline(&a, state, &pass).unwrap());
    let previous = store.pipeline.as_ref().unwrap().1.clone();
    assert!(store
        .bind_pipeline(
            &b,
            RenderState {
                blend_mode: BlendMode::SubpixelDualSource,
                ..state
            },
            &pass
        )
        .is_err());
    assert!(store.bind_pipeline(&unlinked, state, &pass).is_err());
    let mut foreign_store = ProgramStore::default();
    let mut foreign = program(&mut foreign_store);
    assert!(store.bind_pipeline(&foreign, state, &pass).is_err());
    assert_eq!(store.bound, Some(a.id));
    assert!(Rc::ptr_eq(&previous, &store.pipeline.as_ref().unwrap().1));
    assert!(store.resolve_current(&pass, &[], None).is_ok());
    assert!(!store.bind(&a).unwrap());
    assert!(store.pipeline.is_some());
    assert!(store.bind(&b).unwrap());
    assert!(store.pipeline.is_none());
    assert!(store.resolve_current(&pass, &[], None).is_err());
    assert!(store.bind_pipeline(&b, state, &pass).unwrap());
    let b_pipeline = Rc::downgrade(&store.pipeline.as_ref().unwrap().1);
    assert!(store.bind_pipeline(&a, state, &pass).unwrap());
    assert!(Rc::ptr_eq(&previous, &store.pipeline.as_ref().unwrap().1));
    assert!(store.bind_pipeline(&b, state, &pass).unwrap());
    assert!(Rc::ptr_eq(
        &b_pipeline.upgrade().unwrap(),
        &store.pipeline.as_ref().unwrap().1
    ));
    store.unbind();
    assert!(store.pipeline.is_none());
    assert!(b_pipeline.upgrade().is_some());
    store.delete(&mut a).unwrap();
    store.delete(&mut b).unwrap();
    assert!(b_pipeline.upgrade().is_none());
    store.delete(&mut unlinked).unwrap();
    foreign_store.delete(&mut foreign).unwrap();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn pipeline_cache_separates_devices_and_program_lifetimes() {
    let a = device();
    let b = device();
    let target = |device| {
        Texture::new(
            device,
            1,
            1,
            wgt::TextureFormat::Rgba8Unorm,
            TextureFilter::Nearest,
            true,
        )
        .unwrap()
    };
    let target_a = target(&a);
    let target_b = target(&b);
    let mut store = ProgramStore::default();
    let mut shader = program(&mut store);
    let state = RenderState::default();
    store
        .bind_pipeline(&shader, state, &pass(&target_a))
        .unwrap();
    let first = Rc::downgrade(&store.pipeline.as_ref().unwrap().1);
    store
        .bind_pipeline(&shader, state, &pass(&target_b))
        .unwrap();
    let second = Rc::downgrade(&store.pipeline.as_ref().unwrap().1);
    assert!(!Rc::ptr_eq(
        &first.upgrade().unwrap(),
        &second.upgrade().unwrap()
    ));
    store.unbind();
    store
        .bind_pipeline(&shader, state, &pass(&target_a))
        .unwrap();
    assert!(Rc::ptr_eq(
        &first.upgrade().unwrap(),
        &store.pipeline.as_ref().unwrap().1
    ));
    store
        .bind_pipeline(&shader, state, &pass(&target_b))
        .unwrap();
    assert!(Rc::ptr_eq(
        &second.upgrade().unwrap(),
        &store.pipeline.as_ref().unwrap().1
    ));
    store.delete(&mut shader).unwrap();
    assert!(first.upgrade().is_none());
    assert!(second.upgrade().is_none());
    let mut recreated = program(&mut store);
    store
        .bind_pipeline(&recreated, state, &pass(&target_a))
        .unwrap();
    assert!(first.upgrade().is_none());
    store.delete(&mut recreated).unwrap();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn queued_draws_retain_pipelines_across_rebinding_and_deletion() {
    let device = device();
    let queue = SubmissionQueue::new(&Rc::new(BufferPool::new(&device)), 2).unwrap();
    let target = Texture::new(
        &device,
        2,
        1,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        true,
    )
    .unwrap();
    let pass = pass(&target);
    let mut store = ProgramStore::default();
    let mut program = program(&mut store);
    let normal = RenderState::default();
    store.bind_pipeline(&program, normal, &pass).unwrap();
    let opaque = store.resolve_current(&pass, &[], None).unwrap();
    let weak_opaque = Rc::downgrade(&store.pipeline.as_ref().unwrap().1);
    store
        .bind_pipeline(
            &program,
            RenderState {
                color_write: false,
                ..normal
            },
            &pass,
        )
        .unwrap();
    let masked = store.resolve_current(&pass, &[], None).unwrap();
    store
        .bind_pipeline(
            &program,
            RenderState {
                blend_mode: BlendMode::Alpha,
                ..normal
            },
            &pass,
        )
        .unwrap();
    let blended = store.resolve_current(&pass, &[], None).unwrap();
    store.bind_pipeline(&program, normal, &pass).unwrap();
    assert!(Rc::ptr_eq(
        &weak_opaque.upgrade().unwrap(),
        &store.pipeline.as_ref().unwrap().1,
    ));
    store.delete(&mut program).unwrap();
    drop(store);
    assert!(weak_opaque.upgrade().is_some());
    let quad = Buffer::new(
        &device,
        &[0, 0, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 255, 255, 0, 0],
        wgt::BufferUses::VERTEX,
    )
    .unwrap();
    let floats = |color: [f32; 4]| {
        [-1.0f32, -1.0, 1.0, 1.0]
            .iter()
            .chain(&color)
            .flat_map(|f| f.to_ne_bytes())
            .collect::<Vec<_>>()
    };
    let red = floats([1.0, 0.0, 0.0, 1.0]);
    let green = floats([0.0, 1.0, 0.0, 1.0]);
    let blue = floats([0.0, 0.0, 1.0, 0.5]);
    let full = DeviceIntRect::from_size(DeviceIntSize::new(2, 1));
    let right =
        DeviceIntRect::from_origin_and_size(DeviceIntPoint::new(1, 0), DeviceIntSize::new(1, 1));
    pass.record_batches(
        &mut queue.recording().unwrap(),
        &queue,
        &quad,
        None,
        &[
            opaque.batch(&red, 1, full),
            masked.batch(&green, 1, full),
            blended.batch(&blue, 1, right),
        ],
    )
    .unwrap();
    drop(opaque);
    drop(masked);
    drop(blended);
    assert!(weak_opaque.upgrade().is_some());
    queue.wait().unwrap();
    assert!(weak_opaque.upgrade().is_none());
    let pixels = target.readback(full).unwrap().wait().unwrap();
    assert_eq!(&pixels[..4], &[255, 0, 0, 255]);
    assert!((127..=128).contains(&pixels[4]) && (127..=128).contains(&pixels[6]));
    assert_eq!((pixels[5], pixels[7]), (0, 255));
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
