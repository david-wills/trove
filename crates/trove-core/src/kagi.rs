//! Kagi — privacy-first search engine with no server-side history to export.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/kagi.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "kagi",
        name: "Kagi",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Kagi is a privacy-first search engine that deliberately \
                      stores no search history server-side — privacy is the product.",
        domain: "browser",
        vault_path: "browser/searches/",
        toggleable: false,
        setup: &[],
        caveats: "",
    },
    behavior: Behavior::Unavailable {
        reason: "Kagi deliberately stores no search history server-side — \
                 queries are never associated with accounts and are auto-purged \
                 after brief debug retention. There is nothing to export or \
                 fetch via an API. The only conceivable path would be a browser \
                 extension intercepting searches at query time, which is outside \
                 Trove's standalone model.",
    },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
