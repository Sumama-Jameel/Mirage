//! Raw-response snapshots for protocol-drift and parse-failure healing.
//!
//! When an upstream response can no longer be parsed, the evidence needed to
//! repair the parser is the raw body. This module writes bounded snapshots
//! under `<data_dir>/drift/<provider>/` so a one-time recapture (F12 or
//! proxy) can be compared against what the gateway actually received.
//!
//! Policy (InitialPlan §4.2 "protocol drift"): snapshots are evidence for
//! human-in-loop healing; the gateway never auto-regenerates parsers from
//! them.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

const MAX_FILE_BYTES: u64 = 64 * 1024;
const MAX_FILES_PER_PROVIDER: usize = 20;

/// Snapshot sink: `<data_dir>/drift/`. No-op when no data dir is configured.
#[derive(Clone, Default)]
pub struct DriftSnapshots {
    root: Option<PathBuf>,
    /// Per-provider write counter guard so concurrent failures cannot flood
    /// the directory past the retention cap mid-write.
    lock: std::sync::Arc<Mutex<()>>,
}

impl DriftSnapshots {
    pub fn with_data_dir(data_dir: Option<PathBuf>) -> Self {
        Self {
            root: data_dir.map(|d| d.join("drift")),
            lock: std::sync::Arc::new(Mutex::new(())),
        }
    }

    /// Write one snapshot. Best-effort: logging failures must never turn a
    /// parse problem into a request failure.
    ///
    /// `kind` is a short label (`parse`, `drift`, `empty-200`, ...).
    pub fn record(&self, provider: &str, kind: &str, excerpt: &[u8]) {
        let Some(root) = self.root.clone() else { return };
        let dir = root.join(provider);
        let _guard = self.lock.lock().unwrap();

        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        self.enforce_retention(&dir);

        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default();
        let path = dir.join(format!("{ts}-{kind}.txt"));

        let Ok(mut f) = std::fs::File::create(&path) else { return };
        let _ = writeln!(
            f,
            "provider: {provider}\nkind: {kind}\ntime: {} UTC\n---",
            chrono_utc_now()
        );
        // Truncate to the cap; the head of the body is almost always enough
        // to identify the shape change.
        let end = (excerpt.len() as u64).min(MAX_FILE_BYTES) as usize;
        let _ = f.write_all(&excerpt[..end]);
    }

    fn enforce_retention(&self, dir: &std::path::Path) {
        let mut files: Vec<_> = match std::fs::read_dir(dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    let meta = e.metadata().ok()?;
                    Some((e.path(), meta.modified().ok(), meta.len()))
                })
                .collect(),
            Err(_) => return,
        };
        if files.len() < MAX_FILES_PER_PROVIDER {
            return;
        }
        files.sort_by_key(|(_, mtime, _)| *mtime);
        let excess = files.len() - MAX_FILES_PER_PROVIDER + 1;
        for (path, _, _) in files.into_iter().take(excess) {
            let _ = std::fs::remove_file(path);
        }
    }

    /// True when snapshots are enabled (diagnostics/tests).
    #[allow(dead_code)]
    pub fn enabled(&self) -> bool {
        self.root.is_some()
    }
}

fn chrono_utc_now() -> String {
    // RFC3339-ish without pulling chrono into this module's API.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    format!("{secs} epoch-s")
}

static GLOBAL_DRIFT: std::sync::OnceLock<DriftSnapshots> = std::sync::OnceLock::new();

/// Configure the process-wide snapshot sink (called once at startup).
pub fn init_global(data_dir: Option<PathBuf>) {
    let _ = GLOBAL_DRIFT.set(DriftSnapshots::with_data_dir(data_dir));
}

/// Process-wide sink. Disabled unless [`init_global`] ran with a data dir.
pub fn global() -> &'static DriftSnapshots {
    GLOBAL_DRIFT.get_or_init(|| DriftSnapshots::with_data_dir(None))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("drift-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn writes_bounded_snapshot() {
        let dir = temp_dir();
        let snaps = DriftSnapshots::with_data_dir(Some(dir.clone()));
        assert!(snaps.enabled());

        snaps.record("glm", "parse", b"data: not json at all");
        let provider_dir = dir.join("drift").join("glm");
        let entries: Vec<_> = std::fs::read_dir(&provider_dir).unwrap().collect();
        assert_eq!(entries.len(), 1);
        let content = std::fs::read_to_string(provider_dir.read_dir().unwrap().next().unwrap().unwrap().path()).unwrap();
        assert!(content.contains("provider: glm"));
        assert!(content.contains("not json"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn enforces_per_provider_retention() {
        let dir = temp_dir();
        let snaps = DriftSnapshots::with_data_dir(Some(dir.clone()));
        for i in 0..(MAX_FILES_PER_PROVIDER + 5) {
            snaps.record("kimi", "drift", format!("body {i}").as_bytes());
            // Distinct mtimes on fast filesystems are not guaranteed; age one
            // file explicitly if needed by sleeping is too slow — instead
            // rely on count enforcement only.
        }
        let provider_dir = dir.join("drift").join("kimi");
        let count = std::fs::read_dir(&provider_dir).unwrap().count();
        assert!(
            count <= MAX_FILES_PER_PROVIDER,
            "expected <= {MAX_FILES_PER_PROVIDER} files, got {count}"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn disabled_without_data_dir() {
        let snaps = DriftSnapshots::with_data_dir(None);
        assert!(!snaps.enabled());
        // Must not panic.
        snaps.record("glm", "parse", b"ignored");
    }

    #[test]
    fn truncates_to_cap() {
        let dir = temp_dir();
        let snaps = DriftSnapshots::with_data_dir(Some(dir.clone()));
        let big = vec![b'x'; (MAX_FILE_BYTES as usize) * 2];
        snaps.record("grok", "empty-200", &big);
        let f = dir.join("drift").join("grok").read_dir().unwrap().next().unwrap().unwrap().path();
        let len = std::fs::metadata(&f).unwrap().len();
        assert!(len <= MAX_FILE_BYTES + 128, "snapshot exceeded cap: {len}");
        std::fs::remove_dir_all(dir).ok();
    }
}
