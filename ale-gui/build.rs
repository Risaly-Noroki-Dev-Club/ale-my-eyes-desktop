fn main() {
    let emit_debug_info = std::env::var("PROFILE").as_deref() != Ok("release");
    let config = slint_build::CompilerConfiguration::new().with_debug_info(emit_debug_info);
    slint_build::compile_with_config("ui/app.slint", config).unwrap();

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource
            .set_icon("../assets/icon.ico")
            .set("ProductName", "Ale, My Eyes!")
            .set("FileDescription", "Ale, My Eyes! Desktop Assistant")
            .set("LegalCopyright", "Copyright Risaly Noroki Dev Club");
        resource
            .compile()
            .expect("failed to compile Windows resources");
    }
}
