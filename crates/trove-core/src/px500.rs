//! 500px photo hosting — API shut down 2018, no export path.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/500px.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "500px",
        name: "500px",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Photo hosting community acquired by Visual China Group. \
                      No bulk export or API access is available.",
        domain: "photos",
        vault_path: "",
        toggleable: false,
        setup: &[],
        caveats: "",
    },
    behavior: Behavior::Unavailable {
        reason: "500px shut down its public API in 2018 and offers no bulk \
                 data export. The only remaining access would be scraping the \
                 website, which violates its terms of service and Trove's \
                 standalone and privacy principles. Users who want their 500px \
                 photos should retrieve them manually using browser developer \
                 tools or a third-party migration tool.",
    },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
