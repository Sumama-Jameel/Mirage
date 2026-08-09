//! DeepSeek proof-of-work solver.
//!
//! DeepSeek gates `POST /api/v0/chat/completion` behind a PoW header. The
//! algorithm is shipped as a WebAssembly module; we run DeepSeek's own module
//! in a `wasmtime` sandbox (no network/file access) so we don't have to
//! reimplement the hashing logic.

use std::sync::Arc;

use wasmtime::{Engine as WasmtimeEngine, Instance, Memory, Module, Store, Val};

use crate::error::GatewayError;

/// Compiled PoW module plus the typed functions needed to solve challenges.
pub struct DeepSeekPow {
    engine: WasmtimeEngine,
    module: Module,
}

impl DeepSeekPow {
    /// Compile the wasm module once. The compiled module is cheap to clone
    /// into per-solve instances.
    pub fn new(wasm_bytes: &[u8]) -> Result<Self, GatewayError> {
        let engine = WasmtimeEngine::default();
        let module = Module::new(&engine, wasm_bytes).map_err(|e| {
            GatewayError::Internal(format!("failed to compile DeepSeek PoW wasm: {e}"))
        })?;
        Ok(Self { engine, module })
    }

    /// Solve a single PoW challenge.
    ///
    /// `prefix` is `salt_expire_at_` for the chat completion target path.
    /// Returns `None` when the wasm reports the challenge expired/unsolvable.
    pub fn solve(
        &self,
        challenge: &str,
        prefix: &str,
        difficulty: f64,
    ) -> Result<Option<i64>, GatewayError> {
        let mut store = Store::new(&self.engine, ());
        let instance = Instance::new(&mut store, &self.module, &[]).map_err(|e| {
            GatewayError::Internal(format!("failed to instantiate DeepSeek PoW wasm: {e}"))
        })?;

        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| GatewayError::Internal("PoW wasm has no memory export".to_string()))?;
        let add_to_stack = Self::func(&instance, &mut store, "__wbindgen_add_to_stack_pointer")?;
        let malloc = Self::func(&instance, &mut store, "__wbindgen_export_0")?;
        let solve_fn = Self::func(&instance, &mut store, "wasm_solve")?;

        let retptr = Self::call_i32(&add_to_stack, &mut store, &[Val::I32(-16)])?;

        let c_ptr = Self::write_str(&malloc, &mut store, &memory, challenge)?;
        let p_ptr = Self::write_str(&malloc, &mut store, &memory, prefix)?;

        solve_fn
            .call(
                &mut store,
                &[
                    Val::I32(retptr),
                    Val::I32(c_ptr),
                    Val::I32(challenge.len() as i32),
                    Val::I32(p_ptr),
                    Val::I32(prefix.len() as i32),
                    Val::F64(difficulty.to_bits()),
                ],
                &mut [],
            )
            .map_err(|e| GatewayError::Internal(format!("PoW wasm_solve call failed: {e}")))?;

        let mut status_bytes = [0u8; 4];
        memory
            .read(&mut store, retptr as usize, &mut status_bytes)
            .map_err(|e| GatewayError::Internal(format!("failed to read PoW status: {e}")))?;
        let status = i32::from_le_bytes(status_bytes);

        let result = if status == 0 {
            None
        } else {
            let mut answer_bytes = [0u8; 8];
            memory
                .read(&mut store, retptr as usize + 8, &mut answer_bytes)
                .map_err(|e| GatewayError::Internal(format!("failed to read PoW answer: {e}")))?;
            let answer = f64::from_le_bytes(answer_bytes) as i64;
            Some(answer)
        };

        // Restore the shadow stack pointer.
        let _ = Self::call_i32(&add_to_stack, &mut store, &[Val::I32(16)])?;
        Ok(result)
    }

    /// Build the base64 `x-ds-pow-response` header value from the challenge
    /// object returned by `/api/v0/chat/create_pow_challenge`.
    pub fn make_header(
        &self,
        challenge: &serde_json::Value,
    ) -> Result<String, GatewayError> {
        let algorithm = challenge
            .get("algorithm")
            .and_then(|v| v.as_str())
            .ok_or_else(|| GatewayError::Provider("missing algorithm in PoW challenge".to_string()))?;
        let challenge_str = challenge
            .get("challenge")
            .and_then(|v| v.as_str())
            .ok_or_else(|| GatewayError::Provider("missing challenge in PoW challenge".to_string()))?;
        let salt = challenge
            .get("salt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| GatewayError::Provider("missing salt in PoW challenge".to_string()))?;
        let signature = challenge
            .get("signature")
            .and_then(|v| v.as_str())
            .ok_or_else(|| GatewayError::Provider("missing signature in PoW challenge".to_string()))?;
        let target_path = challenge
            .get("target_path")
            .and_then(|v| v.as_str())
            .unwrap_or("/api/v0/chat/completion");
        let expire_at = challenge
            .get("expire_at")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| GatewayError::Provider("missing expire_at in PoW challenge".to_string()))?;
        let difficulty = challenge
            .get("difficulty")
            .and_then(|v| v.as_f64())
            .ok_or_else(|| GatewayError::Provider("missing difficulty in PoW challenge".to_string()))?;

        let prefix = format!("{salt}_{expire_at}_");
        let answer = self
            .solve(challenge_str, &prefix, difficulty)?
            .ok_or_else(|| GatewayError::Provider("PoW solver returned no answer".to_string()))?;

        let payload = serde_json::json!({
            "algorithm": algorithm,
            "challenge": challenge_str,
            "salt": salt,
            "answer": answer,
            "signature": signature,
            "target_path": target_path,
        });
        let raw = serde_json::to_string(&payload).map_err(|e| {
            GatewayError::Internal(format!("failed to serialize PoW response: {e}"))
        })?;
        Ok(base64::Engine::encode(&base64::engine::general_purpose::STANDARD, raw.as_bytes()))
    }

    fn func(
        instance: &Instance,
        store: &mut Store<()>,
        name: &str,
    ) -> Result<wasmtime::Func, GatewayError> {
        instance
            .get_func(&mut *store, name)
            .ok_or_else(|| GatewayError::Internal(format!("PoW wasm missing export: {name}")))
    }

    fn call_i32(
        func: &wasmtime::Func,
        store: &mut Store<()>,
        args: &[Val],
    ) -> Result<i32, GatewayError> {
        let mut results = [Val::I32(0)];
        func.call(store, args, &mut results)
            .map_err(|e| GatewayError::Internal(format!("PoW wasm call failed: {e}")))?;
        match results[0] {
            Val::I32(v) => Ok(v),
            _ => Err(GatewayError::Internal(
                "PoW wasm call returned non-i32".to_string(),
            )),
        }
    }

    fn write_str(
        malloc: &wasmtime::Func,
        store: &mut Store<()>,
        memory: &Memory,
        text: &str,
    ) -> Result<i32, GatewayError> {
        let bytes = text.as_bytes();
        let ptr = Self::call_i32(malloc, store, &[Val::I32(bytes.len() as i32), Val::I32(1)])?;
        memory
            .write(store, ptr as usize, bytes)
            .map_err(|e| GatewayError::Internal(format!("failed to write PoW string: {e}")))?;
        Ok(ptr)
    }
}

/// Thread-safe, lazily-initialized PoW solver.
///
/// The compiled `wasmtime::Module` is `Send + Sync`; each solve creates its own
/// short-lived `Store`/`Instance`, so concurrent requests do not serialize.
pub struct SharedPow {
    inner: Arc<DeepSeekPow>,
}

impl SharedPow {
    pub fn new(wasm_bytes: &[u8]) -> Result<Self, GatewayError> {
        Ok(Self {
            inner: Arc::new(DeepSeekPow::new(wasm_bytes)?),
        })
    }

    pub fn make_header(&self, challenge: &serde_json::Value) -> Result<String, GatewayError> {
        self.inner.make_header(challenge)
    }
}

impl Clone for SharedPow {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

// ── PoWSolver integration ─────────────────────────────────────────────────

use crate::providers::solver::{PoWHeader, PoWSolver};

/// [`PoWSolver`] implementation for DeepSeek's WASM-based PoW challenge.
///
/// The solver compiles DeepSeek's `sha3_wasm_bg.wasm` module at construction
/// time and uses it to solve `create_pow_challenge` responses. Produces the
/// `x-ds-pow-response` header expected by DeepSeek's API.
pub struct DeepSeekSolver {
    inner: SharedPow,
}

impl DeepSeekSolver {
    /// Compile DeepSeek's PoW WASM module and wrap it as a solver.
    pub fn new(wasm_bytes: &[u8]) -> Result<Self, GatewayError> {
        Ok(Self {
            inner: SharedPow::new(wasm_bytes)?,
        })
    }
}

impl PoWSolver for DeepSeekSolver {
    fn name(&self) -> &'static str {
        "deepseek-v1"
    }

    fn solve(&self, challenge: &serde_json::Value) -> Result<PoWHeader, GatewayError> {
        let value = self.inner.make_header(challenge)?;
        Ok(PoWHeader {
            name: "x-ds-pow-response".to_string(),
            value,
        })
    }
}
