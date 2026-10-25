//! Locates libopus for the in-process encoder.
//!
//! The library is deliberately not built from source: a machine that has libopus
//! installed, which is what a distribution packages, should link against it
//! rather than compile a second copy. `OPUS_LIB_DIR` overrides the search for a
//! tree that is not on the usual paths.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=OPUS_LIB_DIR");
    if std::env::var_os("CARGO_FEATURE_NATIVE_OPUS").is_none() {
        return;
    }
    if let Some(dir) = std::env::var_os("OPUS_LIB_DIR") {
        println!("cargo:rustc-link-search=native={}", dir.to_string_lossy());
        println!("cargo:rustc-link-lib=opus");
        return;
    }
    if let Some(flags) = pkg_config() {
        for flag in flags {
            if let Some(path) = flag.strip_prefix("-L") {
                println!("cargo:rustc-link-search=native={path}");
            }
        }
        println!("cargo:rustc-link-lib=opus");
        return;
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
