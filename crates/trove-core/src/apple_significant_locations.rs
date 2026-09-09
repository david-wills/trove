//! Apple Significant Locations — catalogued, permanently unavailable.
//!
//! The canonical hard-blocked source (and the reference for Phase 2's
//! unavailable stubs): macOS keeps the significant-locations cache
//! end-to-end encrypted with Secure-Enclave-held keys, so Full Disk Access
//! opens the file but the content is unreadable by any process but Apple's.
//! Catalogued anyway per the pipeline doctrine — the greyed card answers
//! "why isn't this available?" so nobody re-researches it.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "apple-significant-locations",
        name: "Apple Significant Locations",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description:
            "The location history macOS keeps of places you visit and frequent.",
        domain: "location",
        vault_path: "location/apple-significant-locations/",
        toggleable: false,
        setup: &[],
        caveats: "",
    },
    behavior: Behavior::Unavailable {
        reason: "macOS encrypts Significant Locations with keys held in the \
                 Secure Enclave — the cache is unreadable by any other \
                 process, even with Full Disk Access, and Apple offers no \
                 export. Location history will come from sources like Apple \
                 Photos geotags and GPS workout files instead.",
    },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
