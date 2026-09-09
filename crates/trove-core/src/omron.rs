//! Omron blood-pressure monitors — `Unavailable` (B2B-gated API).
//!
//! Omron is the dominant consumer blood-pressure-monitor brand. Its companion
//! **OMRON Connect** iOS/Android app automatically syncs BP readings (systolic,
//! diastolic, pulse, irregular-pulse flag) to **Apple Health** after every
//! measurement.  Trove's [`crate::health`] integration imports all Apple Health
//! data including BP, so any user who has the OMRON Connect app already lands
//! their Omron readings in the vault through that path.
//!
//! The direct **OMRON Connect Create** cloud API
//! (`digitalhealth.omronconnect.com`) requires a B2B partner registration that
//! is contact-gated and intended for healthcare organisations — not independent
//! personal applications.  The API uses a server-to-server OAuth2 password
//! grant with partner-issued credentials (not a user-facing authorization-code
//! flow), so there is no path for an individual user to authorize Trove
//! directly.  The libomron community library covers only older USB/HID models
//! and is unmaintained.
//!
//! **Result:** this entry is marked `Unavailable`.  Enabling Apple Health in
//! Trove captures all Omron readings automatically via the OMRON Connect →
//! Apple Health path; a separate direct connector is not buildable without a
//! partner agreement.
//!
//! Catalogued in the Phase 2 pass; brief: docs/integrations/omron.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "omron",
        name: "Omron",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description:
            "Omron blood-pressure monitors sync readings (systolic, diastolic, pulse, \
             irregular-pulse flag) to Apple Health via the OMRON Connect app. \
             Enabling the Apple Health integration in Trove captures all Omron \
             readings automatically.",
        domain: "health",
        vault_path: "health/blood-pressure/",
        toggleable: false,
        setup: &[],
        caveats:
            "The direct OMRON Connect Create API is a contact-gated B2B partner \
             programme intended for healthcare organisations; it is not available \
             to independent personal applications. All Omron data reaches Trove \
             through the Apple Health import, which is the recommended path.",
    },
    behavior: Behavior::Unavailable {
        reason: "OMRON Connect Create API is B2B/partner-gated server-to-server \
                 password-grant OAuth, not available to independent personal apps; \
                 readings reach Trove via the OMRON Connect app -> Apple Health, \
                 captured by the Apple Health import.",
    },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omron_def_unavailable() {
        assert_eq!(DEF.meta.id, "omron");
        assert!(
            matches!(DEF.behavior, Behavior::Unavailable { .. }),
            "Omron must be Unavailable: OMRON Connect Create API is B2B/partner-gated"
        );
        assert!(DEF.pull.is_none(), "no pull hook for an Unavailable entry");
        assert!(DEF.connection.is_none(), "no connection for an Unavailable entry");
        assert!(!DEF.meta.toggleable, "Unavailable entries must not be toggleable");
        assert!(!DEF.meta.default_on, "Unavailable entries must not default on");
    }

    #[test]
    fn omron_caveats_mention_partner_gate() {
        assert!(
            DEF.meta.caveats.contains("B2B partner"),
            "caveats must explain why the direct API is unavailable"
        );
    }

    #[test]
    fn omron_unavailable_reason_explains_path() {
        if let Behavior::Unavailable { reason } = DEF.behavior {
            assert!(
                reason.contains("Apple Health"),
                "unavailable reason must explain the Apple Health fallback path"
            );
            assert!(
                reason.contains("B2B") || reason.contains("partner-gated"),
                "unavailable reason must mention the partner gate"
            );
        } else {
            panic!("expected Behavior::Unavailable");
        }
    }
}
