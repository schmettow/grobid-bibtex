//! Working with the PDF files behind the records.
//!
//! Directory discovery for extraction batches, and the rename policy of
//! `pdf2bibtex`: renaming a PDF to the name suggested by the metadata
//! extracted from it, with collision-free names.

use std::path::{Path, PathBuf};

use crate::Biblio;
use crate::bibtex;

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

/// Rename every record's PDF to the name suggested by
/// [`bibtex::suggest_file_name`], keeping the original file extension, and
/// update `records` to the files' final locations (so that `--link` entries
/// point at the renamed files).
///
/// Files are handled in path order, so collision suffixes (`-2`, `-3`, ...)
/// are deterministic; a PDF that already bears the requested name is left
/// alone. One [`Rename`] event per record is returned, in path order.
pub fn rename_pdfs(records: &mut [(PathBuf, Biblio)]) -> Vec<Rename> {
    // Rename in path order so that collision suffixes are deterministic.
    let mut order: Vec<usize> = (0..records.len()).collect();
    order.sort_by(|&a, &b| records[a].0.cmp(&records[b].0));
    let mut events = Vec::with_capacity(records.len());
    for index in order {
        let path = records[index].0.clone();
        let Some(target) = bibtex::suggest_file_name(&path, &records[index].1) else {
            events.push(Rename::Unnamed { path });
            continue;
        };
        if target == path {
            events.push(Rename::Unchanged { path });
            continue;
        }
        let target = bibtex::unique_path(target);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Author;

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
        let biblio = || Biblio {
            authors: vec![Author {
                surname: Some("Smith".to_string()),
                ..Author::default()
            }],
            date: Some("2020".to_string()),
            title: Some("Same title".to_string()),
            ..Biblio::default()
        };
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
}
