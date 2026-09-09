//! Mac Microphone ambient sound level — live dB metering via AVAudioEngine.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/macos-microphone.md

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "macos-microphone",
        name: "Mac Microphone Ambient Sound",
        kind: IntegrationKind::Live,
        default_on: false,
        description: "Samples the ambient sound level from your Mac's microphone using \
                      AVAudioEngine RMS metering. Stores only decibel readings — never \
                      audio — for noise-environment trend analysis.",
        domain: "environment",
        vault_path: "environment/macos-microphone/",
        toggleable: false,
        setup: &[],
        caveats: "Requires macOS Microphone permission (TCC). Values are uncalibrated \
                  RMS dB — useful for relative trends, not absolute loudness.",
    },
    behavior: Behavior::NotWired,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};
