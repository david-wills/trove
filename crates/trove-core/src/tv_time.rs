//! TV Time — unavailable; no official API or export path.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/tv-time.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "tv-time",
        name: "TV Time",
        kind: IntegrationKind::Import,
        default_on: false,
        description:
            "TV Time watch history. Currently unavailable — no official API \
             or export exists.",
        domain: "media",
        vault_path: "",
        toggleable: false,
        setup: &[],
        caveats: "",
    },
    behavior: Behavior::Unavailable {
        reason: "No official API or export. The only extraction paths are \
                 fragile third-party Chrome extensions (TV Time Out / TV Time \
                 Liberator) that replay TV Time's internal API — Trove cannot \
                 automate a browser extension dependency — and an unreliable \
                 GDPR request whose format varies. Simkl supports TV Time \
                 import directly: migrate your history there and use the \
                 Simkl integration instead.",
    },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
