//! Read-only snapshotting of browser profile files.
//!
//! Browsers keep SQLite databases open with WAL locks. Copying the files to a
//! temporary directory before reading avoids lock contention and leaves the
//! live browser profile untouched.

use std::path::Path;

use tempfile::TempDir;

use crate::error::GatewayError;

/// Snapshot the files needed from a live Firefox profile so the gateway can
/// read cookies and localStorage without contending with Firefox's WAL locks.
///
/// The returned `TempDir` owns the snapshot; dropping it deletes the copy.
pub fn snapshot_firefox_profile(src: &Path) -> Result<TempDir, GatewayError> {
    let snapshot = tempfile::tempdir().map_err(|e| {
        GatewayError::Internal(format!(
            "failed to create firefox profile snapshot tempdir: {e}"
        ))
    })?;
    let dst = snapshot.path();

    // Cookies database + NSS key stores (with WAL/SHM sidecars).
    for file in [
        "cookies.sqlite",
        "cookies.sqlite-wal",
        "cookies.sqlite-shm",
        "key4.db",
        "key4.db-wal",
        "key4.db-shm",
        "cert9.db",
        "cert9.db-wal",
        "cert9.db-shm",
    ] {
        try_copy(src, dst, &[file])?;
    }

    // DeepSeek localStorage database.
    try_copy(
        src,
        dst,
        &[
            "storage",
            "default",
            "https+++chat.deepseek.com",
            "ls",
            "data.sqlite",
        ],
    )?;
    try_copy(
        src,
        dst,
        &[
            "storage",
            "default",
            "https+++chat.deepseek.com",
            "ls",
            "data.sqlite-wal",
        ],
    )?;
    try_copy(
        src,
        dst,
        &[
            "storage",
            "default",
            "https+++chat.deepseek.com",
            "ls",
            "data.sqlite-shm",
        ],
    )?;

    // Additional provider localStorage databases (Kimi, GLM, Claude, Gemini, Qwen, Minimax).
    let provider_storage_dirs = &[
        "https+++www.kimi.com",
        "https+++kimi.com",
        "https+++kimi.moonshot.cn",
        "https+++chatglm.cn",
        "https+++chat.z.ai",
        "https+++claude.ai",
        "https+++chat.qwen.ai",
        "https+++agent.minimax.io",
    ];
    for dir in provider_storage_dirs {
        let parts = &["storage", "default", dir, "ls", "data.sqlite"];
        try_copy(src, dst, parts)?;
        let wal_parts = &["storage", "default", dir, "ls", "data.sqlite-wal"];
        try_copy(src, dst, wal_parts)?;
        let shm_parts = &["storage", "default", dir, "ls", "data.sqlite-shm"];
        try_copy(src, dst, shm_parts)?;
    }

    Ok(snapshot)
}

fn try_copy(
    src: &Path,
    dst: &Path,
    parts: &[&str],
) -> Result<(), GatewayError> {
    let src_file = parts.iter().fold(src.to_path_buf(), |p, part| p.join(part));
    if !src_file.exists() {
        let joined = parts.join("/");
        if joined.contains("minimax") {
            tracing::warn!(path = %src_file.display(), "try_copy: source not found for minimax");
        }
        return Ok(());
    }
    let dst_file = parts.iter().fold(dst.to_path_buf(), |p, part| p.join(part));
    if let Some(parent) = dst_file.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            GatewayError::Internal(format!(
                "failed to create snapshot dir {}: {e}",
                parent.display()
            ))
        })?;
    }
    std::fs::copy(&src_file, &dst_file).map_err(|e| {
        GatewayError::Internal(format!(
            "failed to copy {} to snapshot: {e}",
            src_file.display()
        ))
    })?;
    let joined: String = parts.iter().map(|p| p.to_string()).collect::<Vec<_>>().join("/");
    if joined.contains("minimax") || joined.contains("agent.minimax") {
        tracing::info!(src = %src_file.display(), dst = %dst_file.display(), "try_copy: copied minimax file");
    }
    Ok(())
}
