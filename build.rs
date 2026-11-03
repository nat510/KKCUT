fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=src/ui");
    println!("cargo:rerun-if-changed=assets/app.ico");
    println!("cargo:rerun-if-changed=assets/icon.png");
    slint_build::compile_with_config(
        "src/ui/app.slint",
        slint_build::CompilerConfiguration::new().with_style("fluent-light".into()),
    )?;
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // Resource ID 1 is the icon Explorer, the taskbar and the tray show.
        let manifest = std::env::var("CARGO_MANIFEST_DIR")?;
        let icon = std::path::Path::new(&manifest).join("assets").join("app.ico");
        let script = std::path::Path::new(&std::env::var("OUT_DIR")?).join("kkcut.rc");
        let path = icon.to_string_lossy().replace('\\', "\\\\");
        std::fs::write(&script, format!("1 ICON \"{path}\"\n"))?;
        embed_resource::compile(&script, embed_resource::NONE)
            .manifest_optional()
            .map_err(|e| format!("Cannot embed the application icon: {e:?}"))?;
    }
    Ok(())
}
