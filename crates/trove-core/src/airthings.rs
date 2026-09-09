//! Airthings Wave / View — indoor air quality and radon monitor cloud API.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/airthings.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "airthings",
        name: "Airthings",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description:
            "Pulls air quality and radon readings from your Airthings Wave or View sensors \
             via the official consumer API. Radon history is unique to Airthings devices.",
        domain: "home",
        vault_path: "home/airthings/",
        toggleable: false,
        setup: &[],
        caveats: "Requires a free Airthings developer account; both Client ID and Secret are \
                  needed. Retention observed at over a year but not officially stated.",
    },
    behavior: Behavior::NotWired,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
