//! Tinder — official GDPR export import (swipes, matches, messages).
//! Dating app: opt-in with explicit acknowledgement; vault-isolated.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/tinder.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "tinder",
        name: "Tinder",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your Tinder data export — swipe history, matches, \
                      and messages. This is sensitive dating-app data; Trove \
                      keeps it vault-isolated and never indexes it into shared \
                      views without explicit opt-in.",
        domain: "social",
        vault_path: "social/tinder/",
        toggleable: false,
        setup: &[],
        caveats: "Dating-app data requires explicit opt-in acknowledgement. \
                  Export download links expire after 48 hours — import promptly. \
                  Match entries contain IDs only, not names.",
    },
    behavior: Behavior::NotWired,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
