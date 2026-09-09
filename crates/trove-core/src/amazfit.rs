//! Amazfit / Zepp Health smartwatch data — per-workout GPX import path.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/amazfit.md

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "amazfit",
        name: "Amazfit (Zepp)",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import workout GPX files exported from the Zepp app on Amazfit \
                       (Xiaomi) smartwatches. Zepp also syncs to Apple Health, so the \
                       Apple Health export already captures most data.",
        domain: "health",
        vault_path: "health/amazfit/",
        toggleable: false,
        setup: &[],
        caveats: "Unofficial API endpoints are unstable; GPX export is the only reliable \
                  path. Build only on confirmed demand.",
    },
    behavior: Behavior::NotWired,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
