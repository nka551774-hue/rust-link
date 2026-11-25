//! Locates libopus for the in-process encoder.
//!
//! The library is deliberately not built from source: a machine that has libopus
//! installed, which is what a distribution packages, should link against it
//! rather than compile a second copy. `OPUS_LIB_DIR` overrides the search for a
//! tree that is not on the usual paths.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    for var in ["OPUS_LIB_DIR", "RUSTLINK_MSYS2_ROOT", "PKG_CONFIG_PATH", "PKG_CONFIG"] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    if std::env::var_os("CARGO_FEATURE_NATIVE_OPUS").is_none() {
        return;
    }
    if let Some(dir) = std::env::var_os("OPUS_LIB_DIR") {
        println!("cargo:rustc-link-search=native={}", dir.to_string_lossy());
        println!("cargo:rustc-link-lib=opus");
        return;
    }
    if let Some(flags) = pkg_config() {
        let mut lib_name = "opus";
        for flag in flags.split_whitespace() {
            if let Some(path) = flag.strip_prefix("-L") {
                println!("cargo:rustc-link-search=native={path}");
            } else if let Some(name) = flag.strip_prefix("-l").filter(|n| n.contains("opus")) {
                lib_name = name;
            }
        }
        println!("cargo:rustc-link-lib={lib_name}");
        return;
    }
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let msys_root = std::env::var_os("RUSTLINK_MSYS2_ROOT")
            .map_or_else(|| PathBuf::from(r"C:\msys64"), PathBuf::from);
        let ucrt_lib = msys_root.join("ucrt64").join("lib");
        if ucrt_lib.join("libopus.dll.a").is_file() || ucrt_lib.join("libopus.a").is_file() {
            println!("cargo:rustc-link-search=native={}", ucrt_lib.display());
        }
    }
    // Nothing to ask, so let the linker find it the way it would for any other
    // system library.
    println!("cargo:rustc-link-lib=opus");
}

/// Asks pkg-config where libopus is, which is how a Unix system normally answers
/// and how MSYS2 answers on Windows.
fn pkg_config() -> Option<String> {
    ["pkg-config", "pkgconf"].into_iter().find_map(|program| {
        let output = Command::new(program).args(["--libs", "opus"]).output().ok()?;
        output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    })
}
