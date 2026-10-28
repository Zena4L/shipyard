//! Tauri's build: the app's commands declared (step 14.2's review: in Tauri
//! 2 every registered command is allowed to every window unless the app's
//! manifest lists them, and the capability allows each by name).

include!("src/commands.rs");

fn main() {
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS)),
    )
    .unwrap_or_else(|error| panic!("tauri's build: {error:#}"));
}
