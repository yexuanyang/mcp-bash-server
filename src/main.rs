use std::sync::OnceLock;
use std::{net::SocketAddr, sync::Arc};

use anyhow::Result;
use axum::{
    Router,
    body::Body,
    http::Request,
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpService, session::local::LocalSessionManager,
};
use tower_http::cors::{Any, CorsLayer};
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

// Import modules
mod common;
use common::bash_server::BashServer;
use common::config;
use common::oauth::{
    McpOAuthStore, oauth_approve, oauth_authorization_server, oauth_authorize, oauth_register,
    oauth_token, validate_token_middleware,
};

const INDEX_HTML: &str = include_str!("html/mcp_oauth_index.html");

// Init once from environment variable BIND_ADDRESS
pub static BIND_ADDRESS: OnceLock<String> = OnceLock::new();

// Root path handler
async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

// Wrapper function for oauth_authorization_server to handle BIND_ADDRESS
async fn oauth_authorization_server_handler() -> impl IntoResponse {
    let bind_address = BIND_ADDRESS
        .get()
        .expect("BIND_ADDRESS must be initialized in main()");
    oauth_authorization_server(bind_address).await
}

// Log all HTTP requests
async fn log_request(request: Request<Body>, next: Next) -> Response {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let version = request.version();

    // Log headers
    let headers = request.headers().clone();
    let mut header_log = String::new();
    for (key, value) in headers.iter() {
        let value_str = value.to_str().unwrap_or("<binary>");
        header_log.push_str(&format!("\n  {key}: {value_str}"));
    }

    // Try to get request body for form submissions
    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let request_info = if content_type.contains("application/x-www-form-urlencoded")
        || content_type.contains("application/json")
    {
        format!("{method} {uri} {version:?}{header_log}\nContent-Type: {content_type}")
    } else {
        format!("{method} {uri} {version:?}{header_log}")
    };

    info!("REQUEST: {}", request_info);

    // Call the actual handler
    let response = next.run(request).await;

    // Log response status
    let status = response.status();
    info!("RESPONSE: {} for {} {}", status, method, uri);

    response
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    let logs = tracing_appender::rolling::daily("logs", "mcp.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(logs);
    let log_setting = tracing_subscriber::fmt::layer().with_writer(non_blocking);
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "debug".to_string().into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .with(log_setting)
        .init();

    // Read environment mode from config file, default to "production"
    let config = config::Config::read_config("config.toml")?;
    let env_mode = config
        .settings
        .env
        .clone()
        .unwrap_or_else(|| "production".to_string());
    let is_dev = env_mode == "development";

    // Create the OAuth store
    let oauth_store = Arc::new(McpOAuthStore::new());

    let host = config.settings.host.clone();
    let port = config.settings.port;
    let bind_address = format!("{host}:{port}");

    let addr = bind_address.parse::<SocketAddr>()?;
    let _ = BIND_ADDRESS.set(bind_address);

    // Create StreamableHttpServer
    let service = StreamableHttpService::new(
        BashServer::new,
        LocalSessionManager::default().into(),
        Default::default(),
    );

    let server_router = Router::new().nest_service("/mcp", service);

    // Add OAuth authentication middleware only if not in development mode
    let protected_server_router = if is_dev {
        server_router
    } else {
        server_router.layer(middleware::from_fn_with_state(
            oauth_store.clone(),
            validate_token_middleware,
        ))
    };

    // Create CORS layer for the oauth authorization server endpoint
    let cors_layer = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    // Create a sub-router for the oauth authorization server endpoint with CORS
    let oauth_server_router = Router::new()
        .route(
            "/.well-known/oauth-authorization-server",
            get(oauth_authorization_server_handler).options(oauth_authorization_server_handler),
        )
        .route("/token", post(oauth_token).options(oauth_token))
        .route("/register", post(oauth_register).options(oauth_register))
        .layer(cors_layer)
        .with_state(oauth_store.clone());

    // Create HTTP router with request logging middleware
    let app = Router::new()
        .route("/", get(index))
        .route("/authorize", get(oauth_authorize))
        .route("/approve", post(oauth_approve))
        .merge(oauth_server_router) // Merge the CORS-enabled oauth server router
        .merge(protected_server_router)
        .with_state(oauth_store.clone())
        .layer(middleware::from_fn(log_request));

    // Start HTTP server
    info!("MCP OAuth Server started on {}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let _ = axum::serve(listener, app)
        .with_graceful_shutdown(async { tokio::signal::ctrl_c().await.unwrap() })
        .await;

    Ok(())
}
