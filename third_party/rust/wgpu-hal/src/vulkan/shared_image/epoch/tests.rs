use super::*;
use alloc::vec;

fn descriptor() -> crate::TextureDescriptor<'static> {
    crate::TextureDescriptor {
        label: None,
        size: wgt::Extent3d {
            width: 2,
            height: 2,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgt::TextureDimension::D2,
        format: wgt::TextureFormat::Rgba8Unorm,
        usage: wgt::TextureUses::RESOURCE | wgt::TextureUses::COPY_SRC | wgt::TextureUses::COPY_DST,
        memory_flags: crate::MemoryFlags::empty(),
        view_formats: vec![wgt::TextureFormat::Rgba8UnormSrgb],
    }
}

#[test]
fn marker_acceptance_and_payload_release_are_independent_return_requirements() {
    let mut status = Status {
        closed: false,
        payloads: 0,
        marker: Marker::None,
    };
    assert!(!ready(&status));
    status.closed = true;
    for marker in [
        Marker::None,
        Marker::Reserved,
        Marker::Accepted,
        Marker::Abandoned,
    ] {
        status.marker = marker;
        assert!(!ready(&status));
    }
    status.marker = Marker::Retired;
    status.payloads = 1;
    assert!(!ready(&status));
    status.payloads = 0;
    assert!(ready(&status));
}

#[test]
fn missing_epoch_weak_never_resets_a_completed_reservation() {
    let record = Arc::new(Record {
        descriptor: descriptor(),
        producer: Producer {
            device: Weak::new(),
            progress: Weak::new(),
        },
        status: Mutex::new(Status {
            closed: true,
            payloads: 0,
            marker: Marker::Retired,
        }),
    });
    let slot = Slot {
        phase: Phase::Published {
            serial: 7,
            epoch: Weak::new(),
            _record: record,
        },
    };
    assert!(matches!(current(&slot, 7), Err(ImageLeaseError::Abandoned)));
    assert!(matches!(slot.phase, Phase::Published { serial: 7, .. }));
}

#[test]
fn reused_epoch_descriptor_permissions_are_exact_not_the_physical_superset() {
    let physical = descriptor();
    let mut next = physical.clone();
    next.view_formats.clear();
    next.usage.remove(wgt::TextureUses::COPY_SRC);
    assert!(super::super::compatible(&physical, &next));
    assert!(!same_descriptor(&physical, &next));
    let mut renamed = physical.clone();
    renamed.label = Some("new logical label");
    assert!(same_descriptor(&physical, &renamed));
}

#[test]
fn conditional_close_preserves_payloads_and_does_not_claim_an_existing_marker() {
    let mut status = Status {
        closed: false,
        payloads: 1,
        marker: Marker::None,
    };
    assert_eq!(status.begin_close(true), ImageCloseStatus::Busy);
    assert!(!status.closed);
    assert!(status.marker == Marker::None);
    status.payloads = 0;
    assert_eq!(status.begin_close(true), ImageCloseStatus::Claimed);
    assert!(status.marker == Marker::Reserved);
    status.marker = Marker::Accepted;
    assert_eq!(status.begin_close(true), ImageCloseStatus::AlreadyClosed);
    assert!(status.marker == Marker::Accepted);
    let mut generic = Status {
        closed: false,
        payloads: 1,
        marker: Marker::None,
    };
    assert_eq!(generic.begin_close(false), ImageCloseStatus::Claimed);
    assert_eq!(generic.payloads, 1);
}
