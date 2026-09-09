//! Awair Element — indoor air quality via local LAN API and cloud history.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/awair.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "awair",
        name: "Awair",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description:
            "Reads air quality data (CO2, VOC, PM2.5, temperature, humidity) from your \
             Awair Element or 2nd Edition sensor via the local network API.",
        domain: "home",
        vault_path: "home/awair/",
        toggleable: false,
        setup: &[],
        caveats: "The local API returns the current reading only (enable it in the Awair app); \
                  historical data requires the cloud API or a dashboard CSV export.",
    },
    behavior: Behavior::NotWired,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
