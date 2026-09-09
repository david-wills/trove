//! Habitify — habit tracker with streak and journal data behind a Pro-gated API.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/habitify.md

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "habitify",
        name: "Habitify",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description:
            "Syncs your Habitify habits and daily completion journal via the \
             official API. API access requires a Habitify Pro subscription.",
        domain: "habits",
        vault_path: "habits/habitify/",
        toggleable: false,
        setup: &[],
        caveats: "API access is gated behind a Habitify Pro subscription.",
    },
    behavior: Behavior::NotWired,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
