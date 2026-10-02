//! Compiles the Yellowstone Geyser protobufs.
//!
//! Uses a vendored `protoc` so building doesn't require one on PATH.
//! Set `PROTOC` to override.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var_os("PROTOC").is_none() {
        std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?);
    }
    println!("cargo:rerun-if-changed=proto");
    tonic_build::configure()
        // Server code is only used by the in-crate mock server tests, but
        // generating it is cheap and keeps the build config simple.
        .build_server(true)
        .compile_protos(&["proto/geyser.proto"], &["proto"])?;
    Ok(())
}
