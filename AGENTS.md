# Agent entry points

- `docs/features/README.md` — Feature Map. Open the relevant feature file before driving or changing a feature; it has the exact commands, keys, code paths and known traps. Update it in the same PR that changes the feature.
- `CONTEXT.md` — the project vocabulary (Run, Stage, statuses). Use these words, not their listed alternatives.
- `ARCHITECTURE.md` — module layout and design constraints.
- Every milestone must compile and pass `cargo fmt --check`, `cargo clippy`, `cargo test`. Open every PR against `main`; never stack PRs.
