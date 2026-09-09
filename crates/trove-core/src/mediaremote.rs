//! Mac Now Playing (MediaRemote) — universal live now-playing stream on macOS.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/mediaremote.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "mediaremote",
        name: "Mac Now Playing (MediaRemote)",
        kind: IntegrationKind::Live,
        default_on: false,
        description:
            "Captures now-playing events from any app on this Mac — music, \
             podcasts, video — via the macOS MediaRemote framework. Universal \
             coverage regardless of which player is in use.",
        domain: "media",
        vault_path: "media/plays/mediaremote/",
        toggleable: false,
        setup: &[],
        caveats: "Requires a spike to confirm macOS 15.4 entitlement compatibility; \
                  a Perl adapter or JXA fallback may be needed.",
    },
    behavior: Behavior::NotWired,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
