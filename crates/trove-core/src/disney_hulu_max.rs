//! Disney+ / Hulu / Max watch history — permanently unavailable (no stable export or API).
//! Catalogued in the Phase 2 pass; brief: docs/integrations/disney-hulu-max.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "disney-hulu-max",
        name: "Disney+ / Hulu / Max",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Watch history from Disney+, Hulu, and Max. \
                      Currently unavailable — scrobble to Trakt for ongoing capture.",
        domain: "media",
        vault_path: "",
        toggleable: false,
        setup: &[],
        caveats: "",
    },
    behavior: Behavior::Unavailable {
        reason: "No API and no documented export. Privacy-portal requests take up to \
                 30 days, return undocumented formats that vary by service and region, \
                 and download links expire quickly. Scrobble to Trakt for ongoing \
                 capture instead.",
    },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
