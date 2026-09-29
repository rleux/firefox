/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use api::units::{DeviceIntPoint, DeviceIntRect, DeviceIntSize};

fn device() -> Rc<Device> {
    validation_logging();
    Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    )
}

fn rect(x: i32, y: i32, width: i32, height: i32) -> DeviceIntRect {
    DeviceIntRect::from_origin_and_size(
        DeviceIntPoint::new(x, y),
        DeviceIntSize::new(width, height),
    )
}

fn full(texture: &Texture) -> DeviceIntRect {
    rect(
        0,
        0,
        texture.size().width as i32,
        texture.size().height as i32,
    )
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn mip_views_upload_copy_render_and_read_the_selected_level() {
    let device = device();
    let texture = Texture::new(
        &device,
        7,
        3,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Trilinear,
        false,
    )
    .unwrap();
    let queue = upload_queue(&device);
    let views: Vec<_> = (0..3)
        .map(|level| texture.mip_view(level).unwrap())
        .collect();
    for (level, view) in views.iter().enumerate() {
        assert_eq!(view.size().width, [7, 3, 1][level]);
        assert_eq!(view.size().height, [3, 1, 1][level]);
        assert_eq!(view.mip_count(), 1);
        assert_eq!(view.filter(), TextureFilter::Linear);
        let bytes = vec![
            17 + level as u8 * 14;
            view.size().width as usize * view.size().height as usize * 4
        ];
        view.upload(&queue, full(view), &bytes, None, 0, None)
            .unwrap();
    }
    assert!(texture.sample_initialized());
    queue.wait().unwrap();
    for (level, view) in views.iter().enumerate() {
        assert_eq!(
            view.readback(full(view)).unwrap().wait().unwrap(),
            vec![
                17 + level as u8 * 14;
                view.size().width as usize * view.size().height as usize * 4
            ]
        );
    }
    let alias = views[1].mip_view(0).unwrap();
    assert_eq!(
        alias.readback(full(&alias)).unwrap().wait().unwrap(),
        vec![31; 12]
    );
    assert!(views[1].mip_view(1).is_err());
    assert!(texture.mip_view(3).is_err());

    let destination = Texture::new(
        &device,
        8,
        4,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Trilinear,
        false,
    )
    .unwrap();
    destination
        .upload(&queue, full(&destination), &[85; 128], None, 0, None)
        .unwrap();
    let target = destination.mip_view(1).unwrap();
    target
        .copy_from_texture(
            &mut queue.recording().unwrap(),
            &views[1],
            full(&views[1]),
            rect(1, 0, 3, 1),
        )
        .unwrap();
    queue.wait().unwrap();
    let mut expected = vec![0; 32];
    expected[4..16].fill(31);
    assert_eq!(
        target.readback(full(&target)).unwrap().wait().unwrap(),
        expected
    );
    assert_eq!(
        destination
            .readback(full(&destination))
            .unwrap()
            .wait()
            .unwrap(),
        vec![85; 128]
    );
    assert!(!destination.mip_view(2).unwrap().initialized());

    let mut clear_submission = Submission::new(&device).unwrap();

    let mut clear = clear_submission.recording().unwrap();
    views[2]
        .transition(&mut clear, wgt::TextureUses::COLOR_TARGET)
        .unwrap();
    unsafe {
        let encoder = clear.encoder();
        encoder
            .begin_render_pass(&hal::RenderPassDescriptor {
                label: Some("WR mip attachment test"),
                extent: views[2].size(),
                sample_count: 1,
                color_attachments: &[Some(hal::ColorAttachment {
                    target: hal::Attachment {
                        view: views[2].target_view().unwrap(),
                        usage: wgt::TextureUses::COLOR_TARGET,
                    },
                    depth_slice: None,
                    resolve_target: None,
                    ops: hal::AttachmentOps::LOAD_CLEAR | hal::AttachmentOps::STORE,
                    clear_value: wgt::Color {
                        r: 1.0,
                        g: 0.0,
                        b: 0.0,
                        a: 1.0,
                    },
                })],
                depth_stencil_attachment: None,
                multiview_mask: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            })
            .unwrap();
        encoder.end_render_pass();
    }
    views[2].initialize(&mut clear).unwrap();
    drop(clear);
    clear_submission.submit().unwrap();
    clear_submission.wait(None).unwrap();
    assert_eq!(
        views[2].readback(full(&views[2])).unwrap().wait().unwrap(),
        [255, 0, 0, 255]
    );
    assert_eq!(
        texture.readback(full(&texture)).unwrap().wait().unwrap(),
        vec![17; 84]
    );
    drop(texture);
    assert_eq!(
        alias.readback(full(&alias)).unwrap().wait().unwrap(),
        vec![31; 12]
    );
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn mip_views_share_state_and_prevent_premature_pool_reuse() {
    let device = device();
    let texture = Texture::new(
        &device,
        8,
        2,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Trilinear,
        false,
    )
    .unwrap();
    let level1 = texture.mip_view(1).unwrap();
    let alias = level1.mip_view(0).unwrap();
    let level2 = texture.mip_view(2).unwrap();
    let mut first_submission = Submission::new(&device).unwrap();
    let mut first = first_submission.recording().unwrap();
    let mut second_submission = Submission::new(&device).unwrap();
    let mut second = second_submission.recording().unwrap();
    level1
        .transition(&mut first, wgt::TextureUses::COPY_DST)
        .unwrap();
    level1.initialize(&mut first).unwrap();
    assert!(alias.initialized());
    assert!(!texture.initialized());
    assert!(alias
        .transition(&mut second, wgt::TextureUses::COPY_SRC)
        .is_err());
    level2
        .transition(&mut second, wgt::TextureUses::COPY_DST)
        .unwrap();
    assert!(texture
        .transition(&mut second, wgt::TextureUses::RESOURCE)
        .is_err());
    assert_eq!(texture.current_usage(), wgt::TextureUses::UNINITIALIZED);
    drop(first);
    drop(first_submission);
    assert!(!alias.initialized());
    assert_eq!(level1.current_usage(), wgt::TextureUses::UNINITIALIZED);
    drop(second);
    second_submission.submit().unwrap();
    second_submission.wait(None).unwrap();
    let queue = upload_queue(&device);
    alias
        .upload(&queue, full(&alias), &[47; 16], None, 0, None)
        .unwrap();
    queue.wait().unwrap();
    assert!(level1.initialized());
    assert_eq!(level2.current_usage(), wgt::TextureUses::COPY_DST);
    let mut abandoned_submission = Submission::new(&device).unwrap();
    let mut abandoned = abandoned_submission.recording().unwrap();
    alias.invalidate(&mut abandoned).unwrap();
    assert!(!level1.initialized());
    drop(abandoned);
    drop(abandoned_submission);
    assert!(level1.initialized());

    let mut pool = TexturePool::new(&device);
    let original = pool
        .acquire(4, 4, wgt::TextureFormat::Rgba8Unorm, true)
        .unwrap();
    let original_id = Rc::as_ptr(&original);
    let view = original.mip_view(0).unwrap();
    assert_eq!(view.filter(), TextureFilter::Nearest);
    drop(original);
    let other = pool
        .acquire(4, 4, wgt::TextureFormat::Rgba8Unorm, true)
        .unwrap();
    assert_ne!(Rc::as_ptr(&other), original_id);
    view.upload(&queue, full(&view), &[59; 64], None, 0, None)
        .unwrap();
    queue.submit().unwrap();
    drop(view);
    let pending = pool
        .acquire(4, 4, wgt::TextureFormat::Rgba8Unorm, true)
        .unwrap();
    assert_ne!(Rc::as_ptr(&pending), original_id);
    queue.wait().unwrap();
    let reused = pool
        .acquire(4, 4, wgt::TextureFormat::Rgba8Unorm, true)
        .unwrap();
    assert_eq!(Rc::as_ptr(&reused), original_id);
    assert_eq!(
        reused.readback(full(&reused)).unwrap().wait().unwrap(),
        vec![59; 64]
    );
    let plain = Texture::new(
        &device,
        1,
        1,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        false,
    )
    .unwrap();
    assert!(plain.mip_view(0).is_err());
    let depth = pool
        .acquire(1, 1, wgt::TextureFormat::Depth32Float, true)
        .unwrap();
    assert!(depth.mip_view(0).unwrap().target_view().is_some());
    device.lost.set(true);
    assert!(texture.mip_view(0).is_err());
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
