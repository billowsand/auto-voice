fn main() {
    const ICON_PATH: &str = "assets/icons/auto-voice.ico";

    println!("cargo:rerun-if-changed={ICON_PATH}");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon(ICON_PATH);
        resource
            .compile()
            .expect("failed to embed the Windows application icon");
    }
}
