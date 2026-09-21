//! One linker argument, on one platform, for one kind of output.
//!
//! An extension module leaves Python's own symbols — `Py_None`,
//! `Py_InitializeEx` and the rest — to be resolved by the interpreter that
//! loads it. Linux and Windows are content with that. macOS is not: its
//! linker wants every symbol accounted for at link time, and says
//! `ld: symbol(s) not found` for a library that is working exactly as
//! intended.
//!
//! `pyo3`'s `extension-module` feature emits this itself, but that feature is
//! off unless maturin is building the wheel — deliberately, so that
//! `cargo build --workspace` does not need a Python library to link against.
//! Which leaves the plain `cargo test --workspace` that CI runs on macOS
//! failing to link a crate nobody asked it to link.
//!
//! `rustc-cdylib-link-arg` is the narrow way to say it: this crate's cdylib
//! only. Nothing else in the workspace has its link checking loosened, which
//! matters — undefined symbols are a real error everywhere else.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // The *target* os, not the host: a build script runs on the machine doing
    // the building, which is not always the machine being built for.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-cdylib-link-arg=-undefined");
        println!("cargo:rustc-cdylib-link-arg=dynamic_lookup");
    }
}
