//! Ecobee Smart Thermostat — catalogued, currently unavailable (developer registrations closed).
//! Catalogued in the Phase 2 pass; brief: docs/integrations/ecobee.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "ecobee",
        name: "Ecobee",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description:
            "Ecobee smart thermostat runtime, setpoint, and occupancy history via the \
             official OAuth API.",
        domain: "home",
        vault_path: "home/ecobee/",
        toggleable: false,
        setup: &[],
        caveats: "",
    },
    behavior: Behavior::Unavailable {
        reason: "Ecobee paused new developer registrations in April 2024 and no new API keys \
                 are being issued — Trove cannot connect new accounts. HomeKit-capable Ecobee \
                 models (3 and later) surface device config through the HomeKit integration. \
                 This integration will be enabled if Ecobee reopens developer sign-ups.",
    },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
