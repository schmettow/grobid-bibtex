//! `refs2bibtex`: extract all bibliographic references from the PDFs in a
//! directory with GROBID and write them as a BibTeX file.
//!
//! # Usage
//!
//! ```sh
//! cargo run --release --bin refs2bibtex -- ~/papers -s http://localhost:8070
//! ```
//!
//! Unlike the `pdf2bibtex` binary, which writes one entry per document
//! (its header), this binary writes one entry per reference found in the
//! documents (via `/api/processReferences`). References with an empty parse
//! result are skipped, references with the same DOI are deduplicated across
//! documents, and citation keys are made unique. With `-c`/`--consolidate`
//! GROBID consolidates the references against CrossRef while processing;
//! when the binary is built with the `openalex` feature, `--openalex` adds a
//! second completion tier against OpenAlex after parsing. Run with `--help`
//! for all options.
//!
//! This is a front end over the `grobid-bibtex` crate; the same pipeline
//! is available to other tools through `extract`, `complete` and `bibtex`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use grobid_bibtex::{
    Citation, CitationConsolidation, DEFAULT_GROBID_URL, Error, GrobidClient, ProcessOptions,
    RetryPolicy, bibtex, extract, files,
};

/// Timeout for the server liveness probe. An unresponsive server must be
/// detected quickly, not after the long document processing timeout.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Timeout for a single document processing request.
const PROCESS_TIMEOUT: Duration = Duration::from_secs(600);
/// How often the liveness probe is attempted before giving up. GROBID can
/// take a while to become ready, e.g. while preloading its models.
const PROBE_ATTEMPTS: usize = 5;
/// Wait between liveness probe attempts.
const PROBE_RETRY_DELAY: Duration = Duration::from_secs(3);

const USAGE: &str = "\
Usage: refs2bibtex <DIR> [OPTIONS]

Reads all PDFs in <DIR> (recursively), extracts their bibliographic
references with GROBID and writes a BibTeX file with one entry per
reference.

Arguments:
  <DIR>                 directory containing the PDFs

Options:
  -o, --output <FILE>   output .bib file [default: <DIR>.refs.bib]
  -s, --server <URL>    GROBID server URL [default: http://localhost:8070]
  -w, --workers <N>     number of concurrent requests [default: 4]
  -c, --consolidate     consolidate references against CrossRef
      --openalex        complete references against OpenAlex; requires
                        building with --features openalex
  -h, --help            print this help";

struct Args {
    input_dir: PathBuf,
    output: PathBuf,
    server_url: String,
    workers: usize,
    consolidate: bool,
    openalex: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Some(args)) => args,
        Ok(None) => return ExitCode::SUCCESS, // --help
        Err(message) => {
            eprintln!("error: {message}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let pdfs = files::collect_pdfs(&args.input_dir)?;
    if pdfs.is_empty() {
        return Err(format!("no PDFs found in {}", args.input_dir.display()).into());
    }

    // Probe the server before doing any work. The probe uses a short
    // timeout and bounded retries, so a server that does not respond is
    // reported quickly and clearly instead of hanging the batch.
    let probe_client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(PROBE_TIMEOUT)
        .build()?;
    let probe = GrobidClient::builder(&args.server_url)?
        .http_client(probe_client)
        .retry(RetryPolicy {
            max_attempts: 1,
            ..RetryPolicy::default()
        })
        .build();
    wait_for_server(&probe, PROBE_ATTEMPTS, PROBE_RETRY_DELAY).await?;

    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(PROCESS_TIMEOUT)
        .build()?;
    let client = GrobidClient::builder(&args.server_url)?
        .http_client(http)
        .build();

    let mut tiers = Vec::new();
    if args.consolidate {
        tiers.push("consolidated");
    }
    if args.openalex {
        tiers.push("OpenAlex completion");
    }
    let tiers = if tiers.is_empty() {
        String::new()
    } else {
        format!(" ({})", tiers.join(", "))
    };
    println!(
        "extracting references from {} PDF(s) in {} with {} worker(s){tiers}",
        pdfs.len(),
        args.input_dir.display(),
        args.workers
    );

    let options = ProcessOptions {
        consolidate_citations: if args.consolidate {
            CitationConsolidation::Metadata
        } else {
            CitationConsolidation::None
        },
        ..ProcessOptions::default()
    };
    let pdf_count = pdfs.len();
    let outcomes = extract::references(&client, pdfs, options, args.workers).await;
    let mut results: Vec<(PathBuf, Vec<Citation>)> = Vec::new();
    let mut failures = 0usize;
    for outcome in outcomes {
        match outcome {
            Ok(extract::Extracted { path, record }) => {
                println!("ok:   {} ({} reference(s))", path.display(), record.len());
                results.push((path, record));
            }
            Err(err) => {
                failures += 1;
                eprintln!("skip: {err}");
            }
        }
    }

    // Flatten the per-document lists, drop empty and duplicate references
    // and order them by suggested key.
    let collected = extract::collect_references(results);
    println!(
        "{} reference(s) collected ({} skipped as empty, {} duplicate(s) dropped)",
        collected.citations.len(),
        collected.empty,
        collected.duplicates
    );
    if collected.citations.is_empty() {
        return Err(
            format!("no references extracted from {pdf_count} PDF(s) ({failures} failed)").into(),
        );
    }

    // Second tier: fill the gaps GROBID left behind from OpenAlex.
    let citations = collected.citations;
    #[cfg(feature = "openalex")]
    let citations = if args.openalex {
        let completer = grobid_bibtex::openalex::Completer::new();
        let report = grobid_bibtex::complete::citations(&completer, citations, args.workers).await;
        for failure in &report.failures {
            eprintln!(
                "openalex: reference {}: {}",
                failure.position, failure.source
            );
        }
        println!(
            "openalex: matched {} of {} reference(s) ({} lookup(s) failed)",
            report.matched,
            report.records.len(),
            report.failures.len()
        );
        let mut citations = report.records;
        // Completion can fill in author and year, changing the suggested
        // keys; keep the output ordered by key. The sort is stable, so
        // references with equal keys keep their deterministic pre-completion
        // order.
        citations.sort_by_cached_key(|citation| bibtex::suggest_key(citation));
        citations
    } else {
        citations
    };

    let entries = bibtex::format_all(citations.iter().map(|citation| &citation.biblio));
    let bibtex_text = entries.join("\n\n");
    std::fs::write(&args.output, format!("{bibtex_text}\n"))?;
    println!(
        "wrote {} entr{} to {} ({} document(s) failed)",
        entries.len(),
        if entries.len() == 1 { "y" } else { "ies" },
        args.output.display(),
        failures
    );
    Ok(())
}

/// Wait until the GROBID server responds and reports itself alive.
///
/// GROBID can take a while to become ready (e.g. while preloading its
/// models), so the probe is retried for a bounded time. A server that
/// refuses connections or does not answer within the probe timeout is
/// reported with a clear error instead of hanging the batch; a server
/// that responds with an unexpected HTTP status on `/api/isalive` (e.g.
/// a wrong base URL) fails immediately, since retrying cannot help.
async fn wait_for_server(
    client: &GrobidClient,
    attempts: usize,
    retry_delay: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    for attempt in 1..=attempts {
        let remaining = attempts - attempt;
        match client.ping().await {
            Ok(true) => return Ok(()),
            Ok(false) if remaining > 0 => {
                eprintln!(
                    "server at {} not alive yet; retrying in {}s ({}/{})",
                    client.base_url(),
                    retry_delay.as_secs(),
                    attempt,
                    attempts
                );
            }
            Ok(false) => {
                return Err(format!(
                    "GROBID server at {} is up, but reports it is not alive",
                    client.base_url()
                )
                .into());
            }
            Err(Error::HttpStatus { status, .. }) => {
                return Err(format!(
                    "GROBID server at {} answered HTTP {status} on /api/isalive; \
                     is the server URL correct?",
                    client.base_url()
                )
                .into());
            }
            Err(err) if remaining > 0 => {
                eprintln!(
                    "server at {} not responding ({err}); retrying in {}s ({}/{})",
                    client.base_url(),
                    retry_delay.as_secs(),
                    attempt,
                    attempts
                );
            }
            Err(err) => {
                return Err(format!(
                    "cannot reach GROBID server at {} after {attempts} attempts: {err}",
                    client.base_url()
                )
                .into());
            }
        }
        tokio::time::sleep(retry_delay).await;
    }
    unreachable!("the loop returns on its final attempt")
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut input_dir: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut server_url = DEFAULT_GROBID_URL.to_string();
    let mut workers = 4usize;
    let mut consolidate = false;
    #[cfg(feature = "openalex")]
    let mut openalex = false;
    #[cfg(not(feature = "openalex"))]
    let openalex = false;
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < raw.len() {
        let arg = raw[i].clone();
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(None);
            }
            "-c" | "--consolidate" => consolidate = true,
            "--openalex" => {
                #[cfg(not(feature = "openalex"))]
                {
                    return Err("--openalex requires the `openalex` feature; rebuild with \
                                `cargo run --features openalex --bin refs2bibtex`"
                        .to_string());
                }
                #[cfg(feature = "openalex")]
                {
                    openalex = true;
                }
            }
            "-o" | "--output" | "-s" | "--server" | "-w" | "--workers" => {
                i += 1;
                let Some(value) = raw.get(i) else {
                    return Err(format!("missing value for {arg}"));
                };
                match arg.as_str() {
                    "-o" | "--output" => output = Some(PathBuf::from(value)),
                    "-s" | "--server" => server_url = value.clone(),
                    "-w" | "--workers" => {
                        workers = value
                            .parse()
                            .map_err(|_| format!("invalid worker count: {value}"))?
                    }
                    _ => unreachable!(),
                }
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown option: {other}"));
            }
            other => {
                if input_dir.is_some() {
                    return Err("only one input directory is allowed".to_string());
                }
                input_dir = Some(PathBuf::from(other));
            }
        }
        i += 1;
    }
    let input_dir = input_dir.ok_or("missing input directory")?;
    if !input_dir.is_dir() {
        return Err(format!("{} is not a directory", input_dir.display()));
    }
    if workers == 0 {
        return Err("worker count must be at least 1".to_string());
    }
    let output = output.unwrap_or_else(|| {
        let name = input_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "output".to_string());
        input_dir
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(format!("{name}.refs.bib"))
    });
    Ok(Some(Args {
        input_dir,
        output,
        server_url,
        workers,
        consolidate,
        openalex,
    }))
}
