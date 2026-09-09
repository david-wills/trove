//! LINE messenger — catalogued, permanently unavailable.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/line.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "line",
        name: "LINE",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "LINE messenger chat history.",
        domain: "correspondence",
        vault_path: "",
        toggleable: false,
        setup: &[],
        caveats: "",
    },
    behavior: Behavior::Unavailable {
        reason: "LINE offers no personal export feature and no API for message \
                 history; its local database format is undocumented. In-app backup \
                 syncs to LINE's own servers, not a local file. Nothing can be read \
                 until LINE adds an official export or community tooling documents \
                 the format.",
    },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
