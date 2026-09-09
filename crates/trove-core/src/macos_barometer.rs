//! Mac Barometer (CMAltimeter) — permanently unavailable on macOS.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/macos-barometer.md

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "macos-barometer",
        name: "Mac Barometer",
        kind: IntegrationKind::Live,
        default_on: false,
        description: "On-device barometric pressure from the Mac's built-in sensor.",
        domain: "environment",
        vault_path: "",
        toggleable: false,
        setup: &[],
        caveats: "",
    },
    behavior: Behavior::Unavailable {
        reason: "Apple does not expose the Mac's barometer to apps — CMAltimeter is \
                 marked unavailable on macOS regardless of hardware, including Apple \
                 Silicon Macs that contain the sensor. Barometric pressure is already \
                 collected from Open-Meteo (pressure_msl in the weather stream). Apple \
                 Watch pressure samples arrive via the existing Apple Health export \
                 path — no new integration is needed.",
    },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
