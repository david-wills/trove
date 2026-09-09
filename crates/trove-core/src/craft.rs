//! Craft — periodic sync via Craft's official API (2025+); the per-connection
//! API endpoint and Bearer token are generated in-app under Settings > API.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/craft.md

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "craft",
        name: "Craft",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Your Craft documents, synced via Craft's official API. \
                       Paste your API endpoint and Bearer token from \
                       Craft's Settings > API.",
        domain: "notes",
        vault_path: "notes/craft/",
        toggleable: false,
        setup: &[],
        caveats: "The API endpoint is per-connection, not a global base URL — \
                  you must paste both the endpoint and the Bearer token.",
    },
    behavior: Behavior::NotWired,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
