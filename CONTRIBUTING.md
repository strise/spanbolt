# Contributing

Issues and pull requests are welcome. For anything larger than a fix, open an issue first so we
can agree on the approach.

## Before you open a pull request

1. Read `DESIGN.md`: it sets the architecture and the rules (no `unwrap()` on request paths, errors
   map to Spanner's status codes and messages).
2. Run the checks:

   ```sh
   cargo fmt --all
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   ```

   Changes to SQL, the server or anything that needs the GoogleSQL shim (`docs/building.md`):

   ```sh
   source scripts/googlesql-env.sh
   scripts/gate.sh        # fmt, clippy, tests, conformance in both modes
   ```

3. Use conventional commit messages: `type(scope): description`, for example
   `fix(sql): keep NULLs last in DESC order`.

## Behaviour changes need conformance cases

spanbolt's reference is real Spanner, then the official emulator. A change in behaviour comes with
cases in `conformance/cases` whose expectations are recorded from the official emulator (or real
Spanner), never written by hand or edited to make spanbolt pass. How to record:
`crates/conformance/README.md`.

If spanbolt can't support something, it must return `UNIMPLEMENTED`, not a different answer.

## License

By contributing you agree that your contribution is licensed under Apache-2.0, as the rest of the
project (`LICENSE`). Code ported from other projects must keep its license and be named in
`NOTICE`.
