//! Overland (iOS GPS Logger) — live receiver for always-on GPS tracking from the Overland app.
//!
//! Catalogued in the Phase 2 pass; brief: docs/integrations/overland.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "overland",
        name: "Overland (iOS GPS Logger)",
        kind: IntegrationKind::Live,
        default_on: false,
        description:
            "Receive a continuous GPS trail from the open-source Overland iPhone app. \
             Overland batches GeoJSON location points (with speed, altitude, battery, \
             and motion type) and POSTs them to a local endpoint exposed by troved.",
        domain: "location",
        vault_path: "location/overland/",
        toggleable: false,
        setup: &[],
        caveats: "Requires the Overland app on your iPhone and your iPhone to be on the \
                  same network as your Mac (or a configured tunnel).",
    },
    behavior: Behavior::NotWired,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
