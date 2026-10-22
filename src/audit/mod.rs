//! The audit log: local, append-only JSONL, one file per UTC day
//! (`audit-2026-10-22.jsonl`).  Every tool call, proposal, approval,
//! rejection and write lands here with its actor.
//!
//! Files are only ever opened for append; nothing in this module rewrites or
//! deletes a line.  Export is a plain concatenation, ready for a SIEM.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

const PREFIX: &str = "audit-";
const SUFFIX: &str = ".jsonl";

/// One line of the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub ts: DateTime<Utc>,
    /// `jt@example.com`, `agent`, `watch`, `system`, or an MCP client name.
    pub actor: String,
    /// Tool name, or an event (`propose #12`, `approve #12`, `enable writes 60m`).
    pub action: String,
    pub target: String,
    pub result: String,
    /// Writes and proposals: true.  Lets the audit page filter cheaply.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub change: bool,
}

impl AuditEntry {
    pub fn now(actor: impl Into<String>, action: impl Into<String>, target: impl Into<String>, result: impl Into<String>) -> Self {
        Self {
            ts: Utc::now(),
            actor: actor.into(),
            action: action.into(),
            target: target.into(),
            result: result.into(),
            change: false,
        }
    }

    /// Marks a proposal / approval / write line.
    pub fn change(mut self) -> Self {
        self.change = true;
        self
    }
}

/// Which lines [`AuditLog::read`] returns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    /// Only proposals, approvals and writes.
    pub changes_only: bool,
    /// Case-insensitive substring over actor / action / target.
    pub text: Option<String>,
}

impl Filter {
    fn keeps(&self, e: &AuditEntry) -> bool {
        if self.changes_only && !e.change {
            return false;
        }
        match &self.text {
            Some(t) => {
                let t = t.to_lowercase();
                [&e.actor, &e.action, &e.target].iter().any(|f| f.to_lowercase().contains(&t))
            }
            None => true,
        }
    }
}

struct Current {
    day: NaiveDate,
    file: File,
}

/// Append-only writer and reader over a directory of daily files.
pub struct AuditLog {
    dir: PathBuf,
    current: Mutex<Option<Current>>,
}

impl AuditLog {
    /// Opens (creating) the directory.
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        Ok(Self { dir, current: Mutex::new(None) })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path_for(&self, day: NaiveDate) -> PathBuf {
        self.dir.join(format!("{PREFIX}{day}{SUFFIX}"))
    }

    /// Appends one line and flushes it.
    pub fn append(&self, entry: &AuditEntry) -> io::Result<()> {
        let mut line = serde_json::to_vec(entry).map_err(io::Error::other)?;
        line.push(b'\n');
        let day = entry.ts.date_naive();
        let mut current = self.current.lock().map_err(|_| io::Error::other("audit lock poisoned"))?;
        if current.as_ref().is_none_or(|c| c.day != day) {
            *current = Some(Current { day, file: open_append(&self.path_for(day))? });
        }
        let file = &mut current.as_mut().expect("just opened").file;
        // One write_all per line: O_APPEND keeps concurrent lines whole.
        file.write_all(&line)?;
        file.flush()
    }

    /// Daily files, newest first.
    fn files(&self) -> io::Result<Vec<PathBuf>> {
        let mut files: Vec<PathBuf> = fs::read_dir(&self.dir)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(PREFIX) && n.ends_with(SUFFIX))
            })
            .collect();
        files.sort_unstable_by(|a, b| b.cmp(a));
        Ok(files)
    }

    /// Up to `limit` matching lines, newest first.  Malformed lines (a crash
    /// mid-write) are skipped.
    pub fn read(&self, filter: &Filter, limit: usize) -> io::Result<Vec<AuditEntry>> {
        let mut out = Vec::new();
        for path in self.files()? {
            let mut day: Vec<AuditEntry> = BufReader::new(File::open(path)?)
                .lines()
                .map_while(Result::ok)
                .filter_map(|l| serde_json::from_str(&l).ok())
                .filter(|e| filter.keeps(e))
                .collect();
            day.reverse();
            let take = limit - out.len();
            out.extend(day.into_iter().take(take));
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    /// Concatenates every daily file, oldest first, into `to`.  Returns the
    /// number of lines written.
    pub fn export(&self, to: &Path) -> io::Result<usize> {
        let mut files = self.files()?;
        files.reverse();
        let mut out = io::BufWriter::new(File::create(to)?);
        let mut lines = 0;
        for path in files {
            for line in BufReader::new(File::open(path)?).lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                out.write_all(line.as_bytes())?;
                out.write_all(b"\n")?;
                lines += 1;
            }
        }
        out.flush()?;
        Ok(lines)
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(day: u32, hour: u32, action: &str) -> AuditEntry {
        AuditEntry { ts: Utc.with_ymd_and_hms(2026, 10, day, hour, 0, 0).unwrap(), ..AuditEntry::now("agent", action, "t", "ok") }
    }

    #[test]
    fn appends_reads_newest_first_across_days() {
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::open(dir.path()).unwrap();
        for e in [at(21, 9, "a"), at(21, 10, "b"), at(22, 8, "c")] {
            log.append(&e).unwrap();
        }
        let actions: Vec<_> = log.read(&Filter::default(), 10).unwrap().into_iter().map(|e| e.action).collect();
        assert_eq!(actions, ["c", "b", "a"]);
        assert_eq!(log.read(&Filter::default(), 2).unwrap().len(), 2);
        assert!(dir.path().join("audit-2026-10-21.jsonl").exists());
    }

    #[test]
    fn filters_changes_and_text() {
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::open(dir.path()).unwrap();
        log.append(&at(22, 1, "get_events")).unwrap();
        log.append(&at(22, 2, "propose #12").change()).unwrap();
        let changes = log.read(&Filter { changes_only: true, text: None }, 10).unwrap();
        assert_eq!(changes.len(), 1);
        assert!(changes[0].change);
        let text = log.read(&Filter { changes_only: false, text: Some("EVENTS".into()) }, 10).unwrap();
        assert_eq!(text[0].action, "get_events");
    }

    #[test]
    fn skips_torn_lines_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit-2026-10-22.jsonl");
        fs::write(&path, "{\"ts\":\"broken\n").unwrap();
        let log = AuditLog::open(dir.path()).unwrap();
        log.append(&at(22, 3, "ok")).unwrap();
        // A second handle (next launch) appends to the same file.
        AuditLog::open(dir.path()).unwrap().append(&at(22, 4, "later")).unwrap();
        let rows = log.read(&Filter::default(), 10).unwrap();
        assert_eq!(rows.iter().map(|e| e.action.as_str()).collect::<Vec<_>>(), ["later", "ok"]);
    }

    #[test]
    fn export_concatenates_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::open(dir.path().join("audit")).unwrap();
        log.append(&at(22, 1, "second")).unwrap();
        log.append(&at(20, 1, "first")).unwrap();
        let out = dir.path().join("export.jsonl");
        assert_eq!(log.export(&out).unwrap(), 2);
        let text = fs::read_to_string(out).unwrap();
        let first: AuditEntry = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(first.action, "first");
    }

    #[cfg(unix)]
    #[test]
    fn files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::open(dir.path()).unwrap();
        log.append(&at(22, 1, "x")).unwrap();
        let mode = fs::metadata(dir.path().join("audit-2026-10-22.jsonl")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
