// SPDX-FileCopyrightText: 2026 Contributors to ddcutil-varlink <https://github.com/digitaltrails/ddcutil-varlink>
// SPDX-License-Identifier: GPL-2.0-or-later
// src/ffi.rs

// Isolate much of the C crud.

// Suppress warnings from compiling the generated bindings.
// Use an outer attributes (NO exclamation mark) applied directly to an inline block
#[allow(nonstandard_style, dead_code, clippy::all, non_camel_case_types,)]
pub mod bindings {
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

// Re-export everything from the block so other files can still use `use crate::ffi::*;`
pub use bindings::*;