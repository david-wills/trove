//! Monica personal CRM — periodic cloud sync via the official REST API.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/monica.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "monica",
        name: "Monica (Personal CRM)",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Syncs contacts and relationship notes from Monica (monicahq.com), \
                      an open-source personal CRM. Works with both the cloud service and \
                      self-hosted instances via a Bearer token.",
        domain: "contacts",
        vault_path: "contacts/monica/",
        toggleable: false,
        setup: &[],
        caveats: "Verify the API schema for Monica v3 (Chandler) before building — \
                  it differs from v2.",
    },
    behavior: Behavior::NotWired,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
