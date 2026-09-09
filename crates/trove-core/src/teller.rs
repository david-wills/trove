//! Teller.io bank aggregation — catalogued, permanently unavailable.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/teller.md

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "teller",
        name: "Teller.io",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Teller.io aggregates bank accounts via direct bank API connections. \
                      Not available in Trove due to its developer-certificate model.",
        domain: "finance",
        vault_path: "",
        toggleable: false,
        setup: &[],
        caveats: "",
    },
    behavior: Behavior::Unavailable {
        reason: "Teller authenticates using a mutual-TLS certificate issued per developer, \
                 not per user — shipping it in a distributed standalone app would share the \
                 developer's quota across all users, violating Teller's terms of service, \
                 and a relay server would break Trove's local-first promise. SimpleFIN \
                 covers bank sync with credentials you own directly.",
    },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
