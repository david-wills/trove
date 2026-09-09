//! SmugMug — photo-hosting service with an OAuth 1.0a API.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/smugmug.md

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "smugmug",
        name: "SmugMug",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Pulls metadata for your SmugMug photos and albums via the \
                      SmugMug API v2 — titles, captions, dates, geo tags, and \
                      album structure. Requires a user-supplied API key (SmugMug \
                      offers no keyless access).",
        domain: "photos",
        vault_path: "photos/smugmug/",
        toggleable: false,
        setup: &[],
        caveats: "SmugMug requires a developer API key from your own account; \
                  no bulk export path exists — the API is the only route.",
    },
    behavior: Behavior::NotWired,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
