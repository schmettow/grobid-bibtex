# grobid-bibtex

A reference management system for scientific writing, built on
[GROBID](https://grobid.readthedocs.io/) for reference extraction and
[OpenAlex](https://openalex.org/) for reference completion, with a data model
organized around BibTeX/BibLaTeX.

The crate is the foundation for user-facing tools such as editor plugins
(citation keys, duplicate detection, pickers) and LLM- or RAG-based writing
assistants (structured, deduplicated bibliography data). The bundled
`pdf2bibtex` and `refs2bibtex` binaries are front ends over the same API.

## Organization

| Module | Role |
| --- | --- |
| `extract` | PDFs into records with GROBID: batch `headers()` and `references()`, plus `collect_references()` for flattening, DOI-deduplicating and ordering extracted reference lists. |
| `complete` | Second-tier completion of extracted records against OpenAlex (feature `openalex`). |
| `collection` | `Collection` parses, indexes and appends `.bib` files, assigns citation keys and merges records with duplicate detection (content, identifiers, PDF file name). |
| `bibtex` | Rendering and naming for individual records: `format_entry()`, `format_all()`, `suggest_key()`, `unique_key()` and the `Author_Year_Title` file-name policies. |
| `files` | The PDFs behind the records: recursive discovery, the rename policy (`rename_pdfs_with()` with any file-stem style and collision style) and a `Manifest` that skips unchanged files on later runs. |

The pipeline is: discover PDFs (`files`) → extract records (`extract`) →
complete them (`complete`) → render and merge them into a collection
(`collection`, `bibtex`).

## Collection

```rust
use std::path::PathBuf;

use grobid_bibtex::collection::Collection;
use grobid_bibtex::{Author, Biblio};

let biblio = Biblio {
    authors: vec![Author {
        surname: Some("Kahle".to_string()),
        ..Author::default()
    }],
    date: Some("2000".to_string()),
    ..Biblio::default()
};
let mut collection = Collection::new();
let records = [(PathBuf::from("paper.pdf"), biblio)];
let (entries, duplicates) = collection.merge_all(&records, true).unwrap();
assert_eq!(duplicates, 0);
assert!(entries[0].1.starts_with("@misc{Kahle2000,"));
assert!(entries[0].1.contains("file = {paper.pdf},"));

// Merging the same records again detects the duplicate.
let (entries, duplicates) = collection.merge_all(&records, true).unwrap();
assert_eq!(duplicates, 1);
assert!(entries.is_empty());
```

Beyond `Collection`, the `bibtex` module provides the single-record helpers:
`entry_type()`, `year()`, `suggest_key()` and `unique_key()` for entry types
and collision-free citation keys, `format_entry()`/`format_entry_with_file()`
for typed BibLaTeX output, `format_all()` for batches, and
`suggest_file_name()`, `suggest_file_stem_with()`, `unique_path()` and
`unique_path_with_year()` for the file-naming policies (`FileStemStyle::Full`
and `FileStemStyle::Keyed`).

## Binaries

Both binaries process all PDFs in a directory recursively through a running
GROBID server and write their bibliographic metadata as BibTeX. They share
the common options below; run either with `--help` for all of them.

### pdf2bibtex

Writes one entry per document (its header):

```sh
cargo run --release --bin pdf2bibtex -- ~/papers -s http://localhost:8070
```

With `-r`/`--rename`, each PDF is renamed after its metadata has been
extracted to `Author_Year_<first 10 title words>.pdf`, e.g.
`Kahle_2000_The_Barc_model_for_continuous_variables.pdf`. Parts GROBID could
not extract (author, year or title) are dropped, non-ASCII characters are
removed, and colliding names get a `-2`, `-3`, ... suffix.

With `-l`/`--link`, each entry records the path of its PDF in a `file`
field, so reference managers can open the document; combined with
`-r`/`--rename`, the field points at the renamed file. Paths are recorded
as passed on the command line, so relative input paths stay relative.

With `-a`/`--append` or `-m`/`--merge`, the entries go into an existing
BibTeX file instead of a new one. Both load the target collection first and
reserve its citation keys, so a new entry whose suggested key already exists
gets a `-2`, `-3`, ... suffix. `--append` adds every extracted entry;
`--merge` skips records that are already there, where a record counts as
present when its normalized field content, one of its identifiers (DOI,
PMID, arXiv) or its PDF file name matches an existing entry. `--output`,
`--append` and `--merge` are mutually exclusive.

With `--openalex` (requires building with `--features openalex`), each
extracted header is completed against OpenAlex before entries are written
and PDFs are renamed: missing authors, journal, volume, pages, DOI, ... are
filled in from the matching work. See *Reference completion against
OpenAlex* in the [`grobid` documentation](https://docs.rs/grobid).

### refs2bibtex

Extracts the bibliographic *references* of all PDFs in a directory (via
`/api/processReferences`) and writes one BibTeX entry per reference:

```sh
cargo run --release --bin refs2bibtex -- ~/papers -s http://localhost:8070
```

Both binaries discover PDFs recursively and process them with a bounded
number of concurrent requests (`-w`, default 4); per-document failures are
reported on stderr and skipped, and an unresponsive server is detected by a
liveness probe with bounded retries and a clear error message. Entry types
(`@article`, `@incollection`, `@techreport`, `@book`, `@misc`) and keys
(first author surname + year, deduplicated) are derived from the parsed
metadata via the `bibtex` helpers. `refs2bibtex` skips empty parse results
and drops references with a duplicate DOI, and supports reference
consolidation against CrossRef with `-c`/`--consolidate`. Both binaries
accept `--openalex` when built with `--features openalex`: `refs2bibtex`
completes the collected references, `pdf2bibtex` each document header,
before the entries are written:

```sh
cargo run --release --features openalex --bin refs2bibtex -- ~/papers --openalex
cargo run --release --features openalex --bin pdf2bibtex -- ~/papers --openalex
```

## Features

- `openalex`: enables the OpenAlex completion tier of the `grobid` client,
  the `complete` module and the `--openalex` option of both binaries.
