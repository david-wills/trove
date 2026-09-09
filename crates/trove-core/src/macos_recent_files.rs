//! macOS Recent Files (SFL2 / SharedFileList) — catalogued, permanently unavailable.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/macos-recent-files.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "macos-recent-files",
        name: "macOS Recent Files",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "The list of recently opened files macOS maintains \
                      across apps.",
        domain: "files",
        vault_path: "",
        toggleable: false,
        setup: &[],
        caveats: "",
    },
    behavior: Behavior::Unavailable {
        reason: "macOS stores recent-file lists as opaque NSKeyedArchiver \
                 Bookmark blobs — the file paths are not readable without \
                 forensics-grade reverse engineering. Spotlight's \
                 kMDItemLastUsedDate metadata is the sanctioned path to the \
                 same information and will be used instead if this source is \
                 ever built.",
    },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
