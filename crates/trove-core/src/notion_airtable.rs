//! Notion / Airtable Contacts — personal contact databases built in Notion or
//! Airtable, synced via their respective REST APIs.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/notion-airtable.md

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "notion-airtable",
        name: "Notion / Airtable Contacts",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Sync a contact database you maintain in Notion or Airtable \
                      into your vault. Because schemas vary per workspace, a \
                      field-mapping step is required before the first sync.",
        domain: "contacts",
        vault_path: "contacts/notion-airtable/",
        toggleable: false,
        setup: &[],
        caveats: "Requires a field-mapping step — no fixed schema exists across \
                  Notion or Airtable workspaces.",
    },
    behavior: Behavior::NotWired,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
