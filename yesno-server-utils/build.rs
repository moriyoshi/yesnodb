fn main() -> Result<(), Box<dyn std::error::Error>> {
    std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?);
    prost_build::Config::new().compile_protos(&["proto/archive.proto"], &["proto"])?;
    Ok(())
}
