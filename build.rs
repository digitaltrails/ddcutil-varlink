use std::env;
use std::path::PathBuf;

fn main() {
    // ------------------------------------------------------------
    // 1. Generate libddcutil bindings with bindgen
    // ------------------------------------------------------------
    println!("cargo:rustc-link-lib=ddcutil");
    let bindings = bindgen::Builder::default()
    .header("wrapper.h")                 // includes <ddcutil_c_api.h>
    .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
    .generate()
    .expect("Unable to generate bindings for libddcutil");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
    .write_to_file(out_path.join("bindings.rs"))
    .expect("Couldn't write bindings");

    // ------------------------------------------------------------
    // 2. Generate Varlink interface code (into src/)
    // ------------------------------------------------------------
    // varlink_generator::cargo_build_tosource expects the .varlink file path.
    // It will generate a Rust module in src/ with the same base name.
    varlink_generator::cargo_build_tosource(
        "src/com.ddcutil.service.varlink",
        true,   // run rustfmt on generated code
    );
}
