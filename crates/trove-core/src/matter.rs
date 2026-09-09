//! Matter — read-later app; permanently unavailable (no API or export path).
//! Catalogued in the Phase 2 pass; brief: docs/integrations/matter.md

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "matter",
        name: "Matter",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Matter is a read-later and highlights app popular as a Pocket replacement.",
        domain: "reading",
        vault_path: "",
        toggleable: false,
        setup: &[],
        caveats: "",
    },
    behavior: Behavior::Unavailable {
        reason: "Matter has no public API and no programmatic export path. Its \
                 Premium Notion/Obsidian export requires manual in-app interaction \
                 and is not automatable. There is no macOS local data store to read. \
                 This entry will be revisited if Matter ships a developer API.",
    },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
