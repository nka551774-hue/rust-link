//! Locates libopus for the in-process encoder.
//!
//! The library is deliberately not built from source: a machine that has libopus
//! installed, which is what a distribution packages, should link against it
//! rather than compile a second copy. `OPUS_LIB_DIR` overrides the search for a
//! tree that is not on the usual paths.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=OPUS_LIB_DIR");
    println!("cargo:rerun-if-env-changed=RUSTLINK_MSYS2_ROOT");
    if std::env::var_os("CARGO_FEATURE_NATIVE_OPUS").is_none() {
        return;
    }
    if let Some(dir) = std::env::var_os("OPUS_LIB_DIR") {
        println!("cargo:rustc-link-search=native={}", dir.to_string_lossy());
        println!("cargo:rustc-link-lib=opus");
        return;
    }
    if let Some(flags) = pkg_config() {
        let mut lib_name = "opus".to_string();
        for flag in flags {
            if let Some(path) = flag.strip_prefix("-L") {
                println!("cargo:rustc-link-search=native={path}");
            } else if let Some(name) = flag.strip_prefix("-l") {
                if name.contains("opus") {
                    lib_name = name.to_string();
                }
            }
        }
        println!("cargo:rustc-link-lib={lib_name}");
        return;
    }
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let msys_root = std::env::var_os("RUSTLINK_MSYS2_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(r"C:\msys64"));
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
fn pkg_config() -> Option<Vec<String>> {
    for program in ["pkg-config", "pkgconf"] {
        let Ok(output) = Command::new(program).args(["--libs", "opus"]).output() else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        return Some(text.split_whitespace().map(str::to_string).collect());
    }
    None
}
