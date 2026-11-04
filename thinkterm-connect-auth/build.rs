fn main() {
    // The confirmation window comes from version 6 of the common controls,
    // which a program has to ask for: without this it does not even load.
    #[cfg(windows)]
    {
        use std::io::Write;
        println!("cargo:rerun-if-changed=windows.manifest");
        let manifest = std::path::Path::new(&std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
            .join("windows.manifest");
        let resource = std::path::Path::new(&std::env::var_os("OUT_DIR").unwrap()).join("auth.rc");
        // 24 is RT_MANIFEST, spelled out to need no system header.
        writeln!(
            std::fs::File::create(&resource).unwrap(),
            "1 24 \"{}\"",
            manifest.display().to_string().replace('\\', "\\\\"),
        )
        .unwrap();
        embed_resource::compile(resource);
    }
}
