fn main() {
    // Embed Info.plist into the binary so TCC consent prompts (Calendar,
    // Reminders, Media Library) can render for this bare, unbundled binary —
    // those usage strings are mandatory and the corresponding System Settings
    // panes have no manual-add fallback. The section survives the re-signing
    // in scripts/build-troved.sh (it is part of the linked image).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        let plist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Info.plist");
        println!(
            "cargo:rustc-link-arg=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
        println!("cargo:rerun-if-changed=Info.plist");
    }
}
