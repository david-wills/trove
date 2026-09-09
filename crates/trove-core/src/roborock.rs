//! Roborock robot vacuum — cleaning history and maps via local MIIO protocol.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/roborock.md

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "roborock",
        name: "Roborock",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Reads cleaning-session history and maps from your \
                      Roborock robot vacuum over the local network using the \
                      MIIO protocol. After a one-time cloud token extraction \
                      the robot is queried entirely on-device.",
        domain: "home",
        vault_path: "home/roborock/",
        toggleable: false,
        setup: &[],
        caveats: "One-time Xiaomi/Roborock cloud login required to extract the \
                  device token; local-only after that. Floor maps are in a \
                  proprietary binary format and may not render.",
    },
    behavior: Behavior::NotWired,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
