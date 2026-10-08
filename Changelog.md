# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
