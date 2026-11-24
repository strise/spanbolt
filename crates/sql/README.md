# spanbolt-sql

The SQL engine: planner, executor, scalar and aggregate functions, DML, full-text search and
INFORMATION_SCHEMA. Statements are analysed by GoogleSQL through `googlesql-sys`, which links a
prebuilt native library (see that crate).

Part of [spanbolt](https://github.com/strise/spanbolt), a fast drop-in replacement for the Google
Cloud Spanner emulator. This crate is an internal building block of the `spanbolt` binary
(`cargo install spanbolt`); its API follows spanbolt's needs and may change in any release.
