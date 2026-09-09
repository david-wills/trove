//! Nintendo Switch — unavailable; no official playtime API exists.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/nintendo-switch.md

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "nintendo-switch",
        name: "Nintendo Switch",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description:
            "Nintendo Switch playtime and library data. No official playtime \
             API exists; the only known path requires an external relay service \
             that violates Trove's standalone rule.",
        domain: "gaming",
        vault_path: "",
        toggleable: false,
        setup: &[],
        caveats: "",
    },
    behavior: Behavior::Unavailable {
        reason: "Nintendo has no official playtime API. The only available path \
                 (nxapi Parental Controls) requires an external relay service \
                 to spoof the Switch Online app — a dependency on a third-party \
                 server that violates Trove's standalone rule. Monthly Parental \
                 Controls reports have no structured export. This will be \
                 revisited if an official API or self-hostable relay appears.",
    },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
