//! Epic Games Store — unavailable; no personal playtime API or export.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/epic-games.md

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "epic-games",
        name: "Epic Games Store",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description:
            "Epic Games Store library and playtime. No personal API or \
             structured export exists; Epic data may appear via GOG Galaxy \
             when its Epic integration plugin is enabled.",
        domain: "gaming",
        vault_path: "",
        toggleable: false,
        setup: &[],
        caveats: "",
    },
    behavior: Behavior::Unavailable {
        reason: "Epic offers no personal playtime API or structured data export. \
                 The account data-request archive contains account history but \
                 no playtime records. Epic games do appear in GOG Galaxy's local \
                 database when its Epic integration plugin is enabled — that \
                 path is covered by the GOG Galaxy integration. No standalone \
                 Epic path exists.",
    },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
