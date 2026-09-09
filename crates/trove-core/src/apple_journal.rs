//! Apple Journal — Mac app shipping with macOS 26 (fall 2026); data format
//! undocumented, no export or API yet.  Catalogued in the Phase 2 pass;
//! brief: docs/integrations/apple-journal.md

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "apple-journal",
        name: "Apple Journal",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Your Apple Journal entries, pulled from the local store \
                       once the Mac app ships with macOS 26.",
        domain: "notes",
        vault_path: "notes/apple-journal/",
        toggleable: false,
        setup: &[],
        caveats: "",
    },
    behavior: Behavior::Unavailable {
        reason: "Apple Journal only arrives on the Mac with macOS 26 (fall \
                 2026). Its local data format is undocumented and it has no \
                 export feature or API yet. We'll investigate as soon as the \
                 app ships.",
    },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
