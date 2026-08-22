use std::net::SocketAddr;

use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

mod api;
mod auth_state;
mod browser;
mod capture;
mod config;
mod error;
mod models;
mod providers;
mod session;
mod state;
mod vault;

use crate::config::Config;
use crate::error::GatewayError;
use crate::providers::{
    chatgpt::ChatGPTProvider,
    claude::ClaudeProvider,
    deepseek::{DeepSeekProvider, DeepSeekSolver, PowAssets},
    gemini::GeminiProvider,
    glm::GlmProvider,
    grok::GrokProvider,
    kimi::KimiProvider,
    metaai::MetaAiProvider,
    minimax::MinimaxProvider,
    mimo::MiMoProvider,
    mistral::MistralProvider,
    qwen::QwenProvider,
    solver::{SolverChain, SolverRegistry},
    ProviderRegistry,
};
use crate::session::SessionManager;
use crate::state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    // Parse CLI args: --capture <provider> runs capture mode, otherwise start server.
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 3 && args[1] == "--capture" {
        let provider = &args[2];
        let config = Config::load()?;
        capture::run_capture(provider, &config).await?;
        return Ok(());
    }

    let config = Config::load()?;
    info!(
        host = %config.server.host,
        port = config.server.port,
        "Loaded configuration"
    );

    // Ensure session persistence directory exists.
    if let Some(ref data_dir) = config.data_dir {
        tokio::fs::create_dir_all(data_dir).await
            .map_err(|e| anyhow::anyhow!("failed to create data_dir {data_dir:?}: {e}"))?;
    }

    // Bind the port before doing anything network-bound (session warm-up,
    // PoW download). The OS accepts connections into the backlog immediately,
    // so startup never hangs waiting for provider navigation.
    let addr: SocketAddr = format!("{}:{}", config.server.host, config.server.port)
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid server address: {e}"))?;
    let listener = TcpListener::bind(addr).await?;
    info!("Obscura gateway listening on http://{}", addr);

    // Non-blocking: returns immediately, warms the pool in the background.
    let sessions = SessionManager::spawn(&config).await?;
    info!("Browser session pool warm-up started in background");

    let mut registry = ProviderRegistry::new();
    // DeepSeek's legacy direct adapter needs a provider-supplied PoW module.
    // Its optional failure must not prevent the browser-UI providers from
    // starting. Kimi, GLM and Claude do not use this path.
    match download_deepseek_pow_wasm().await {
        Ok(wasm_bytes) => {
            let mut solver_registry = SolverRegistry::new();
            solver_registry.register(std::sync::Arc::new(DeepSeekSolver::new(&wasm_bytes)?));
            let solver_chain: SolverChain = vec!["deepseek-v1".to_string()];
            registry.register(std::sync::Arc::new(DeepSeekProvider::with_data_dir(
                solver_registry,
                solver_chain,
                config.data_dir.clone(),
            )))?;
        }
        Err(error) => {
            tracing::warn!(error = %error, "DeepSeek direct provider disabled; browser-UI providers remain available");
        }
    }

    // Register Gemini provider (no PoW solver needed).
    registry.register(std::sync::Arc::new(GeminiProvider::with_data_dir(config.data_dir.clone())))?;
    info!("Gemini provider registered");

    // Register ChatGPT provider (no PoW solver needed — pure Rust SHA3-512).
    registry.register(std::sync::Arc::new(ChatGPTProvider::with_data_dir(config.data_dir.clone())))?;
    info!("ChatGPT provider registered");

    // Register Kimi direct provider
    registry.register(std::sync::Arc::new(KimiProvider::with_data_dir(config.data_dir.clone())))?;
    info!("Kimi provider registered");

    // Register GLM direct provider
    registry.register(std::sync::Arc::new(GlmProvider::with_data_dir(config.data_dir.clone())))?;
    info!("GLM provider registered");

    // Register Claude direct provider
    registry.register(std::sync::Arc::new(ClaudeProvider::with_data_dir(config.data_dir.clone())))?;
    info!("Claude provider registered");

    // Register Grok direct provider (uses grok.com web UI cookies + x-statsig-id anti-bot bypass).
    registry.register(std::sync::Arc::new(GrokProvider::with_data_dir(config.data_dir.clone())))?;
    info!("Grok provider registered");

    // Register Qwen direct provider (uses chat.qwen.ai JWT from Firefox localStorage).
    registry.register(std::sync::Arc::new(QwenProvider::with_data_dir(config.data_dir.clone())))?;
    info!("Qwen provider registered");

    // Register Minimax direct provider (uses agent.minimax.io JWT from localStorage).
    registry.register(std::sync::Arc::new(MinimaxProvider::with_data_dir(config.data_dir.clone())))?;
    info!("Minimax provider registered");

    // Register MiMo direct provider (uses aistudio.xiaomimimo.com cookies:
    // serviceToken, userId, xiaomichatbot_ph).
    registry.register(std::sync::Arc::new(MiMoProvider::with_data_dir(config.data_dir.clone())))?;
    info!("MiMo provider registered");

    // Register Mistral direct provider (uses chat.mistral.ai session cookie;
    // continuation via chatId/messageId persisted in state).
    registry.register(std::sync::Arc::new(MistralProvider::with_data_dir(
        config.data_dir.clone(),
    )))?;
    info!("Mistral provider registered");

    // Register Meta AI direct provider (DGW WebSocket; meta.ai session cookies
    // + the page-injected ecto1 token, with META_AI_ECTO1_TOKEN override).
    registry.register(std::sync::Arc::new(MetaAiProvider::with_data_dir(config.data_dir.clone())))?;
    info!("Meta AI provider registered");

    let state = AppState::new(config.clone(), registry, sessions);
    let shutdown_state = state.clone();
    let app = api::router(state);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("Shutting down browser sessions");
    shutdown_state.sessions.shutdown().await.ok();

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};
        signal(SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    info!("Shutdown signal received");
}

/// Download DeepSeek's PoW WASM module from the well-known fallback URL.
///
/// At startup the gateway fetches the module once and compiles it into a
/// [`DeepSeekSolver`] that is shared across all requests via the
/// [`SolverRegistry`]. This avoids re-downloading on every request and
/// removes the need for lazy initialization inside the provider.
async fn download_deepseek_pow_wasm() -> Result<Vec<u8>, GatewayError> {
    PowAssets::new()?.fetch().await
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let subscriber = tracing_subscriber::registry().with(filter).with(
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_ansi(false),
    );

    if subscriber.try_init().is_err() {
        // Tracing may already be initialized in tests or embedded use.
        // This is not fatal.
    }
}
