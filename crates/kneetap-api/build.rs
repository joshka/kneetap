//! Generates gRPC code when the `grpc` feature is on.

#[cfg(feature = "grpc")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use prost::Message;

    const PROTO: &str = "proto/kneetap/v1/reporter.proto";

    // protox compiles the proto in Rust, so building doesn't require an installed `protoc`.
    let descriptors = protox::compile([PROTO], ["proto"])?;
    let descriptor_path = std::path::PathBuf::from(std::env::var("OUT_DIR")?).join("kneetap.bin");
    std::fs::write(&descriptor_path, descriptors.encode_to_vec())?;
    tonic_prost_build::configure()
        .build_transport(false)
        .file_descriptor_set_path(descriptor_path)
        .compile_fds(descriptors)?;
    println!("cargo:rerun-if-changed={PROTO}");
    Ok(())
}

#[cfg(not(feature = "grpc"))]
fn main() {}
