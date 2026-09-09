//! Hinge — import via Hinge's official in-app data export (matches.json /
//! events.json). Dating app: ships with opt-in acknowledgement and vault
//! isolation.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/hinge.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "hinge",
        name: "Hinge",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Imports your Hinge match history and interaction data \
                      from Hinge's official in-app export. Requires an explicit \
                      opt-in due to the sensitive nature of dating-app data.",
        domain: "social",
        vault_path: "social/hinge/",
        toggleable: false,
        setup: &[],
        caveats: "Unlike Tinder, Hinge exports include match display names. \
                  Built alongside Tinder as part of the dating-app import category.",
    },
    behavior: Behavior::NotWired,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
