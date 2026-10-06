//! Working with the PDF files behind the records.
//!
//! Directory discovery for extraction batches, the rename policy of
//! `pdf2bibtex`, and a sidecar [`Manifest`] that makes repeated batch runs
//! skip files that have not changed. The manifest can cache each processed
//! file's extracted record, so a bibliography can be rebuilt from it without
//! querying GROBID and OpenAlex again.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::Biblio;
use crate::bibtex::{self, FileStemOptions};

/// Recursively collect all PDFs under `dir`, in sorted order.
///
/// # Errors
///
/// Returns the underlying I/O error if `dir` or a subdirectory cannot be
/// read.
pub fn collect_pdfs(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                walk(&path, out)?;
            } else if path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
            {
                out.push(path);
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, &mut out)?;
    out.sort();
    Ok(out)
}

/// What [`rename_pdfs`] did with one record's PDF.
#[derive(Debug)]
pub enum Rename {
    /// The PDF was renamed.
    Renamed {
        /// The path before the rename.
        from: PathBuf,
        /// The path after the rename.
        to: PathBuf,
    },
    /// The PDF already bears the suggested name.
    Unchanged {
        /// The unchanged path.
        path: PathBuf,
    },
    /// No name could be suggested: no usable author, year or title was
    /// extracted.
    Unnamed {
        /// The path that keeps its name.
        path: PathBuf,
    },
    /// The rename failed; the PDF keeps its path.
    Failed {
        /// The path before the attempted rename.
        from: PathBuf,
        /// The attempted target path.
        to: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
}

/// How [`rename_pdfs_with`] disambiguates a name that is already taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Collision {
    /// Append a counter before the extension: `Title.pdf`, `Title-2.pdf`.
    #[default]
    Suffix,
    /// Number the year in place: `... - 2020 - Title` becomes
    /// `... - 2020-1 - Title`, keeping authors and title in their place.
    Year,
}

/// Rename every record's PDF to the name suggested by
/// [`bibtex::suggest_file_name`], keeping the original file extension, and
/// update `records` to the files' final locations (so that `--link` entries
/// point at the renamed files).
///
/// Files are handled in path order, so collision suffixes (`-2`, `-3`, ...)
/// are deterministic; a PDF that already bears the requested name is left
/// alone. One [`Rename`] event per record is returned, in path order.
///
/// This is [`rename_pdfs_with`] with the default file-stem policy
/// ([`FileStemOptions::default`]) and [`Collision::Suffix`].
pub fn rename_pdfs(records: &mut [(PathBuf, Biblio)]) -> Vec<Rename> {
    rename_pdfs_with(records, &FileStemOptions::default(), Collision::Suffix)
}

/// Like [`rename_pdfs`], with an explicit file-stem policy
/// ([`FileStemOptions`]) and collision style ([`Collision`]).
pub fn rename_pdfs_with(
    records: &mut [(PathBuf, Biblio)],
    stem: &FileStemOptions,
    collision: Collision,
) -> Vec<Rename> {
    // Rename in path order so that collision handling is deterministic.
    let mut order: Vec<usize> = (0..records.len()).collect();
    order.sort_by(|&a, &b| records[a].0.cmp(&records[b].0));
    let mut events = Vec::with_capacity(records.len());
    for index in order {
        let path = records[index].0.clone();
        let Some(target) = bibtex::suggest_file_name_with(&path, &records[index].1, stem) else {
            events.push(Rename::Unnamed { path });
            continue;
        };
        if target == path {
            events.push(Rename::Unchanged { path });
            continue;
        }
        let target = match collision {
            Collision::Suffix => bibtex::unique_path(target),
            Collision::Year => {
                bibtex::unique_path_with_year(target, bibtex::year(&records[index].1).as_deref())
            }
        };
        match std::fs::rename(&path, &target) {
            Ok(()) => {
                records[index].0 = target.clone();
                events.push(Rename::Renamed {
                    from: path,
                    to: target,
                });
            }
            Err(source) => events.push(Rename::Failed {
                from: path,
                to: target,
                source,
            }),
        }
    }
    events
}

/// Errors from loading or saving a [`Manifest`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ManifestError {
    /// The manifest file could not be read.
    #[error("cannot read {path}: {source}")]
    Read {
        /// The manifest path.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The manifest file is not valid JSON.
    #[error("cannot parse {path}: {source}")]
    Parse {
        /// The manifest path.
        path: PathBuf,
        /// The underlying parse error.
        #[source]
        source: serde_json::Error,
    },
    /// The manifest could not be serialized.
    #[error("cannot serialize manifest for {path}: {source}")]
    Serialize {
        /// The manifest path.
        path: PathBuf,
        /// The underlying serialization error.
        #[source]
        source: serde_json::Error,
    },
    /// The manifest file could not be written.
    #[error("cannot write {path}: {source}")]
    Write {
        /// The manifest path.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// A sidecar record of the files a batch has already processed.
///
/// Repeated extraction runs should not pay the parse and completion cost for
/// documents they have already seen. A manifest stores a size/mtime
/// fingerprint for every processed file relative to a corpus folder and
/// persists it as JSON. The intended flow is:
///
/// 1. filter `collect_pdfs` results with [`Manifest::is_unchanged`],
/// 2. process the remaining files,
/// 3. [`Manifest::record`] each processed file (under its final name),
/// 4. [`Manifest::prune`] files that no longer exist, and
/// 5. [`Manifest::save`].
///
/// With [`Manifest::record_with_biblio`], a processed file's extracted
/// record is cached alongside its fingerprint and can be read back with
/// [`Manifest::biblio`]; [`Manifest::record`] refreshes a fingerprint without
/// dropping a cached record. The JSON is compatible with manifests written
/// before records were cached.
pub struct Manifest {
    path: PathBuf,
    files: HashMap<String, FileRecord>,
}

/// A file's identity: size and modification time in whole seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Fingerprint {
    size: u64,
    mtime: u64,
}

/// A processed file: its fingerprint and, when extraction ran, its record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct FileRecord {
    size: u64,
    mtime: u64,
    /// The extracted and completed bibliographic record, when cached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    biblio: Option<Biblio>,
}

impl FileRecord {
    /// Whether the file still has the recorded fingerprint.
    fn matches(&self, fingerprint: &Fingerprint) -> bool {
        self.size == fingerprint.size && self.mtime == fingerprint.mtime
    }
}

/// The on-disk JSON shape of a [`Manifest`].
#[derive(Default, Serialize, Deserialize)]
struct ManifestData {
    #[serde(default)]
    files: HashMap<String, FileRecord>,
}

impl Manifest {
    /// An empty manifest that saves to `path`.
    pub fn empty(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            files: HashMap::new(),
        }
    }

    /// Load the manifest at `path`; a missing file yields an empty manifest.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError`] when the file exists but cannot be read or
    /// parsed.
    pub fn load(path: &Path) -> Result<Self, ManifestError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::empty(path));
            }
            Err(source) => {
                return Err(ManifestError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        let data: ManifestData =
            serde_json::from_str(&text).map_err(|source| ManifestError::Parse {
                path: path.to_path_buf(),
                source,
            })?;
        Ok(Self {
            path: path.to_path_buf(),
            files: data.files,
        })
    }

    /// Whether `file` is recorded under `folder` with the same fingerprint.
    pub fn is_unchanged(&self, folder: &Path, file: &Path) -> bool {
        let Some(fingerprint) = fingerprint(file) else {
            return false;
        };
        self.files
            .get(&Self::key(folder, file))
            .is_some_and(|record| record.matches(&fingerprint))
    }

    /// Record (or update) the fingerprint of `file`, keeping a cached record
    /// that [`Manifest::record_with_biblio`] stored earlier.
    pub fn record(&mut self, folder: &Path, file: &Path) {
        let Some(fingerprint) = fingerprint(file) else {
            return;
        };
        let key = Self::key(folder, file);
        match self.files.get_mut(&key) {
            Some(record) => {
                record.size = fingerprint.size;
                record.mtime = fingerprint.mtime;
            }
            None => {
                self.files.insert(
                    key,
                    FileRecord {
                        size: fingerprint.size,
                        mtime: fingerprint.mtime,
                        biblio: None,
                    },
                );
            }
        }
    }

    /// Record the fingerprint of `file` together with its extracted record.
    pub fn record_with_biblio(&mut self, folder: &Path, file: &Path, biblio: &Biblio) {
        let Some(fingerprint) = fingerprint(file) else {
            return;
        };
        self.files.insert(
            Self::key(folder, file),
            FileRecord {
                size: fingerprint.size,
                mtime: fingerprint.mtime,
                biblio: Some(biblio.clone()),
            },
        );
    }

    /// The cached record of `file`, if one was recorded.
    pub fn biblio(&self, folder: &Path, file: &Path) -> Option<&Biblio> {
        self.files.get(&Self::key(folder, file))?.biblio.as_ref()
    }

    /// Drop entries whose file no longer exists under `folder`.
    pub fn prune(&mut self, folder: &Path) {
        self.files.retain(|key, _| folder.join(key).exists());
    }

    /// Write the manifest back to its path.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError`] when the manifest cannot be serialized or
    /// written.
    pub fn save(&self) -> Result<(), ManifestError> {
        let json = serde_json::to_string_pretty(&ManifestData {
            files: self.files.clone(),
        })
        .map_err(|source| ManifestError::Serialize {
            path: self.path.clone(),
            source,
        })?;
        std::fs::write(&self.path, json).map_err(|source| ManifestError::Write {
            path: self.path.clone(),
            source,
        })
    }

    /// The path of `file` relative to `folder`, as the manifest key.
    fn key(folder: &Path, file: &Path) -> String {
        file.strip_prefix(folder)
            .unwrap_or(file)
            .to_string_lossy()
            .into_owned()
    }
}

/// The size/mtime fingerprint of `path`, if it can be read.
fn fingerprint(path: &Path) -> Option<Fingerprint> {
    let metadata = path.metadata().ok()?;
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    Some(Fingerprint {
        size: metadata.len(),
        mtime,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Author;

    fn record(surname: &str, year: &str, title: &str) -> Biblio {
        Biblio {
            authors: vec![Author {
                surname: Some(surname.to_string()),
                ..Author::default()
            }],
            date: Some(year.to_string()),
            title: Some(title.to_string()),
            ..Biblio::default()
        }
    }

    #[test]
    fn test_collect_pdfs_is_sorted_and_recursive() {
        let dir = std::env::temp_dir().join(format!("grobid-collect-{}", std::process::id()));
        let nested = dir.join("nested");
        std::fs::create_dir_all(&nested).expect("create temp dir");
        for name in ["b.pdf", "a.PDF", "notes.txt"] {
            std::fs::write(dir.join(name), b"pdf").expect("write file");
        }
        std::fs::write(nested.join("c.pdf"), b"pdf").expect("write file");
        let pdfs = collect_pdfs(&dir).expect("collect");
        let names: Vec<String> = pdfs
            .iter()
            .map(|path| {
                path.strip_prefix(&dir)
                    .expect("under dir")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, vec!["a.PDF", "b.pdf", "nested/c.pdf"]);
        std::fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn test_rename_pdfs_collision() {
        let dir = std::env::temp_dir().join(format!("grobid-rename-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let make = |name: &str| {
            let path = dir.join(name);
            std::fs::write(&path, b"pdf").expect("write file");
            path
        };
        let biblio = || record("Smith", "2020", "Same title");
        // Both records map to the same name; the first in path order gets
        // the plain name, the second a `-2` suffix.
        let mut results = vec![(make("b.pdf"), biblio()), (make("a.pdf"), biblio())];
        let events = rename_pdfs(&mut results);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Rename::Renamed { .. }))
                .count(),
            2
        );
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .expect("read temp dir")
            .map(|entry| entry.expect("dir entry").file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["Smith_2020_Same_title-2.pdf", "Smith_2020_Same_title.pdf"]
        );
        // The results now point at the renamed files, for `--link` output.
        let mut paths: Vec<String> = results
            .iter()
            .map(|(path, _)| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        paths.sort();
        assert_eq!(paths, names);
        std::fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn test_rename_pdfs_with_keyed_names_and_year_collision() {
        let dir = std::env::temp_dir().join(format!("grobid-rename-keyed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let make = |name: &str| {
            let path = dir.join(name);
            std::fs::write(&path, b"pdf").expect("write file");
            path
        };
        let stem = FileStemOptions {
            style: crate::bibtex::FileStemStyle::Keyed,
            title_words: 10,
            ascii_only: false,
        };
        let biblio = || record("Smith", "2020", "Same title");
        let mut results = vec![(make("b.pdf"), biblio()), (make("a.pdf"), biblio())];
        let events = rename_pdfs_with(&mut results, &stem, Collision::Year);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Rename::Renamed { .. }))
                .count(),
            2
        );
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .expect("read temp dir")
            .map(|entry| entry.expect("dir entry").file_name().into_string().unwrap())
            .collect();
        names.sort();
        // The collision is numbered on the year, keeping authors and title.
        assert_eq!(
            names,
            vec![
                "Smith2020 - Smith - Same title - 2020-1.pdf",
                "Smith2020 - Smith - Same title - 2020.pdf"
            ]
        );
        std::fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn test_manifest_skips_only_unchanged_files() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("paper.pdf");
        std::fs::write(&file, b"one").expect("write");
        let path = dir.path().join(".manifest.json");
        let mut manifest = Manifest::empty(&path);
        assert!(!manifest.is_unchanged(dir.path(), &file));

        manifest.record(dir.path(), &file);
        assert!(manifest.is_unchanged(dir.path(), &file));

        // A different size means the content changed and is rescanned.
        std::fs::write(&file, b"much longer").expect("write");
        assert!(!manifest.is_unchanged(dir.path(), &file));
    }

    #[test]
    fn test_manifest_roundtrip_and_prune() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(".manifest.json");
        let kept = dir.path().join("kept.pdf");
        let gone = dir.path().join("gone.pdf");
        std::fs::write(&kept, b"pdf").expect("write");
        std::fs::write(&gone, b"pdf").expect("write");

        let mut manifest = Manifest::empty(&path);
        manifest.record(dir.path(), &kept);
        manifest.record(dir.path(), &gone);
        manifest.save().expect("save manifest");

        let manifest = Manifest::load(&path).expect("load manifest");
        assert!(manifest.is_unchanged(dir.path(), &kept));
        assert!(manifest.is_unchanged(dir.path(), &gone));

        // A missing manifest is empty, not an error.
        assert!(
            !Manifest::load(&dir.path().join("other.json"))
                .expect("missing manifest")
                .is_unchanged(dir.path(), &kept)
        );

        std::fs::remove_file(&gone).expect("remove file");
        let mut manifest = manifest;
        manifest.prune(dir.path());
        manifest.save().expect("save pruned manifest");
        let manifest = Manifest::load(&path).expect("reload manifest");
        assert!(manifest.is_unchanged(dir.path(), &kept));
        assert!(!manifest.is_unchanged(dir.path(), &gone));
    }

    #[test]
    fn test_manifest_caches_biblio() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("paper.pdf");
        std::fs::write(&file, b"pdf").expect("write");
        let path = dir.path().join(".manifest.json");
        let biblio = record("Kahle", "2000", "The Barc model");
        let mut manifest = Manifest::empty(&path);
        assert!(manifest.biblio(dir.path(), &file).is_none());

        manifest.record_with_biblio(dir.path(), &file, &biblio);
        assert_eq!(manifest.biblio(dir.path(), &file), Some(&biblio));
        manifest.save().expect("save manifest");

        let mut manifest = Manifest::load(&path).expect("load manifest");
        assert!(manifest.is_unchanged(dir.path(), &file));
        assert_eq!(manifest.biblio(dir.path(), &file), Some(&biblio));

        // Refreshing the fingerprint keeps the cached record.
        manifest.record(dir.path(), &file);
        assert!(manifest.is_unchanged(dir.path(), &file));
        assert_eq!(manifest.biblio(dir.path(), &file), Some(&biblio));

        // Re-recording with a new record replaces the cache.
        let newer = record("Kahle", "2001", "Second edition");
        manifest.record_with_biblio(dir.path(), &file, &newer);
        assert_eq!(manifest.biblio(dir.path(), &file), Some(&newer));

        // Pruning a deleted file drops its record too.
        std::fs::remove_file(&file).expect("remove file");
        manifest.prune(dir.path());
        assert!(manifest.biblio(dir.path(), &file).is_none());
    }

    #[test]
    fn test_manifest_loads_legacy_json_without_records() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("paper.pdf");
        std::fs::write(&file, b"pdf").expect("write");
        let path = dir.path().join(".manifest.json");
        // The manifest format before records were cached: only fingerprints.
        let metadata = std::fs::metadata(&file).expect("metadata");
        let mtime = metadata
            .modified()
            .expect("mtime")
            .duration_since(std::time::UNIX_EPOCH)
            .expect("after epoch")
            .as_secs();
        std::fs::write(
            &path,
            format!(
                "{{\"files\":{{\"paper.pdf\":{{\"size\":{},\"mtime\":{mtime}}}}}}}",
                metadata.len()
            ),
        )
        .expect("write legacy manifest");

        let mut manifest = Manifest::load(&path).expect("legacy manifest loads");
        assert!(manifest.is_unchanged(dir.path(), &file));
        assert!(manifest.biblio(dir.path(), &file).is_none());

        // Caching a record upgrades the entry in place.
        let biblio = record("Kahle", "2000", "The Barc model");
        manifest.record_with_biblio(dir.path(), &file, &biblio);
        manifest.save().expect("save upgraded manifest");
        let manifest = Manifest::load(&path).expect("reload");
        assert_eq!(manifest.biblio(dir.path(), &file), Some(&biblio));
    }
}
