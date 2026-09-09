//! CLIP image embeddings — on-device semantic search index derived from photos.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/clip-embeddings.md.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "clip-embeddings",
        name: "CLIP Image Embeddings (derived)",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Generates on-device CLIP vector embeddings for your photos, \
                      enabling semantic image search entirely without a cloud service. \
                      Requires an opt-in one-time model download (~300 MB).",
        domain: "photos",
        vault_path: "photos/clip-embeddings/",
        toggleable: false,
        setup: &[],
        caveats: "Opt-in only: requires a ~300 MB ONNX model download and is CPU-intensive \
                  on large libraries.",
    },
    behavior: Behavior::NotWired,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
