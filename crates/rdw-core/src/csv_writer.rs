//! Stages CSV (or ZIP of numbered CSV parts) output to a temp file.
//!
//! The caller must only invoke [`assemble`] once every RDW page has been
//! fetched and merge-joined successfully; a partial or incorrect CSV must
//! never be delivered. If assembly itself fails partway (e.g. disk full),
//! the temp file is removed before the error is returned, so no orphaned
//! partial file and no partial response ever exist.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Excel's usable data-row limit: 1,048,576 rows total including the header.
pub const EXCEL_MAX_DATA_ROWS: usize = 1_048_575;

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The result of staging an export: either a single CSV file or a ZIP of
/// numbered CSV parts, plus its size for the `Content-Length` header.
pub enum Assembled {
    Csv {
        path: PathBuf,
        content_length: u64,
    },
    Zip {
        path: PathBuf,
        content_length: u64,
        part_count: usize,
    },
}

impl Assembled {
    pub fn path(&self) -> &Path {
        match self {
            Assembled::Csv { path, .. } => path,
            Assembled::Zip { path, .. } => path,
        }
    }
}

/// Delete the staged temp file, ignoring a missing-file error (already
/// cleaned up, or never created).
pub fn cleanup(assembled: &Assembled) {
    if let Err(e) = std::fs::remove_file(assembled.path()) {
        if e.kind() != io::ErrorKind::NotFound {
            tracing::warn!(path = %assembled.path().display(), error = %e, "failed to clean up temp export file");
        }
    }
}

fn unique_temp_path(suffix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "rdw-fuel-export-{}-{}-{}{}",
        std::process::id(),
        nanos,
        seq,
        suffix
    ))
}

/// Stage the complete, already-fetched-and-merged rows to a temp CSV file,
/// or split them into a ZIP of numbered CSV parts when they exceed Excel's
/// row limit. Never called until every page has succeeded.
///
/// Convenience wrapper over [`Assembler`] for callers (tests, small
/// exports) that already hold every row in memory. The production pipeline
/// uses [`Assembler`] directly so it never buffers more than one page.
pub fn assemble(header: &[String], rows: &[Vec<String>]) -> io::Result<Assembled> {
    let mut assembler = Assembler::new(header.to_vec());
    if let Err(e) = assembler.write_rows(rows) {
        assembler.abort();
        return Err(e);
    }
    assembler.finish()
}

/// One in-progress temp-file part: its path, an open CSV writer, and how
/// many data rows have been written to it so far.
struct Part {
    path: PathBuf,
    writer: csv::Writer<File>,
    row_count: usize,
}

/// Incrementally stages export rows to disk, one page's worth of rows at a
/// time, so the pipeline never holds the whole export in memory.
///
/// Rows are written to numbered part files as they arrive. A part is closed
/// once it reaches [`EXCEL_MAX_DATA_ROWS`] and a new one is opened. The
/// total row count (and therefore whether the final output is a single CSV
/// or a ZIP of parts) is only known once [`Assembler::finish`] is called,
/// which is only ever invoked by the caller after every page has succeeded.
pub struct Assembler {
    header: Vec<String>,
    part_paths: Vec<PathBuf>,
    current: Option<Part>,
}

impl Assembler {
    pub fn new(header: Vec<String>) -> Self {
        Self {
            header,
            part_paths: Vec::new(),
            current: None,
        }
    }

    fn open_new_part(&mut self) -> io::Result<()> {
        let path = unique_temp_path(".part.csv");
        let file = File::create(&path)?;
        let mut writer = csv::WriterBuilder::new().from_writer(file);
        writer.write_record(&self.header)?;
        self.current = Some(Part {
            path,
            writer,
            row_count: 0,
        });
        Ok(())
    }

    fn close_current_part(&mut self) -> io::Result<()> {
        if let Some(mut part) = self.current.take() {
            part.writer.flush()?;
            drop(part.writer);
            self.part_paths.push(part.path);
        }
        Ok(())
    }

    /// Write one batch (typically one RDW page) of already-widened rows.
    /// Keeps only this batch in memory; nothing from prior or future
    /// batches is retained.
    pub fn write_rows(&mut self, rows: &[Vec<String>]) -> io::Result<()> {
        for row in rows {
            if self.current.is_none() {
                self.open_new_part()?;
            }
            if self
                .current
                .as_ref()
                .map(|p| p.row_count >= EXCEL_MAX_DATA_ROWS)
                .unwrap_or(false)
            {
                self.close_current_part()?;
                self.open_new_part()?;
            }
            let part = self.current.as_mut().expect("part opened above");
            part.writer.write_record(row)?;
            part.row_count += 1;
        }
        Ok(())
    }

    /// Finish assembly: close the final part and, depending on how many
    /// parts were produced, return either a single CSV or a ZIP of numbered
    /// parts. Only ever call this once every batch has been written
    /// successfully; on any earlier failure call [`Assembler::abort`]
    /// instead so no partial file is left as if it were a complete export.
    pub fn finish(mut self) -> io::Result<Assembled> {
        if self.current.is_none() && self.part_paths.is_empty() {
            // No rows at all: still emit a header-only single CSV.
            self.open_new_part()?;
        }
        self.close_current_part()?;

        if self.part_paths.len() == 1 {
            let path = self.part_paths.remove(0);
            let content_length = std::fs::metadata(&path)?.len();
            Ok(Assembled::Csv {
                path,
                content_length,
            })
        } else {
            self.zip_parts()
        }
    }

    fn zip_parts(&mut self) -> io::Result<Assembled> {
        let zip_path = unique_temp_path(".zip");
        let result = self.try_zip_parts(&zip_path);
        for part_path in &self.part_paths {
            let _ = std::fs::remove_file(part_path);
        }
        match result {
            Ok((content_length, part_count)) => Ok(Assembled::Zip {
                path: zip_path,
                content_length,
                part_count,
            }),
            Err(e) => {
                let _ = std::fs::remove_file(&zip_path);
                Err(e)
            }
        }
    }

    fn try_zip_parts(&self, zip_path: &Path) -> io::Result<(u64, usize)> {
        let file = File::create(zip_path)?;
        let mut zip = zip::ZipWriter::new(file);
        let options: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);

        let part_count = self.part_paths.len();
        for (idx, part_path) in self.part_paths.iter().enumerate() {
            let part_name = format!("part-{}.csv", idx + 1);
            zip.start_file(part_name, options)
                .map_err(|e| io::Error::other(e.to_string()))?;
            let mut part_file = File::open(part_path)?;
            io::copy(&mut part_file, &mut zip)?;
        }
        zip.finish().map_err(|e| io::Error::other(e.to_string()))?;
        Ok((std::fs::metadata(zip_path)?.len(), part_count))
    }

    /// Remove every temp file written so far without producing a final
    /// output. Call this when a page fetch or merge-join fails partway
    /// through an export, so no partial file is ever mistaken for a
    /// complete one and no orphaned file is left behind.
    pub fn abort(mut self) {
        if let Some(part) = self.current.take() {
            let _ = std::fs::remove_file(&part.path);
        }
        for part_path in &self.part_paths {
            let _ = std::fs::remove_file(part_path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn header() -> Vec<String> {
        vec!["kenteken".to_string(), "merk".to_string()]
    }

    fn rows(n: usize) -> Vec<Vec<String>> {
        (0..n)
            .map(|i| vec![format!("KENTEKEN{i}"), "TOYOTA".to_string()])
            .collect()
    }

    #[test]
    fn happy_path_small_export_is_a_single_csv_file() {
        let assembled = assemble(&header(), &rows(3)).unwrap();
        match &assembled {
            Assembled::Csv {
                path,
                content_length,
            } => {
                assert!(*content_length > 0);
                let mut contents = String::new();
                File::open(path)
                    .unwrap()
                    .read_to_string(&mut contents)
                    .unwrap();
                assert_eq!(contents.lines().count(), 4); // header + 3 rows
            }
            Assembled::Zip { .. } => panic!("expected a single CSV file"),
        }
        cleanup(&assembled);
        assert!(!assembled.path().exists());
    }

    #[test]
    fn edge_exactly_at_excel_limit_is_still_a_single_csv() {
        let assembled = assemble(&header(), &rows(EXCEL_MAX_DATA_ROWS)).unwrap();
        assert!(matches!(assembled, Assembled::Csv { .. }));
        cleanup(&assembled);
    }

    #[test]
    fn edge_one_row_past_excel_limit_triggers_zip_with_two_parts() {
        let assembled = assemble(&header(), &rows(EXCEL_MAX_DATA_ROWS + 1)).unwrap();
        match &assembled {
            Assembled::Zip { part_count, .. } => assert_eq!(*part_count, 2),
            Assembled::Csv { .. } => panic!("expected a ZIP file"),
        }
        cleanup(&assembled);
    }

    #[test]
    fn failure_zip_parts_each_stay_within_excel_row_limit() {
        let total = EXCEL_MAX_DATA_ROWS * 2 + 10;
        let assembled = assemble(&header(), &rows(total)).unwrap();
        let path = match &assembled {
            Assembled::Zip {
                path, part_count, ..
            } => {
                assert_eq!(*part_count, 3);
                path.clone()
            }
            Assembled::Csv { .. } => panic!("expected a ZIP file"),
        };
        let file = File::open(&path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).unwrap();
            let mut contents = String::new();
            entry.read_to_string(&mut contents).unwrap();
            let data_row_count = contents.lines().count() - 1; // minus header
            assert!(
                data_row_count <= EXCEL_MAX_DATA_ROWS,
                "part {i} exceeds Excel row limit"
            );
        }
        cleanup(&assembled);
    }

    #[test]
    fn happy_path_incremental_writes_across_many_small_batches_match_buffered_output() {
        // Simulates the pipeline writing one RDW page at a time: many small
        // batches instead of one big slice, proving the assembler never
        // needs the whole export in memory at once to produce correct output.
        let mut assembler = Assembler::new(header());
        for chunk in rows(2_500).chunks(37) {
            assembler.write_rows(chunk).unwrap();
        }
        let assembled = assembler.finish().unwrap();
        match &assembled {
            Assembled::Csv { path, .. } => {
                let mut contents = String::new();
                File::open(path)
                    .unwrap()
                    .read_to_string(&mut contents)
                    .unwrap();
                assert_eq!(contents.lines().count(), 2_501); // header + 2500 rows
            }
            Assembled::Zip { .. } => panic!("expected a single CSV file"),
        }
        cleanup(&assembled);
    }

    #[test]
    fn edge_part_boundary_crossed_mid_batch_still_splits_at_excel_limit() {
        // A single write_rows() call whose rows straddle the Excel row-limit
        // boundary must still close part 1 at exactly EXCEL_MAX_DATA_ROWS
        // and continue writing part 2, not lose or duplicate rows.
        let mut assembler = Assembler::new(header());
        assembler
            .write_rows(&rows(EXCEL_MAX_DATA_ROWS - 5))
            .unwrap();
        assembler.write_rows(&rows(10)).unwrap(); // crosses the boundary
        let assembled = assembler.finish().unwrap();
        let path = match &assembled {
            Assembled::Zip {
                path, part_count, ..
            } => {
                assert_eq!(*part_count, 2);
                path.clone()
            }
            Assembled::Csv { .. } => panic!("expected a ZIP file"),
        };
        let file = File::open(&path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut total_data_rows = 0;
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).unwrap();
            let mut contents = String::new();
            entry.read_to_string(&mut contents).unwrap();
            let data_row_count = contents.lines().count() - 1; // minus header
            assert!(
                data_row_count <= EXCEL_MAX_DATA_ROWS,
                "part {i} exceeds Excel row limit"
            );
            total_data_rows += data_row_count;
        }
        assert_eq!(total_data_rows, EXCEL_MAX_DATA_ROWS - 5 + 10);
        cleanup(&assembled);
    }

    #[test]
    fn failure_abort_removes_partially_written_temp_files() {
        let mut assembler = Assembler::new(header());
        assembler.write_rows(&rows(5)).unwrap();
        let part_path = assembler.current.as_ref().unwrap().path.clone();
        assert!(part_path.exists());
        assembler.abort();
        assert!(!part_path.exists());
    }
}
