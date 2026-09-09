//! Life360 — catalogued, permanently unavailable (no official API or export).
//!
//! Catalogued in the Phase 2 pass; brief: docs/integrations/life360.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "life360",
        name: "Life360",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Family location sharing history from Life360.",
        domain: "location",
        vault_path: "location/life360/",
        toggleable: false,
        setup: &[],
        caveats: "",
    },
    behavior: Behavior::Unavailable {
        reason: "Life360 offers no data export and no official API. The only known path is \
                 an unofficial reverse-engineered API that Life360 actively blocks via \
                 Cloudflare, and whose use violates their terms of service. Life360 has also \
                 been criticized for selling precise location data to data brokers.",
    },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
