//! GoCardless Bank Account Data — EU/UK Open Banking (PSD2) bank transaction
//! sync via a bring-your-own free developer account.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/gocardless.md

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "gocardless",
        name: "GoCardless Bank Account Data (EU/UK)",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Sync EU and UK bank transactions via GoCardless Bank Account Data \
                      (formerly Nordigen), the PSD2 Open Banking aggregator. Register a free \
                      developer account and connect your banks — no Trove-held credentials \
                      touch your accounts.",
        domain: "finance",
        vault_path: "finance/gocardless/",
        toggleable: false,
        setup: &[],
        caveats: "Requires a free GoCardless developer account (bankaccountdata.gocardless.com); \
                  coverage is EU and UK banks only.",
    },
    behavior: Behavior::NotWired,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
