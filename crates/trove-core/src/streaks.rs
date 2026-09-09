//! Streaks — iOS/Mac habit tracker; no API or export, iCloud container opaque.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/streaks.md

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "streaks",
        name: "Streaks",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description:
            "Habit tracker for iOS and Mac. No API or data export is available; \
             workout-linked habits may surface through the Apple Health export.",
        domain: "habits",
        vault_path: "habits/streaks/",
        toggleable: false,
        setup: &[],
        caveats: "",
    },
    behavior: Behavior::Unavailable {
        reason: "Streaks has no API and no documented export path. Its iCloud \
                 sync container schema is undocumented and opaque — the data \
                 cannot be read by Trove or any third party. Workout-linked \
                 habits may still appear via the Apple Health export.",
    },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
