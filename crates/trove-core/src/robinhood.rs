//! Robinhood data export import — trades and transfers.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/robinhood.md

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "robinhood",
        name: "Robinhood",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your Robinhood trade and transfer history from a CSV export. \
                      Covers executed trades and account transfers; real-time positions \
                      require a manual export.",
        domain: "finance",
        vault_path: "finance/robinhood/",
        toggleable: false,
        setup: &[],
        caveats: "Export via Account → Settings → Privacy & Security → Download my data; \
                  no date range filter on the main download.",
    },
    behavior: Behavior::NotWired,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
