use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=AUTOMODPACK_WASM");
    if let Some(prebuilt) = env::var_os("AUTOMODPACK_WASM") {
        println!(
            "cargo:rustc-env=AUTOMODPACK_WASM={}",
            PathBuf::from(prebuilt).display()
        );
        return;
    }

    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let plugin_dir = manifest_dir.join("..");
    let workspace = plugin_dir.join("..");
    let sdk = workspace
        .join("..")
        .join("Infrarust")
        .join("crates")
        .join("infrarust-plugin-sdk");
    for watched in [
        plugin_dir.join("src"),
        plugin_dir.join("Cargo.toml"),
        workspace.join("Cargo.toml"),
        workspace.join("Cargo.lock"),
        sdk,
    ] {
        println!("cargo:rerun-if-changed={}", watched.display());
    }

    let target_dir = workspace.join("target");
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .current_dir(&workspace)
        .args([
            "build",
            "--release",
            "--target",
            "wasm32-wasip2",
            "-p",
            "infrarust-plugin-automodpack",
            "--target-dir",
        ])
        .arg(&target_dir)
        .env_remove("CARGO_TARGET_DIR")
        .output();
    match output {
        Ok(output) if output.status.success() => {}
        Ok(output) => panic!(
            "building the automodpack component failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ),
        Err(error) => panic!("running cargo to build the automodpack component: {error}"),
    }

    let artifact = target_dir
        .join("wasm32-wasip2")
        .join("release")
        .join("infrarust_plugin_automodpack.wasm");
    println!("cargo:rustc-env=AUTOMODPACK_WASM={}", artifact.display());
}
