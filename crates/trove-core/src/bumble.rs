//! Bumble — import via Bumble's official data-request export (JSON).
//! Dating app: ships with opt-in acknowledgement and vault isolation.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/bumble.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "bumble",
        name: "Bumble",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Imports your Bumble match history and conversation data \
                      from Bumble's official GDPR data-request export. \
                      Requires an explicit opt-in due to the sensitive nature \
                      of dating-app data.",
        domain: "social",
        vault_path: "social/bumble/",
        toggleable: false,
        setup: &[],
        caveats: "Bumble's export processing takes up to 30 days; the JSON \
                  schema is community-documented — parser ships once a real \
                  sample is confirmed.",
    },
    behavior: Behavior::NotWired,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
