//! The contract between the runtime and `openloopd`: the NSXPC control plane, the shared-memory
//! session region both processes map, and the wait-free primitives the region is built from.
//! See `docs/ARCHITECTURE.md` § Runtime ↔ openloopd.
//!
//! Everything in the region is written by exactly one process, and each side treats what the
//! other wrote as untrusted: a misbehaving peer can feed garbage or stall its own stream, never
//! make the reader block, panic or touch memory outside the region.
#![cfg(target_os = "macos")]

mod access_unit;
pub mod control;
mod doorbell;
mod pod;
pub mod records;
mod region;
mod ring;
mod snapshot;
#[doc(hidden)]
pub mod testing;
pub mod xpc;

pub use access_unit::{AccessUnitRing, AuConsumer, AuProducer, AuWriter, PopError};
pub use doorbell::{Doorbell, Wake, now_ns};
pub use pod::Pod;
pub use region::{
    AU_SLOT_BYTES, AU_SLOTS, AccessUnits, Events, LAYOUT_FINGERPRINT, RegionError, RegionInfo,
    SessionLayout, SessionRegion, SharedMemory, Zeroable, Zeroed,
};
pub use ring::{Consumer, Corrupt, Producer, PushError, SpscRing};
pub use snapshot::{Publisher, ReadError, Snapshots, Versioned};

/// Bumped on any incompatible change to the NSXPC protocol or the session region. The runtime
/// and `openloopd` refuse each other across versions.
pub const IPC_VERSION: u32 = 1;

/// `openloopd`'s Mach service, registered by its launchd plist.
pub const SERVICE_NAME: &str = "dev.openloop.openloopd";

/// Points the runtime at another `openloopd`'s Mach service, such as a test agent's.
pub const SERVICE_VAR: &str = "OPENLOOP_SERVICE";

/// `openloopd`'s code-signing identifier, which the runtime pins.
pub const OPENLOOPD_IDENTIFIER: &str = "dev.openloop.openloopd";

/// The Apple Development team whose certificate signs `openloopd`: `OPENLOOP_TEAM_ID` at build time
/// (build.rs, set by scripts/signing.sh), empty when the build had none.
pub const TEAM_ID: &str = env!("OPENLOOP_TEAM_ID");

/// What `openloopd`'s signature must satisfy before the runtime exchanges anything with it
/// (csreq(1), TN3127): any process of the user's can register a Mach service name, but only the
/// team that built OpenLoop can sign this identifier. A build without a team checks the identifier
/// alone.
pub fn openloopd_requirement() -> String {
    if TEAM_ID.is_empty() {
        format!("identifier \"{OPENLOOPD_IDENTIFIER}\"")
    } else {
        format!(
            "identifier \"{OPENLOOPD_IDENTIFIER}\" and anchor apple generic \
             and certificate leaf[subject.OU] = \"{TEAM_ID}\""
        )
    }
}

/// The Mach service the runtime connects to: [`SERVICE_VAR`] if set and non-empty, else
/// [`SERVICE_NAME`].
pub fn service_name() -> String {
    std::env::var(SERVICE_VAR)
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| SERVICE_NAME.to_owned())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_requirement_names_the_identifier_and_team() {
        let requirement = super::openloopd_requirement();
        assert!(requirement.contains(super::OPENLOOPD_IDENTIFIER));
        assert!(requirement.contains(super::TEAM_ID));
    }
}
