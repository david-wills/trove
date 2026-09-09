//! Plausible Analytics — out of scope (site analytics, not personal data).
//! Catalogued in the Phase 2 pass; brief: docs/integrations/plausible.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "plausible",
        name: "Plausible Analytics",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Plausible is a privacy-friendly analytics platform for \
                      website owners. Its data describes your site's visitors, \
                      not your own personal activity.",
        domain: "social",
        vault_path: "",
        toggleable: false,
        setup: &[],
        caveats: "",
    },
    behavior: Behavior::Unavailable {
        reason: "Trove is a vault for your personal data. Plausible Analytics \
                 tracks aggregate statistics about your website's visitors — \
                 that is audience data, not your own activity. Own-website \
                 analytics may be added in a later pass if there is demand.",
    },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
