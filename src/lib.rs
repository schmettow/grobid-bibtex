//! A reference management system for scientific writing.
//!
//! `grobid-bibtex` combines [GROBID] for reference extraction and [OpenAlex]
//! for reference completion with a data model organized around
//! BibTeX/BibLaTeX. It is the foundation for user-facing tools such as
//! editor plugins (citation keys, duplicate detection, pickers) and LLM- or
//! RAG-based writing assistants (structured, deduplicated bibliography
//! data).
//!
//! The modules follow the pipeline of that workflow:
//!
//! - [`extract`]: turn PDFs into [`Biblio`] records with a GROBID server.
//! - `complete` (feature `openalex`): fill gaps in extracted records from
//!   OpenAlex.
//! - [`collection`]: parse, index and append `.bib` files; assign citation
//!   keys; detect and merge duplicates.
//! - [`bibtex`]: render individual records as BibLaTeX and derive citation
//!   keys and file names.
//! - [`files`]: work with the PDFs behind the records (discovery, renaming).
//!
//! The `pdf2bibtex` and `refs2bibtex` binaries are front ends over these
//! modules.
//!
//! # Example
//!
//! ```
//! use std::path::PathBuf;
//!
//! use grobid_bibtex::collection::Collection;
//! use grobid_bibtex::{Author, Biblio};
//!
//! let biblio = Biblio {
//!     authors: vec![Author {
//!         surname: Some("Kahle".to_string()),
//!         ..Author::default()
//!     }],
//!     date: Some("2000".to_string()),
//!     ..Biblio::default()
//! };
//! let collection = Collection::new();
//! let entries = collection.format_all(&[(PathBuf::from("paper.pdf"), biblio)], true);
//! assert!(entries[0].1.starts_with("@misc{Kahle2000,"));
//! ```
//!
//! [GROBID]: https://grobid.readthedocs.io/
//! [OpenAlex]: https://openalex.org/
//!
//! The full README is included below.
#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(clippy::missing_errors_doc)]
#![warn(clippy::missing_panics_doc)]

pub mod bibtex;
pub mod collection;
#[cfg(feature = "openalex")]
pub mod complete;
pub mod extract;
pub mod files;

#[cfg(feature = "openalex")]
pub use grobid::openalex;

pub use collection::Collection;
pub use grobid::tei;
pub use grobid::{
    Author, Biblio, Citation, CitationConsolidation, DEFAULT_GROBID_URL, Error, GrobidClient,
    GrobidClientBuilder, HeaderConsolidation, PdfInput, ProcessOptions, RetryPolicy,
};
