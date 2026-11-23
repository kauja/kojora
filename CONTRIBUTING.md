# Contributing

Thank you for considering Kojora. This project is in pre-alpha: contracts,
implementation details and layout can change substantially.

## Good first contributions

- Review domain boundaries, isolation and lifecycle assumptions.
- Propose reproducible resource benchmarks for multiple agent sessions.
- Prototype adapter interfaces with a small, permission-scoped executable.
- Add tests demonstrating crash recovery or scheduler edge cases.

## Development checks

```sh
cargo fmt --all -- --check
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
```

CI uses the Rust version in `rust-toolchain.toml` and the committed
`Cargo.lock`. Workspace lints reject panicking shortcuts in production code,
unfinished implementations, debugging macros and undocumented unsafe blocks.
A test fixture may use assertions or unwrap a result to fail immediately.

Use a credential-free runner or fixture to exercise a provider change.
Record the exact commit and checks you ran. Document any credentialed smoke
test that still needs to be run on the target machine.

Before implementing a large feature, please open an issue or discussion
covering requirements, alternatives and measurable acceptance criteria.
Never submit live credentials or production session data in bug reports.

All code contributions should be compatible with the MIT license.