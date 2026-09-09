//! WeChat — permanently unavailable; encrypted local database, no export.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/wechat.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "wechat",
        name: "WeChat",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "WeChat message history from the macOS desktop app.",
        domain: "correspondence",
        vault_path: "correspondence/wechat/",
        toggleable: false,
        setup: &[],
        caveats: "",
    },
    behavior: Behavior::Unavailable {
        reason: "WeChat encrypts its local database with a key that exists only in the \
                 running app's memory; extracting it requires attaching to the WeChat \
                 process, which WeChat's terms of service prohibit. WeChat offers no \
                 export feature and the key-extraction tooling was discontinued in \
                 October 2025. This integration can only be revisited if WeChat ships \
                 an official export.",
    },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
