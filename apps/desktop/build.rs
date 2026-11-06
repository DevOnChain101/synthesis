//! Windows only: embed the app icon and the DPI-aware manifest (same one gpui ships for its examples)
//! into `synthesis-desktop.exe`. Other targets have nothing to do (macOS/Linux icons live in the package).

fn main() {
    println!("cargo:rerun-if-changed=windows");
    println!("cargo:rerun-if-changed=assets/icons/synthesis.ico");
    #[cfg(windows)]
    embed_resource::compile("windows/synthesis.rc", embed_resource::NONE)
        .manifest_required()
        .unwrap();
}
