//! Transit Cards (Clipper, Oyster, ORCA, …) — permanently unavailable.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/transit-cards.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "transit-cards",
        name: "Transit Cards (Clipper, Oyster, ORCA, …)",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Journey history from regional transit cards such as \
                      Clipper (Bay Area), Oyster (London), ORCA (Seattle), \
                      and Ventra (Chicago).",
        domain: "travel",
        vault_path: "",
        toggleable: false,
        setup: &[],
        caveats: "",
    },
    behavior: Behavior::Unavailable {
        reason: "Transit card operators (Clipper, Oyster, ORCA, Ventra, …) \
                 offer no API and at best PDF-only statements — there is no \
                 reliable machine-readable way to export your journey history. \
                 Oyster retains only 8 weeks of data online. This will be \
                 revisited if a major operator ships a CSV or JSON export.",
    },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
