//! Upload of `WebView2` crash dumps left by a previous run.
//!
//! A crash in a `WebView2` child process (renderer, GPU, utility) or its browser
//! process happens outside Turnout's own process, so neither the native minidump
//! reporter nor `@sentry/browser` sees it, and Tauri exposes no crash hook on
//! Windows. The `WebView2` runtime handles those crashes with its own Crashpad
//! handler, which writes a minidump per crash into the environment's
//! `FailureReportFolderPath`: `<user data folder>\EBWebView\Crashpad\reports`.
//! Tauri sets the user data folder to the app's local data dir, so the folder is
//! per-user and readable without elevation.
//!
//! On launch (off the main thread) Turnout lists that folder, picks the dumps it
//! has not uploaded yet within the count, size and age caps below, sends each to
//! Sentry as a minidump attachment, and records the uploaded file names in a
//! small ledger next to the folder so they are not sent again.
//!
//! The selection and ledger logic is platform independent and unit-tested on
//! every platform. Only the wiring to the app's paths and to Sentry is
//! Windows-only.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// File extension Crashpad gives its minidumps.
const DUMP_EXTENSION: &str = "dmp";

/// Most dumps uploaded per launch, newest first. A crash loop leaves many dumps
/// of the same failure, so a few per launch are enough to diagnose it.
const MAX_DUMPS_PER_LAUNCH: usize = 3;

/// Largest single dump uploaded. `WebView2` minidumps are usually well under a
/// few MiB; anything bigger is skipped rather than uploaded over a slow link.
const MAX_DUMP_BYTES: u64 = 20 * 1024 * 1024;

/// Total dump bytes uploaded per launch.
const MAX_BYTES_PER_LAUNCH: u64 = 40 * 1024 * 1024;

/// A dump modified more recently than this may still be being written by the
/// Crashpad handler, so it is left for the next launch.
const MIN_DUMP_AGE: Duration = Duration::from_secs(10);

/// Dumps older than this are ignored: they predate any recent release and Sentry
/// drops events with timestamps this far in the past anyway.
const MAX_DUMP_AGE: Duration = Duration::from_hours(14 * 24);

/// One minidump file found in the reports folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumpFile {
    pub path: PathBuf,
    /// The file name, which Crashpad makes unique (a report UUID). The ledger
    /// keys on it.
    pub name: String,
    pub size: u64,
    pub modified: SystemTime,
}

/// Wrap an I/O error with the path it concerns.
fn with_path(err: &io::Error, action: &str, path: &Path) -> io::Error {
    io::Error::new(err.kind(), format!("{action} {}: {err}", path.display()))
}

/// Whether `path` names a Crashpad minidump (`*.dmp`, any case).
fn is_dump_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case(DUMP_EXTENSION))
}

/// Read one directory entry as a [`DumpFile`], or `None` when it is not a
/// regular `*.dmp` file or its metadata is unreadable.
fn dump_file(entry: &std::fs::DirEntry) -> Option<DumpFile> {
    let path = entry.path();
    if !is_dump_path(&path) {
        return None;
    }
    let meta = entry.metadata().ok().filter(std::fs::Metadata::is_file)?;
    Some(DumpFile {
        name: entry.file_name().to_str()?.to_string(),
        size: meta.len(),
        modified: meta.modified().ok()?,
        path,
    })
}

/// List the minidumps directly inside `dir`. A missing folder (`WebView2` has
/// never crashed on this machine) is an empty list, not an error.
pub fn scan_dir(dir: &Path) -> io::Result<Vec<DumpFile>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(with_path(&err, "list crash dumps in", dir)),
    };
    Ok(entries
        .flatten()
        .filter_map(|entry| dump_file(&entry))
        .collect())
}

/// Whether a dump's age (at `now`) is inside the upload window: settled, so the
/// Crashpad handler has finished writing it, and not stale. A modification time
/// in the future counts as unsettled.
fn age_in_window(modified: SystemTime, now: SystemTime) -> bool {
    now.duration_since(modified)
        .is_ok_and(|age| (MIN_DUMP_AGE..=MAX_DUMP_AGE).contains(&age))
}

/// Pick which dumps to upload this launch: not already uploaded, non-empty,
/// within [`MAX_DUMP_BYTES`] and the age window, newest first, capped at
/// [`MAX_DUMPS_PER_LAUNCH`] files and [`MAX_BYTES_PER_LAUNCH`] in total.
pub fn select(
    mut dumps: Vec<DumpFile>,
    uploaded: &HashSet<String>,
    now: SystemTime,
) -> Vec<DumpFile> {
    dumps.retain(|d| {
        !uploaded.contains(&d.name)
            && (1..=MAX_DUMP_BYTES).contains(&d.size)
            && age_in_window(d.modified, now)
    });
    dumps.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.name.cmp(&b.name))
    });

    let mut total = 0u64;
    dumps
        .into_iter()
        .filter(|d| {
            let fits = total + d.size <= MAX_BYTES_PER_LAUNCH;
            if fits {
                total += d.size;
            }
            fits
        })
        .take(MAX_DUMPS_PER_LAUNCH)
        .collect()
}

/// The names of dumps already uploaded, persisted as JSON between launches.
#[derive(Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Ledger {
    uploaded: HashSet<String>,
}

impl Ledger {
    /// Load the ledger at `path`. Missing means nothing uploaded yet. A corrupt
    /// ledger is logged and treated as empty: the per-launch caps bound the
    /// re-upload that follows.
    pub fn load(path: &Path) -> Self {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Self::default(),
            Err(err) => {
                tracing::warn!("{}", with_path(&err, "read crash dump ledger", path));
                return Self::default();
            }
        };
        serde_json::from_slice(&bytes).unwrap_or_else(|err| {
            tracing::warn!(
                "crash dump ledger {} is corrupt, starting fresh: {err}",
                path.display()
            );
            Self::default()
        })
    }

    pub fn uploaded(&self) -> &HashSet<String> {
        &self.uploaded
    }

    /// Record `names` as uploaded, then drop entries whose dump no longer exists
    /// in `present` (Crashpad pruned it), so the ledger never outgrows the folder.
    pub fn record(&mut self, names: impl IntoIterator<Item = String>, present: &[DumpFile]) {
        self.uploaded.extend(names);
        let present: HashSet<&str> = present.iter().map(|d| d.name.as_str()).collect();
        self.uploaded.retain(|name| present.contains(name.as_str()));
    }

    /// Write the ledger to `path` via a temporary file and a rename, so a crash
    /// mid-write leaves the previous ledger intact.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let json = serde_json::to_vec(self).map_err(io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)
            .map_err(|err| with_path(&err, "write crash dump ledger", &tmp))?;
        std::fs::rename(&tmp, path)
            .map_err(|err| with_path(&err, "replace crash dump ledger", path))
    }
}

/// Folder inside the app's local data dir (the `WebView2` user data folder Tauri
/// configures) where the `WebView2` runtime's Crashpad handler writes minidumps.
#[cfg(windows)]
const REPORTS_SUBDIR: [&str; 3] = ["EBWebView", "Crashpad", "reports"];

/// Ledger file name, stored in the app's local data dir.
#[cfg(windows)]
const LEDGER_FILE: &str = "webview_crash_dumps.json";

/// How long the upload thread waits for Sentry to send the dumps before giving
/// up. On a timeout the ledger is not updated, so the dumps are retried next
/// launch.
#[cfg(windows)]
const FLUSH_TIMEOUT: Duration = Duration::from_mins(1);

/// Start uploading `WebView2` crash dumps from earlier runs on a background
/// thread. A no-op when crash reporting is off: [`crate::crash_reporting::init`]
/// binds no Sentry client when the user opted out or no DSN is baked in.
#[cfg(windows)]
pub fn spawn_upload(app: &tauri::AppHandle) {
    use tauri::Manager;

    let Some(client) = sentry::Hub::current().client().filter(|c| c.is_enabled()) else {
        return;
    };
    let data_dir = match app.path().app_local_data_dir() {
        Ok(dir) => dir,
        Err(err) => {
            tracing::warn!("WebView2 crash dump upload skipped: no local data dir: {err}");
            return;
        }
    };
    let spawned = std::thread::Builder::new()
        .name("webview-dump-upload".into())
        .spawn(move || upload_pending(&data_dir, &client));
    if let Err(err) = spawned {
        tracing::warn!("WebView2 crash dump upload skipped: thread spawn failed: {err}");
    }
}

/// Upload the selected dumps from `data_dir`'s reports folder and record them.
#[cfg(windows)]
fn upload_pending(data_dir: &Path, client: &sentry::Client) {
    let reports = REPORTS_SUBDIR
        .iter()
        .fold(data_dir.to_path_buf(), |dir, part| dir.join(part));
    let ledger_path = data_dir.join(LEDGER_FILE);

    let present = match scan_dir(&reports) {
        Ok(present) => present,
        Err(err) => {
            tracing::warn!("WebView2 crash dump upload skipped: {err}");
            return;
        }
    };
    let mut ledger = Ledger::load(&ledger_path);
    let selected = select(present.clone(), ledger.uploaded(), SystemTime::now());
    if selected.is_empty() {
        return;
    }

    let sent: Vec<String> = selected
        .iter()
        .filter_map(|dump| match capture_dump(dump) {
            Ok(()) => Some(dump.name.clone()),
            Err(err) => {
                tracing::warn!("WebView2 crash dump not uploaded: {err}");
                None
            }
        })
        .collect();
    if !client.flush(Some(FLUSH_TIMEOUT)) {
        tracing::warn!("WebView2 crash dump upload timed out; retrying next launch");
        return;
    }
    tracing::info!("uploaded {} WebView2 crash dump(s)", sent.len());

    ledger.record(sent, &present);
    if let Err(err) = ledger.save(&ledger_path) {
        tracing::warn!("{err}");
    }
}

/// Send one dump to Sentry as a fatal event carrying the minidump attachment,
/// the same shape `sentry-rust-minidump` uses for main-process crashes. The
/// event is stamped with the dump's modification time (when the crash happened)
/// and inherits the scope's GPU, system and release tags.
#[cfg(windows)]
fn capture_dump(dump: &DumpFile) -> io::Result<()> {
    use sentry::protocol::{Attachment, AttachmentType, Event, Value};

    let buffer =
        std::fs::read(&dump.path).map_err(|err| with_path(&err, "read crash dump", &dump.path))?;
    sentry::with_scope(
        |scope| {
            scope.set_tag("crash_source", "webview2");
            scope.set_extra("dump_file", Value::String(dump.name.clone()));
            scope.add_attachment(Attachment {
                buffer,
                filename: dump.name.clone(),
                ty: Some(AttachmentType::Minidump),
                ..Default::default()
            });
        },
        || {
            sentry::capture_event(Event {
                level: sentry::Level::Fatal,
                message: Some("WebView2 process crashed in a previous run".to_string()),
                timestamp: dump.modified,
                ..Default::default()
            })
        },
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        scan_dir, select, DumpFile, Ledger, MAX_BYTES_PER_LAUNCH, MAX_DUMPS_PER_LAUNCH,
        MAX_DUMP_AGE, MAX_DUMP_BYTES, MIN_DUMP_AGE,
    };
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    /// A typical renderer minidump size.
    const DUMP_SIZE: u64 = 512 * 1024;

    /// A fresh, empty directory unique to `test`, removed when dropped.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(test: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "turnout-webview-dumps-{test}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000)
    }

    fn dump(name: &str, size: u64, age: Duration) -> DumpFile {
        DumpFile {
            path: PathBuf::from(name),
            name: name.to_string(),
            size,
            modified: now() - age,
        }
    }

    fn names(dumps: &[DumpFile]) -> Vec<&str> {
        dumps.iter().map(|d| d.name.as_str()).collect()
    }

    const HOUR: Duration = Duration::from_hours(1);

    #[test]
    fn selects_newest_first_and_caps_count() {
        let dumps = (0..5u32)
            .map(|i| dump(&format!("{i}.dmp"), DUMP_SIZE, HOUR * (i + 1)))
            .collect();
        let picked = select(dumps, &HashSet::new(), now());
        assert_eq!(picked.len(), MAX_DUMPS_PER_LAUNCH);
        assert_eq!(names(&picked), ["0.dmp", "1.dmp", "2.dmp"]);
    }

    #[test]
    fn skips_already_uploaded() {
        let dumps = vec![
            dump("a.dmp", DUMP_SIZE, HOUR),
            dump("b.dmp", DUMP_SIZE, HOUR * 2),
        ];
        let uploaded = HashSet::from(["a.dmp".to_string()]);
        assert_eq!(names(&select(dumps, &uploaded, now())), ["b.dmp"]);
    }

    #[test]
    fn skips_dumps_still_being_written_or_stale() {
        let dumps = vec![
            dump("fresh.dmp", DUMP_SIZE, MIN_DUMP_AGE / 2),
            dump("settled.dmp", DUMP_SIZE, MIN_DUMP_AGE),
            dump("stale.dmp", DUMP_SIZE, MAX_DUMP_AGE + HOUR),
            DumpFile {
                modified: now() + HOUR,
                ..dump("future.dmp", DUMP_SIZE, HOUR)
            },
        ];
        assert_eq!(
            names(&select(dumps, &HashSet::new(), now())),
            ["settled.dmp"]
        );
    }

    #[test]
    fn skips_empty_and_oversized_dumps() {
        let dumps = vec![
            dump("empty.dmp", 0, HOUR),
            dump("huge.dmp", MAX_DUMP_BYTES + 1, HOUR),
            dump("max.dmp", MAX_DUMP_BYTES, HOUR),
        ];
        assert_eq!(names(&select(dumps, &HashSet::new(), now())), ["max.dmp"]);
    }

    #[test]
    fn caps_total_bytes_per_launch() {
        // a and b leave exactly DUMP_SIZE of the byte budget: the max-size c does
        // not fit and is skipped, while the smaller, older d fills it.
        let first = MAX_DUMP_BYTES.min(MAX_BYTES_PER_LAUNCH - 2 * DUMP_SIZE);
        let second = MAX_BYTES_PER_LAUNCH - first - DUMP_SIZE;
        let dumps = vec![
            dump("a.dmp", first, HOUR),
            dump("b.dmp", second, HOUR * 2),
            dump("c.dmp", MAX_DUMP_BYTES, HOUR * 3),
            dump("d.dmp", DUMP_SIZE, HOUR * 4),
        ];
        let picked = select(dumps, &HashSet::new(), now());
        assert_eq!(names(&picked), ["a.dmp", "b.dmp", "d.dmp"]);
        assert_eq!(
            picked.iter().map(|d| d.size).sum::<u64>(),
            MAX_BYTES_PER_LAUNCH
        );
    }

    #[test]
    fn scan_lists_only_dump_files() {
        let dir = TempDir::new("scan");
        std::fs::write(dir.path().join("3f2a.dmp"), b"MDMP").expect("write dump");
        std::fs::write(dir.path().join("UPPER.DMP"), b"MDMP").expect("write dump");
        std::fs::write(dir.path().join("settings.dat"), b"x").expect("write settings");
        std::fs::create_dir(dir.path().join("nested.dmp")).expect("create dir");

        let mut found = scan_dir(dir.path()).expect("scan");
        found.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(names(&found), ["3f2a.dmp", "UPPER.DMP"]);
        assert!(found.iter().all(|d| d.size == 4));
    }

    #[test]
    fn scan_of_missing_dir_is_empty() {
        let dir = TempDir::new("missing");
        assert!(scan_dir(&dir.path().join("Crashpad"))
            .expect("scan")
            .is_empty());
    }

    #[test]
    fn ledger_round_trips_and_prunes_vanished_dumps() {
        let dir = TempDir::new("ledger");
        let path = dir.path().join("ledger.json");
        let present = vec![
            dump("a.dmp", DUMP_SIZE, HOUR),
            dump("b.dmp", DUMP_SIZE, HOUR),
        ];

        let mut ledger = Ledger::load(&path);
        assert!(ledger.uploaded().is_empty());
        ledger.record(["a.dmp".to_string(), "b.dmp".to_string()], &present);
        ledger.save(&path).expect("save");
        assert_eq!(Ledger::load(&path), ledger);

        // Crashpad pruned b.dmp; recording c.dmp drops b.dmp from the ledger.
        let present = vec![
            dump("a.dmp", DUMP_SIZE, HOUR),
            dump("c.dmp", DUMP_SIZE, HOUR),
        ];
        ledger.record(["c.dmp".to_string()], &present);
        let expected: HashSet<String> = ["a.dmp", "c.dmp"].map(String::from).into();
        assert_eq!(ledger.uploaded(), &expected);
    }

    #[test]
    fn corrupt_ledger_loads_empty() {
        let dir = TempDir::new("corrupt");
        let path = dir.path().join("ledger.json");
        std::fs::write(&path, b"{not json").expect("write");
        assert!(Ledger::load(&path).uploaded().is_empty());
    }
}
