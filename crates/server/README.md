# spanbolt-server

The emulator's gRPC services, REST gateway and streaming, as a library. Without the `googlesql`
feature every SQL call returns `UNIMPLEMENTED`; the `spanbolt` binary turns it on.

Part of [spanbolt](https://github.com/strise/spanbolt), a fast drop-in replacement for the Google
Cloud Spanner emulator. This crate is an internal building block of the `spanbolt` binary
(`cargo install spanbolt`); its API follows spanbolt's needs and may change in any release.
