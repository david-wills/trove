//! Dex personal CRM — periodic cloud sync via REST API.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/dex.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "dex",
        name: "Dex (Personal CRM)",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Syncs your Dex personal CRM contacts and relationship notes. \
                      Requires a paid Professional plan; uses the api.getdex.com REST \
                      endpoint with a pasted API token.",
        domain: "contacts",
        vault_path: "contacts/dex/",
        toggleable: false,
        setup: &[],
        caveats: "API access requires a paid Professional plan (~$20/mo). \
                  VC-funded product — long-term API stability is not guaranteed.",
    },
    behavior: Behavior::NotWired,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
