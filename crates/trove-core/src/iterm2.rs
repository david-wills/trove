//! iTerm2 command and directory history — catalogued in the Phase 2 pass; brief: docs/integrations/iterm2.md.
//!
//! Iceboxed: shell history covers the same commands without the ~200-command
//! cap; the unique value (cd/directory history) needs filesystem inspection
//! to locate a stable read path before building.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "iterm2",
        name: "iTerm2",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description:
            "Command and directory history recorded by iTerm2. \
             Captures the working directory per command, which shell history alone does not.",
        domain: "developer",
        vault_path: "developer/iterm2/",
        toggleable: false,
        setup: &[],
        caveats: "Shell history already covers executed commands; \
                  iTerm2's unique value is directory context.",
    },
    behavior: Behavior::NotWired,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
