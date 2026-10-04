//! The BibTeX-centered reference collection.
//!
//! A [`Collection`] owns a parsed BibTeX/BibLaTeX bibliography plus the derived
//! state that reference management needs: the citation keys in use and an
//! index of record identities for duplicate detection. Extraction and
//! completion (see [`crate::extract`] and the `complete` module) produce
//! [`Biblio`] records; this module renders them as entries and merges them
//! into existing collections, while [`crate::bibtex`] handles single entries.
//!
//! Duplicate detection uses three strategies:
//!
//! 1. **content**: the normalized field content of an entry (all fields
//!    except the citation key, lowercased with whitespace collapsed),
//! 2. **identifiers**: DOI, PMID, PMCID and arXiv identifiers, normalized
//!    for spelling differences (`https://doi.org/...`, `doi:`, version
//!    suffixes, ...), and
//! 3. **file name**: the lowercased name of the PDF recorded in the `file`
//!    field, or the path a record was extracted from.
//!
//! A record matching on any of these strategies is a duplicate.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use biblatex::{Bibliography, ChunksExt, Entry};

use crate::Biblio;
use crate::bibtex;

/// Errors from loading a bibliography file.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LoadError {
    /// The file could not be read.
    #[error("cannot read {path}: {source}")]
    Read {
        /// The file that could not be read.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The file is not valid BibTeX/BibLaTeX.
    #[error("cannot parse {path}: {source}")]
    Parse {
        /// The file that could not be parsed.
        path: PathBuf,
        /// The underlying parse error.
        #[source]
        source: biblatex::ParseError,
    },
}

/// A rendered entry that could not be interpreted for duplicate detection.
///
/// The renderer only emits valid entries, so this indicates a bug or a
/// `biblatex` incompatibility rather than bad input.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RenderError {
    /// The rendered entry did not parse.
    #[error("cannot parse rendered entry: {source}")]
    Parse {
        /// The underlying parse error.
        #[source]
        source: biblatex::ParseError,
    },
    /// The rendered text contained no entry.
    #[error("rendered entry is empty: {entry}")]
    Empty {
        /// The text that was rendered.
        entry: String,
    },
}

/// An in-memory BibTeX/BibLaTeX collection.
///
/// The parsed [`biblatex::Bibliography`] is the source of truth; citation
/// keys and duplicate identities are derived from it on load and kept up to
/// date by [`Collection::insert`] and [`Collection::merge_all`].
pub struct Collection {
    bibliography: Bibliography,
    keys: HashSet<String>,
    duplicates: MergeIndex,
}

impl Collection {
    /// An empty collection.
    pub fn new() -> Self {
        Self::from_bibliography(Bibliography::new())
    }

    /// Parse a collection from BibTeX/BibLaTeX source.
    ///
    /// # Errors
    ///
    /// Returns the `biblatex` parse error if `source` is not a valid
    /// bibliography.
    pub fn parse(source: &str) -> Result<Self, biblatex::ParseError> {
        Bibliography::parse(source).map(Self::from_bibliography)
    }

    /// Read and parse a collection from a `.bib` file.
    ///
    /// # Errors
    ///
    /// Returns [`LoadError`] if the file cannot be read or is not a valid
    /// bibliography.
    pub fn load(path: &Path) -> Result<Self, LoadError> {
        let source = std::fs::read_to_string(path).map_err(|source| LoadError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let bibliography = Bibliography::parse(&source).map_err(|source| LoadError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(Self::from_bibliography(bibliography))
    }

    fn from_bibliography(bibliography: Bibliography) -> Self {
        let keys = bibliography.keys().map(str::to_string).collect();
        let duplicates = bibliography.iter().map(entry_identity).collect();
        Self {
            bibliography,
            keys,
            duplicates,
        }
    }

    /// The parsed bibliography.
    pub fn bibliography(&self) -> &Bibliography {
        &self.bibliography
    }

    /// The citation keys of all entries.
    pub fn keys(&self) -> &HashSet<String> {
        &self.keys
    }

    /// The number of entries.
    pub fn len(&self) -> usize {
        self.bibliography.len()
    }

    /// Whether the collection has no entries.
    pub fn is_empty(&self) -> bool {
        self.bibliography.is_empty()
    }

    /// Iterate over the entries.
    pub fn iter(&self) -> impl Iterator<Item = &Entry> {
        self.bibliography.iter()
    }

    /// Whether `identity` matches an entry already in the collection.
    pub fn is_duplicate(&self, identity: &EntryIdentity) -> bool {
        self.duplicates.contains(identity)
    }

    /// Add an already parsed entry, registering its key and identity.
    pub fn insert(&mut self, entry: Entry) {
        self.keys.insert(entry.key.clone());
        self.duplicates.insert(entry_identity(&entry));
        let _ = self.bibliography.insert(entry);
    }

    /// Render `records` as BibTeX entries, sorted by suggested key, with
    /// citation keys that are unique in this collection. With `link`, each
    /// entry records its PDF's path in a `file` field.
    pub fn format_all(&self, records: &[(PathBuf, Biblio)], link: bool) -> Vec<(PathBuf, String)> {
        let mut used = self.keys.clone();
        sorted_records(records)
            .into_iter()
            .map(|(biblio, path, _)| {
                let key = bibtex::unique_key(biblio, &mut used);
                (path.clone(), render_entry(&key, biblio, path, link))
            })
            .collect()
    }

    /// Render the records that are not already in this collection and add them
    /// to it.
    ///
    /// Returns the new entries together with the number of records skipped
    /// as duplicates, in the same order as [`Collection::format_all`]. Keys are
    /// unique in the collection after the merge.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError`] if a rendered entry cannot be parsed back
    /// for duplicate detection.
    pub fn merge_all(
        &mut self,
        records: &[(PathBuf, Biblio)],
        link: bool,
    ) -> Result<(Vec<(PathBuf, String)>, usize), RenderError> {
        let mut used = self.keys.clone();
        let mut entries = Vec::new();
        let mut duplicates = 0usize;
        for (biblio, path, suggested) in sorted_records(records) {
            // Identify the record as it would be written; the key is not
            // part of the identity, so the suggested key can be used
            // provisionally.
            let rendered = render_entry(&suggested, biblio, path, link);
            let (mut entry, identity) = rendered_identity(&rendered, biblio, path)?;
            if self.is_duplicate(&identity) {
                duplicates += 1;
                continue;
            }
            let key = bibtex::unique_key(biblio, &mut used);
            entries.push((path.clone(), render_entry(&key, biblio, path, link)));
            entry.key = key.clone();
            self.duplicates.insert(identity);
            self.keys.insert(key);
            let _ = self.bibliography.insert(entry);
        }
        Ok((entries, duplicates))
    }
}

impl Default for Collection {
    fn default() -> Self {
        Self::new()
    }
}

/// Append rendered BibTeX to the file at `path`, separating it from the
/// existing content with one blank line.
///
/// # Errors
///
/// Returns the underlying I/O error if the file cannot be read or opened
/// for appending.
pub fn append(path: &Path, bibtex: &str) -> std::io::Result<()> {
    use std::io::Write;

    let existing = std::fs::read_to_string(path)?;
    let mut addition = String::new();
    if !existing.is_empty() {
        if !existing.ends_with('\n') {
            addition.push('\n');
        }
        if !existing.ends_with("\n\n") {
            addition.push('\n');
        }
    }
    addition.push_str(bibtex);
    addition.push('\n');
    std::fs::OpenOptions::new()
        .append(true)
        .open(path)?
        .write_all(addition.as_bytes())
}

/// The identity of a record for duplicate detection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EntryIdentity {
    /// Normalized content of all fields except the citation key.
    pub fingerprint: String,
    /// Normalized identifiers, e.g. `doi:10.1234/x` or `pmid:123`.
    pub identifiers: Vec<String>,
    /// Lowercased PDF file name, from the `file` field or the input path.
    pub file_name: Option<String>,
}

/// The identities of all entries a merge batch compares against: the
/// records of the collection plus the ones accepted so far.
#[derive(Default)]
struct MergeIndex {
    fingerprints: HashSet<String>,
    identifiers: HashSet<String>,
    file_names: HashSet<String>,
}

impl MergeIndex {
    /// Whether `identity` matches any known entry.
    fn contains(&self, identity: &EntryIdentity) -> bool {
        self.fingerprints.contains(&identity.fingerprint)
            || identity
                .identifiers
                .iter()
                .any(|identifier| self.identifiers.contains(identifier))
            || identity
                .file_name
                .as_ref()
                .is_some_and(|name| self.file_names.contains(name))
    }

    /// Record the identity of an accepted entry.
    fn insert(&mut self, identity: EntryIdentity) {
        self.fingerprints.insert(identity.fingerprint);
        self.identifiers.extend(identity.identifiers);
        self.file_names.extend(identity.file_name);
    }
}

impl FromIterator<EntryIdentity> for MergeIndex {
    fn from_iter<T: IntoIterator<Item = EntryIdentity>>(iter: T) -> Self {
        let mut index = Self::default();
        for identity in iter {
            index.insert(identity);
        }
        index
    }
}

/// Records sorted by suggested key (and path, for determinism) before keys
/// are assigned collision-free.
fn sorted_records(results: &[(PathBuf, Biblio)]) -> Vec<(&Biblio, &PathBuf, String)> {
    let mut keyed: Vec<(&Biblio, &PathBuf, String)> = results
        .iter()
        .map(|(path, biblio)| (biblio, path, bibtex::suggest_key(biblio)))
        .collect();
    keyed.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| a.1.cmp(b.1)));
    keyed
}

/// Render one record under `key`, adding the PDF path as `file` with
/// `link`.
fn render_entry(key: &str, biblio: &Biblio, path: &Path, link: bool) -> String {
    if link {
        bibtex::format_entry_with_file(key, biblio, path)
    } else {
        bibtex::format_entry(key, biblio)
    }
}

/// Parse a rendered entry and derive its identity, adding the identifiers
/// from `biblio` and the PDF path that the renderer does not carry.
fn rendered_identity(
    rendered: &str,
    biblio: &Biblio,
    path: &Path,
) -> Result<(Entry, EntryIdentity), RenderError> {
    let bibliography =
        Bibliography::parse(rendered).map_err(|source| RenderError::Parse { source })?;
    let entry = bibliography
        .into_vec()
        .into_iter()
        .next()
        .ok_or_else(|| RenderError::Empty {
            entry: rendered.to_string(),
        })?;
    let mut identity = entry_identity(&entry);
    identity.identifiers.extend(biblio_identifiers(biblio));
    identity.file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_lowercase());
    Ok((entry, identity))
}

/// The identity of an entry parsed from an existing bibliography.
fn entry_identity(entry: &Entry) -> EntryIdentity {
    EntryIdentity {
        fingerprint: entry_fingerprint(entry),
        identifiers: entry_identifiers(entry),
        file_name: entry_text(entry, "file")
            .or_else(|| entry_text(entry, "pdf"))
            .and_then(|file| normalize_file_name(&file)),
    }
}

/// Normalized content of all fields of an entry. The citation key is not
/// content; field names are already lowercase, values are lowercased and
/// whitespace-collapsed.
fn entry_fingerprint(entry: &Entry) -> String {
    let mut fingerprint = format!("{:?}\n", entry.entry_type);
    for (name, value) in &entry.fields {
        fingerprint.push_str(name);
        fingerprint.push('=');
        fingerprint.push_str(&normalize_text(&value.format_verbatim()));
        fingerprint.push('\n');
    }
    fingerprint
}

/// Lowercase and collapse whitespace for content comparison.
fn normalize_text(raw: &str) -> String {
    raw.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Normalized identifiers of a parsed entry, as `kind:value` strings. The
/// fields commonly used for them are all read: `doi`, `pmid`, `pmcid`,
/// `arxiv` and an `eprint` typed as arXiv.
fn entry_identifiers(entry: &Entry) -> Vec<String> {
    let mut identifiers = Vec::new();
    if let Some(doi) = entry_text(entry, "doi").and_then(|raw| normalize_doi(&raw)) {
        identifiers.push(format!("doi:{doi}"));
    }
    if let Some(pmid) = entry_text(entry, "pmid").and_then(|raw| normalize_pmid(&raw)) {
        identifiers.push(format!("pmid:{pmid}"));
    }
    if let Some(pmcid) = entry_text(entry, "pmcid").and_then(|raw| normalize_pmcid(&raw)) {
        identifiers.push(format!("pmcid:{pmcid}"));
    }
    if let Some(arxiv) = entry_arxiv(entry) {
        identifiers.push(format!("arxiv:{arxiv}"));
    }
    identifiers
}

/// Normalized identifiers of a parsed record. The kinds mirror
/// [`entry_identifiers`], so that records written later match parsed ones.
fn biblio_identifiers(biblio: &Biblio) -> Vec<String> {
    let mut identifiers = Vec::new();
    if let Some(doi) = biblio.doi.as_deref().and_then(normalize_doi) {
        identifiers.push(format!("doi:{doi}"));
    }
    if let Some(pmid) = biblio.pmid.as_deref().and_then(normalize_pmid) {
        identifiers.push(format!("pmid:{pmid}"));
    }
    if let Some(pmcid) = biblio.pmcid.as_deref().and_then(normalize_pmcid) {
        identifiers.push(format!("pmcid:{pmcid}"));
    }
    if let Some(arxiv) = biblio.arxiv_id.as_deref().and_then(normalize_arxiv) {
        identifiers.push(format!("arxiv:{arxiv}"));
    }
    identifiers
}

/// The verbatim text of a field of a parsed entry.
fn entry_text(entry: &Entry, field: &str) -> Option<String> {
    let text = entry.get(field)?.format_verbatim();
    let trimmed = text.trim();
    (!trimmed.is_empty()).then_some(trimmed.to_string())
}

/// The arXiv identifier of an entry: an `arxiv` field, or an `eprint`
/// field whose `eprinttype`/`archiveprefix` is arXiv.
fn entry_arxiv(entry: &Entry) -> Option<String> {
    if let Some(arxiv) = entry_text(entry, "arxiv").and_then(|raw| normalize_arxiv(&raw)) {
        return Some(arxiv);
    }
    let kind = entry_text(entry, "eprinttype").or_else(|| entry_text(entry, "archiveprefix"))?;
    if !kind.eq_ignore_ascii_case("arxiv") {
        return None;
    }
    entry_text(entry, "eprint").and_then(|raw| normalize_arxiv(&raw))
}

/// Normalize a DOI: strip a resolver or `doi:` prefix, lowercase and trim.
fn normalize_doi(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let lower = trimmed.to_ascii_lowercase();
    let stripped = [
        "https://doi.org/",
        "http://doi.org/",
        "https://dx.doi.org/",
        "http://dx.doi.org/",
        "doi:",
    ]
    .iter()
    .find_map(|prefix| lower.strip_prefix(prefix))
    .unwrap_or(&lower)
    .trim_end_matches(['.', ',', ';'])
    .trim();
    (!stripped.is_empty()).then_some(stripped.to_string())
}

/// Normalize a PubMed ID: digits only, behind an optional `pmid:` prefix.
fn normalize_pmid(raw: &str) -> Option<String> {
    let lower = raw.trim().to_ascii_lowercase();
    let digits = lower.strip_prefix("pmid:").unwrap_or(&lower).trim();
    (!digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())).then_some(digits.to_string())
}

/// Normalize a PubMed Central ID: digits only, behind an optional
/// `pmcid:`/`pmc:` prefix and an optional `PMC`.
fn normalize_pmcid(raw: &str) -> Option<String> {
    let lower = raw.trim().to_ascii_lowercase();
    let rest = lower
        .strip_prefix("pmcid:")
        .or_else(|| lower.strip_prefix("pmc:"))
        .unwrap_or(&lower);
    let digits = rest.strip_prefix("pmc").unwrap_or(rest).trim();
    (!digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())).then_some(digits.to_string())
}

/// Normalize an arXiv identifier: strip an `arxiv:` prefix and a version
/// suffix, so `2404.14498v2` matches `2404.14498`.
fn normalize_arxiv(raw: &str) -> Option<String> {
    let lower = raw.trim().to_ascii_lowercase();
    let rest = lower.strip_prefix("arxiv:").unwrap_or(&lower).trim();
    let base = match rest.rsplit_once('v') {
        Some((base, version))
            if !base.is_empty()
                && !version.is_empty()
                && version.chars().all(|c| c.is_ascii_digit()) =>
        {
            base
        }
        _ => rest,
    };
    (!base.is_empty()).then_some(base.to_string())
}

/// The lowercased file-name component of a path or of a recorded `file`
/// field value.
fn normalize_file_name(raw: &str) -> Option<String> {
    let name = raw.rsplit(['/', '\\']).next()?.trim();
    (!name.is_empty()).then_some(name.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Author, Biblio};

    /// A record for merge and append tests.
    fn record(surname: &str, year: &str, doi: Option<&str>) -> Biblio {
        Biblio {
            authors: vec![Author {
                surname: Some(surname.to_string()),
                ..Author::default()
            }],
            date: Some(year.to_string()),
            title: Some("A title".to_string()),
            doi: doi.map(str::to_string),
            ..Biblio::default()
        }
    }

    #[test]
    fn test_format_all_unique_keys() {
        let make = |surname: &str| {
            let mut biblio = Biblio::default();
            biblio.authors.push(Author {
                surname: Some(surname.to_string()),
                ..Author::default()
            });
            biblio.date = Some("2020".to_string());
            biblio
        };
        // Three records, two with the same surname: keys must be unique and
        // deterministically suffixed.
        let results = vec![
            (PathBuf::from("b.pdf"), make("Smith")),
            (PathBuf::from("a.pdf"), make("Smith")),
            (PathBuf::from("c.pdf"), make("Jones")),
        ];
        let entries = Collection::new().format_all(&results, false);
        let keys: Vec<&str> = entries
            .iter()
            .map(|(_, entry)| entry.lines().next().unwrap())
            .collect();
        assert_eq!(
            keys,
            vec!["@misc{Jones2020,", "@misc{Smith2020,", "@misc{Smith2020-2,"]
        );
    }

    #[test]
    fn test_format_all_link() {
        let biblio = record("Smith", "2020", None);
        let results = vec![(PathBuf::from("papers/a.pdf"), biblio)];
        let collection = Collection::new();
        let linked = collection.format_all(&results, true);
        assert!(
            linked[0].1.contains("file = {papers/a.pdf},"),
            "{}",
            linked[0].1
        );
        let plain = collection.format_all(&results, false);
        assert!(!plain[0].1.contains("file = "), "{}", plain[0].1);
    }

    #[test]
    fn test_keys_are_reserved() {
        // The key of an existing entry must not be reused: the new entry is
        // suffixed instead.
        let collection = Collection::parse("@misc{Smith2020,\n  title = {Existing},\n}\n")
            .expect("parse collection");
        let results = vec![(PathBuf::from("a.pdf"), record("Smith", "2020", None))];
        let entries = collection.format_all(&results, false);
        assert!(
            entries[0].1.starts_with("@misc{Smith2020-2,"),
            "{}",
            entries[0].1
        );
        // An empty collection uses the natural key.
        let entries = Collection::new().format_all(&results, false);
        assert!(
            entries[0].1.starts_with("@misc{Smith2020,"),
            "{}",
            entries[0].1
        );
    }

    #[test]
    fn test_append_into_existing_file() {
        let dir = std::env::temp_dir().join(format!("grobid-append-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("refs.bib");
        // The existing entry ends with a single newline; the appended entry
        // must be separated by one blank line.
        std::fs::write(&path, "@misc{Smith2019,\n  title = {Existing},\n}\n")
            .expect("write existing bib");

        let collection = Collection::load(&path).expect("load existing");
        assert_eq!(collection.len(), 1);
        let results = vec![(PathBuf::from("a.pdf"), record("Smith", "2019", None))];
        let entries = collection.format_all(&results, false);
        let bibtex = entries
            .iter()
            .map(|(_, entry)| entry.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        append(&path, &bibtex).expect("append entries");

        let appended = std::fs::read_to_string(&path).expect("read appended bib");
        assert!(appended.starts_with("@misc{Smith2019,"), "{appended}");
        assert!(appended.contains("}\n\n@misc{Smith2019-2,"), "{appended}");
        assert!(appended.ends_with("}\n"), "{appended}");
        // The file itself parses back without duplicate keys.
        let bibliography = Bibliography::parse(&appended).expect("parse appended bib");
        assert_eq!(bibliography.len(), 2);
        std::fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn test_merge_skips_exact_duplicate() {
        let dir = std::env::temp_dir().join(format!("grobid-merge-exact-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("refs.bib");
        let biblio = record("Smith", "2020", None);
        // A different key must not defeat the content comparison.
        let rendered = bibtex::format_entry("OldKey2020", &biblio);
        std::fs::write(&path, format!("{rendered}\n")).expect("write existing bib");

        let mut collection = Collection::load(&path).expect("load existing");
        let results = vec![(PathBuf::from("again.pdf"), biblio)];
        let (entries, duplicates) = collection.merge_all(&results, false).expect("merge");
        assert!(entries.is_empty(), "{entries:?}");
        assert_eq!(duplicates, 1);
        assert_eq!(collection.len(), 1);
        std::fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn test_merge_skips_identifier_match() {
        let dir = std::env::temp_dir().join(format!("grobid-merge-doi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("refs.bib");
        let existing = record("Smith", "2020", Some("https://doi.org/10.1234/ABC.1"));
        let rendered = bibtex::format_entry("Smith2020", &existing);
        std::fs::write(&path, format!("{rendered}\n")).expect("write existing bib");

        let mut collection = Collection::load(&path).expect("load existing");
        // A different extraction of the same work: the DOI matches although
        // title and key differ.
        let mut fresh = record("Smith", "2020", Some("doi:10.1234/abc.1"));
        fresh.title = Some("Another title".to_string());
        let results = vec![(PathBuf::from("b.pdf"), fresh)];
        let (entries, duplicates) = collection.merge_all(&results, false).expect("merge");
        assert!(entries.is_empty(), "{entries:?}");
        assert_eq!(duplicates, 1);
        std::fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn test_merge_skips_same_file_name() {
        let dir = std::env::temp_dir().join(format!("grobid-merge-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("refs.bib");
        let existing = record("Smith", "2019", Some("10.1/old"));
        let rendered =
            bibtex::format_entry_with_file("Smith2019", &existing, "papers/deep/paper.pdf");
        std::fs::write(&path, format!("{rendered}\n")).expect("write existing bib");

        let mut collection = Collection::load(&path).expect("load existing");
        // Different metadata and no identifier, but the same PDF file name.
        let mut fresh = record("Jones", "2021", None);
        fresh.title = Some("Different".to_string());
        let results = vec![(PathBuf::from("/elsewhere/paper.pdf"), fresh)];
        let (entries, duplicates) = collection.merge_all(&results, true).expect("merge");
        assert!(entries.is_empty(), "{entries:?}");
        assert_eq!(duplicates, 1);
        std::fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn test_merge_adds_new_and_dedupes_batch() {
        let dir = std::env::temp_dir().join(format!("grobid-merge-batch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("refs.bib");
        let existing = record("Smith", "2019", Some("10.1/old"));
        let rendered = bibtex::format_entry("Smith2019", &existing);
        std::fs::write(&path, format!("{rendered}\n")).expect("write existing bib");

        let mut collection = Collection::load(&path).expect("load existing");
        let results = vec![
            // Already in the file.
            (PathBuf::from("a.pdf"), existing),
            // New, and once more in the same batch under a different DOI
            // spelling.
            (
                PathBuf::from("b.pdf"),
                record("Jones", "2021", Some("10.1/new")),
            ),
            (
                PathBuf::from("c.pdf"),
                record("Jones", "2021", Some("https://doi.org/10.1/NEW")),
            ),
        ];
        let (entries, duplicates) = collection.merge_all(&results, false).expect("merge");
        assert_eq!(duplicates, 2);
        assert_eq!(entries.len(), 1);
        assert!(
            entries[0].1.starts_with("@misc{Jones2021,"),
            "{}",
            entries[0].1
        );
        assert!(
            entries[0].1.contains("doi = {10.1/new}"),
            "{}",
            entries[0].1
        );
        // The merged records are part of the collection now.
        assert_eq!(collection.len(), 2);
        assert!(collection.keys().contains("Jones2021"));
        std::fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn test_normalize_identifiers() {
        assert_eq!(
            normalize_doi("https://doi.org/10.1234/ABC.1."),
            Some("10.1234/abc.1".to_string())
        );
        assert_eq!(normalize_pmid("PMID: 12345"), Some("12345".to_string()));
        assert_eq!(normalize_pmcid("PMC12345"), Some("12345".to_string()));
        assert_eq!(
            normalize_arxiv("arXiv:2404.14498v2"),
            Some("2404.14498".to_string())
        );
        assert_eq!(
            normalize_file_name("papers/deep/Paper.PDF"),
            Some("paper.pdf".to_string())
        );
    }

    #[test]
    fn test_append_to_empty_file() {
        let dir = std::env::temp_dir().join(format!("grobid-append-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("refs.bib");
        std::fs::write(&path, "").expect("write empty bib");
        assert!(Collection::load(&path).expect("load").is_empty());
        append(&path, "@misc{a,\n}").expect("append to empty file");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "@misc{a,\n}\n"
        );
        std::fs::remove_dir_all(&dir).expect("remove temp dir");
    }
}
