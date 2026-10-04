//! One spelling for "the filesystem is in memory": `cfg(vfs_memory)`, set on
//! every wasm32 target and natively under the `memory` feature.
fn main() {
    println!("cargo::rustc-check-cfg=cfg(vfs_memory)");
    let wasm = std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("wasm32");
    let memory = std::env::var_os("CARGO_FEATURE_MEMORY").is_some();
    if wasm || memory {
        println!("cargo::rustc-cfg=vfs_memory");
    }
}
