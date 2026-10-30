
fn main() {
    println!("cargo:rerun-if-changed=schema/update_metadata.proto");
    prost_build::compile_protos(&["schema/update_metadata.proto"], &["schema"]).unwrap();
    let out = std::env::var("OUT_DIR").unwrap();
    println!(
        "cargo:rustc-env=OTA_SCHEMA_FILE={}/chromeos_update_engine.rs",
        out,
    );
}
