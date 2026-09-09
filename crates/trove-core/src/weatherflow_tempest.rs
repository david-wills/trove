//! WeatherFlow Tempest — personal weather station (UDP local + cloud REST).
//! Catalogued in the Phase 2 pass; brief: docs/integrations/weatherflow-tempest.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "weatherflow-tempest",
        name: "WeatherFlow Tempest",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description:
            "Collects hyper-local weather readings from your Tempest personal \
             weather station — wind, rain, temperature, lightning, and more — \
             via the local UDP broadcast and cloud REST backfill.",
        domain: "home",
        vault_path: "home/weatherflow-tempest/",
        toggleable: false,
        setup: &[],
        caveats: "Cloud history requires a personal token from tempestwx.com; \
                  local UDP (port 50222) works on the same LAN with no credentials.",
    },
    behavior: Behavior::NotWired,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
