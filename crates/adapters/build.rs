fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/hex_arm/public_api_types.proto");
    println!("cargo:rerun-if-changed=proto/hex_arm/public_api_down.proto");
    println!("cargo:rerun-if-changed=proto/hex_arm/public_api_up.proto");

    if std::env::var_os("CARGO_FEATURE_HEX_ARM_CONTROL").is_none() {
        return Ok(());
    }

    let mut config = prost_build::Config::new();
    config.protoc_executable(protoc_bin_vendored::protoc_bin_path()?);
    config.compile_protos(
        &[
            "proto/hex_arm/public_api_types.proto",
            "proto/hex_arm/public_api_down.proto",
            "proto/hex_arm/public_api_up.proto",
        ],
        &["proto/hex_arm"],
    )?;
    Ok(())
}
