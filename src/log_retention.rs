//! Task log retention and monthly archive maintenance.
//!
//! TinyButler stores task logs beside each task. This module keeps those
//! directories bounded by compressing completed retained months and deleting
//! files outside the calendar-month retention window.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Datelike, Local, NaiveDateTime};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use tar::{Archive, Builder};
use tokio::fs;

const RETAINED_LOG_MONTHS: i32 = 6;

/// Enforce TinyButler's task log retention policy for one task `logs/` directory.
///
/// Retention is calendar-month based: the current month plus the previous five
/// months are kept, completed retained months are archived as `YYYY-MM.tgz`,
/// and older raw logs or archives are deleted.
pub(crate) async fn maintain_task_logs(logs_dir: &Path, now: DateTime<Local>) -> Result<()> {
    fs::create_dir_all(logs_dir)
        .await
        .with_context(|| format!("failed to create {}", logs_dir.display()))?;

    let current_month = LogMonth::from_datetime(now);
    let oldest_retained_month = current_month.add_months(1 - RETAINED_LOG_MONTHS);
    let mut completed_logs_by_month = BTreeMap::<LogMonth, Vec<PathBuf>>::new();

    let mut entries = fs::read_dir(logs_dir)
        .await
        .with_context(|| format!("failed to read {}", logs_dir.display()))?;
    while let Some(entry) = entries.next_entry().await? {
        let file_type = entry.file_type().await?;
        if !file_type.is_file() {
            continue;
        }

        let path = entry.path();
        if let Some(month) = log_month_from_path(&path) {
            if month < oldest_retained_month {
                remove_file_if_present(&path).await?;
            } else if month < current_month {
                completed_logs_by_month.entry(month).or_default().push(path);
            }
        } else if let Some(month) = archive_month_from_path(&path)
            && month < oldest_retained_month
        {
            remove_file_if_present(&path).await?;
        }
    }

    for (month, log_paths) in completed_logs_by_month {
        archive_completed_month(logs_dir, month, log_paths).await?;
    }

    Ok(())
}

/// Read a task log from its raw path, or from the matching monthly archive.
pub(crate) async fn read_log_text(task_dir: &Path, relative_log_path: &str) -> Result<String> {
    let raw_path = task_dir.join(relative_log_path);
    match fs::read_to_string(&raw_path).await {
        Ok(text) => Ok(text),
        Err(err) => {
            let (archive_path, entry_name) = archived_log_location(task_dir, relative_log_path)
                .ok_or_else(|| {
                    anyhow!(
                        "failed to read {} and no matching archive exists: {err}",
                        raw_path.display()
                    )
                })?;
            let archive_display = archive_path.display().to_string();
            let entry_display = entry_name.clone();
            tokio::task::spawn_blocking(move || read_archive_entry(&archive_path, &entry_name))
                .await?
                .with_context(|| {
                    format!(
                        "failed to read archived log {} from {}",
                        entry_display, archive_display
                    )
                })
        }
    }
}

async fn archive_completed_month(
    logs_dir: &Path,
    month: LogMonth,
    mut log_paths: Vec<PathBuf>,
) -> Result<()> {
    log_paths.sort();
    let archive_path = logs_dir.join(month.archive_name());
    let temp_path = logs_dir.join(format!(
        ".{}.{}.tmp",
        month.archive_name(),
        std::process::id()
    ));

    let archive_path_for_write = archive_path.clone();
    let temp_path_for_write = temp_path.clone();
    let log_paths_for_write = log_paths.clone();
    let archive_result = tokio::task::spawn_blocking(move || {
        write_month_archive(
            &archive_path_for_write,
            &temp_path_for_write,
            &log_paths_for_write,
        )
    })
    .await?;

    if let Err(err) = archive_result {
        let _ = fs::remove_file(&temp_path).await;
        return Err(err);
    }

    for log_path in log_paths {
        remove_file_if_present(&log_path).await?;
    }

    Ok(())
}

fn write_month_archive(archive_path: &Path, temp_path: &Path, log_paths: &[PathBuf]) -> Result<()> {
    let temp_file = File::create(temp_path)
        .with_context(|| format!("failed to create {}", temp_path.display()))?;
    let encoder = GzEncoder::new(temp_file, Compression::default());
    let mut builder = Builder::new(encoder);
    let mut archived_names = BTreeSet::new();

    if archive_path.exists() {
        copy_existing_archive_entries(archive_path, &mut builder, &mut archived_names)
            .with_context(|| format!("failed to merge {}", archive_path.display()))?;
    }

    for log_path in log_paths {
        let entry_name = log_path
            .file_name()
            .context("log path has no file name")?
            .to_os_string();
        let entry_name_path = PathBuf::from(entry_name.clone());
        let entry_name_text = entry_name.to_string_lossy().to_string();
        if !archived_names.insert(entry_name_text) {
            continue;
        }
        builder
            .append_path_with_name(log_path, &entry_name_path)
            .with_context(|| format!("failed to add {} to archive", log_path.display()))?;
    }

    let encoder = builder
        .into_inner()
        .context("failed to finish tar archive")?;
    encoder.finish().context("failed to finish gzip archive")?;
    std::fs::rename(temp_path, archive_path).with_context(|| {
        format!(
            "failed to move {} to {}",
            temp_path.display(),
            archive_path.display()
        )
    })?;
    Ok(())
}

fn copy_existing_archive_entries(
    archive_path: &Path,
    builder: &mut Builder<GzEncoder<File>>,
    archived_names: &mut BTreeSet<String>,
) -> Result<()> {
    let file = File::open(archive_path)
        .with_context(|| format!("failed to open {}", archive_path.display()))?;
    let decoder = GzDecoder::new(file);
    let mut archive = Archive::new(decoder);

    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }

        let mut header = entry.header().clone();
        let entry_path = entry.path()?.into_owned();
        let Some(file_name) = entry_path.file_name() else {
            continue;
        };
        let entry_name_path = PathBuf::from(file_name.to_os_string());
        let entry_name_text = file_name.to_string_lossy().to_string();
        if !is_timestamped_log_file_name(&entry_name_text)
            || !archived_names.insert(entry_name_text)
        {
            continue;
        }

        header.set_cksum();
        builder.append_data(&mut header, &entry_name_path, &mut entry)?;
    }

    Ok(())
}

fn read_archive_entry(archive_path: &Path, entry_name: &str) -> Result<String> {
    let file = File::open(archive_path)
        .with_context(|| format!("failed to open {}", archive_path.display()))?;
    let decoder = GzDecoder::new(file);
    let mut archive = Archive::new(decoder);

    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let entry_path = entry.path()?.into_owned();
        if entry_path.file_name().and_then(|name| name.to_str()) != Some(entry_name) {
            continue;
        }

        let mut text = String::new();
        entry.read_to_string(&mut text)?;
        return Ok(text);
    }

    bail!("archived log entry not found: {entry_name}")
}

async fn remove_file_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("failed to remove {}", path.display())),
    }
}

fn archived_log_location(task_dir: &Path, relative_log_path: &str) -> Option<(PathBuf, String)> {
    let file_name = Path::new(relative_log_path).file_name()?.to_str()?;
    let month = LogMonth::from_log_file_name(file_name)?;
    Some((
        task_dir.join("logs").join(month.archive_name()),
        file_name.to_string(),
    ))
}

fn log_month_from_path(path: &Path) -> Option<LogMonth> {
    let file_name = path.file_name()?.to_str()?;
    LogMonth::from_log_file_name(file_name)
}

fn archive_month_from_path(path: &Path) -> Option<LogMonth> {
    let file_name = path.file_name()?.to_str()?;
    let stem = file_name.strip_suffix(".tgz")?;
    LogMonth::from_archive_stem(stem)
}

fn is_timestamped_log_file_name(file_name: &str) -> bool {
    LogMonth::from_log_file_name(file_name).is_some()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct LogMonth {
    year: i32,
    month: u32,
}

impl LogMonth {
    fn from_datetime(datetime: DateTime<Local>) -> Self {
        Self {
            year: datetime.year(),
            month: datetime.month(),
        }
    }

    fn from_log_file_name(file_name: &str) -> Option<Self> {
        let stem = file_name.strip_suffix(".log")?;
        let timestamp = NaiveDateTime::parse_from_str(stem, "%Y-%m-%dT%H-%M-%S").ok()?;
        Some(Self {
            year: timestamp.year(),
            month: timestamp.month(),
        })
    }

    fn from_archive_stem(stem: &str) -> Option<Self> {
        if stem.len() != "YYYY-MM".len() {
            return None;
        }
        let (year, month) = stem.split_once('-')?;
        let year = year.parse::<i32>().ok()?;
        let month = month.parse::<u32>().ok()?;
        if !(1..=12).contains(&month) {
            return None;
        }
        Some(Self { year, month })
    }

    fn add_months(self, offset: i32) -> Self {
        let zero_based_month = self.month as i32 - 1;
        let shifted = self.year * 12 + zero_based_month + offset;
        Self {
            year: shifted.div_euclid(12),
            month: (shifted.rem_euclid(12) + 1) as u32,
        }
    }

    fn archive_name(self) -> String {
        format!("{:04}-{:02}.tgz", self.year, self.month)
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Read;
    use std::path::Path;

    use chrono::{Local, TimeZone};
    use flate2::Compression;
    use flate2::read::GzDecoder;
    use flate2::write::GzEncoder;
    use tar::{Archive, Builder, Header};

    use super::{maintain_task_logs, read_log_text};

    fn fixed_now() -> chrono::DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 3, 15, 12, 0, 0)
            .single()
            .expect("fixed local time")
    }

    fn write_archive(path: &Path, entries: &[(&str, &str)]) {
        let file = File::create(path).expect("archive file");
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = Builder::new(encoder);
        for (name, text) in entries {
            let mut header = Header::new_gnu();
            header.set_size(text.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, *name, text.as_bytes())
                .expect("archive entry");
        }
        let encoder = builder.into_inner().expect("finish tar");
        encoder.finish().expect("finish gzip");
    }

    fn archive_entries(path: &Path) -> Vec<(String, String)> {
        let file = File::open(path).expect("archive file");
        let decoder = GzDecoder::new(file);
        let mut archive = Archive::new(decoder);
        let mut entries = Vec::new();
        for entry in archive.entries().expect("archive entries") {
            let mut entry = entry.expect("archive entry");
            let name = entry.path().expect("entry path").display().to_string();
            let mut text = String::new();
            entry.read_to_string(&mut text).expect("entry text");
            entries.push((name, text));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    }

    #[tokio::test]
    async fn archives_completed_retained_months_and_removes_expired_logs() {
        let temp = tempfile::tempdir().expect("temp dir");
        let logs_dir = temp.path();
        std::fs::write(logs_dir.join("2026-02-01T00-00-00.log"), "feb one\n").expect("feb log");
        std::fs::write(logs_dir.join("2026-02-02T00-00-00.log"), "feb two\n").expect("feb log");
        std::fs::write(logs_dir.join("2026-03-01T00-00-00.log"), "march\n").expect("current log");
        std::fs::write(logs_dir.join("2025-09-01T00-00-00.log"), "expired\n").expect("expired log");
        std::fs::write(logs_dir.join("notes.txt"), "leave me\n").expect("unmanaged file");
        write_archive(
            &logs_dir.join("2025-09.tgz"),
            &[("2025-09-01T00-00-00.log", "expired archive\n")],
        );

        maintain_task_logs(logs_dir, fixed_now())
            .await
            .expect("maintain logs");

        assert!(!logs_dir.join("2026-02-01T00-00-00.log").exists());
        assert!(!logs_dir.join("2026-02-02T00-00-00.log").exists());
        assert!(logs_dir.join("2026-03-01T00-00-00.log").exists());
        assert!(!logs_dir.join("2025-09-01T00-00-00.log").exists());
        assert!(!logs_dir.join("2025-09.tgz").exists());
        assert!(logs_dir.join("notes.txt").exists());

        assert_eq!(
            archive_entries(&logs_dir.join("2026-02.tgz")),
            vec![
                (
                    "2026-02-01T00-00-00.log".to_string(),
                    "feb one\n".to_string()
                ),
                (
                    "2026-02-02T00-00-00.log".to_string(),
                    "feb two\n".to_string()
                ),
            ]
        );
    }

    #[tokio::test]
    async fn reads_archived_log_text_when_raw_file_was_removed() {
        let temp = tempfile::tempdir().expect("temp dir");
        let task_dir = temp.path();
        let logs_dir = task_dir.join("logs");
        std::fs::create_dir_all(&logs_dir).expect("logs dir");
        write_archive(
            &logs_dir.join("2026-02.tgz"),
            &[("2026-02-01T00-00-00.log", "archived log\n")],
        );

        let text = read_log_text(task_dir, "logs/2026-02-01T00-00-00.log")
            .await
            .expect("read archived log");

        assert_eq!(text, "archived log\n");
    }
}
