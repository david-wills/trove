//! Honeywell Home (Resideo) thermostat — cloud API state and history pull.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/honeywell-resideo.md

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "honeywell-resideo",
        name: "Honeywell Home (Resideo)",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Polls your Honeywell Home (Resideo) T-Series or Lyric thermostat \
                      via the official developer API to build a local history of \
                      temperature, setpoints, and HVAC run times.",
        domain: "home",
        vault_path: "home/honeywell-resideo/",
        toggleable: false,
        setup: &[],
        caveats: "Requires registering a BYO app at developer.honeywellhome.com. \
                  The API returns current state only — history is built by periodic \
                  polling.",
    },
    behavior: Behavior::NotWired,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
