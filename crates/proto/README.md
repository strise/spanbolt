# spanbolt-proto

Generated prost/tonic types for the Spanner gRPC APIs plus spanbolt's own `Control` service.

`proto/google/**` is vendored from googleapis at commit `6f8ddc394a90ea2c6a355afd2e9d21f56622df14`
(only the files reachable from `google/spanner/{v1,admin/database/v1,admin/instance/v1}`,
`google/longrunning`, `google/rpc`, `google/type/expr.proto` and `google/iam/v1`).
`google/protobuf/*` comes from protoc's bundled includes. `proto/spanbolt/v1/control.proto` is ours.

Part of [spanbolt](https://github.com/strise/spanbolt), a fast drop-in replacement for the Google
Cloud Spanner emulator. This crate is an internal building block of the `spanbolt` binary
(`cargo install spanbolt`); its API follows spanbolt's needs and may change in any release.
