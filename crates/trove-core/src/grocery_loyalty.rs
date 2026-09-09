//! Grocery and retail loyalty programs — catalogued, permanently unavailable.
//!
//! No loyalty program (Kroger, Safeway/Albertsons, etc.) offers an API or
//! structured export for your itemized purchase history. Privacy-law data
//! requests (CCPA, GDPR) take weeks and typically return non-machine-readable
//! files. Catalogued per pipeline doctrine — the greyed card answers "why
//! isn't this available?" so nobody re-researches it.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/grocery-loyalty.md

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "grocery-loyalty",
        name: "Grocery & Retail Loyalty Programs",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Your itemized purchase history from grocery and retail loyalty cards \
                      (Kroger, Safeway, Albertsons, and similar programs).",
        domain: "finance",
        vault_path: "",
        toggleable: false,
        setup: &[],
        caveats: "",
    },
    behavior: Behavior::Unavailable {
        reason: "Grocery and retail loyalty programs (Kroger, Safeway, Albertsons, and others) \
                 offer no API or structured export for your itemized purchase history. \
                 Privacy-law data requests (CCPA, GDPR) take weeks to fulfill and typically \
                 return inconsistent, non-machine-readable files. Trove will revisit if \
                 regulation (such as CFPB Section 1033) forces a programmatic path. In the \
                 meantime, itemized grocery data is available indirectly from delivery-order \
                 email receipts (Instacart, Amazon Fresh) once email-receipt parsing is built.",
    },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
