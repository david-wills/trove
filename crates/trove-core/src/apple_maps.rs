//! Apple Maps Visited Places — catalogued, permanently unavailable.
//!
//! Catalogued in the Phase 2 pass; brief: docs/integrations/apple-maps.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "apple-maps",
        name: "Apple Maps Visited Places",
        kind: IntegrationKind::Import,
        default_on: false,
        description:
            "The visited-places log iOS 26 keeps in the Maps app of places you have been. \
             Includes place names, coordinates, and visit duration.",
        domain: "location",
        vault_path: "location/apple-maps/",
        toggleable: false,
        setup: &[],
        caveats: "",
    },
    behavior: Behavior::Unavailable {
        reason: "Apple Maps Visited Places is an on-device iOS feature whose data is \
                 end-to-end encrypted with device keys — no app, including Trove, can \
                 read it, and Apple provides no export. The feature does not exist on \
                 macOS. Location history from other sources such as Google Timeline or \
                 GPS logger apps can cover similar data.",
    },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
