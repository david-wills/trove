//! Timery — iOS/macOS Toggl Track front-end with no independent data store.
//!
//! Timery is a popular Apple-platform app (iPhone, iPad, Mac) that provides a
//! richer UI over Toggl Track — saved timer presets, widgets, Shortcuts
//! integration — but it stores no authoritative data of its own.  Every time
//! entry it displays lives in Toggl Track and is accessible there via the
//! Toggl API.  The Timery iCloud cache is a derived copy; the system of record
//! is Toggl.
//!
//! **Behavior: `CoveredBy("toggl-track")`** — once the Toggl Track integration
//! is enabled the user's data is collected automatically; this registry entry
//! exists so the hub can explain why there is no separate Timery connector.
//!
//! Catalogued in the Phase 2 pass; brief: docs/integrations/timery.md

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "timery",
        name: "Timery",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Timery is a native-app interface for Toggl Track; all time \
                      entries live in Toggl Track, so enabling the Toggl Track \
                      integration captures everything Timery records.",
        domain: "time-entries",
        vault_path: "time-entries/toggl-track/",
        toggleable: false,
        setup: &[],
        caveats: "All data is owned by Toggl Track — this entry is documented for \
                  discoverability; the Toggl Track integration covers it entirely.",
    },
    behavior: Behavior::CoveredBy("toggl-track"),
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
