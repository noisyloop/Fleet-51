// Gives the Windows programs their icon. Nothing to do on other systems.
fn main() {
    println!("cargo:rerun-if-changed=packaging/windows/fleet51.rc");
    println!("cargo:rerun-if-changed=packaging/windows/fleet51.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("packaging/windows/fleet51.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("embedding the Windows icon");
    }
}
