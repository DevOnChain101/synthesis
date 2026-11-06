//! Windows only: embed the app icon into `synthesis-desktop.exe`. Other targets have nothing to do (macOS/Linux icons live in the package).
//! The manifest (DPI awareness) is already linked in by gpui; a second one fails with CVT1100 (duplicate resource).

fn main() {
    println!("cargo:rerun-if-changed=windows");
    println!("cargo:rerun-if-changed=assets/icons/synthesis.ico");
    #[cfg(windows)]
    embed_resource::compile("windows/synthesis.rc", embed_resource::NONE)
        .manifest_required()
        .unwrap();
}
