//! How a drive's status is reported, on every surface.
//!
//! testing ticket 23
//! is three faults in one, and two of them are rendering: a status that could
//! not fit its column, and "I could not look" reported as "I looked and could
//! not tell". Both are decided here rather than in the CLI or the GUI, so the
//! two surfaces cannot answer differently.

use rudy_core::models::{PartitionScheme, ProbeObstacle, RudyStatus, STATUS_LABEL_MAX_CHARS};
use std::io::{Error, ErrorKind};

fn every_status() -> Vec<RudyStatus> {
    vec![
        RudyStatus::Installed {
            version: Some("1.0.99".into()),
            partition_scheme: PartitionScheme::Gpt,
        },
        RudyStatus::Installed {
            version: None,
            partition_scheme: PartitionScheme::Mbr,
        },
        // A version string is read off the drive, so it is attacker-adjacent
        // input as far as the table is concerned.
        RudyStatus::Installed {
            version: Some("9.9.9-rc1+build.20260829.deadbeef".into()),
            partition_scheme: PartitionScheme::Gpt,
        },
        RudyStatus::NotInstalled,
        RudyStatus::LayoutOnly,
        RudyStatus::corrupt(
            "Partition 2 is 12345 sectors, expected exactly 65536 (32 MiB); \
             this does not look like a Rudy drive",
        ),
        RudyStatus::unreadable(
            &Error::from(ErrorKind::PermissionDenied),
            "Cannot open /dev/sdb to read it: Permission denied (os error 13)",
        ),
        RudyStatus::unreadable(
            &Error::from(ErrorKind::NotFound),
            "Cannot open /dev/sdz to read it: No such file or directory (os error 2)",
        ),
    ]
}

/// The third fault: the CLI put a whole `io::Error` in a 16-column field, and
/// the table stopped being a table. Nothing a drive carries may widen a column.
#[test]
fn no_status_can_overflow_the_table_column() {
    for status in every_status() {
        let label = status.short_label();
        assert!(
            label.chars().count() <= STATUS_LABEL_MAX_CHARS,
            "{label:?} is {} chars, over the {STATUS_LABEL_MAX_CHARS}-char column",
            label.chars().count()
        );
        assert!(
            !label.contains('\n'),
            "{label:?} would break the row across two lines"
        );
    }
}

#[test]
fn a_long_version_is_elided_rather_than_allowed_to_smash_the_row() {
    let status = RudyStatus::Installed {
        version: Some("9.9.9-rc1+build.20260829.deadbeef".into()),
        partition_scheme: PartitionScheme::Gpt,
    };

    assert_eq!(status.short_label(), "Installed (9.9.9-r…)");
}

/// The second fault: `Unknown` meant both "I could not look" and "I looked and
/// could not tell". The reason has to reach the user, and so does what to do
/// about it — but not through the column.
#[test]
fn an_unreadable_drive_keeps_its_reason_and_gains_a_remedy() {
    let status = RudyStatus::unreadable(
        &Error::from(ErrorKind::PermissionDenied),
        "Cannot open /dev/sdb to read it: Permission denied (os error 13)",
    );

    assert_eq!(status.short_label(), "Unreadable");
    let detail = status.detail().expect("an unreadable drive owes a reason");
    assert!(detail.contains("Permission denied (os error 13)"));
    assert!(
        detail.contains("privileges"),
        "the remedy must be there too: {detail}"
    );
    assert!(!status.was_probed());
}

#[test]
fn a_status_read_from_the_drive_counts_as_evidence() {
    for status in [
        RudyStatus::NotInstalled,
        RudyStatus::corrupt("truncated GPT"),
        RudyStatus::Installed {
            version: None,
            partition_scheme: PartitionScheme::Gpt,
        },
    ] {
        assert!(
            status.was_probed(),
            "{status:?} was read off a drive and is a finding"
        );
    }
}

/// Not every failure is a permission problem, and the remedies differ.
#[test]
fn the_obstacle_is_classified_from_the_io_error() {
    assert_eq!(
        ProbeObstacle::classify(&Error::from(ErrorKind::PermissionDenied)),
        ProbeObstacle::PermissionDenied
    );
    assert_eq!(
        ProbeObstacle::classify(&Error::from(ErrorKind::NotFound)),
        ProbeObstacle::NotFound
    );
    assert_eq!(
        ProbeObstacle::classify(&Error::from(ErrorKind::BrokenPipe)),
        ProbeObstacle::Io
    );

    // Nothing suggests widening access — that is a host system-settings change
    // and explicitly not what the ticket asked for.
    assert!(!ProbeObstacle::PermissionDenied
        .remedy()
        .contains("disk group"));
}

/// `Unknown(String)` and `Corrupt(String)` were newtype variants of an
/// internally tagged enum, which serde cannot serialize at all — every attempt
/// was a runtime error, so the reason a drive could not be read was reachable
/// from no machine-readable surface. Struct variants are what fixed it.
#[test]
fn every_status_survives_a_json_round_trip() {
    for status in every_status() {
        let json = serde_json::to_string(&status)
            .unwrap_or_else(|error| panic!("{status:?} must serialize: {error}"));
        let back: RudyStatus = serde_json::from_str(&json).expect("and must come back");
        assert_eq!(back, status);
    }
}

#[test]
fn json_carries_the_whole_reason_the_column_could_not() {
    let status = RudyStatus::unreadable(
        &Error::from(ErrorKind::PermissionDenied),
        "Cannot open /dev/sdb to read it: Permission denied (os error 13)",
    );

    let json = serde_json::to_string(&status).expect("serialize");
    assert!(json.contains("\"status\":\"unreadable\""), "{json}");
    assert!(
        json.contains("\"obstacle\":\"permission_denied\""),
        "{json}"
    );
    assert!(json.contains("Permission denied (os error 13)"), "{json}");
}
