//! Motion — AI-scheduling task and calendar manager via the official REST API.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/motion.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "motion",
        name: "Motion",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Syncs your tasks and auto-scheduled slots from Motion. An active paid \
                      subscription is required to generate an API key.",
        domain: "tasks",
        vault_path: "tasks/motion/",
        toggleable: false,
        setup: &[],
        caveats: "API access requires a paid Motion subscription.",
    },
    behavior: Behavior::NotWired,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
