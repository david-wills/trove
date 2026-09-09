//! Blitzortung lightning-strike detection — permanently unavailable.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/blitzortung.md

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "blitzortung",
        name: "Blitzortung Lightning",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Community lightning-strike network covering the globe via ~3,000 \
                      volunteer sensors.",
        domain: "environment",
        vault_path: "",
        toggleable: false,
        setup: &[],
        caveats: "",
    },
    behavior: Behavior::Unavailable {
        reason: "Blitzortung's usage policy requires third-party apps to relay data \
                 through their own servers rather than connecting every client directly \
                 to the community broker — a model that doesn't fit a local-first app \
                 where each install would hit the broker independently. Commercial \
                 lightning APIs (OpenWeather, Xweather, Vaisala) all require paid keys. \
                 NWS severe-thunderstorm alerts cover the practical warning use case \
                 without this integration.",
    },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
