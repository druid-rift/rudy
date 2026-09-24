fn main() {
    // Debug info is what makes the generated item tree searchable from a test:
    // without it `ElementHandle` finds nothing, because every element reports an
    // element count of zero. `src/slint_behaviour.rs` drives the destructive
    // flow through that API, so this is a test requirement, not a convenience.
    // It is set here rather than left to `SLINT_EMIT_DEBUG_INFO=1` so that a
    // plain `cargo test` is enough.
    //
    // It applies to release builds too, deliberately: gating it on the profile
    // would break `cargo test --release`, which is a worse trade than a
    // searchable item tree in the shipped binary.
    let config = slint_build::CompilerConfiguration::new().with_debug_info(true);
    slint_build::compile_with_config("ui/appwindow.slint", config).expect("Slint build failed");
}
