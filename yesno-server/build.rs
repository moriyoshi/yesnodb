fn main() -> Result<(), Box<dyn std::error::Error>> {
    // A control-plane client should not need a system package merely to compile
    // the schema shared with `yesnod`.
    std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?);
    let mut prost = prost_build::Config::new();
    prost.boxed(".yesno.control.v1.SubscribeItem.item.event");
    tonic_prost_build::configure().compile_with_config(
        prost,
        &["proto/control.proto", "proto/replication.proto"],
        &["proto"],
    )?;
    Ok(())
}
