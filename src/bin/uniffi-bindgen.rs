//! Bindings generator entry point.
//!
//! UniFFI's `--library` mode introspects a built artifact to emit bindings, so the
//! generator has to be a binary inside this crate rather than a standalone tool.
//! Driven by `bindings/generate.sh`.

fn main() {
    uniffi::uniffi_bindgen_main()
}
