//! Extraction of bibliographic records from PDFs with GROBID.
//!
//! This is the input side of the reference management pipeline: batch
//! processing of PDF directories through a GROBID server, either for the
//! documents' headers ([`headers`]) or for the references cited in them
//! ([`references`]). Both bound the number of concurrent requests and return
//! one result per input PDF, in input order. Failures are reported per
//! document instead of aborting the batch, so one unreadable PDF does not
//! lose the rest.
//!
//! Extracted references of several documents are flattened, deduplicated and
//! ordered with [`collect_references`].

use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::bibtex;
use crate::{Biblio, Citation, Error, GrobidClient, PdfInput, ProcessOptions};

/// A record extracted from a PDF, together with the PDF it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Extracted<T> {
    /// The PDF that was processed.
    pub path: PathBuf,
    /// The extracted record.
    pub record: T,
}

/// A PDF that could not be processed.
#[derive(Debug)]
pub struct ExtractionError {
    /// The PDF that could not be processed.
    pub path: PathBuf,
    /// The error reported by the GROBID client.
    pub source: Error,
}

impl fmt::Display for ExtractionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.path.display(), self.source)
    }
}

impl std::error::Error for ExtractionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Process `pdfs` through GROBID's `/api/processHeaderDocument` service and
/// return one document header per PDF.
///
/// At most `workers` requests are in flight at a time. The results are in
/// the order of `pdfs`; a failed PDF carries its path and the client error.
///
/// # Panics
///
/// Panics if `workers` is zero.
pub async fn headers(
    client: &GrobidClient,
    pdfs: Vec<PathBuf>,
    options: ProcessOptions,
    workers: usize,
) -> Vec<Result<Extracted<Biblio>, ExtractionError>> {
    let client = client.clone();
    bounded(pdfs, workers, move |pdf| {
        let client = client.clone();
        let options = options.clone();
        async move {
            let document = client
                .process_header_document(PdfInput::from(pdf.as_path()), &options)
                .await?;
            Ok(document.header.biblio)
        }
    })
    .await
}

/// Process `pdfs` through GROBID's `/api/processReferences` service and
/// return the cited references of each PDF.
///
/// At most `workers` requests are in flight at a time. The results are in
/// the order of `pdfs`; a failed PDF carries its path and the client error.
///
/// # Panics
///
/// Panics if `workers` is zero.
pub async fn references(
    client: &GrobidClient,
    pdfs: Vec<PathBuf>,
    options: ProcessOptions,
    workers: usize,
) -> Vec<Result<Extracted<Vec<Citation>>, ExtractionError>> {
    let client = client.clone();
    bounded(pdfs, workers, move |pdf| {
        let client = client.clone();
        let options = options.clone();
        async move {
            client
                .process_references(PdfInput::from(pdf.as_path()), &options)
                .await
        }
    })
    .await
}

/// Run `process` for every PDF under a concurrency bound, one task each,
/// and restore the input order.
async fn bounded<T, F, Fut>(
    pdfs: Vec<PathBuf>,
    workers: usize,
    mut process: F,
) -> Vec<Result<Extracted<T>, ExtractionError>>
where
    T: Send + 'static,
    F: FnMut(PathBuf) -> Fut,
    Fut: Future<Output = Result<T, Error>> + Send + 'static,
{
    assert!(workers > 0, "at least one worker is required");
    let semaphore = Arc::new(Semaphore::new(workers));
    let mut tasks = JoinSet::new();
    for (index, pdf) in pdfs.into_iter().enumerate() {
        let permit = Arc::clone(&semaphore);
        let future = process(pdf.clone());
        tasks.spawn(async move {
            // Bound the number of in-flight requests.
            let _permit = permit.acquire_owned().await.expect("semaphore not closed");
            let outcome = future
                .await
                .map(|record| Extracted {
                    path: pdf.clone(),
                    record,
                })
                .map_err(|source| ExtractionError { path: pdf, source });
            (index, outcome)
        });
    }

    let mut outcomes = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        outcomes.push(joined.expect("worker task panicked"));
    }
    outcomes.sort_by_key(|(index, _)| *index);
    outcomes.into_iter().map(|(_, outcome)| outcome).collect()
}

/// A flattened, deduplicated, deterministically ordered reference list.
#[derive(Debug)]
pub struct CollectedReferences {
    /// The references of all documents, ordered by suggested citation key.
    pub citations: Vec<Citation>,
    /// References dropped because their parse result was empty.
    pub empty: usize,
    /// References dropped because their DOI was already seen.
    pub duplicates: usize,
}

/// Flatten the per-document reference lists into a single collection:
/// empty parse results are skipped, references sharing a DOI are kept only
/// once (the same paper cited by several documents yields one entry), and
/// the result is ordered by suggested citation key.
pub fn collect_references(results: Vec<(PathBuf, Vec<Citation>)>) -> CollectedReferences {
    let mut seen_dois: HashSet<String> = HashSet::new();
    let mut keyed: Vec<(Citation, String, PathBuf, usize)> = Vec::new();
    let mut empty = 0usize;
    let mut duplicates = 0usize;
    for (path, citations) in results {
        for (index, citation) in citations.into_iter().enumerate() {
            if citation.is_empty() {
                empty += 1;
                continue;
            }
            if let Some(doi) = citation.doi.as_deref().map(str::to_ascii_lowercase) {
                if !seen_dois.insert(doi) {
                    duplicates += 1;
                    continue;
                }
            }
            let suggested = bibtex::suggest_key(&citation);
            keyed.push((citation, suggested, path.clone(), index));
        }
    }
    // Sort by suggested key; path and in-document index break ties for a
    // deterministic order across runs.
    keyed.sort_by(|a, b| {
        a.1.cmp(&b.1)
            .then_with(|| a.2.cmp(&b.2))
            .then_with(|| a.3.cmp(&b.3))
    });
    CollectedReferences {
        citations: keyed.into_iter().map(|(citation, ..)| citation).collect(),
        empty,
        duplicates,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Author;

    fn citation(doi: Option<&str>, surname: Option<&str>) -> Citation {
        let mut biblio = Biblio::default();
        if let Some(surname) = surname {
            biblio.authors.push(Author {
                surname: Some(surname.to_string()),
                ..Author::default()
            });
        }
        biblio.doi = doi.map(str::to_string);
        biblio.date = Some("2020".to_string());
        Citation { index: 0, biblio }
    }

    #[test]
    fn test_collect_references_dedupe_and_sort() {
        let results = vec![
            (
                PathBuf::from("b.pdf"),
                vec![
                    citation(Some("10.1/x"), Some("Zed")),
                    citation(Some("10.1/y"), Some("Able")),
                ],
            ),
            (
                PathBuf::from("a.pdf"),
                vec![citation(Some("10.1/x"), Some("Zed")), Citation::default()],
            ),
        ];
        let collected = collect_references(results);
        // The empty citation is skipped and the duplicate DOI dropped.
        assert_eq!(collected.empty, 1);
        assert_eq!(collected.duplicates, 1);
        let keys: Vec<String> = collected
            .citations
            .iter()
            .map(|citation| bibtex::suggest_key(citation))
            .collect();
        assert_eq!(keys, vec!["Able2020", "Zed2020"]);
    }
}
