fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=d.rc");
        println!("cargo:rerun-if-changed=d.ico");
        println!("cargo:rerun-if-changed=d.manifest");

        embed_resource::compile("d.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("failed to embed d.ico");
    }
}




