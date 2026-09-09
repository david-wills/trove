//! Spotify — streaming history import from the GDPR Extended Streaming History
//! JSON export, with an optional OAuth path for recent plays.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/spotify.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "spotify",
        name: "Spotify",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your full Spotify streaming history — music and podcasts — from \
                      the official GDPR Extended Streaming History export. Includes milliseconds \
                      played and skip data for every track back to account creation.",
        domain: "media",
        vault_path: "media/plays/spotify/",
        toggleable: false,
        setup: &[
            "spotify.com → Account → Privacy settings → Download your data → \
             select Extended streaming history.",
            "Spotify emails a download link in 1–5 days. Import the ZIP here.",
        ],
        caveats: "The GDPR export takes 1–5 days to arrive. The live recently-played API \
                  is capped at roughly 1,275 items and requires a Premium developer-mode app \
                  (5-user limit since February 2026); the export is the recommended path.",
    },
    behavior: Behavior::NotWired,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
