//! OkCupid — dating-app GDPR data request import.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/okcupid.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "okcupid",
        name: "OkCupid",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your OkCupid data export — matches, messages, and \
                      profile history. Requires submitting a GDPR/CCPA data \
                      request to OkCupid support; no self-serve export exists.",
        domain: "social",
        vault_path: "social/okcupid/",
        toggleable: false,
        setup: &[],
        caveats: "Export must be requested via OkCupid support; format is \
                  undocumented — parser built when a real sample is available.",
    },
    behavior: Behavior::NotWired,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
