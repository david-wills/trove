//! SoundCloud — catalogued but unavailable; no listening-history endpoint exists.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/soundcloud.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "soundcloud",
        name: "SoundCloud",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "SoundCloud listening history in the unified media stream.",
        domain: "media",
        vault_path: "media/plays/soundcloud/",
        toggleable: false,
        setup: &[],
        caveats: "",
    },
    behavior: Behavior::Unavailable {
        reason: "SoundCloud's API has no listening-history endpoint — its activity feed \
                 tracks likes and reposts, not plays — and new developer-app approvals are \
                 slow or blocked. Enable SoundCloud's built-in Last.fm scrobbling in your \
                 SoundCloud settings to capture listens through the Last.fm integration instead.",
    },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
