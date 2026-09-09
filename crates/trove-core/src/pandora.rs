//! Pandora — radio streaming; developer API closed, no export path.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/pandora.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "pandora",
        name: "Pandora",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description:
            "Pandora internet radio — thumbed stations and listener history.",
        domain: "media",
        vault_path: "",
        toggleable: false,
        setup: &[],
        caveats: "",
    },
    behavior: Behavior::Unavailable {
        reason: "Pandora's developer API is closed to new applicants and there \
                 is no official data export. As a radio-style service it keeps \
                 thumbed stations, not per-track listening history, so even a \
                 privacy request yields little actionable data.",
    },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
