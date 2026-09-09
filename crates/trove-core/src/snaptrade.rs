//! SnapTrade brokerage aggregator — catalogued, permanently unavailable.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/snaptrade.md

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "snaptrade",
        name: "SnapTrade",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "SnapTrade aggregates 30+ brokerages through one API. Not available in \
                      Trove due to its developer-key model.",
        domain: "finance",
        vault_path: "",
        toggleable: false,
        setup: &[],
        caveats: "",
    },
    behavior: Behavior::Unavailable {
        reason: "SnapTrade issues developer-held API keys that cannot safely ship inside a \
                 standalone app — distributing them would expose them to extraction, and a \
                 relay server would break Trove's local-first promise. May return as a \
                 bring-your-own-key option for power users once native Schwab and IBKR \
                 support ships and the BYOK UX is validated.",
    },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
