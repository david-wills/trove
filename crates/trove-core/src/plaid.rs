//! Plaid bank aggregation — catalogued, permanently unavailable.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/plaid.md

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "plaid",
        name: "Plaid",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Plaid is a bank-data aggregation service with broad institution coverage. \
                      Not available in Trove due to its developer-key model.",
        domain: "finance",
        vault_path: "",
        toggleable: false,
        setup: &[],
        caveats: "",
    },
    behavior: Behavior::Unavailable {
        reason: "Plaid issues developer-held API keys that cannot safely ship inside a \
                 standalone app — distributing them would expose them to extraction, and \
                 routing your bank data through a relay server would break Trove's \
                 local-first promise. SimpleFIN covers bank sync with credentials you \
                 own directly. Plaid may return as a bring-your-own-key option for \
                 power users who register their own developer account.",
    },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
