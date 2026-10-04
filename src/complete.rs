//! Second-tier completion of extracted records against OpenAlex.
//!
//! Available with the `openalex` feature. Each record is looked up
//! independently with the [`Completer`], with a bounded number of concurrent
//! requests and the pacing the keyless OpenAlex pool expects. A lookup that
//! fails leaves its record unchanged and is reported as a [`Failure`], so an
//! unreachable or rate-limited API does not lose data. The order of the
//! input is kept.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::openalex::{Completer, Error};
use crate::{Biblio, Citation};

/// A lookup that failed; the affected record is left unchanged.
#[derive(Debug)]
pub struct Failure {
    /// 1-based position of the record in the completed batch.
    pub position: usize,
    /// The OpenAlex error.
    pub source: Error,
}

/// The outcome of a completion batch.
#[derive(Debug)]
pub struct Completed<T> {
    /// All records in input order, completed where a match was found.
    pub records: Vec<T>,
    /// The number of records matched on OpenAlex.
    pub matched: usize,
    /// The lookups that failed.
    pub failures: Vec<Failure>,
}

/// Complete extracted document headers against OpenAlex.
///
/// At most `workers` lookups are in flight at a time.
///
/// # Panics
///
/// Panics if `workers` is zero.
pub async fn biblios(
    completer: &Completer,
    records: Vec<(PathBuf, Biblio)>,
    workers: usize,
) -> Completed<(PathBuf, Biblio)> {
    let completer = completer.clone();
    gather(records, workers, move |mut record| {
        let completer = completer.clone();
        async move {
            match completer.complete(&record.1).await {
                Ok(Some(completion)) => {
                    record.1 = completion.biblio;
                    (record, Ok(true))
                }
                Ok(None) => (record, Ok(false)),
                Err(err) => (record, Err(err)),
            }
        }
    })
    .await
}

/// Complete extracted references against OpenAlex.
///
/// At most `workers` lookups are in flight at a time.
///
/// # Panics
///
/// Panics if `workers` is zero.
pub async fn citations(
    completer: &Completer,
    citations: Vec<Citation>,
    workers: usize,
) -> Completed<Citation> {
    let completer = completer.clone();
    gather(citations, workers, move |mut citation| {
        let completer = completer.clone();
        async move {
            match completer.complete(&citation.biblio).await {
                Ok(Some(completion)) => {
                    citation.biblio = completion.biblio;
                    (citation, Ok(true))
                }
                Ok(None) => (citation, Ok(false)),
                Err(err) => (citation, Err(err)),
            }
        }
    })
    .await
}

/// Look every item up under a concurrency bound and restore the input
/// order. `complete` returns the (possibly changed) item and whether it was
/// matched; `Ok(true)` is counted as a match.
async fn gather<T, F, Fut>(items: Vec<T>, workers: usize, mut complete: F) -> Completed<T>
where
    T: Send + 'static,
    F: FnMut(T) -> Fut,
    Fut: Future<Output = (T, Result<bool, Error>)> + Send + 'static,
{
    assert!(workers > 0, "at least one worker is required");
    let total = items.len();
    let semaphore = Arc::new(Semaphore::new(workers));
    let mut tasks = JoinSet::new();
    for (index, item) in items.into_iter().enumerate() {
        let permit = Arc::clone(&semaphore);
        let future = complete(item);
        tasks.spawn(async move {
            // Bound the number of concurrent lookups.
            let _permit = permit.acquire_owned().await.expect("semaphore not closed");
            let (item, outcome) = future.await;
            (index, item, outcome)
        });
        // OpenAlex asks the common (keyless) pool for at most 10 requests
        // per second, so pace the dispatch of the workers.
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let mut outcomes = Vec::with_capacity(total);
    let mut matched = 0usize;
    let mut failures = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        let (index, item, outcome) = joined.expect("worker task panicked");
        match outcome {
            Ok(true) => matched += 1,
            Ok(false) => {}
            Err(source) => failures.push(Failure {
                position: index + 1,
                source,
            }),
        }
        outcomes.push((index, item));
    }
    outcomes.sort_by_key(|(index, _)| *index);
    failures.sort_by_key(|failure| failure.position);
    Completed {
        records: outcomes.into_iter().map(|(_, item)| item).collect(),
        matched,
        failures,
    }
}
