//! Amazing Marvin — subscription-gated task/habit manager with a cloud API.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/amazing-marvin.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "amazing-marvin",
        name: "Amazing Marvin",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Pulls your tasks, projects, habits, and daily check-ins from Amazing \
                      Marvin. Requires an active subscription and an API key from your account \
                      settings.",
        domain: "tasks",
        vault_path: "tasks/amazing-marvin/",
        toggleable: false,
        setup: &[],
        caveats: "API access requires a paid Amazing Marvin subscription.",
    },
    behavior: Behavior::NotWired,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
