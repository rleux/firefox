/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::wgpu::{Options, Submission};
use crate::device::wgpu::tests::{validation_logging, Command, ERRORS};
use std::sync::atomic::Ordering;

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn noncoherent_uniform_updates_flush_once_before_submission() {
    validation_logging();
    let owner = Rc::new(Device::new(&Options { validation: true, ..Default::default() }).unwrap());
    let mut cache = crate::device::wgpu::binding_cache::BindingCache::default();
    let mut submission = Submission::new(&owner).unwrap();
    let mut last = None;
    {
        let mut commands = submission.recording().unwrap();
        for value in 0..3 {
            let (buffer, offset) = cache.uniform(&mut commands, &owner, [value; 16], |_, size| {
                let mut buffer = Buffer::new_with(&owner, size, wgt::BufferUses::UNIFORM, |_| Ok(())).unwrap();
                Rc::get_mut(&mut buffer).unwrap().mapping.is_coherent = false;
                Ok(buffer)
            }).unwrap();
            buffer.transition(&mut commands, wgt::BufferUses::UNIFORM).unwrap();
            last = Some((buffer, u64::from(offset) + 64));
        }
        assert!(!owner.trace.borrow().iter().any(|c| matches!(c, Command::FlushUniform(_))));
    }
    let (buffer, end) = last.unwrap();
    submission.submit().unwrap();
    assert_eq!(owner.trace.borrow().iter().filter_map(|c| match c { Command::FlushUniform(end) => Some(*end), _ => None }).collect::<Vec<_>>(), [end]);
    assert_eq!(buffer.dirty_end.get(), 0);
    submission.wait(None).unwrap();
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
