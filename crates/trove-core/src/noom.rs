//! Noom — weight loss and nutrition coaching app; no self-serve export.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/noom.md

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "noom",
        name: "Noom",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Noom weight-loss and nutrition coaching app. \
                      Nutrition logged to Apple Health on iPhone is captured \
                      by the Apple Health export.",
        domain: "health",
        vault_path: "health/nutrition/",
        toggleable: false,
        setup: &[],
        caveats: "",
    },
    behavior: Behavior::Unavailable {
        reason: "Noom has no self-serve export or developer API — your data \
                 is only retrievable via a GDPR/CCPA request to gdprsupport@noom.com \
                 (up to 30 days, undocumented format). Nutrition you logged to \
                 Apple Health on iPhone is already captured by the Apple Health \
                 export; dedicated trackers like Cronometer or MyFitnessPal \
                 provide richer structured exports.",
    },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
