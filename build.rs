fn main() {
    // Embed the app icon on Windows. A missing resource compiler must never fail the build.
    if std::env::var("CARGO_CFG_TARGET_OS").map(|o| o == "windows").unwrap_or(false) {
        let _ = std::panic::catch_unwind(|| {
            embed_resource::compile("app.rc", embed_resource::NONE);
        });
    }
}
