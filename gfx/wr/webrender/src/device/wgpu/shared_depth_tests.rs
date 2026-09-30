/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

fn target(store: &mut TextureStore, size: DeviceIntSize, depth: bool) -> TextureHandle {
    store
        .create(
            ImageBufferKind::Texture2D,
            ImageFormat::RGBA8,
            size,
            TextureFilter::Nearest,
            Some(RenderTargetInfo { has_depth: depth }),
        )
        .unwrap()
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn depth_targets_share_storage_by_dimensions_and_release_last_user() {
    let device = device();
    let mut store = TextureStore::new(&device);
    let size = DeviceIntSize::new(2, 3);
    let mut first = target(&mut store, size, true);
    let mut second = target(&mut store, size, true);
    let mut third = target(&mut store, size, false);
    let mut other = target(&mut store, DeviceIntSize::new(3, 2), true);
    store
        .reuse_render_target(&mut third, RenderTargetInfo { has_depth: true })
        .unwrap();
    let (first_color, first_depth) = store.render_target(first.target_id, true).unwrap();
    let (second_color, second_depth) = store.render_target(second.target_id, true).unwrap();
    let first_depth = first_depth.unwrap();
    let second_depth = second_depth.unwrap();
    assert!(!Rc::ptr_eq(&first_color, &second_color));
    assert!(Rc::ptr_eq(&first_depth, &second_depth));
    assert!(Rc::ptr_eq(
        &first_depth,
        &store
            .render_target(third.target_id, true)
            .unwrap()
            .1
            .unwrap()
    ));
    assert!(!Rc::ptr_eq(
        &first_depth,
        &store
            .render_target(other.target_id, true)
            .unwrap()
            .1
            .unwrap()
    ));
    assert_eq!(store.depth_bytes(), 48);
    let weak = Rc::downgrade(&first_depth);
    drop(first_depth);
    drop(second_depth);
    store.delete(&mut first).unwrap();
    store.delete(&mut second).unwrap();
    assert_eq!(store.depth_bytes(), 48);
    assert!(weak.upgrade().is_some());
    store
        .reuse_render_target(&mut third, RenderTargetInfo { has_depth: false })
        .unwrap();
    assert_eq!(store.depth_bytes(), 24);
    assert!(weak.upgrade().is_none());
    store
        .reuse_render_target(&mut third, RenderTargetInfo { has_depth: false })
        .unwrap();
    store.delete(&mut third).unwrap();
    store.delete(&mut other).unwrap();
    assert_eq!(store.depth_bytes(), 0);
    assert!(store.depth_targets.is_empty());
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
