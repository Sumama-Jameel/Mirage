use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use crate::error::GatewayError;

/// A raw cookie row as stored by Firefox.
#[derive(Debug, Clone)]
pub struct FirefoxCookie {
    pub host: String,
    pub name: String,
    pub value: String,
    pub path: String,
    pub expiry: Option<i64>,
    pub is_secure: bool,
    pub is_http_only: bool,
    pub same_site: i64,
    pub encrypted_value: Option<Vec<u8>>,
}

/// Read cookies from a Firefox `cookies.sqlite` database.
///
/// Opens the database read-only and returns every row. Decryption of
/// `encryptedValue` is left to the caller so that NSS initialization can be
/// performed once per profile rather than once per cookie.
pub fn read_cookies(db_path: &Path) -> Result<Vec<FirefoxCookie>, GatewayError> {
    if !db_path.exists() {
        return Err(GatewayError::Internal(format!(
            "cookies.sqlite not found at {}",
            db_path.display()
        )));
    }

    // The snapshot copies cookies.sqlite together with its WAL/SHM sidecars.
    // Firefox keeps the DB in WAL mode and may not have checkpointed recent
    // cookie writes (session rotations, refreshed tokens) into the main file.
    // A read-only open cannot replay the WAL when the -shm index is absent,
    // which leaves the snapshot with stale cookies. Open the copy read-write
    // (it is our private snapshot) and checkpoint so the WAL is folded into
    // the main DB before reading.
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_WRITE).map_err(
        |e| GatewayError::Internal(format!("failed to open cookies.sqlite: {e}")),
    )?;
    let _ = conn.pragma_update(None, "wal_checkpoint", "TRUNCATE");

    // Modern Firefox (ESR 128+) stores cookies as plaintext in the `value`
    // column and no longer has `encryptedValue`. Older Firefox stores plaintext
    // in `value` and the encrypted payload in `encryptedValue`. Try the legacy
    // schema first, then fall back to the modern schema.
    let legacy_sql = "SELECT host, name, value, path, expiry, isSecure, isHttpOnly, sameSite, encryptedValue \
                      FROM moz_cookies";
    let modern_sql = "SELECT host, name, value, path, expiry, isSecure, isHttpOnly, sameSite \
                      FROM moz_cookies";

    let (mut stmt, has_encrypted_value) = match conn.prepare(legacy_sql) {
        Ok(stmt) => (stmt, true),
        Err(_) => conn
            .prepare(modern_sql)
            .map(|stmt| (stmt, false))
            .map_err(|e| GatewayError::Internal(format!("failed to prepare cookie query: {e}")))?,
    };

    let rows = stmt
        .query_map([], |row| {
            Ok(FirefoxCookie {
                host: row.get(0)?,
                name: row.get(1)?,
                value: row.get(2)?,
                path: row.get(3)?,
                expiry: row.get(4)?,
                is_secure: row.get::<_, i64>(5)? != 0,
                is_http_only: row.get::<_, i64>(6)? != 0,
                same_site: row.get::<_, i64>(7)?,
                encrypted_value: if has_encrypted_value {
                    row.get::<_, Option<Vec<u8>>>(8)?
                } else {
                    None
                },
            })
        })
        .map_err(|e| GatewayError::Internal(format!("failed to query cookies: {e}")))?;

    let mut cookies = Vec::new();
    for row in rows {
        cookies.push(row.map_err(|e| {
            GatewayError::Internal(format!("failed to read cookie row: {e}"))
        })?);
    }

    Ok(cookies)
}
