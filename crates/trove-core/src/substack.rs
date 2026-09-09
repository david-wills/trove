//! Substack — writer-side official export import (posts, stats).
//! Catalogued in the Phase 2 pass; brief: docs/integrations/substack.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "substack",
        name: "Substack",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your Substack writer export — posts and stats. \
                      Reader subscriptions and newsletter inbox have no \
                      export path; those arrive via the email integration.",
        domain: "social",
        vault_path: "social/substack/",
        toggleable: false,
        setup: &[],
        caveats: "Only writer-side data is exportable; reader history and \
                  subscription lists have no official export.",
    },
    behavior: Behavior::NotWired,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
