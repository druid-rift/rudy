use rudy_core::{
    ObservedTarget, RequestedExceptions, SystemProtection, TargetSafetyError, TargetSafetyPolicy,
    TargetTransport,
};

fn ordinary_internal_disk() -> ObservedTarget {
    ObservedTarget {
        transport: TargetTransport::Other,
        size_bytes: 500_000_000_000,
        system_protection: SystemProtection::Clear,
    }
}

#[test]
fn internal_disk_requires_an_explicit_dangerous_drive_exception() {
    let error =
        TargetSafetyPolicy::authorize(ordinary_internal_disk(), RequestedExceptions::default())
            .unwrap_err();
    assert_eq!(
        error,
        TargetSafetyError::InternalTransport(TargetTransport::Other)
    );

    let authorized = TargetSafetyPolicy::authorize(
        ordinary_internal_disk(),
        RequestedExceptions {
            internal_drive: true,
            oversized: false,
        },
    )
    .expect("explicit internal-drive exception authorizes a non-system disk");
    assert_eq!(authorized.transport(), TargetTransport::Other);
}

#[test]
fn oversized_disk_requires_its_own_exception_regardless_of_transport() {
    let oversized_usb = ObservedTarget {
        transport: TargetTransport::Usb,
        size_bytes: 2_000_000_000_001,
        system_protection: SystemProtection::Clear,
    };
    assert_eq!(
        TargetSafetyPolicy::authorize(oversized_usb.clone(), RequestedExceptions::default())
            .unwrap_err(),
        TargetSafetyError::Oversized {
            size_bytes: 2_000_000_000_001,
            threshold_bytes: 2_000_000_000_000,
        }
    );

    TargetSafetyPolicy::authorize(
        oversized_usb,
        RequestedExceptions {
            internal_drive: false,
            oversized: true,
        },
    )
    .expect("independent oversized acknowledgement authorizes an external disk");

    let oversized_internal = ObservedTarget {
        transport: TargetTransport::Other,
        size_bytes: 2_000_000_000_001,
        system_protection: SystemProtection::Clear,
    };
    assert!(matches!(
        TargetSafetyPolicy::authorize(
            oversized_internal,
            RequestedExceptions {
                internal_drive: true,
                oversized: false,
            },
        ),
        Err(TargetSafetyError::Oversized { .. })
    ));
}

#[test]
fn system_roles_and_incomplete_evidence_are_never_overridable() {
    let all_exceptions = RequestedExceptions {
        internal_drive: true,
        oversized: true,
    };
    for (protection, expected) in [
        (
            SystemProtection::Protected("root filesystem".into()),
            TargetSafetyError::ProtectedSystem("root filesystem".into()),
        ),
        (
            SystemProtection::EvidenceUnavailable("pagefile scan failed".into()),
            TargetSafetyError::EvidenceUnavailable("pagefile scan failed".into()),
        ),
    ] {
        let error = TargetSafetyPolicy::authorize(
            ObservedTarget {
                transport: TargetTransport::Usb,
                size_bytes: 64_000_000_000,
                system_protection: protection,
            },
            all_exceptions,
        )
        .unwrap_err();
        assert_eq!(error, expected);
    }
}

#[test]
fn missing_capacity_evidence_is_never_authorized() {
    let error = TargetSafetyPolicy::authorize(
        ObservedTarget {
            transport: TargetTransport::Usb,
            size_bytes: 0,
            system_protection: SystemProtection::Clear,
        },
        RequestedExceptions {
            internal_drive: true,
            oversized: true,
        },
    )
    .unwrap_err();

    assert_eq!(
        error,
        TargetSafetyError::EvidenceUnavailable("target capacity is unavailable".into())
    );
}

/// A transport that could not be established is refused by its own name, and no
/// acknowledgement reaches it.
///
/// This is the distinction `TargetTransport::Unknown` exists to make. `Other`
/// is a conclusion — the kernel's topology was read, and this disk is not on a
/// removable-class bus — so a user may knowingly override it. `Unknown` is the
/// absence of a conclusion, and an internal-drive acknowledgement that covered
/// it would let a device nobody could classify be erased on the strength of a
/// claim about a different device.
#[test]
fn an_unclassifiable_transport_is_never_authorized() {
    let error = TargetSafetyPolicy::authorize(
        ObservedTarget {
            transport: TargetTransport::Unknown,
            size_bytes: 8 * 1024 * 1024 * 1024,
            system_protection: SystemProtection::Clear,
        },
        RequestedExceptions {
            internal_drive: true,
            oversized: true,
        },
    )
    .unwrap_err();

    assert_eq!(error, TargetSafetyError::TransportUnknown);

    // And the acknowledgement still does what it is for, so the refusal above
    // is a distinction rather than a blanket.
    let authorized = TargetSafetyPolicy::authorize(
        ObservedTarget {
            transport: TargetTransport::Other,
            size_bytes: 8 * 1024 * 1024 * 1024,
            system_protection: SystemProtection::Clear,
        },
        RequestedExceptions {
            internal_drive: true,
            oversized: false,
        },
    )
    .expect("an acknowledged internal disk is still authorized");
    assert_eq!(authorized.transport(), TargetTransport::Other);
}
