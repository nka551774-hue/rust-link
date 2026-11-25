//! Locates system `libopus` for the in-process encoder (`OPUS_LIB_DIR` overrides discovery).

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    for var in [
        "OPUS_LIB_DIR",
        "RUSTLINK_MSYS2_ROOT",
        "PKG_CONFIG",
        "PKG_CONFIG_PATH",
        "PKG_CONFIG_LIBDIR",
        "PKG_CONFIG_SYSROOT_DIR",
        "PKG_CONFIG_ALLOW_CROSS",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    if env::var_os("CARGO_FEATURE_NATIVE_OPUS").is_none() {
        return;
    }
    if let Some(dir) = env::var_os("OPUS_LIB_DIR") {
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
    if env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let msys_root = env::var_os("RUSTLINK_MSYS2_ROOT")
            .map_or_else(|| PathBuf::from(r"C:\msys64"), PathBuf::from);
        let ucrt_lib = msys_root.join("ucrt64").join("lib");
        let is_gnu = env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu");
        if ucrt_lib.join("libopus.dll.a").is_file()
            || (is_gnu && ucrt_lib.join("libopus.a").is_file())
        {
            println!("cargo:rustc-link-search=native={}", ucrt_lib.display());
        }
    }
    println!("cargo:rustc-link-lib=opus");
}

fn pkg_config() -> Option<String> {
    let cross = env::var_os("HOST") != env::var_os("TARGET");
    if cross
        && env::var_os("PKG_CONFIG_ALLOW_CROSS").as_deref() != Some("1".as_ref())
        && env::var_os("PKG_CONFIG_SYSROOT_DIR").is_none()
        && env::var_os("PKG_CONFIG_LIBDIR").is_none()
    {
        return None;
    }
    ["pkg-config", "pkgconf"].into_iter().find_map(|program| {
        let output = Command::new(program).args(["--libs", "opus"]).output().ok()?;
        output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    })
}
