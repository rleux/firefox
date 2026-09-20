/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use api::units::{DeviceIntPoint, DeviceIntRect, DeviceIntSize};

const SIZE: [u32; 2] = [640, 480];

fn rect(x: i32, y: i32, width: i32, height: i32) -> DeviceIntRect {
    DeviceIntRect::from_origin_and_size(
        DeviceIntPoint::new(x, y),
        DeviceIntSize::new(width, height),
    )
}

#[test]
fn first_unknown_and_nonlatest_outputs_require_full_repair() {
    let mut history = OutputHistory::default();
    assert_eq!(history.repair(101, SIZE, None), Repair::Full);
    history.record(101, SIZE, rect(8, 12, 16, 20));
    assert_eq!(history.repair(101, SIZE, None), Repair::Full);
    assert_eq!(history.repair(101, SIZE, Some(101)), Repair::Unchanged);

    history.record(109, SIZE, rect(40, 44, 24, 28));
    assert_eq!(history.repair(101, SIZE, Some(101)), Repair::Full);
    assert_eq!(history.repair(101, SIZE, Some(109)), Repair::Full);
    assert_eq!(history.repair(109, SIZE, Some(107)), Repair::Full);
}

#[test]
fn gapped_serials_accumulate_damage_for_rotating_images() {
    let first = rect(8, 12, 16, 20);
    let second = rect(80, 24, 24, 28);
    let third = rect(32, 96, 20, 16);
    let fourth = rect(144, 120, 12, 32);
    let mut history = OutputHistory::default();
    history.record(101, SIZE, first);
    history.record(109, SIZE, second);
    assert_eq!(history.repair(109, SIZE, Some(101)), Repair::Partial(second));
    history.record(140, SIZE, third);
    assert_eq!(
        history.repair(140, SIZE, Some(101)),
        Repair::Partial(second.union(&third)),
    );
    assert_eq!(history.repair(140, SIZE, Some(109)), Repair::Partial(third));
    history.record(181, SIZE, fourth);
    assert_eq!(
        history.repair(181, SIZE, Some(109)),
        Repair::Partial(third.union(&fourth)),
    );
    assert_eq!(history.repair(181, SIZE, Some(181)), Repair::Unchanged);
}

#[test]
fn size_changes_break_the_previous_image_boundary() {
    let resized = [800, 600];
    let mut history = OutputHistory::default();
    history.record(101, SIZE, rect(8, 12, 16, 20));
    history.record(109, resized, rect(20, 24, 32, 36));
    assert_eq!(history.repair(109, resized, Some(101)), Repair::Full);
    assert_eq!(history.repair(109, SIZE, Some(109)), Repair::Full);
    history.record(140, resized, rect(96, 64, 20, 24));
    assert_eq!(
        history.repair(140, resized, Some(109)),
        Repair::Partial(rect(96, 64, 20, 24)),
    );
}

#[test]
fn invalid_or_full_union_damage_requires_full_repair() {
    for damage in [
        DeviceIntRect::zero(),
        rect(-1, 0, 8, 8),
        rect(632, 472, 16, 16),
    ] {
        let mut history = OutputHistory::default();
        history.record(101, SIZE, rect(8, 12, 16, 20));
        history.record(109, SIZE, damage);
        assert_eq!(history.repair(109, SIZE, Some(101)), Repair::Full);
    }

    let mut history = OutputHistory::default();
    history.record(101, SIZE, rect(8, 12, 16, 20));
    history.record(109, SIZE, rect(0, 0, 320, 480));
    history.record(140, SIZE, rect(320, 0, 320, 480));
    assert_eq!(history.repair(140, SIZE, Some(101)), Repair::Full);
}

#[test]
fn bounded_history_rejects_evicted_image_versions() {
    let serials = [
        101, 103, 107, 109, 113, 127, 131, 137, 139, 149, 151,
        157, 163, 167, 173, 179, 181, 191, 193, 197, 199, 211,
        223, 227, 229, 233, 239, 241, 251, 257, 263, 269, 271,
    ];
    let damage = rect(8, 12, 16, 20);
    let mut history = OutputHistory::default();
    for serial in serials {
        history.record(serial, SIZE, damage);
    }
    assert_eq!(history.repair(271, SIZE, Some(101)), Repair::Full);
    assert_eq!(history.repair(271, SIZE, Some(103)), Repair::Partial(damage));
    assert_eq!(history.repair(271, SIZE, Some(271)), Repair::Unchanged);
}
