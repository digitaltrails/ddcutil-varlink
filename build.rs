// SPDX-FileCopyrightText: 2026 Contributors to ddcutil-varlink <https://github.com/digitaltrails/ddcutil-varlink>
// SPDX-License-Identifier: GPL-2.0-or-lateruse std::env;
// build.rs
use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=varlink/com.ddcutil.service.varlink");

    // Generate libddcutil bindings
    println!("cargo:rustc-link-lib=ddcutil");
    let bindings = bindgen::Builder::default()
        .header("wrapper.h") // includes <ddcutil_c_api.h>
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("Unable to generate bindings for libddcutil");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings");

    // Generate Varlink interface code (into OUT_DIR)
    // varlink_generator::cargo_build expects the .varlink file path.
    // It will generate a Rust module in OUT_DIR with the same base name.
    varlink_generator::cargo_build("varlink/com.ddcutil.service.varlink");
}
