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
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Before implementing a large feature, please open an issue or discussion
covering requirements, alternatives and measurable acceptance criteria.
Never submit live credentials or production session data in bug reports.

All code contributions should be compatible with the MIT license.