fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protos = [
        "../../proto/record-store/internal/system/v1/system.proto",
        "../../proto/record-store/internal/consensus/v1/consensus.proto",
        "../../proto/record-store/internal/replica/v1/replica.proto",
    ];
    for proto in protos {
        println!("cargo:rerun-if-changed={proto}");
    }

    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    let mut prost = prost_build::Config::new();
    prost.protoc_executable(protoc);
    tonic_build::configure().compile_protos_with_config(prost, &protos, &["../../proto"])?;
    Ok(())
}
