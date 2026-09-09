//! macOS FSEvents Journal — catalogued, permanently unavailable.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/macos-fsevents.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "macos-fsevents",
        name: "FSEvents Journal",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "The low-level file-system event journal macOS maintains \
                      of every file change on the volume.",
        domain: "files",
        vault_path: "",
        toggleable: false,
        setup: &[],
        caveats: "",
    },
    behavior: Behavior::Unavailable {
        reason: "Reading the raw /.fseventsd/ journal requires root access, \
                 and it records every file-system event — forensic-level noise \
                 with no practical personal-data value. Download history and \
                 git activity answer 'what files changed' far more cleanly, \
                 without root.",
    },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
