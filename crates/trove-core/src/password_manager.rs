//! Password manager metadata import (1Password, Bitwarden, Dashlane, LastPass).
//! Catalogued in the Phase 2 pass; brief: docs/integrations/password-manager.md.
//!
//! ⚠ Privacy: all secret fields (passwords, TOTP seeds, card numbers, SSNs)
//! are hard-stripped at parse time — only item metadata is stored.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "password-manager",
        name: "Password Manager Metadata (1Password, Bitwarden)",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Imports item metadata — titles, URLs, tags, and creation \
                      dates — from a 1Password (.1pux), Bitwarden, Dashlane, or \
                      LastPass export. All passwords, TOTP seeds, card numbers, \
                      and other secrets are stripped at parse time and never \
                      written to the vault.",
        domain: "files",
        vault_path: "files/password-manager/",
        toggleable: false,
        setup: &[],
        caveats: "Requires a user-initiated export performed outside Trove; \
                  the secret-stripping parser must be reviewed before first ship.",
    },
    behavior: Behavior::NotWired,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
