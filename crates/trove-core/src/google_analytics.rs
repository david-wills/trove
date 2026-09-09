//! Google Analytics (GA4) — catalogued as out of scope: site analytics are
//! aggregate audience statistics, not the user's own personal data.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/google-analytics.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "google-analytics",
        name: "Google Analytics (GA4)",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Website audience analytics for sites you own or manage.",
        domain: "social",
        vault_path: "",
        toggleable: false,
        setup: &[],
        caveats: "",
    },
    behavior: Behavior::Unavailable {
        reason: "Trove is a vault for your personal data. Google Analytics \
                 reports aggregate statistics about your website's visitors — \
                 they describe other people's behaviour, not your own. \
                 Own-website analytics may be revisited in a later pass if \
                 there is demand.",
    },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
