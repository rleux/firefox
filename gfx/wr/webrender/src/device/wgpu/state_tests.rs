/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

#[test]
fn abandoned_usage_changes_preserve_committed_state() {
    let state = super::super::state::UsageState::new(wgt::TextureUses::UNINITIALIZED);
    let first = Rc::new(());
    let second = Rc::new(());
    assert_eq!(
        state.prepare(&first, wgt::TextureUses::COPY_DST).unwrap(),
        (wgt::TextureUses::UNINITIALIZED, true)
    );
    assert!(state.prepare(&second, wgt::TextureUses::RESOURCE).is_err());
    assert_eq!(state.current(), wgt::TextureUses::COPY_DST);
    drop(first);
    assert_eq!(state.current(), wgt::TextureUses::UNINITIALIZED);
    assert_eq!(
        state.prepare(&second, wgt::TextureUses::COPY_DST).unwrap(),
        (wgt::TextureUses::UNINITIALIZED, true)
    );
    assert_eq!(
        state.prepare(&second, wgt::TextureUses::RESOURCE).unwrap(),
        (wgt::TextureUses::COPY_DST, false)
    );
    state.commit();
    drop(second);
    assert_eq!(state.current(), wgt::TextureUses::RESOURCE);
    let abandoned = Rc::new(());
    state
        .prepare(&abandoned, wgt::TextureUses::COPY_SRC)
        .unwrap();
    drop(abandoned);
    assert_eq!(state.current(), wgt::TextureUses::RESOURCE);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn resource_transitions_commit_and_discard() {
    validation_logging();
    let device = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    eprintln!("Vulkan adapter: {:?}", device.info());
    let buffer = Buffer::new(
        &device,
        &[1, 2, 3, 4],
        wgt::BufferUses::COPY_SRC | wgt::BufferUses::VERTEX,
    )
    .unwrap();
    let texture = Texture::new(
        &device,
        7,
        5,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Nearest,
        true,
    )
    .unwrap();
    let mut first_submission = Submission::new(&device).unwrap();
    let mut first = first_submission.recording().unwrap();
    let mut competing_submission = Submission::new(&device).unwrap();
    let mut competing = competing_submission.recording().unwrap();
    buffer
        .transition(&mut first, wgt::BufferUses::VERTEX)
        .unwrap();
    drop(first);
    assert_eq!(buffer.current_usage(), wgt::BufferUses::VERTEX);
    let mut first = first_submission.recording().unwrap();
    buffer
        .transition(&mut first, wgt::BufferUses::VERTEX)
        .unwrap();
    texture
        .transition(&mut first, wgt::TextureUses::COPY_DST)
        .unwrap();
    assert!(buffer
        .transition(&mut competing, wgt::BufferUses::COPY_SRC)
        .is_err());
    assert!(texture
        .transition(&mut competing, wgt::TextureUses::COLOR_TARGET)
        .is_err());
    assert_eq!(buffer.current_usage(), wgt::BufferUses::VERTEX);
    assert_eq!(texture.current_usage(), wgt::TextureUses::COPY_DST);
    drop(first);
    drop(first_submission);
    assert_eq!(buffer.current_usage(), wgt::BufferUses::MAP_WRITE);
    assert_eq!(texture.current_usage(), wgt::TextureUses::UNINITIALIZED);
    for usage in [
        wgt::BufferUses::empty(),
        wgt::BufferUses::COPY_DST,
        wgt::BufferUses::MAP_WRITE | wgt::BufferUses::COPY_SRC,
    ] {
        assert!(buffer.transition(&mut competing, usage).is_err());
    }
    for usage in [
        wgt::TextureUses::UNINITIALIZED,
        wgt::TextureUses::PRESENT,
        wgt::TextureUses::COPY_SRC | wgt::TextureUses::COPY_DST,
    ] {
        assert!(texture.transition(&mut competing, usage).is_err());
    }
    drop(competing);
    drop(competing_submission);
    assert_eq!(read_upload(&device, &buffer), [1, 2, 3, 4]);
    let expected = super::texture::clear_and_read(&device, texture.clone());
    assert_eq!(texture.current_usage(), wgt::TextureUses::COPY_SRC);
    let mut abandoned_submission = Submission::new(&device).unwrap();
    let mut abandoned = abandoned_submission.recording().unwrap();
    texture
        .transition(&mut abandoned, wgt::TextureUses::COPY_DST)
        .unwrap();
    drop(abandoned);
    drop(abandoned_submission);
    assert_eq!(texture.current_usage(), wgt::TextureUses::COPY_SRC);
    assert_eq!(
        super::texture::clear_and_read(&device, texture.clone()),
        expected
    );

    let mipmapped = Texture::new(
        &device,
        7,
        5,
        wgt::TextureFormat::Rgba8Unorm,
        TextureFilter::Trilinear,
        true,
    )
    .unwrap();
    let mut mip_commands_submission = Submission::new(&device).unwrap();
    let mut mip_commands = mip_commands_submission.recording().unwrap();
    mipmapped
        .transition(&mut mip_commands, wgt::TextureUses::RESOURCE)
        .unwrap();
    mipmapped
        .transition(&mut mip_commands, wgt::TextureUses::RESOURCE)
        .unwrap();
    drop(mip_commands);
    mip_commands_submission.submit().unwrap();
    assert!(mip_commands_submission.wait(None).unwrap());
    assert_eq!(mipmapped.current_usage(), wgt::TextureUses::RESOURCE);
    drop(mip_commands_submission);
    drop(mipmapped);

    let other = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    let mut foreign_submission = Submission::new(&other).unwrap();
    let mut foreign = foreign_submission.recording().unwrap();
    assert!(buffer
        .transition(&mut foreign, wgt::BufferUses::COPY_SRC)
        .is_err());
    assert!(texture
        .transition(&mut foreign, wgt::TextureUses::RESOURCE)
        .is_err());
    drop(foreign);
    drop(foreign_submission);
    drop(other);

    let mut blocked = Submission::new(&device).unwrap();
    {
        let mut recording = blocked.recording().unwrap();
        device.lost.set(true);
        let _ = recording.encoder();
    }
    assert!(device.is_lost());
    assert!(Submission::new(&device).is_err());
    assert!(blocked.recording().is_err());
    assert!(blocked.submit().is_err());
    drop(blocked);
    drop(buffer);
    drop(texture);
    assert_eq!(Rc::strong_count(&device), 1);
    drop(device);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
