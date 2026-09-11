use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let trainer = manifest.join("../relay-semantic/scripts/train_nli_bundle.py");
    let staged =
        PathBuf::from(std::env::var("OUT_DIR").expect("out dir")).join("train_nli_bundle.py");
    println!("cargo:rerun-if-changed={}", trainer.display());
    let bytes = std::fs::read(&trainer).unwrap_or_default();
    std::fs::write(&staged, bytes).expect("stage the semantic trainer");
}
