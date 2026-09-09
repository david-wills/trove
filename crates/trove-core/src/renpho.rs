//! Renpho smart scale body-composition data via manual CSV export.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/renpho.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "renpho",
        name: "Renpho",
        kind: IntegrationKind::Import,
        default_on: false,
        description:
            "Import body-composition readings (weight, body fat, muscle \
             mass, and more) from a Renpho smart scale CSV export. Weight \
             totals already reach Apple Health via the Renpho app.",
        domain: "health",
        vault_path: "health/renpho/",
        toggleable: false,
        setup: &[],
        caveats:
            "The unofficial Renpho API is fragile; manual CSV export is \
             the reliable path. Body-composition detail not available in \
             Apple Health export is the primary value.",
    },
    behavior: Behavior::NotWired,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
