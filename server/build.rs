use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rustc-link-lib=sqlite3");
    println!("cargo:rerun-if-changed=../shim/src/lib.rs");
    println!("cargo:rerun-if-changed=../shim/src/api.rs");
    println!("cargo:rerun-if-changed=../shim/build.rs");
    println!("cargo:rerun-if-changed=../shim/Cargo.toml");
    println!("cargo:rerun-if-changed=../protocol/src/lib.rs");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let target_dir = out_dir.join("shim-target");
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let profile = env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());

    let mut cmd = Command::new(&cargo);
    cmd.arg("build")
        .arg("--manifest-path")
        .arg(manifest_dir.join("../Cargo.toml"))
        .arg("-p")
        .arg("sqlite-broker-shim")
        .arg("--target-dir")
        .arg(&target_dir);
    // PROFILE is set by the parent Cargo, but a nested `cargo build` ignores it
    // and writes the dev profile unless told otherwise.
    if profile != "debug" {
        cmd.arg("--profile").arg(&profile);
    }
    for (key, _) in env::vars() {
        if key.starts_with("CARGO") {
            cmd.env_remove(&key);
        }
    }
    cmd.env("CARGO", &cargo);
    let status = cmd
        .status()
        .expect("spawn cargo to build sqlite-broker-shim");
    if !status.success() {
        panic!("failed to build sqlite-broker-shim");
    }

    let built = target_dir.join(&profile).join("libsqlite_broker.so");
    assert!(
        built.is_file(),
        "sqlite-broker-shim did not produce {}",
        built.display()
    );
    println!("cargo:rustc-env=SQLITE_BROKER_SHIM={}", built.display());
}
