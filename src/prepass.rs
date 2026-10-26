//! Reusable GROBID pre-pass orchestration.
//!
//! A directory corpus (or a set of dropped files) is brought in sync with a
//! running GROBID server:
//!
//! 1. **Fingerprint** — the sidecar manifest [`MANIFEST_NAME`] in the corpus
//!    folder records size/mtime and the parsed record of every processed
//!    PDF, so unchanged files are never parsed twice and a bibliography can
//!    be rebuilt from the cache.
//! 2. **Extract** — new or changed PDFs are parsed by GROBID
//!    (`/api/processHeaderDocument`).
//! 3. **Complete** — parsed headers are completed against OpenAlex; failures
//!    keep the parsed header.
//! 4. **Rename** — with [`PrepassConfig::rename`], files are renamed to
//!    `Key - Full authors - Full title - Year` ([`RENAME_OPTIONS`]), shortened
//!    to fit the filesystem's 255-byte file-name limit, with collisions
//!    numbered on the year.
//! 5. **Merge** — with [`PrepassConfig::bibtex`], records are appended to the
//!    `.bib` file with duplicate detection and a `file` link to the renamed
//!    PDF.
//!
//! Every non-fatal problem (a PDF GROBID cannot parse, a failed OpenAlex
//! lookup, a failed rename) is recorded in [`PrepassReport`] or logged; only
//! an unreadable bibliography, an unreachable server, or a batch in which
//! every PDF fails aborts the run. A caller that needs UI progress or
//! cancellation implements [`PrepassObserver`] and uses [`prepass_with`].
//!
//! This module is available with the `pipeline` feature.

use std::path::{Path, PathBuf};
use std::time::Duration;

use log::{info, warn};

use crate::bibtex::{FileStemOptions, FileStemStyle};
use crate::collection::{self, Collection, LoadError, MergeError};
use crate::files::{self, Collision, Manifest, Rename};
use crate::openalex::Completer;
use crate::{Biblio, DEFAULT_GROBID_URL, Error, GrobidClient, ProcessOptions, complete, extract};

/// Sidecar manifest with per-file fingerprints of everything already
/// processed, so unchanged PDFs are not parsed again. It also caches each
/// file's extracted record, so a bibliography can be rebuilt from it.
pub const MANIFEST_NAME: &str = ".ragrig_grobid.json";

/// Rename policy: `Key - Full authors - Full title - Year`; syntax
/// characters are stripped, Unicode letters are kept (the citation key
/// itself is always ASCII). Names are shortened to fit the filesystem's
/// 255-byte file-name limit.
pub const RENAME_OPTIONS: FileStemOptions = FileStemOptions {
    style: FileStemStyle::Keyed,
    // `title_words` only applies to `Compact`; `Keyed` keeps the full title.
    title_words: 10,
    ascii_only: false,
};

/// Default number of concurrent GROBID/OpenAlex requests.
pub const DEFAULT_WORKERS: usize = 4;

/// Default number of liveness probes before giving up on the server.
pub const DEFAULT_PROBE_ATTEMPTS: usize = 5;

/// Default delay between liveness probes; a cold GROBID container can take a
/// while to preload its models.
pub const DEFAULT_PROBE_DELAY: Duration = Duration::from_secs(3);

/// Default per-request timeout for OpenAlex lookups, so an unreachable API
/// host cannot stall a worker for the OS TCP timeout.
pub const DEFAULT_OPENALEX_TIMEOUT: Duration = Duration::from_secs(15);

/// The BibTeX file the pre-pass merges records into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BibtexTarget {
    /// The `.bib` file; created when the first entry is merged.
    pub path: PathBuf,
    /// Record each PDF's path in the entry's `file` field.
    pub link: bool,
}

/// How a pre-pass run is parameterised.
///
/// [`PrepassConfig::new`] fills in the defaults shared by the co-developed
/// hosts; set the remaining fields directly. The OpenAlex and probe fields
/// are seams for hermetic tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepassConfig {
    /// The corpus folder: where the manifest lives, and the rename base.
    pub folder: PathBuf,
    /// The files to consider; `None` scans [`PrepassConfig::folder`]
    /// recursively. Hosts that ingest individual drops pass the local files
    /// here; corpus maintenance passes `None`.
    pub sources: Option<Vec<PathBuf>>,
    /// Merge target; `None` runs a rename-only pass.
    pub bibtex: Option<BibtexTarget>,
    /// Base URL of the GROBID server.
    pub grobid_url: String,
    /// OpenAlex-compatible base URL; `None` uses the public API.
    pub openalex_url: Option<String>,
    /// Whether to complete parsed headers against OpenAlex at all.
    pub openalex: bool,
    /// Rename processed PDFs to `Key - Full authors - Full title - Year`.
    pub rename: bool,
    /// Maximum number of concurrent GROBID/OpenAlex requests.
    pub workers: usize,
    /// Liveness probes before giving up on the GROBID server.
    pub probe_attempts: usize,
    /// Delay between liveness probes.
    pub probe_delay: Duration,
    /// Per-request timeout for OpenAlex lookups.
    pub openalex_timeout: Duration,
}

impl PrepassConfig {
    /// A configuration for `folder` with the shared defaults: OpenAlex
    /// completion and renaming on, [`DEFAULT_WORKERS`] workers and the
    /// standard probe/OpenAlex timings.
    pub fn new(folder: impl Into<PathBuf>) -> Self {
        Self {
            folder: folder.into(),
            sources: None,
            bibtex: None,
            grobid_url: DEFAULT_GROBID_URL.to_string(),
            openalex_url: None,
            openalex: true,
            rename: true,
            workers: DEFAULT_WORKERS,
            probe_attempts: DEFAULT_PROBE_ATTEMPTS,
            probe_delay: DEFAULT_PROBE_DELAY,
            openalex_timeout: DEFAULT_OPENALEX_TIMEOUT,
        }
    }
}

/// The stage a pre-pass progress update belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrepassStage {
    /// New or changed PDFs are parsed by the GROBID server.
    Parsing,
    /// Parsed headers are completed against OpenAlex.
    Completing,
    /// Files are renamed to their metadata name.
    Renaming,
    /// Records are merged into the bibliography.
    Merging,
}

/// One progress update from a running pre-pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepassProgress {
    /// The stage the run is in.
    pub stage: PrepassStage,
    /// 1-based step within the stage.
    pub done: usize,
    /// Steps in this stage.
    pub total: usize,
    /// The file the step is about, when the stage has one.
    pub path: Option<PathBuf>,
}

/// Receives progress and cancellation from a running pre-pass.
///
/// Implement this to drive a UI or to abort a long run. Both methods have
/// inert defaults, so an observer only overrides what it needs;
/// [`NoObserver`] is the no-op implementation.
pub trait PrepassObserver {
    /// Called after each batch (and once per single-step stage).
    fn progress(&mut self, _progress: PrepassProgress) {}

    /// Polled between batches and before each stage; `true` aborts the run
    /// with [`PrepassError::Cancelled`].
    fn cancelled(&self) -> bool {
        false
    }
}

/// A [`PrepassObserver`] that ignores progress and never cancels.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoObserver;

impl PrepassObserver for NoObserver {}

/// A PDF that could not be parsed or renamed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepassFailure {
    /// The affected file.
    pub path: PathBuf,
    /// The failure, suitable for a one-line report.
    pub reason: String,
}

/// What one pre-pass run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrepassReport {
    /// Files that ended with a record (freshly parsed plus cache hits).
    pub processed: usize,
    /// Files parsed by GROBID in this run.
    pub parsed: usize,
    /// Files whose record came from the manifest cache.
    pub cached: usize,
    /// Files renamed to their metadata name.
    pub renamed: usize,
    /// Files that already had their metadata name.
    pub unchanged: usize,
    /// Records completed against OpenAlex.
    pub completed: usize,
    /// Files for which GROBID returned no usable author/year/title.
    pub unnamed: Vec<PathBuf>,
    /// Entries newly appended to the bibliography.
    pub merged: usize,
    /// Records the merge skipped because they were already in the file.
    pub duplicates: usize,
    /// Per-file failures (parse and rename).
    pub failed: Vec<PrepassFailure>,
}

/// Errors that abort a pre-pass run.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PrepassError {
    /// The bibliography exists but cannot be read or parsed. The run fails
    /// before any work because it could not record what it parses.
    #[error("cannot read the BibTeX file {path}: {source}")]
    Bibliography {
        /// The bibliography that failed.
        path: PathBuf,
        /// The underlying load error.
        #[source]
        source: LoadError,
    },
    /// The corpus folder could not be scanned for PDFs.
    #[error("cannot scan {path} for PDFs: {source}")]
    Scan {
        /// The folder that failed.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The configured GROBID server URL is invalid.
    #[error("invalid GROBID server URL {url:?}: {source}")]
    ServerUrl {
        /// The configured URL.
        url: String,
        /// The client error.
        #[source]
        source: Error,
    },
    /// The GROBID server did not become ready.
    #[error("GROBID server at {url} is not ready: {source}")]
    NotReady {
        /// The configured URL.
        url: String,
        /// The probe error.
        #[source]
        source: Error,
    },
    /// The OpenAlex completer could not be built.
    #[error("cannot build the OpenAlex completer: {source}")]
    OpenAlex {
        /// The client error.
        #[source]
        source: crate::openalex::Error,
    },
    /// Every PDF in the batch failed to parse, so there is nothing to merge.
    #[error("none of the PDFs could be parsed by the server at {url}: {reason}")]
    NothingParsed {
        /// The configured URL.
        url: String,
        /// The first parse failure, for context.
        reason: String,
    },
    /// The records could not be merged into the bibliography.
    #[error("cannot update the BibTeX file {path}: {source}")]
    Merge {
        /// The target that failed.
        path: PathBuf,
        /// The underlying merge error.
        #[source]
        source: MergeError,
    },
    /// The observer asked to stop.
    #[error("the pre-pass was cancelled")]
    Cancelled,
}

/// Run a pre-pass with a no-op observer.
///
/// See [`prepass_with`] for the full contract.
///
/// # Errors
///
/// Returns [`PrepassError`] when the bibliography is invalid, the folder
/// cannot be scanned, the server is unreachable, every PDF fails, or the
/// merge cannot be written.
pub async fn prepass(config: &PrepassConfig) -> Result<PrepassReport, PrepassError> {
    let mut observer = NoObserver;
    prepass_with(config, &mut observer).await
}

/// Run a pre-pass for [`PrepassConfig::folder`] or
/// [`PrepassConfig::sources`].
///
/// The run is resilient: a PDF GROBID cannot parse or rename failure lands in
/// [`PrepassReport::failed`], an OpenAlex failure keeps the parsed header,
/// and a corrupt manifest starts empty. Only the conditions behind
/// [`PrepassError`] abort the run.
///
/// # Errors
///
/// Returns [`PrepassError`] when the bibliography is invalid, the folder
/// cannot be scanned, the server is unreachable, every PDF fails, the
/// observer cancels, or the merge cannot be written.
pub async fn prepass_with<O: PrepassObserver>(
    config: &PrepassConfig,
    observer: &mut O,
) -> Result<PrepassReport, PrepassError> {
    let mut report = PrepassReport::default();

    // A corrupt bibliography must fail before any work: the run could not
    // record what it parses.
    if let Some(bibtex) = &config.bibtex {
        Collection::load_or_new(&bibtex.path).map_err(|source| PrepassError::Bibliography {
            path: bibtex.path.clone(),
            source,
        })?;
    }

    let manifest_path = config.folder.join(MANIFEST_NAME);
    let mut manifest = match Manifest::load(&manifest_path) {
        Ok(manifest) => manifest,
        Err(err) => {
            warn!("GROBID: ignoring corrupt manifest: {err}");
            Manifest::empty(&manifest_path)
        }
    };

    let pdfs = match &config.sources {
        Some(sources) => sources.clone(),
        None => match files::collect_pdfs(&config.folder) {
            Ok(pdfs) => pdfs,
            // A missing folder has nothing to do; the caller's own sync
            // reports it when that matters.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(report),
            Err(source) => {
                return Err(PrepassError::Scan {
                    path: config.folder.clone(),
                    source,
                });
            }
        },
    };

    // Partition the PDFs: new and changed files need GROBID; unchanged files
    // with a cached record can be merged as they are. Unchanged files
    // without a cached record — e.g. after a run that only renamed, or a
    // manifest written before records were cached — are parsed once so the
    // cache and the bibliography catch up.
    let (needed, cached) = partition(&manifest, config, &config.folder, pdfs);
    report.cached = cached.len();
    if needed.is_empty() && cached.is_empty() {
        info!(
            "GROBID pre-pass: no new or changed PDFs in {}.",
            config.folder.display()
        );
        return Ok(report);
    }

    // Cached records alone need no server: top up the bibliography offline
    // instead of probing GROBID.
    let mut results: Vec<(PathBuf, Biblio)> = Vec::new();
    if needed.is_empty() {
        info!(
            "GROBID pre-pass: merging {} cached record(s) from {} (no PDFs to parse).",
            cached.len(),
            config.folder.display()
        );
    } else {
        check_cancelled(observer)?;
        parse_and_rename(
            config,
            observer,
            &mut manifest,
            needed,
            &mut results,
            &mut report,
        )
        .await?;
    }

    let mut records = cached;
    records.extend(results);
    report.processed = records.len();

    if let Some(bibtex) = &config.bibtex {
        if !records.is_empty() {
            check_cancelled(observer)?;
            observer.progress(PrepassProgress {
                stage: PrepassStage::Merging,
                done: 1,
                total: 1,
                path: Some(bibtex.path.clone()),
            });
            let merged =
                collection::merge_file(&bibtex.path, &records, bibtex.link).map_err(|source| {
                    PrepassError::Merge {
                        path: bibtex.path.clone(),
                        source,
                    }
                })?;
            report.merged = merged.entries.len();
            report.duplicates = merged.duplicates;
            let count = report.merged;
            info!(
                "BibTeX: merged {count} entr{} into {} ({} duplicate(s) skipped, {} total).",
                if count == 1 { "y" } else { "ies" },
                bibtex.path.display(),
                merged.duplicates,
                merged.total
            );
        }
    }

    Ok(report)
}

/// Parse, complete and rename the new or changed `needed` files.
async fn parse_and_rename<O: PrepassObserver>(
    config: &PrepassConfig,
    observer: &mut O,
    manifest: &mut Manifest,
    needed: Vec<PathBuf>,
    results: &mut Vec<(PathBuf, Biblio)>,
    report: &mut PrepassReport,
) -> Result<(), PrepassError> {
    let total = needed.len();
    let workers = config.workers.max(1);
    let mut notes = Vec::new();
    if config.openalex {
        notes.push("completing against OpenAlex");
    }
    if config.rename {
        notes.push("renaming to Key - Full authors - Full title - Year");
    }
    if config.bibtex.is_some() {
        notes.push("merging into the BibTeX file");
    }
    let notes = if notes.is_empty() {
        String::new()
    } else {
        format!(" ({})", notes.join(", "))
    };
    info!(
        "GROBID pre-pass: {total} PDF(s) in {} \u{2014} parsing headers{notes} \
         with {workers} worker(s).",
        config.folder.display()
    );

    let client =
        GrobidClient::new(&config.grobid_url).map_err(|source| PrepassError::ServerUrl {
            url: config.grobid_url.clone(),
            source,
        })?;
    info!(
        "GROBID pre-pass: waiting for the server at {} ...",
        config.grobid_url
    );
    client
        .wait_until_ready(config.probe_attempts.max(1), config.probe_delay)
        .await
        .map_err(|source| PrepassError::NotReady {
            url: config.grobid_url.clone(),
            source,
        })?;

    // Parse the headers in worker-sized batches: the batch size is the
    // concurrency bound and gives progress and cancellation a granularity
    // finer than the whole run.
    let mut done = 0usize;
    for batch in needed.chunks(workers) {
        check_cancelled(observer)?;
        let outcomes = extract::headers(
            &client,
            batch.to_vec(),
            ProcessOptions::default(),
            batch.len(),
        )
        .await;
        for outcome in outcomes {
            match outcome {
                Ok(extract::Extracted { path, record }) => results.push((path, record)),
                Err(err) => {
                    warn!("GROBID: skipping {}: {}", err.path.display(), err.source);
                    report.failed.push(PrepassFailure {
                        path: err.path,
                        reason: err.source.to_string(),
                    });
                }
            }
        }
        done += batch.len();
        observer.progress(PrepassProgress {
            stage: PrepassStage::Parsing,
            done,
            total,
            path: batch.last().cloned(),
        });
    }
    if results.is_empty() {
        let reason = report
            .failed
            .first()
            .map(|failure| failure.reason.clone())
            .unwrap_or_else(|| "no header could be parsed".to_string());
        return Err(PrepassError::NothingParsed {
            url: config.grobid_url.clone(),
            reason,
        });
    }

    // Complete the parsed headers against OpenAlex; failures keep the
    // GROBID header as parsed.
    if config.openalex {
        complete_and_report(config, observer, results, report).await?;
    }

    // Rename in deterministic path order; collisions are numbered on the
    // year, so authors and title keep their place in the name.
    if config.rename && !results.is_empty() {
        check_cancelled(observer)?;
        observer.progress(PrepassProgress {
            stage: PrepassStage::Renaming,
            done: 1,
            total: 1,
            path: None,
        });
        for event in files::rename_pdfs_with(results, &RENAME_OPTIONS, Collision::Year) {
            match event {
                Rename::Renamed { from, to } => {
                    info!(
                        "GROBID: renamed {} \u{2192} {}",
                        from.display(),
                        to.display()
                    );
                    report.renamed += 1;
                }
                Rename::Unnamed { path } => {
                    info!(
                        "GROBID: no usable author/year/title for {}; keeping name",
                        path.display()
                    );
                    report.unnamed.push(path);
                }
                Rename::Failed { from, source, .. } => {
                    warn!("GROBID: cannot rename {}: {source}", from.display());
                    report.failed.push(PrepassFailure {
                        path: from,
                        reason: format!("cannot rename: {source}"),
                    });
                }
                Rename::Unchanged { .. } => report.unchanged += 1,
            }
        }
    }

    // Record every processed file under its final name, including failed
    // renames and files that already had the right name; the cached record
    // lets later runs merge without GROBID.
    for (path, biblio) in results.iter() {
        manifest.record_with_biblio(&config.folder, path, biblio);
    }
    manifest.prune(&config.folder);
    if let Err(err) = manifest.save() {
        warn!("GROBID: {err}");
    }

    report.parsed = results.len();
    if config.rename {
        let processed = report.parsed;
        let renamed = report.renamed;
        info!(
            "GROBID pre-pass done: {renamed} of {processed} PDF(s) renamed, \
             {} OpenAlex-completed, {} failed.",
            report.completed,
            report.failed.len()
        );
    } else {
        info!(
            "GROBID pre-pass done: {} OpenAlex-completed, {} failed.",
            report.completed,
            report.failed.len()
        );
    }
    Ok(())
}

/// Complete `results` against OpenAlex in worker-sized batches.
async fn complete_and_report<O: PrepassObserver>(
    config: &PrepassConfig,
    observer: &mut O,
    results: &mut Vec<(PathBuf, Biblio)>,
    report: &mut PrepassReport,
) -> Result<(), PrepassError> {
    let mut builder = Completer::builder().timeout(config.openalex_timeout);
    if let Some(base_url) = &config.openalex_url {
        builder = builder
            .base_url(base_url)
            .map_err(|source| PrepassError::OpenAlex { source })?;
    }
    let completer = builder
        .build()
        .map_err(|source| PrepassError::OpenAlex { source })?;

    let workers = config.workers.max(1);
    let total = results.len();
    let mut done = 0usize;
    let mut completed: Vec<(PathBuf, Biblio)> = Vec::with_capacity(total);
    for batch in results.chunks(workers) {
        check_cancelled(observer)?;
        let batch_report = complete::biblios(&completer, batch.to_vec(), batch.len()).await;
        for failure in &batch_report.failures {
            if let Some((path, _)) = batch_report.records.get(failure.position - 1) {
                warn!(
                    "OpenAlex lookup failed for {}: {}",
                    path.display(),
                    failure.source
                );
            }
        }
        report.completed += batch_report.matched;
        completed.extend(batch_report.records);
        done += batch.len();
        observer.progress(PrepassProgress {
            stage: PrepassStage::Completing,
            done,
            total,
            path: None,
        });
    }
    *results = completed;
    Ok(())
}

/// Split `pdfs` into the files that need GROBID and the files whose cached
/// record can be merged directly.
///
/// A changed file always needs processing; an unchanged file with a cached
/// record is merged from the cache; an unchanged file without one is
/// processed once to fill the cache (when merging is configured). Rename-only
/// runs skip unchanged files entirely.
fn partition(
    manifest: &Manifest,
    config: &PrepassConfig,
    folder: &Path,
    pdfs: Vec<PathBuf>,
) -> (Vec<PathBuf>, Vec<(PathBuf, Biblio)>) {
    let mut needed: Vec<PathBuf> = Vec::new();
    let mut cached: Vec<(PathBuf, Biblio)> = Vec::new();
    for path in pdfs {
        if !manifest.is_unchanged(folder, &path) {
            needed.push(path);
            continue;
        }
        if config.bibtex.is_none() {
            continue;
        }
        match manifest.biblio(folder, &path) {
            Some(biblio) => cached.push((path, biblio.clone())),
            None => needed.push(path),
        }
    }
    (needed, cached)
}

/// Turn cancellation into the library error the run reports.
fn check_cancelled<O: PrepassObserver>(observer: &O) -> Result<(), PrepassError> {
    if observer.cancelled() {
        return Err(PrepassError::Cancelled);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Author;

    fn biblio(author: Option<&str>, year: Option<&str>, title: Option<&str>) -> Biblio {
        Biblio {
            authors: author
                .map(|surname| Author {
                    surname: Some(surname.to_string()),
                    ..Author::default()
                })
                .into_iter()
                .collect(),
            date: year.map(str::to_string),
            title: title.map(str::to_string),
            ..Biblio::default()
        }
    }

    #[test]
    fn target_is_key_full_authors_title_year() {
        let biblio = biblio(
            Some("Kahle"),
            Some("2000-03-01"),
            Some("The Barc model for continuous variables with extra words beyond ten"),
        );
        let target = crate::bibtex::suggest_file_name_with(
            Path::new("/papers/orig.pdf"),
            &biblio,
            &RENAME_OPTIONS,
        )
        .expect("target");
        assert_eq!(
            target.file_name().unwrap(),
            "Kahle2000 - Kahle - The Barc model for continuous variables with extra words beyond ten - 2000.pdf"
        );
    }

    #[test]
    fn target_omits_missing_parts() {
        let biblio = biblio(None, Some("2019"), Some("A lone title"));
        let target =
            crate::bibtex::suggest_file_name_with(Path::new("x.pdf"), &biblio, &RENAME_OPTIONS)
                .expect("target");
        // Without an author or year there is no key; the title leads.
        assert_eq!(target.file_name().unwrap(), "A lone title - 2019.pdf");
        assert!(
            crate::bibtex::suggest_file_name_with(
                Path::new("x.pdf"),
                &Biblio::default(),
                &RENAME_OPTIONS
            )
            .is_none()
        );
    }

    /// A corpus folder with one fake PDF, plus a manifest that records it
    /// as already processed with `biblio` as its extracted record.
    fn cached_corpus(dir: &Path, name: &str, biblio: &Biblio) -> PathBuf {
        let folder = dir.join(name);
        std::fs::create_dir_all(&folder).expect("create corpus folder");
        let pdf = folder.join("scan_0012.pdf");
        std::fs::write(&pdf, b"%PDF-1.4 fake").expect("write pdf");
        let mut manifest = Manifest::empty(folder.join(MANIFEST_NAME));
        manifest.record_with_biblio(&folder, &pdf, biblio);
        manifest.save().expect("save manifest");
        folder
    }

    /// A pre-pass that must never contact the server: nothing listens on the
    /// URL, so a probe would fail the test.
    fn offline_config(folder: &Path, bibtex: Option<BibtexTarget>) -> PrepassConfig {
        let mut config = PrepassConfig::new(folder);
        config.grobid_url = "http://127.0.0.1:1".to_string();
        config.workers = 1;
        config.rename = false;
        config.bibtex = bibtex;
        config
    }

    #[tokio::test]
    async fn cached_records_merge_without_a_server() {
        let dir = tempfile::tempdir().expect("temp dir");
        let record = biblio(Some("Kahle"), Some("2000"), Some("The Barc model"));
        let folder = cached_corpus(dir.path(), "papers", &record);
        let bib = dir.path().join("refs.bib");
        let config = offline_config(
            &folder,
            Some(BibtexTarget {
                path: bib.clone(),
                link: true,
            }),
        );

        prepass(&config).await.expect("prepass");
        let text = std::fs::read_to_string(&bib).expect("read bib");
        assert!(text.starts_with("@misc{Kahle2000,"), "{text}");
        assert!(text.contains("file = {"), "{text}");
        assert!(text.contains("scan_0012.pdf"), "{text}");

        // Idempotent: nothing new, the file is left as it is.
        prepass(&config).await.expect("second prepass");
        assert_eq!(std::fs::read_to_string(&bib).expect("read"), text);
    }

    #[tokio::test]
    async fn deleted_bibtex_is_rebuilt_from_cache() {
        let dir = tempfile::tempdir().expect("temp dir");
        let record = biblio(Some("Kahle"), Some("2000"), Some("The Barc model"));
        let folder = cached_corpus(dir.path(), "papers", &record);
        let bib = dir.path().join("refs.bib");
        let config = offline_config(
            &folder,
            Some(BibtexTarget {
                path: bib.clone(),
                link: true,
            }),
        );

        prepass(&config).await.expect("first prepass");
        let text = std::fs::read_to_string(&bib).expect("read bib");
        std::fs::remove_file(&bib).expect("delete bib");

        // The unchanged PDF is not re-parsed; the record comes from the
        // manifest cache and the file is recreated.
        prepass(&config).await.expect("second prepass");
        assert_eq!(std::fs::read_to_string(&bib).expect("read bib"), text);
    }

    #[tokio::test]
    async fn merge_without_link_omits_file_field() {
        let dir = tempfile::tempdir().expect("temp dir");
        let record = biblio(Some("Kahle"), Some("2000"), Some("The Barc model"));
        let folder = cached_corpus(dir.path(), "papers", &record);
        let bib = dir.path().join("refs.bib");
        let config = offline_config(
            &folder,
            Some(BibtexTarget {
                path: bib.clone(),
                link: false,
            }),
        );

        prepass(&config).await.expect("prepass");
        let text = std::fs::read_to_string(&bib).expect("read bib");
        assert!(text.starts_with("@misc{Kahle2000,"), "{text}");
        assert!(!text.contains("file = "), "{text}");
    }

    #[test]
    fn partition_upgrades_unchanged_files_without_cached_records() {
        let dir = tempfile::tempdir().expect("temp dir");
        let folder = dir.path().join("papers");
        std::fs::create_dir_all(&folder).expect("create folder");
        let pdf = folder.join("a.pdf");
        std::fs::write(&pdf, b"pdf").expect("write pdf");
        let mut manifest = Manifest::empty(folder.join(MANIFEST_NAME));
        // Fingerprint only, as a rename-only run or an older manifest would
        // leave it.
        manifest.record(&folder, &pdf);

        let config = offline_config(
            &folder,
            Some(BibtexTarget {
                path: dir.path().join("refs.bib"),
                link: true,
            }),
        );
        let (needed, cached) = partition(&manifest, &config, &folder, vec![pdf.clone()]);
        assert_eq!(needed, vec![pdf.clone()], "must be parsed once");
        assert!(cached.is_empty());

        // A rename-only run still skips unchanged files entirely.
        let rename_only = offline_config(&folder, None);
        let (needed, cached) = partition(&manifest, &rename_only, &folder, vec![pdf.clone()]);
        assert!(needed.is_empty());
        assert!(cached.is_empty());

        // With a cached record, the file is merged without parsing.
        manifest.record_with_biblio(
            &folder,
            &pdf,
            &biblio(Some("Kahle"), Some("2000"), Some("The Barc model")),
        );
        let (needed, cached) = partition(&manifest, &config, &folder, vec![pdf.clone()]);
        assert!(needed.is_empty());
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].0, pdf);

        // A changed file needs processing even with a cached record.
        std::fs::write(&pdf, b"pdf changed").expect("rewrite pdf");
        let (needed, cached) = partition(&manifest, &config, &folder, vec![pdf.clone()]);
        assert_eq!(needed, vec![pdf]);
        assert!(cached.is_empty());
    }

    #[tokio::test]
    async fn explicit_sources_limit_the_run() {
        let dir = tempfile::tempdir().expect("temp dir");
        let folder = dir.path().join("papers");
        std::fs::create_dir_all(&folder).expect("create corpus folder");
        let picked = folder.join("picked.pdf");
        let other = folder.join("other.pdf");
        std::fs::write(&picked, b"%PDF-1.4 fake").expect("write picked");
        std::fs::write(&other, b"%PDF-1.4 other").expect("write other");
        let record = biblio(Some("Kahle"), Some("2000"), Some("The Barc model"));
        let mut manifest = Manifest::empty(folder.join(MANIFEST_NAME));
        manifest.record_with_biblio(&folder, &picked, &record);
        manifest.save().expect("save manifest");

        let bib = dir.path().join("refs.bib");
        let mut config = offline_config(
            &folder,
            Some(BibtexTarget {
                path: bib.clone(),
                link: true,
            }),
        );
        config.sources = Some(vec![picked]);

        prepass(&config).await.expect("prepass");
        let text = std::fs::read_to_string(&bib).expect("read bib");
        assert_eq!(text.matches("@misc{").count(), 1, "{text}");
        assert_eq!(text.matches("Kahle2000").count(), 1, "{text}");
    }

    #[tokio::test]
    async fn corrupt_bibtex_fails_before_grobid() {
        let dir = tempfile::tempdir().expect("temp dir");
        let folder = dir.path().join("papers");
        std::fs::create_dir_all(&folder).expect("create corpus folder");
        // A PDF that needs processing, so the server would be contacted if
        // the pre-pass got that far.
        std::fs::write(folder.join("new.pdf"), b"%PDF-1.4 fake").expect("write pdf");
        let bib = dir.path().join("refs.bib");
        std::fs::write(&bib, "@misc{broken,\n  title = {Unclosed").expect("write bib");
        let config = offline_config(
            &folder,
            Some(BibtexTarget {
                path: bib.clone(),
                link: true,
            }),
        );

        let error = prepass(&config)
            .await
            .expect_err("corrupt bibliography must fail");
        let message = format!("{error}");
        assert!(message.contains("cannot read the BibTeX file"), "{message}");
        assert!(message.contains("cannot parse"), "{message}");
        // The corrupt file is left untouched.
        assert_eq!(
            std::fs::read_to_string(&bib).expect("read bib"),
            "@misc{broken,\n  title = {Unclosed"
        );
    }

    struct Recording {
        stages: Vec<PrepassStage>,
        cancel: bool,
    }

    impl PrepassObserver for Recording {
        fn progress(&mut self, progress: PrepassProgress) {
            self.stages.push(progress.stage);
        }

        fn cancelled(&self) -> bool {
            self.cancel
        }
    }

    #[tokio::test]
    async fn observer_sees_every_stage_and_can_cancel() {
        let dir = tempfile::tempdir().expect("temp dir");
        let folder = dir.path().join("papers");
        std::fs::create_dir_all(&folder).expect("create corpus folder");
        std::fs::write(folder.join("scan.pdf"), b"%PDF-1.4 fake").expect("write pdf");
        let addr = spawn_mock_grobid(MOCK_TEI).await;
        let bib = dir.path().join("refs.bib");
        let mut config = PrepassConfig::new(&folder);
        config.grobid_url = format!("http://{addr}");
        config.workers = 2;
        config.rename = true;
        config.openalex = false;
        config.bibtex = Some(BibtexTarget {
            path: bib.clone(),
            link: true,
        });

        let mut observer = Recording {
            stages: Vec::new(),
            cancel: false,
        };
        prepass_with(&config, &mut observer).await.expect("prepass");
        for stage in [
            PrepassStage::Parsing,
            PrepassStage::Renaming,
            PrepassStage::Merging,
        ] {
            assert!(observer.stages.contains(&stage), "{:?}", observer.stages);
        }

        let mut cancelled = Recording {
            stages: Vec::new(),
            cancel: true,
        };
        let error = prepass_with(&config, &mut cancelled)
            .await
            .expect_err("cancelled");
        assert!(matches!(error, PrepassError::Cancelled), "{error:?}");
    }

    // ── Mock GROBID server ────────────────────────────────────────────────

    /// The title "Tiny." stays below the OpenAlex search threshold, so
    /// nothing in this test reaches the OpenAlex API.
    const MOCK_TEI: &str = r#"<TEI xmlns="http://www.tei-c.org/ns/1.0">
    <teiHeader>
        <encodingDesc><appInfo>
            <application version="0.9.1" when="2026-01-01T00:00+0000"/>
        </appInfo></encodingDesc>
        <fileDesc>
            <titleStmt><title level="a" type="main">Tiny</title></titleStmt>
            <publicationStmt><publisher/></publicationStmt>
            <sourceDesc>
                <biblStruct>
                    <analytic>
                        <title level="a" type="main">Tiny.</title>
                        <author><persName>
                            <forename type="first">Jane</forename>
                            <surname>Smith</surname>
                        </persName></author>
                    </analytic>
                    <monogr><imprint><date type="published" when="2020"/></imprint></monogr>
                </biblStruct>
            </sourceDesc>
        </fileDesc>
    </teiHeader>
    <text/>
</TEI>"#;

    /// A header with a title long enough to trigger an OpenAlex title search.
    const MOCK_TEI_LONG_TITLE: &str = r#"<TEI xmlns="http://www.tei-c.org/ns/1.0">
    <teiHeader>
        <encodingDesc><appInfo>
            <application version="0.9.1" when="2026-01-01T00:00+0000"/>
        </appInfo></encodingDesc>
        <fileDesc>
            <titleStmt><title level="a" type="main">The Barc model</title></titleStmt>
            <publicationStmt><publisher/></publicationStmt>
            <sourceDesc>
                <biblStruct>
                    <analytic>
                        <title level="a" type="main">The Barc model for continuous variables</title>
                        <author><persName><surname>Kahle</surname></persName></author>
                    </analytic>
                    <monogr><imprint><date type="published" when="2000"/></imprint></monogr>
                </biblStruct>
            </sourceDesc>
        </fileDesc>
    </teiHeader>
    <text/>
</TEI>"#;

    /// A minimal mock of the two GROBID endpoints the pre-pass uses.
    async fn spawn_mock_grobid(tei: &'static str) -> std::net::SocketAddr {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let Some(head) = read_request_head(&mut stream).await else {
                        return;
                    };
                    let request_line = head.lines().next().unwrap_or("");
                    let (status, body) = if request_line.contains("/api/isalive") {
                        ("200 OK", "true".to_string())
                    } else if request_line.contains("/api/processHeaderDocument") {
                        ("200 OK", tei.to_string())
                    } else {
                        ("404 Not Found", String::new())
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        addr
    }

    /// Read a full request (headers and body, by `Content-Length`) and
    /// return the header block. Keeping the whole body in the read path
    /// avoids closing the connection while the client is still sending.
    async fn read_request_head(stream: &mut tokio::net::TcpStream) -> Option<String> {
        use tokio::io::AsyncReadExt;

        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            let n = stream.read(&mut tmp).await.ok()?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            let Some(header_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let header_end = header_end + 4;
            let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
            let content_length = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    if name.eq_ignore_ascii_case("content-length") {
                        value.trim().parse::<usize>().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            if buf.len() >= header_end + content_length {
                return Some(head);
            }
        }
        Some(String::from_utf8_lossy(&buf).into_owned())
    }

    #[tokio::test]
    async fn completion_failures_keep_the_parsed_record() {
        let dir = tempfile::tempdir().expect("temp dir");
        let folder = dir.path().join("papers");
        std::fs::create_dir_all(&folder).expect("create corpus folder");
        std::fs::write(folder.join("scan.pdf"), b"%PDF-1.4 fake").expect("write pdf");
        let addr = spawn_mock_grobid(MOCK_TEI_LONG_TITLE).await;

        // The mock answers 404 for OpenAlex requests: the lookup fails, the
        // GROBID header is kept and the run still succeeds.
        let mut config = PrepassConfig::new(&folder);
        config.grobid_url = format!("http://{addr}");
        config.openalex = true;
        config.openalex_url = Some(format!("http://{addr}"));
        config.rename = false;
        let report = prepass(&config).await.expect("prepass");

        assert_eq!(report.completed, 0, "the OpenAlex lookup failed");
        assert_eq!(report.parsed, 1);
        assert!(report.failed.is_empty(), "{:?}", report.failed);
    }

    #[tokio::test]
    async fn prepass_extracts_renames_and_merges_against_mock_server() {
        let dir = tempfile::tempdir().expect("temp dir");
        let folder = dir.path().join("papers");
        std::fs::create_dir_all(&folder).expect("create corpus folder");
        let pdf = folder.join("scan_0012.pdf");
        std::fs::write(&pdf, b"%PDF-1.4 fake").expect("write pdf");

        let addr = spawn_mock_grobid(MOCK_TEI).await;
        let bib = dir.path().join("refs.bib");
        let mut config = PrepassConfig::new(&folder);
        config.grobid_url = format!("http://{addr}");
        config.workers = 2;
        config.rename = true;
        config.openalex = false;
        config.bibtex = Some(BibtexTarget {
            path: bib.clone(),
            link: true,
        });
        prepass(&config).await.expect("prepass");

        // The PDF was renamed from the extracted, completed header.
        let pdfs = files::collect_pdfs(&folder).expect("collect");
        assert_eq!(pdfs.len(), 1, "{pdfs:?}");
        let name = pdfs[0].file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("Smith2020 - "), "{name}");
        assert!(name.contains("Tiny"), "{name}");
        assert!(name.ends_with(" - 2020.pdf"), "{name}");

        // The entry points at the final name.
        let text = std::fs::read_to_string(&bib).expect("read bib");
        assert!(text.starts_with("@misc{Smith2020,"), "{text}");
        assert!(
            text.contains(&format!("file = {{{}}}", pdfs[0].display())),
            "{text}"
        );

        // The manifest caches the record under the final name.
        let manifest = Manifest::load(&folder.join(MANIFEST_NAME)).expect("manifest");
        assert!(manifest.biblio(&folder, &pdfs[0]).is_some());
        assert!(manifest.is_unchanged(&folder, &pdfs[0]));

        // A second run is served from the cache and changes nothing.
        prepass(&config).await.expect("second prepass");
        assert_eq!(std::fs::read_to_string(&bib).expect("read bib"), text);
    }
}
