// Regenerates the checked-in C header `include/vmette.h` from the
// `#[no_mangle] extern "C"` items in `src/ffi.rs`.
//
// Header generation is gated behind the off-by-default `regenerate-header`
// feature, which is the *only* thing that pulls in `cbindgen`. That keeps
// `cbindgen` (and `syn`) out of every downstream consumer's build graph — they
// get the committed header as-is — and guarantees a normal `cargo build`
// (including the verification build `cargo publish` runs) never writes into the
// source tree. Refresh the header with `make header` or
// `cargo build -p vmette --features regenerate-header`; CI fails if it drifts.

fn main() {
    println!("cargo:rerun-if-changed=src/ffi.rs");
    println!("cargo:rerun-if-changed=src/lib.rs");
    println!("cargo:rerun-if-changed=cbindgen.toml");

    // The cdylib (`libvmette.dylib`) is consumed by non-Rust callers over the C
    // ABI. By default the macOS linker stamps its install name (LC_ID_DYLIB)
    // with the absolute build-output path, so a binary that links `-lvmette`
    // bakes in a path that only exists on the build machine and fails at
    // runtime with `dyld: Library not loaded`. Stamp it `@rpath/...` instead so
    // consumers resolve it through an rpath (`-Wl,-rpath,<dir-holding-the-dylib>`).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-cdylib-link-arg=-Wl,-install_name,@rpath/libvmette.dylib");
    }

    #[cfg(feature = "regenerate-header")]
    regenerate_header();
}

#[cfg(feature = "regenerate-header")]
fn regenerate_header() {
    use std::env;
    use std::path::PathBuf;

    let crate_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let include_dir = crate_dir.join("include");
    let _ = std::fs::create_dir_all(&include_dir);
    let header_path = include_dir.join("vmette.h");

    let config =
        cbindgen::Config::from_file(crate_dir.join("cbindgen.toml")).expect("read cbindgen.toml");

    match cbindgen::Builder::new()
        .with_crate(&crate_dir)
        .with_config(config)
        .generate()
    {
        Ok(bindings) => {
            bindings.write_to_file(&header_path);
        }
        Err(e) => {
            // Don't fail the build — print a warning instead. Header
            // generation is a nice-to-have; the cdylib/staticlib still
            // work without a fresh header.
            println!("cargo:warning=cbindgen failed: {e}");
        }
    }
}
