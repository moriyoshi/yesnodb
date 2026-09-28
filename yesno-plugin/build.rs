//! Build the C test plugin, so an integration test can `dlopen` it.
//!
//! # Why a build script and not a `cdylib` member crate
//!
//! A `cdylib` in the workspace would be a Rust crate, and the thing most worth
//! proving is that a plugin written in **C** can use the header -- and that it
//! cannot accidentally link `yesno-core`, which a Rust plugin can. Cargo also
//! gives no ordering between a `cdylib` target and a test that wants its path,
//! while a build script has one by construction.
//!
//! # Why a failure here does not fail the build
//!
//! A machine without a C compiler should still be able to `cargo build` this
//! crate. But a test that silently skips is a test that reports success for a
//! surface nobody exercised, so the reason is passed through to the test, which
//! fails with it. Absent is loud; broken is louder.

fn main() {
    println!("cargo:rerun-if-changed=tests/plugin.c");
    println!("cargo:rerun-if-changed=include/yesno_plugin.h");

    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let so = out.join(if cfg!(target_os = "macos") {
        "libyesno_test_plugin.dylib"
    } else {
        "libyesno_test_plugin.so"
    });

    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let status = std::process::Command::new(&cc)
        .args([
            "-std=c11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-fPIC",
            "-shared",
            "-Iinclude",
        ])
        .arg("tests/plugin.c")
        .arg("-o")
        .arg(&so)
        .status();

    match status {
        Ok(s) if s.success() => {
            println!("cargo:rustc-env=YESNO_TEST_PLUGIN={}", so.display());
        }
        Ok(s) => println!(
            "cargo:rustc-env=YESNO_TEST_PLUGIN_ERROR={cc} exited with {s}, so the C test \
             plugin was not built"
        ),
        Err(e) => println!(
            "cargo:rustc-env=YESNO_TEST_PLUGIN_ERROR=cannot run {cc}: {e}; the C test \
             plugin was not built"
        ),
    }
}
