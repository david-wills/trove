//! Stripe billing and invoice export import — for Stripe account holders.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/stripe.md

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "stripe",
        name: "Stripe Billing",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your Stripe billing and invoice data from a dashboard CSV export. \
                      Covers charges, refunds, and payout history for Stripe account holders.",
        domain: "finance",
        vault_path: "finance/purchases/stripe/",
        toggleable: false,
        setup: &[],
        caveats: "Relevant only if you have a Stripe account; email-receipt parsing covers \
                  Stripe-billed charges for everyone else.",
    },
    behavior: Behavior::NotWired,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
