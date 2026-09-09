//! Beeper unified messaging client — opportunistic local collector via the
//! Beeper Desktop HTTP API (localhost only, no external service).
//! Catalogued in the Phase 2 pass; brief: docs/integrations/beeper.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "beeper",
        name: "Beeper",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Reads messages from Beeper's local Desktop API, covering all the \
                      networks Beeper bridges (iMessage, WhatsApp, Telegram, and more) \
                      in one pass.",
        domain: "correspondence",
        vault_path: "correspondence/beeper/",
        toggleable: false,
        setup: &[],
        caveats: "Beeper Desktop must be installed and running; this collector is \
                  opt-in only and never a hard dependency.",
    },
    behavior: Behavior::NotWired,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
