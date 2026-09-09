//! NOAA Climate Data Online — historical station weather observations.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/noaa-cdo.md

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "noaa-cdo",
        name: "NOAA Climate Data Online",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Fetches historical weather observations from NOAA's CDO archive \
                      of ground-station records. Adds station ground-truth over model \
                      estimates for users who want observed (not interpolated) data.",
        domain: "environment",
        vault_path: "environment/noaa-cdo/",
        toggleable: false,
        setup: &[],
        caveats: "Requires a free token emailed by NOAA (ncei.noaa.gov/cdo-web). \
                  Open-Meteo ERA5 already covers most historical-backfill needs; \
                  build this only if you need ground-station observations specifically.",
    },
    behavior: Behavior::NotWired,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
