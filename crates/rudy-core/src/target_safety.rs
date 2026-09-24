use serde::{Deserialize, Serialize};

pub const OVERSIZED_TARGET_THRESHOLD_BYTES: u64 = 2_000_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetTransport {
    Usb,
    Sd,
    Mmc,
    Other,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemProtection {
    Clear,
    Protected(String),
    EvidenceUnavailable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedTarget {
    pub transport: TargetTransport,
    pub size_bytes: u64,
    pub system_protection: SystemProtection,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestedExceptions {
    pub internal_drive: bool,
    pub oversized: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TargetSafetyError {
    #[error("target transport {0:?} is not USB, SD, or MMC")]
    InternalTransport(TargetTransport),
    /// The transport could not be established at all.
    ///
    /// Kept apart from [`Self::InternalTransport`] because the two are refused
    /// for opposite reasons and only one of them is negotiable. `Other` says
    /// the kernel's topology was read and this disk is not on a removable-class
    /// bus, which a user may acknowledge and override. `Unknown` says nothing
    /// was read, so there is no claim to acknowledge — the internal-drive
    /// exception must not be able to wave through a device that was never
    /// classified.
    #[error("target transport could not be established from kernel topology")]
    TransportUnknown,
    #[error("target has a protected system role: {0}")]
    ProtectedSystem(String),
    #[error("system-disk evidence is incomplete: {0}")]
    EvidenceUnavailable(String),
    #[error(
        "target capacity {size_bytes} bytes exceeds the {threshold_bytes}-byte safety threshold"
    )]
    Oversized {
        size_bytes: u64,
        threshold_bytes: u64,
    },
}

/// Proof that the policy said yes about one observation.
///
/// A capability: the field is private, so `rudy-core` is the only crate that
/// can mint one, and [`TargetSafetyPolicy::authorize`] is the only function
/// here that does. `rudy_platform::AuthorizedTarget::new` requires one by
/// value, which is what makes the post-descriptor authorization a thing the
/// compiler checks rather than a `?` somebody remembered to write.
///
/// **Deliberately not `Clone`.** An approval is about the facts that were on
/// the table when it was minted, and it is consumed by the session it
/// authorizes. Copying one would be the beginning of carrying an earlier
/// approval forward to a later target, which is the thing every layer of this
/// design exists to prevent.
///
/// Named `TargetApproval` rather than `AuthorizedTarget` because the platform
/// crate has a type by the latter name — the session handed to a destructive
/// operation — and `authorized_target.rs` had both in scope at once.
#[derive(Debug)]
pub struct TargetApproval {
    observed: ObservedTarget,
}

impl TargetApproval {
    pub fn transport(&self) -> TargetTransport {
        self.observed.transport
    }

    pub fn size_bytes(&self) -> u64 {
        self.observed.size_bytes
    }
}

pub struct TargetSafetyPolicy;

impl TargetSafetyPolicy {
    /// Logs the verdict, then returns it unchanged.
    ///
    /// The decision itself lives in `decide` so there is exactly one place a
    /// refusal is recorded rather than one per branch — "why did it refuse my
    /// drive" is the single most common question this tool has to answer, and a
    /// log that answers it cannot be allowed to drift from the branch that
    /// actually returned.
    pub fn authorize(
        observed: ObservedTarget,
        exceptions: RequestedExceptions,
    ) -> Result<TargetApproval, TargetSafetyError> {
        let (transport, size_bytes) = (observed.transport, observed.size_bytes);
        let result = Self::decide(observed, exceptions);
        match &result {
            Ok(_) => tracing::info!(
                ?transport,
                size_bytes,
                internal_drive_exception = exceptions.internal_drive,
                oversized_exception = exceptions.oversized,
                "target authorized"
            ),
            Err(reason) => tracing::warn!(
                ?transport,
                size_bytes,
                %reason,
                "target refused"
            ),
        }
        result
    }

    fn decide(
        observed: ObservedTarget,
        exceptions: RequestedExceptions,
    ) -> Result<TargetApproval, TargetSafetyError> {
        match &observed.system_protection {
            SystemProtection::Clear => {}
            SystemProtection::Protected(reason) => {
                return Err(TargetSafetyError::ProtectedSystem(reason.clone()));
            }
            SystemProtection::EvidenceUnavailable(reason) => {
                return Err(TargetSafetyError::EvidenceUnavailable(reason.clone()));
            }
        }
        // Both of the refusals below are missing evidence rather than a
        // policy judgement, so neither consults `exceptions`: there is nothing
        // for a user to acknowledge about a fact nobody established.
        if observed.transport == TargetTransport::Unknown {
            return Err(TargetSafetyError::TransportUnknown);
        }
        if observed.size_bytes == 0 {
            return Err(TargetSafetyError::EvidenceUnavailable(
                "target capacity is unavailable".into(),
            ));
        }
        let external = matches!(
            observed.transport,
            TargetTransport::Usb | TargetTransport::Sd | TargetTransport::Mmc
        );
        if !external && !exceptions.internal_drive {
            return Err(TargetSafetyError::InternalTransport(observed.transport));
        }
        if observed.size_bytes > OVERSIZED_TARGET_THRESHOLD_BYTES && !exceptions.oversized {
            return Err(TargetSafetyError::Oversized {
                size_bytes: observed.size_bytes,
                threshold_bytes: OVERSIZED_TARGET_THRESHOLD_BYTES,
            });
        }
        Ok(TargetApproval { observed })
    }
}
