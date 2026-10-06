//! `pdf2bibtex`: process all PDFs in a directory with GROBID and write the
//! extracted bibliographic metadata as a BibTeX file.
//!
//! # Usage
//!
//! ```sh
//! cargo run --release --bin pdf2bibtex -- ~/papers -s http://localhost:8070
//! ```
//!
//! Run with `--help` for all options. PDFs are discovered recursively; the
//! documents are processed through GROBID's `/api/processHeaderDocument`
//! service with a bounded number of concurrent requests, and one BibTeX
//! entry is written per document. With `-r`/`--rename`, each PDF is renamed
//! to `Auth_Year_<first 10 title words>.pdf` once its metadata has been
//! extracted; with `-l`/`--link`, every entry records the path of its PDF
//! in a `file` field.
//!
//! With `-a`/`--append` or `-m`/`--merge`, the entries go into an existing
//! BibTeX file instead of a new one. Both load the target collection first,
//! so that generated citation keys stay unique. `--append` adds every
//! entry; `--merge` skips entries that are already in the collection, where an
//! entry counts as present when its normalized field content, one of its
//! identifiers (DOI, PMID, arXiv) or its PDF file name matches an existing
//! record. Documents that fail to process are reported on stderr and
//! skipped. When built with the `openalex` feature, `--openalex` adds a
//! second completion tier against OpenAlex: headers are completed before
//! entries are formatted and PDFs are renamed.
//!
//! This is a front end over the `grobid-bibtex` crate; the same pipeline
//! is available to other tools through `extract`, `complete`, `files` and
//! `collection`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use grobid_bibtex::collection::{self, Collection};
use grobid_bibtex::{
    DEFAULT_GROBID_URL, GrobidClient, ProcessOptions, RetryPolicy, extract, files,
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
Usage: pdf2bibtex <DIR> [OPTIONS]

Reads all PDFs in <DIR> (recursively), extracts their bibliographic
metadata with GROBID and writes a BibTeX file.

Arguments:
  <DIR>                 directory containing the PDFs

Options:
  -o, --output <FILE>   output .bib file [default: <DIR>.bib, next to <DIR>]
  -a, --append <FILE>   append all entries to an existing .bib file; the
                        file is parsed first to keep the new keys unique
  -m, --merge <FILE>    like --append, but skip entries whose content,
                        identifier (DOI, PMID, arXiv) or PDF file name
                        already occurs in the file
  -s, --server <URL>    GROBID server URL [default: http://localhost:8070]
  -w, --workers <N>     number of concurrent requests [default: 4]
  -r, --rename          rename each PDF to Auth_Year_<first 10 title words>
  -l, --link            add the PDF path as a file field to each entry
      --openalex        complete headers against OpenAlex; requires
                        building with --features openalex
  -h, --help            print this help

Only one of --output, --append and --merge may be given.";

struct Args {
    input_dir: PathBuf,
    destination: Destination,
    server_url: String,
    workers: usize,
    rename: bool,
    link: bool,
    openalex: bool,
}

/// Where the formatted entries are written: a new (or overwritten) output
/// file, or an existing BibTeX file that they are added to.
enum Destination {
    /// `-o`/`--output`: write all entries to this file.
    Output(PathBuf),
    /// `-a`/`--append`: append all entries to this existing BibTeX file. Its
    /// keys are parsed before processing, so that no generated key collides
    /// with an entry already in the file.
    Append(PathBuf),
    /// `-m`/`--merge`: append only the entries that are not already in this
    /// existing BibTeX file, judged by normalized field content, identifier
    /// (DOI, PMID, arXiv) or PDF file name.
    Merge(PathBuf),
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
    // With `--append` and `--merge`, load the target collection first: no PDF
    // must be processed (or renamed) when its keys cannot be determined,
    // and its keys and duplicate index drive the output below.
    let collection = match &args.destination {
        Destination::Append(path) => {
            let collection = Collection::load(path)?;
            println!(
                "appending to {} ({})",
                path.display(),
                entry_count(collection.len())
            );
            collection
        }
        Destination::Merge(path) => {
            let collection = Collection::load(path)?;
            println!(
                "merging into {} ({})",
                path.display(),
                entry_count(collection.len())
            );
            collection
        }
        Destination::Output(_) => Collection::new(),
    };

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
    println!("waiting for GROBID at {} ...", probe.base_url());
    probe
        .wait_until_ready(PROBE_ATTEMPTS, PROBE_RETRY_DELAY)
        .await?;

    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(PROCESS_TIMEOUT)
        .build()?;
    let client = GrobidClient::builder(&args.server_url)?
        .http_client(http)
        .build();

    let mut notes = Vec::new();
    if args.rename {
        notes.push("renaming PDFs");
    }
    if args.openalex {
        notes.push("OpenAlex completion");
    }
    let notes = if notes.is_empty() {
        String::new()
    } else {
        format!(" ({})", notes.join(", "))
    };
    println!(
        "processing {} PDF(s) from {} with {} worker(s){notes}",
        pdfs.len(),
        args.input_dir.display(),
        args.workers
    );

    let outcomes = extract::headers(&client, pdfs, ProcessOptions::default(), args.workers).await;
    let mut results = Vec::new();
    let mut failures = 0usize;
    for outcome in outcomes {
        match outcome {
            Ok(extract::Extracted { path, record }) => {
                println!("ok:   {}", path.display());
                results.push((path, record));
            }
            Err(err) => {
                failures += 1;
                eprintln!("skip: {err}");
            }
        }
    }

    if results.is_empty() {
        return Err(format!(
            "all {failures} document(s) failed to process; check the server and the messages above"
        )
        .into());
    }

    // Second tier: fill the gaps GROBID left behind from OpenAlex, before
    // renaming so that file names are built from the completed metadata.
    #[cfg(feature = "openalex")]
    if args.openalex {
        let completer = grobid_bibtex::openalex::Completer::new();
        let report = grobid_bibtex::complete::biblios(&completer, results, args.workers).await;
        for failure in &report.failures {
            eprintln!(
                "openalex: {}: {}",
                report.records[failure.position - 1].0.display(),
                failure.source
            );
        }
        println!(
            "openalex: matched {} of {} document(s) ({} lookup(s) failed)",
            report.matched,
            report.records.len(),
            report.failures.len()
        );
        results = report.records;
    }

    // Rename before formatting, so that `--link` entries point at the
    // files' final locations. A failed rename keeps the original path, so
    // the entry still refers to an existing file.
    if args.rename {
        let mut renamed = 0usize;
        for event in files::rename_pdfs(&mut results) {
            match event {
                files::Rename::Renamed { from, to } => {
                    renamed += 1;
                    println!("renamed: {} -> {}", from.display(), to.display());
                }
                files::Rename::Unnamed { path } => eprintln!(
                    "warn: cannot rename {}: no author, year or title extracted",
                    path.display()
                ),
                files::Rename::Failed { from, to, source } => eprintln!(
                    "warn: cannot rename {} to {}: {source}",
                    from.display(),
                    to.display()
                ),
                files::Rename::Unchanged { .. } => {}
            }
        }
        println!("renamed {renamed} of {} PDF(s)", results.len());
    }

    // Assign deterministic, collision-free keys and format the entries.
    // With `--merge`, records already present in the file are left out: the
    // library function reloads the file, merges and appends in one step.
    if let Destination::Merge(path) = &args.destination {
        let report = collection::merge_file(path, &results, args.link)?;
        println!(
            "merged {} into {} ({} duplicate(s) skipped, {} failed)",
            entry_count(report.entries.len()),
            path.display(),
            report.duplicates,
            failures
        );
        return Ok(());
    }
    let entries = collection.format_all(&results, args.link);
    let bibtex = entries
        .iter()
        .map(|(_, entry)| entry.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let count = entry_count(entries.len());
    match &args.destination {
        Destination::Output(path) => {
            std::fs::write(path, format!("{bibtex}\n"))?;
            println!("wrote {count} to {} ({} failed)", path.display(), failures);
        }
        Destination::Append(path) => {
            collection::append(path, &bibtex)?;
            println!(
                "appended {count} to {} ({} failed)",
                path.display(),
                failures
            );
        }
        Destination::Merge(_) => unreachable!("handled by the early merge return"),
    }
    Ok(())
}

/// `1 entry`, `2 entries`, ...
fn entry_count(count: usize) -> String {
    format!("{count} entr{}", if count == 1 { "y" } else { "ies" })
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut input_dir: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut append: Option<PathBuf> = None;
    let mut merge: Option<PathBuf> = None;
    let mut server_url = DEFAULT_GROBID_URL.to_string();
    let mut workers = 4usize;
    let mut rename = false;
    let mut link = false;
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
            "-r" | "--rename" => rename = true,
            "-l" | "--link" => link = true,
            "--openalex" => {
                #[cfg(not(feature = "openalex"))]
                {
                    return Err("--openalex requires the `openalex` feature; rebuild with \
                                `cargo run --features openalex --bin pdf2bibtex`"
                        .to_string());
                }
                #[cfg(feature = "openalex")]
                {
                    openalex = true;
                }
            }
            "-o" | "--output" | "-a" | "--append" | "-m" | "--merge" | "-s" | "--server" | "-w"
            | "--workers" => {
                i += 1;
                let Some(value) = raw.get(i) else {
                    return Err(format!("missing value for {arg}"));
                };
                match arg.as_str() {
                    "-o" | "--output" => output = Some(PathBuf::from(value)),
                    "-a" | "--append" => append = Some(PathBuf::from(value)),
                    "-m" | "--merge" => merge = Some(PathBuf::from(value)),
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
    let destination = match (output, append, merge) {
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) | (_, Some(_), Some(_)) => {
            return Err("--output, --append and --merge are mutually exclusive".to_string());
        }
        (_, Some(path), None) => Destination::Append(path),
        (_, None, Some(path)) => Destination::Merge(path),
        (output, None, None) => Destination::Output(output.unwrap_or_else(|| {
            let name = input_dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "output".to_string());
            input_dir
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(format!("{name}.bib"))
        })),
    };
    Ok(Some(Args {
        input_dir,
        destination,
        server_url,
        workers,
        rename,
        link,
        openalex,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_wait_until_ready_unreachable() {
        // Bind and release a port so that nothing is listening on it:
        // connection attempts are refused.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local addr");
        drop(listener);
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(1))
            .build()
            .expect("http client");
        let client = GrobidClient::builder(format!("http://{addr}"))
            .expect("client")
            .http_client(http)
            .retry(RetryPolicy {
                max_attempts: 1,
                ..RetryPolicy::default()
            })
            .build();
        let error = client
            .wait_until_ready(2, Duration::from_millis(1))
            .await
            .expect_err("unreachable server must yield an error");
        let message = error.to_string();
        assert!(
            message.contains("did not respond in 2 attempt(s)"),
            "{message}"
        );
    }
}
