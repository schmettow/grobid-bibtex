# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased

### Fixed

- Renaming a PDF with a long title no longer fails with `filename too long
  (os error 36)`. Suggested file names are shortened to the filesystem's
  255-byte `NAME_MAX` at a character boundary, leaving room for collision
  counters; `unique_path()` and `unique_path_with_year()` keep their
  suffix-numbered names within the same limit.

## v0.3.0

### Added

- `cite`: reusable citation attach. `cite_keys()` scans a query for `@Key`
  mentions; `CiteState::locate()` resolves them against a bibliography (exact
  first, then ASCII case-insensitive) and the active corpus directories,
  distinguishing unknown keys, missing documents, paths outside the corpora
  and ambiguous keyed file names; `load_documents()` loads the located files
  through a caller-supplied parser. The resolver caches the bibliography by
  size/mtime, scans all document formats recursively (`files::collect_files`)
  and honors relative `file` fields.
- `prepass` (feature `pipeline`): the complete GROBID pre-pass as library
  API — the `.ragrig_grobid.json` fingerprint manifest, `extract::headers`,
  optional `complete::biblios`, the `Key - Full authors - Full title - Year`
  rename policy and `collection::merge_file`. `PrepassConfig` selects the
  folder, explicit sources, merge target and timings; `PrepassObserver`
  reports progress and cancels; `PrepassReport` carries the counts and
  per-file failures. `prepass()` runs without an observer.
- `files::collect_files()` recursively lists every regular file, sorted;
  `collect_pdfs()` is the filtered form.

### Changed

- `ragrig-cli` and sciscraper now call the shared `cite`/`prepass` modules
  instead of maintaining their own copies; the pre-pass logs, manifest format
  and rename policy no longer drift between hosts.

### Quality

- **Test coverage** (`cargo llvm-cov -p grobid-bibtex --all-features`;
  unit, integration and binary tests; doctests are not measured on stable
  Rust): `bibtex.rs` 97.4%, `cite.rs` 94.4%, `collection.rs` 93.9%,
  `files.rs` 93.0%, `prepass.rs` 88.0%, `extract.rs` 80.0% and
  `complete.rs` 66.7% line coverage — 92.2% across the library modules.
  Including the two largely untested binaries the total is 80.9%
  (`bin/pdf2bibtex.rs` 10.9%, `bin/refs2bibtex.rs` 0.0%).
- **Verification**: `cargo fmt --check`, `cargo check --all-targets
  --all-features`, `cargo test --all-features`, `cargo clippy --all-targets
  --all-features -- -D warnings` and `cargo doc --no-deps --all-features`
  (with `RUSTDOCFLAGS="-D warnings"`) are all green. `cargo test` runs 71
  unit tests, 1 binary test, 2 integration tests and 17 doctests.
  `cargo package --list` ships 19 files (with `AGENTS.md` excluded and the
  new `src/cite.rs`/`src/prepass.rs` included), and `cargo publish --dry-run`
  packages and verifies the crate (307.7 KiB, 73.5 KiB compressed) with
  `grobid` 0.6.0 resolved from crates.io.
- **Live GROBID**: no server was provided for this release pass, so the
  binaries were not exercised end to end; the mock-server tests cover header
  extraction, OpenAlex failure handling, renaming, merging and the pre-pass
  observer/cancellation paths. No release blockers found.

## v0.2.1

### Added

- `collection::CiteEntry` plus `Collection::cite_entries()` and
  `Collection::cite_entry()`: look up entries by citation key (exact first,
  then ASCII case-insensitive) and list every entry as plain data — the
  canonical key and the PDF recorded in the `file` field (or its `pdf`
  alias). Chat assistants and editor pickers can resolve `@key` references
  without touching the `biblatex` types. The name avoids the `Citation`
  type that the `grobid` client re-exports at the crate root.

### Quality

- **Test coverage** (`cargo llvm-cov --all-features`; unit, integration
  and binary tests; doctests are not measured on stable Rust): `bibtex.rs`
  97.4%, `collection.rs` 93.9%, `files.rs` 92.5%, `extract.rs` 80.0% and
  `complete.rs` 63.0% line coverage — ≈92% across the library modules.
  The CLI binaries are exercised by live runs rather than unit tests
  (`bin/pdf2bibtex.rs` 10.9%, `bin/refs2bibtex.rs` 0.0%).
- **Verification**: `cargo fmt --check`, `cargo check --locked
  --all-targets --all-features`, `cargo clippy --locked --all-targets
  --all-features -- -D warnings`, `cargo doc --no-deps --all-features`
  (0 warnings) and `cargo test --all-features` (44 unit tests, 1 binary
  test, 2 integration tests and 17 doctests) are all green.
  `cargo package --list` ships the expected 17 files (with `AGENTS.md`
  excluded), and `cargo publish --dry-run` packages and verifies the
  crate (221.9 KiB, 55.1 KiB compressed) with `grobid` 0.6.0 resolved
  from crates.io. The live-GROBID end-to-end pass recorded for v0.2.0
  carries over; no server run was made for this report.

## v0.2.0

### Added

- `collection::merge_file()` merges records into a `.bib` file in one step:
  it reads the file (a missing file starts empty), skips records that are
  already present (normalized content, identifiers, PDF file name), appends
  the new entries and returns a `MergeReport` with the added entries, the
  duplicate count and the resulting size. `Collection::load_or_new()` parses
  a file that may not exist yet, and `append()` now creates a missing file.
  `pdf2bibtex --merge` uses the new function, removing its duplicated
  merge-and-append code.
- `files::Manifest` can cache each processed file's extracted record:
  `record_with_biblio()` stores the fingerprint together with the `Biblio`,
  `biblio()` reads it back, and `record()` refreshes a fingerprint without
  dropping a cached record. The JSON is compatible with manifests written
  before records were cached, so a corpus can rebuild its bibliography
  without querying GROBID and OpenAlex again.

### Quality

- **Test coverage** (`cargo llvm-cov -p grobid-bibtex --all-features`,
  unit and integration tests; doctests are not measured on stable Rust):
  `bibtex.rs` 97.4%, `collection.rs` 93.5%, `files.rs` 92.5%,
  `extract.rs` 80.0% and `complete.rs` 63.0% line coverage — 92% across
  the library modules.  The CLI binaries are exercised by the live run
  below rather than by unit tests.
- **Verification**: `cargo fmt --check`, `cargo check --all-targets
  --all-features`, `cargo clippy --all-targets --all-features --
  -D warnings`, `cargo doc --no-deps --all-features` (with
  `RUSTDOCFLAGS="-D warnings"`) and `cargo test --all-features` (42 unit
  tests, 1 binary test, 2 integration tests and 17 doctests) all pass.
  `cargo package --list` ships the expected 17 files (with `AGENTS.md`
  excluded), and `cargo publish --dry-run` packages and verifies the
  crate (217.9 KiB, 54.0 KiB compressed) with `grobid` 0.6.0 resolved
  from crates.io.  Against a live GROBID server at
  `http://localhost:8070`: `pdf2bibtex` extracted 2 entries from 2 PDFs;
  a second run with `--merge` skipped both as duplicates; `--append`
  added them with `-2` keys; `--rename --link` produced the
  `Author_<title words>` file names with matching `file` fields;
  `refs2bibtex` extracted 24 + 27 references from the same PDFs and
  collected 30 after dropping 21 DOI duplicates.

## v0.1.1

### Fixed

Cargo.toml: grobid dependency from local to crates.io.

## v0.1.0

### Added

- Initial release: the BibTeX tooling that started in `grobid::bibtex` and
  the `pdf2bibtex` and `refs2bibtex` examples of the `grobid` crate.
- `collection::Collection`: an in-memory BibTeX/BibLaTeX collection. It
  parses and indexes `.bib` files, assigns collision-free citation keys,
  and merges records with duplicate detection by normalized field content,
  by DOI/PMID/arXiv identifier or by PDF file name; `EntryIdentity`
  exposes the identity used for that, `append()` writes rendered entries
  to an existing file.
- `extract`: batch extraction of document headers and references from PDFs
  through a GROBID server, with a bounded number of concurrent requests;
  `collect_references()` flattens, DOI-deduplicates and orders extracted
  reference lists.
- `complete` (feature `openalex`): batch completion of extracted records
  against OpenAlex with the pacing the keyless pool expects.
- `files`: recursive PDF discovery and the rename policy, reported as
  `Rename` events; `rename_pdfs_with()` accepts any file-stem style
  (`FileStemOptions`) and collision style (`Collision`, suffix or year).
- `files::Manifest`: a JSON sidecar with size/mtime fingerprints of
  processed files, so repeated extraction runs skip unchanged PDFs.
- Integration tests with a mock GROBID server exercise `extract::headers`
  end to end (HTTP client, multipart upload, TEI parser, worker pool); with
  the `openalex` feature the completion path is covered too.
- `bibtex::format_all()` for batches of records, in addition to the
  single-record formatting, key and file-naming helpers.
- `pdf2bibtex`: `-a`/`--append` appends the extracted entries to an existing
  BibTeX file instead of writing a new one; the target file is parsed first
  and its citation keys are reserved, so a new entry whose suggested key
  already exists gets a `-2`, `-3`, ... suffix.
- `pdf2bibtex`: `-m`/`--merge` merges the extracted entries into an existing
  BibTeX file, skipping records that are already present. Duplicates are
  detected by normalized field content, by DOI/PMID/arXiv identifier
  or by PDF file name. `--output`, `--append` and `--merge` are mutually
  exclusive.
- `bibtex::FileStemStyle::Keyed` names files
  `<key> - <full authors> - <full title> - <year>`: the citation key from
  `suggest_key` (omitted when no author or year yields one), every author
  with given and family name (falling back to the full name), the complete
  punctuation-stripped title, and the year at the end.
- The `grobid` client types used by the helpers and binaries (`Biblio`,
  `Author`, `GrobidClient`, `ProcessOptions`, ...) are re-exported.
- The `openalex` feature forwards `grobid/openalex` for the `--openalex`
  option of both binaries.

### Changed

- The `pdf2bibtex` and `refs2bibtex` binaries are front ends now: the
  extraction, completion, merge and rename logic lives in the crate's
  modules `extract`, `complete`, `collection` and `files`, so other tools
  can build on the same pipeline.

### Quality

- **Documentation**: every public item is documented
  (`#![warn(missing_docs)]`); `cargo rustc --lib --all-features --
  -D warnings` and `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
  --all-features` build without a warning. The crate docs and README
  organize the library along the workflow (`extract`, `complete`,
  `collection`, `bibtex`, `files`) and name the intended consumers
  (editor plugins, RAG/LLM writing assistants).
- **Test coverage** (`cargo llvm-cov -p grobid-bibtex --all-features`,
  unit and integration tests; doctests are not measured on stable Rust):
  `bibtex.rs` 97.4%, `files.rs` 91.0%, `collection.rs` 90.2%,
  `extract.rs` 80.0% and `complete.rs` 63.0% line coverage — 91% across
  the library modules. The CLI binaries are exercised by the live run
  below rather than by unit tests.
- **Verification**: `cargo fmt --check`, `cargo check --all-targets
  --all-features`, `cargo clippy --all-targets --all-features --
  -D warnings` and `cargo test --all-features` (33 unit tests, 1 binary
  test, 2 integration tests and 15 doctests) all pass. Against a live
  GROBID server at `http://localhost:8070`: `pdf2bibtex` extracted 2
  entries from 2 PDFs; a second run with `--merge` skipped both as
  duplicates; `--append` added them with `-2` keys; `--rename --link`
  produced the keyed file name and a matching `file` field; `refs2bibtex`
  extracted 76 references (27 + 49) from the same PDFs.

## Roadmap

Planned work beyond v0.2.0. The version numbers are intentions, not
commitments.

- **0.3.0 — Deduplifier trait**: extract duplicate detection into a trait,
  so callers can plug in their own identity strategies (content,
  identifiers, file names) instead of relying on the built-in ones.
- **0.4.0 — fuzzy deduplifying**: add similarity-based matching for records
  that have no identifiers and differ slightly in their metadata.
- **0.5.0 — File level safety routines**: locking and write-event detection
  for shared `.bib` files and manifests, so concurrent tools cannot corrupt
  each other's writes.
