//! Sunsama — daily-planning task manager. Catalogued in the Phase 2 pass;
//! brief: docs/integrations/sunsama.md

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "sunsama",
        name: "Sunsama",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "A daily-planning app that aggregates tasks from Todoist, \
                       Asana, Linear, and Jira into a focused daily schedule. \
                       Connect those upstream sources directly for richer history.",
        domain: "tasks",
        vault_path: "",
        toggleable: false,
        setup: &[],
        caveats: "",
    },
    behavior: Behavior::Unavailable {
        reason: "Sunsama has no public API — access is gated behind the \
                 $65/month Power Pro plan and there is no reliable data export. \
                 Connect the upstream sources Sunsama pulls from (Todoist, \
                 Asana, Linear, Jira) instead; those integrations give you the \
                 same task history with full detail.",
    },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
