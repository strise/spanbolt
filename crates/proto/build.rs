fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `PROTOC` picks a protoc; without it the build uses the binary (and the well-known type
    // protos) bundled in protoc-bin-vendored, so a clean machine needs no system protoc.
    let mut includes = vec![std::path::PathBuf::from("proto")];
    if std::env::var_os("PROTOC").is_none() {
        // SAFETY: build scripts are single-threaded.
        unsafe { std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?) };
        includes.push(protoc_bin_vendored::include_path()?);
    }
    println!("cargo:rerun-if-env-changed=PROTOC");
    let protos = [
        "proto/google/spanner/v1/spanner.proto",
        "proto/google/spanner/admin/database/v1/spanner_database_admin.proto",
        "proto/google/spanner/admin/instance/v1/spanner_instance_admin.proto",
        "proto/google/longrunning/operations.proto",
        "proto/google/rpc/error_details.proto",
        "proto/spanbolt/v1/control.proto",
    ]
    .map(std::path::PathBuf::from);
    println!("cargo:rerun-if-changed=proto");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR")?);
    tonic_prost_build::configure()
        .file_descriptor_set_path(out.join("spanbolt_descriptor.bin"))
        .build_server(true)
        .build_client(true)
        .compile_protos(&protos, &includes)?;
    Ok(())
}
