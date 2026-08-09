use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};
use std::path::Path;

use libloading::{Library, Symbol};

use crate::error::GatewayError;

const PR_FALSE: c_int = 0;

/// In-memory representation of NSS `SECItem`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct SecItem {
    type_: u32,
    data: *mut u8,
    len: u32,
}

/// Minimal NSS/NSPR loader and decryptor for Firefox cookies.
///
/// This struct dynamically loads `libnss3.so` and `libnspr4.so`, initializes
/// NSS against the given Firefox profile, and decrypts cookie values via
/// `PK11SDR_Decrypt`. The libraries are loaded once and released when the
/// context is dropped.
pub struct NssContext {
    #[allow(dead_code)]
    nss: &'static Library,
    #[allow(dead_code)]
    nspr: &'static Library,
    nss_init: Symbol<'static, unsafe extern "C" fn(configdir: *const c_char) -> c_int>,
    nss_shutdown: Symbol<'static, unsafe extern "C" fn() -> c_int>,
    pk11_sdr_decrypt: Symbol<
        'static,
        unsafe extern "C" fn(data: *const SecItem, result: *mut SecItem, cx: *mut c_void) -> c_int,
    >,
    secitem_free_item: Symbol<'static, unsafe extern "C" fn(item: *mut SecItem, freeit: c_int)>,
    initialized: bool,
}

impl NssContext {
    /// Load NSS libraries from the system search path.
    pub fn new() -> Result<Self, GatewayError> {
        // SAFETY: libloading requires `unsafe`. We only load standard system
        // libraries and extract function pointers with correct signatures.
        let nss = unsafe { Library::new("libnss3.so") }.map_err(|e| {
            GatewayError::Internal(format!(
                "failed to load libnss3.so (is Firefox/NSS installed?): {e}"
            ))
        })?;
        let nspr = unsafe { Library::new("libnspr4.so") }.map_err(|e| {
            GatewayError::Internal(format!(
                "failed to load libnspr4.so (is Firefox/NSS installed?): {e}"
            ))
        })?;

        // Leak the libraries to give symbols 'static lifetime. The libraries
        // stay loaded for the process lifetime, which matches NSS semantics.
        let nss_ref: &'static Library = Box::leak(Box::new(nss));
        let _nspr_ref: &'static Library = Box::leak(Box::new(nspr));

        let nss_init = unsafe { nss_ref.get(b"NSS_Init") }.map_err(|e| {
            GatewayError::Internal(format!("NSS_Init symbol not found: {e}"))
        })?;
        let nss_shutdown = unsafe { nss_ref.get(b"NSS_Shutdown") }.map_err(|e| {
            GatewayError::Internal(format!("NSS_Shutdown symbol not found: {e}"))
        })?;
        let pk11_sdr_decrypt = unsafe { nss_ref.get(b"PK11SDR_Decrypt") }.map_err(|e| {
            GatewayError::Internal(format!("PK11SDR_Decrypt symbol not found: {e}"))
        })?;
        let secitem_free_item = unsafe { nss_ref.get(b"SECITEM_FreeItem") }.map_err(|e| {
            GatewayError::Internal(format!("SECITEM_FreeItem symbol not found: {e}"))
        })?;

        Ok(Self {
            nss: nss_ref,
            nspr: _nspr_ref,
            nss_init,
            nss_shutdown,
            pk11_sdr_decrypt,
            secitem_free_item,
            initialized: false,
        })
    }

    /// Initialize NSS against a Firefox profile directory.
    ///
    /// The profile directory is prefixed with `sql:` because modern Firefox
    /// stores certificates in SQLite-backed cert9.db.
    pub fn init(&mut self, profile_dir: &Path) -> Result<(), GatewayError> {
        let config = format!("sql:{}", profile_dir.display());
        let config_c = CString::new(config).map_err(|e| {
            GatewayError::Internal(format!("invalid NSS config path: {e}"))
        })?;

        let status = unsafe { (self.nss_init)(config_c.as_ptr()) };
        if status != 0 {
            return Err(GatewayError::Internal(
                "NSS_Init failed (wrong profile path or Firefox is running with a master password?)".to_string(),
            ));
        }

        self.initialized = true;
        Ok(())
    }

    /// Decrypt a single Firefox `encryptedValue` blob.
    pub fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>, GatewayError> {
        if !self.initialized {
            return Err(GatewayError::Internal(
                "NSS context not initialized".to_string(),
            ));
        }

        let input = SecItem {
            type_: 0,
            data: ciphertext.as_ptr() as *mut u8,
            len: ciphertext.len() as u32,
        };
        let mut output = SecItem {
            type_: 0,
            data: std::ptr::null_mut(),
            len: 0,
        };

        let status = unsafe {
            (self.pk11_sdr_decrypt)(
                &input,
                &mut output,
                std::ptr::null_mut(),
            )
        };

        if status != 0 {
            return Err(GatewayError::Internal(
                "PK11SDR_Decrypt failed (master password or corrupt cookie?)".to_string(),
            ));
        }

        if output.data.is_null() {
            return Ok(Vec::new());
        }

        let plaintext = unsafe {
            std::slice::from_raw_parts(output.data, output.len as usize).to_vec()
        };

        unsafe {
            (self.secitem_free_item)(&mut output, PR_FALSE);
        }

        Ok(plaintext)
    }
}

impl Drop for NssContext {
    fn drop(&mut self) {
        if self.initialized {
            unsafe {
                let _ = (self.nss_shutdown)();
            }
        }
    }
}
