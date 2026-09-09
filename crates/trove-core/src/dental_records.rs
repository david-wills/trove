//! Dental Records — no practical structured patient-access path in 2026.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/dental-records.md

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "dental-records",
        name: "Dental Records",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Your dental visit history, X-rays, and treatment records.",
        domain: "health",
        vault_path: "",
        toggleable: false,
        setup: &[],
        caveats: "",
    },
    behavior: Behavior::Unavailable {
        reason: "No practical patient-accessible structured path exists in 2026 — \
                 the HL7 dental FHIR standard has near-zero vendor adoption. \
                 Dental PDFs from practice portals can be dropped into the \
                 generic medical-document import instead.",
    },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
