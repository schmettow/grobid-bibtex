# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
