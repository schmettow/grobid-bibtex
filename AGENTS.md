# AGENTS.md

Guidelines for AI coding agents working in this repository.

## Preparing for publication on crates.io

When the user asks to prepare this crate for publication on crates.io,
perform steps 1–4 below. Documentation is out of scope: do not analyze
documentation coverage, and do not add or change documentation metadata
(the `documentation` field in `Cargo.toml` or `[package.metadata.docs.rs]`);
those are maintained separately.

1. Analyze the **test** coverage (`cargo llvm-cov -p grobid-bibtex
   --all-features`; doctests are not measured on stable Rust) and write a
   report into `Changelog.md` under the release's `### Quality` section.
2. Run all tests (`cargo test --all-features`) and report the results in
   `Changelog.md`. When the task says a GROBID server is running, also
   exercise the `pdf2bibtex` and `refs2bibtex` binaries against it end to
   end and include those results.
3. Run and keep green the mandatory checks:
   `cargo fmt --check`, `cargo check --all-targets --all-features`,
   `cargo test --all-features`,
   `cargo clippy --all-targets --all-features -- -D warnings`,
   `cargo doc --no-deps --all-features`, `cargo package --list` and
   `cargo publish --dry-run`. Fix problems instead of silencing them, and
   report release blockers (e.g. a dependency version that is not on
   crates.io yet).
4. Update the package metadata in `Cargo.toml`: version, description,
   keywords, categories, repository and readme. Leave the documentation
   metadata alone (see above).

## After changing the code

After changing the code base, always come back with a commit message
(for copy-and-paste) that summarizes the change.
