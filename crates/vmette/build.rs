// Generates `include/vmette.h` from the `#[no_mangle] extern "C"` items
// in `src/ffi.rs`. The header is checked into git so C-only consumers
// don't need cbindgen installed.

use std::env;
use std::path::PathBuf;

fn main() {
    let crate_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let crate_dir = PathBuf::from(crate_dir);

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

    println!("cargo:rerun-if-changed=src/ffi.rs");
    println!("cargo:rerun-if-changed=src/lib.rs");
    println!("cargo:rerun-if-changed=cbindgen.toml");
}
