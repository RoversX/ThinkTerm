fn main() {
    // cfg!() in a build script tests the HOST, not the TARGET; use the
    // cargo-provided target var so cross builds don't link Apple frameworks.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-lib=framework=UserNotifications");
    }
}
