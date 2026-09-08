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

use crate::failure::FuelFailureSummary;

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
    writer: csv::Writer<flate2::write::GzEncoder<File>>,
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
        // Parts are written gzip-compressed. This CSV compresses about 13x,
        // and the staging directory on a serverless host is small: Vercel gives
        // a function 500MB of writable /tmp, while a full Toyota export is
        // 736MB uncompressed and crashed the container. Compressed it is 63MB.
        // Compressing once here also avoids compressing again per response.
        let file = File::create(&path)?;
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut writer = csv::WriterBuilder::new().from_writer(encoder);
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
            // Finish the gzip stream explicitly: dropping the encoder would
            // swallow a write error and could leave a truncated member, which
            // is exactly the silently-corrupt file this export must never
            // produce.
            part.writer
                .into_inner()
                .map_err(io::Error::other)?
                .finish()?;
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
        self.close_final_part()?;
        self.finish_parts(None)
    }

    /// Finish assembly like [`Assembler::finish`], but when the output is a
    /// ZIP (more than one part), also append a `_EXPORT_REPORT.txt` entry
    /// summarizing `summary`: successful vs. failed fuel ranges, vehicles
    /// affected, and (on failure) each failed range's boundaries and
    /// timestamp. A single-CSV output never gets a report entry; a
    /// CSV-only export relies on the response's filename and
    /// `X-Export-Warnings` header instead. Called only by the production
    /// handler; existing callers of plain `finish()` are unaffected.
    pub fn finish_with_report(mut self, summary: &FuelFailureSummary) -> io::Result<Assembled> {
        self.close_final_part()?;
        self.finish_parts(Some(summary))
    }

    fn close_final_part(&mut self) -> io::Result<()> {
        if self.current.is_none() && self.part_paths.is_empty() {
            // No rows at all: still emit a header-only single CSV.
            self.open_new_part()?;
        }
        self.close_current_part()
    }

    fn finish_parts(
        &mut self,
        report_summary: Option<&FuelFailureSummary>,
    ) -> io::Result<Assembled> {
        if self.part_paths.len() == 1 {
            // A single-part export is always a plain CSV, never a ZIP, so it
            // never gets a report entry regardless of `report_summary`: a
            // CSV-only export relies on the filename and
            // `X-Export-Warnings` header instead.
            let path = self.part_paths.remove(0);
            let content_length = std::fs::metadata(&path)?.len();
            return Ok(Assembled::Csv {
                path,
                content_length,
            });
        }
        self.zip_parts(report_summary)
    }

    fn zip_parts(&mut self, report_summary: Option<&FuelFailureSummary>) -> io::Result<Assembled> {
        let zip_path = unique_temp_path(".zip");
        let report_text = report_summary.map(|s| build_report_text(s, now_unix()));
        let result = self.try_zip_parts(&zip_path, report_text.as_deref());
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

    fn try_zip_parts(
        &self,
        zip_path: &Path,
        report_text: Option<&str>,
    ) -> io::Result<(u64, usize)> {
        let file = File::create(zip_path)?;
        let mut zip = zip::ZipWriter::new(file);
        let options: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);

        let part_count = self.part_paths.len();
        for (idx, part_path) in self.part_paths.iter().enumerate() {
            let part_name = format!("part-{}.csv", idx + 1);
            zip.start_file(part_name, options)
                .map_err(|e| io::Error::other(e.to_string()))?;
            // Parts are staged gzip-compressed to survive a small /tmp, so
            // they are expanded back to plain CSV on the way into the archive;
            // the ZIP applies its own deflate. Streaming the decode keeps this
            // bounded no matter how large the part is.
            let part_file = File::open(part_path)?;
            let mut decoder = flate2::read::GzDecoder::new(part_file);
            io::copy(&mut decoder, &mut zip)?;
        }
        if let Some(text) = report_text {
            zip.start_file("_EXPORT_REPORT.txt", options)
                .map_err(|e| io::Error::other(e.to_string()))?;
            io::Write::write_all(&mut zip, text.as_bytes())?;
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

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Format Unix seconds as a UTC `YYYY-MM-DDTHH:MM:SSZ` timestamp without
/// pulling in a date/time crate. Uses Howard Hinnant's `civil_from_days`
/// algorithm, which is closed-form (no external table, no dependency).
fn unix_to_iso8601(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let time_of_day = secs.rem_euclid(86_400);
    let hour = time_of_day / 3600;
    let minute = (time_of_day % 3600) / 60;
    let second = time_of_day % 60;

    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Howard Hinnant's `civil_from_days`: days since the Unix epoch -> (year,
/// month, day) in the proleptic Gregorian calendar.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Build the `_EXPORT_REPORT.txt` contents for a ZIP export: success/failure
/// counts and, when any range failed, each failed range's boundaries,
/// affected vehicle count, and failure timestamp.
fn build_report_text(summary: &FuelFailureSummary, generated_at_unix: i64) -> String {
    let successful = summary.attempted.saturating_sub(summary.failures);
    let mut text = format!(
        "Export Report\nGenerated: {}\n\nFuel Data Fetch Results:\n\
         - Successful ranges: {successful} of {attempted}\n\
         - Failed ranges: {failures}\n\
         - Vehicles affected by failures: {vehicles_affected}\n\n",
        unix_to_iso8601(generated_at_unix),
        attempted = summary.attempted,
        failures = summary.failures,
        vehicles_affected = summary.vehicles_affected,
    );

    if summary.failed_ranges.is_empty() {
        text.push_str("All fuel data successfully fetched.\n\n");
    } else {
        text.push_str("Failed Ranges:\n");
        for range in &summary.failed_ranges {
            text.push_str(&format!(
                "- Range {} to {}: fetch failed, {} vehicles (at {})\n",
                range.lo,
                range.hi,
                range.vehicle_count,
                unix_to_iso8601(range.failed_at_unix)
            ));
        }
        text.push('\n');
    }

    text.push_str(
        "Status column in CSV: export_status (values: ok, no_fuel_data, fuel_unavailable)\n",
    );
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Read a staged part back as plain text. Parts are written gzip-compressed
    /// so a large export fits the small writable /tmp a serverless host gives.
    fn read_gzipped(path: &std::path::Path) -> String {
        let mut out = String::new();
        flate2::read::GzDecoder::new(File::open(path).unwrap())
            .read_to_string(&mut out)
            .unwrap();
        out
    }

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
                // Parts are staged gzip-compressed, so read them back through
                // the decoder the response path uses.
                let contents = read_gzipped(path);
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
                let contents = read_gzipped(path);
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

    // Criterion 8: _EXPORT_REPORT.txt in ZIP exports.

    fn zip_entry_names(path: &Path) -> Vec<String> {
        let file = File::open(path).unwrap();
        let archive = zip::ZipArchive::new(file).unwrap();
        archive.file_names().map(str::to_string).collect()
    }

    fn zip_entry_text(path: &Path, name: &str) -> String {
        let file = File::open(path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut entry = archive.by_name(name).unwrap();
        let mut text = String::new();
        entry.read_to_string(&mut text).unwrap();
        text
    }

    #[test]
    fn happy_path_zip_with_failures_lists_failed_ranges_in_report() {
        let mut assembler = Assembler::new(header());
        assembler
            .write_rows(&rows(EXCEL_MAX_DATA_ROWS + 1))
            .unwrap();
        let summary = FuelFailureSummary {
            attempted: 5,
            failures: 1,
            vehicles_affected: 47,
            failed_ranges: vec![crate::failure::FailedRange {
                lo: "0001VH".to_string(),
                hi: "5000VH".to_string(),
                vehicle_count: 47,
                failed_at_unix: 1_757_255_535, // 2025-09-07T14:32:15Z
            }],
        };
        let assembled = assembler.finish_with_report(&summary).unwrap();
        let path = match &assembled {
            Assembled::Zip { path, .. } => path.clone(),
            Assembled::Csv { .. } => panic!("expected a ZIP file"),
        };
        let names = zip_entry_names(&path);
        assert!(names.contains(&"_EXPORT_REPORT.txt".to_string()));
        let report = zip_entry_text(&path, "_EXPORT_REPORT.txt");
        assert!(report.contains("0001VH"));
        assert!(report.contains("5000VH"));
        assert!(report.contains("47 vehicles"));
        assert!(report.contains("export_status"));
        cleanup(&assembled);
    }

    #[test]
    fn edge_zip_with_zero_failures_states_success_in_report() {
        let mut assembler = Assembler::new(header());
        assembler
            .write_rows(&rows(EXCEL_MAX_DATA_ROWS + 1))
            .unwrap();
        let summary = FuelFailureSummary {
            attempted: 500,
            failures: 0,
            vehicles_affected: 0,
            failed_ranges: vec![],
        };
        let assembled = assembler.finish_with_report(&summary).unwrap();
        let path = match &assembled {
            Assembled::Zip { path, .. } => path.clone(),
            Assembled::Csv { .. } => panic!("expected a ZIP file"),
        };
        let report = zip_entry_text(&path, "_EXPORT_REPORT.txt");
        assert!(report.contains("All fuel data successfully fetched."));
        cleanup(&assembled);
    }

    #[test]
    fn failure_single_csv_export_never_gets_a_report_entry() {
        let mut assembler = Assembler::new(header());
        assembler.write_rows(&rows(3)).unwrap();
        let summary = FuelFailureSummary {
            attempted: 1,
            failures: 1,
            vehicles_affected: 3,
            failed_ranges: vec![crate::failure::FailedRange {
                lo: "AA001A".to_string(),
                hi: "AA001A".to_string(),
                vehicle_count: 3,
                failed_at_unix: 0,
            }],
        };
        let assembled = assembler.finish_with_report(&summary).unwrap();
        assert!(matches!(assembled, Assembled::Csv { .. }));
        cleanup(&assembled);
    }

    #[test]
    fn unix_to_iso8601_formats_known_timestamp() {
        // 2025-09-07T14:32:15Z, used as the report's example timestamp.
        assert_eq!(unix_to_iso8601(1_757_255_535), "2025-09-07T14:32:15Z");
    }

    #[test]
    fn civil_from_days_matches_a_reference_across_every_day_to_2100() {
        // Walk every day from 1970-01-01 to 2100-01-01, including every leap
        // year and every century rule, and compare against an independent
        // day-counting reference.
        fn is_leap(y: i64) -> bool {
            (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
        }
        let mdays = [31u32, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
        let (mut y, mut m, mut d) = (1970i64, 1u32, 1u32);
        for day in 0..47_482i64 {
            let got = civil_from_days(day);
            assert_eq!(got, (y, m, d), "mismatch at day {day}");
            let len = if m == 2 && is_leap(y) {
                29
            } else {
                mdays[(m - 1) as usize]
            };
            d += 1;
            if d > len {
                d = 1;
                m += 1;
            }
            if m > 12 {
                m = 1;
                y += 1;
            }
        }
    }

    #[test]
    fn unix_to_iso8601_formats_the_epoch() {
        assert_eq!(unix_to_iso8601(0), "1970-01-01T00:00:00Z");
    }
}
