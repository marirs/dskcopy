use axum::{
    extract::Path,
    http::{header, StatusCode, Uri},
    response::{Html, IntoResponse, Redirect},
    routing::get,
    Router,
};
use rust_embed::RustEmbed;
use std::sync::Arc;
use tera::{Context, Tera};

#[derive(RustEmbed)]
#[folder = "static/"]
struct Asset;

#[derive(RustEmbed)]
#[folder = "templates/"]
struct TemplateAsset;

struct AppState {
    tera: Tera,
    repo: String,
    version: String,
}

const REPO_DEFAULT: &str = "marirs/dskcopy";
const VERSION_DEFAULT: &str = env!("CARGO_PKG_VERSION");

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let mut tera = Tera::default();
    for file in TemplateAsset::iter() {
        if let Some(content) = TemplateAsset::get(&file) {
            let content_str = std::str::from_utf8(content.data.as_ref()).unwrap();
            tera.add_raw_template(&file, content_str).unwrap();
        }
    }

    let repo = std::env::var("DSKCOPY_REPO").unwrap_or_else(|_| REPO_DEFAULT.to_string());
    let version =
        std::env::var("DSKCOPY_VERSION").unwrap_or_else(|_| VERSION_DEFAULT.to_string());

    let state = Arc::new(AppState { tera, repo, version });

    let app = Router::new()
        .route("/", get(index))
        .route("/download/:platform", get(download))
        .route("/static/*file", get(static_handler))
        .with_state(state);

    let bind = std::env::var("DSKCOPY_WEB_BIND").unwrap_or_else(|_| "0.0.0.0:3000".to_string());
    let listener = tokio::net::TcpListener::bind(&bind).await.unwrap();
    tracing::info!("Listening on http://{}", bind);
    axum::serve(listener, app).await.unwrap();
}

async fn index(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse {
    let mut ctx = Context::new();
    ctx.insert("repo", &state.repo);
    ctx.insert("version", &state.version);
    match state.tera.render("index.html", &ctx) {
        Ok(rendered) => Html(rendered).into_response(),
        Err(e) => {
            tracing::error!("Template rendering error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
        }
    }
}

/// Redirect /download/:platform → the matching GitHub release asset.
/// Accepts shapes like `gui-macos-aarch64`, `cli-linux-x86_64`, `gui-windows-x86_64`.
async fn download(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Path(platform): Path<String>,
) -> impl IntoResponse {
    let file = match platform.as_str() {
        // GUI
        "gui-macos-aarch64" => "dskcopy-gui-macos-aarch64.app.tar.gz",
        "gui-macos-x86_64" => "dskcopy-gui-macos-x86_64.app.tar.gz",
        "gui-windows-x86_64" => "dskcopy-gui-windows-x86_64.zip",
        "gui-windows-aarch64" => "dskcopy-gui-windows-aarch64.zip",
        "gui-linux-x86_64" => "dskcopy-gui-linux-x86_64.tar.gz",
        // CLI
        "cli-macos-aarch64" => "dskcopy-cli-macos-aarch64.tar.gz",
        "cli-macos-x86_64" => "dskcopy-cli-macos-x86_64.tar.gz",
        "cli-windows-x86_64" => "dskcopy-cli-windows-x86_64.zip",
        "cli-windows-aarch64" => "dskcopy-cli-windows-aarch64.zip",
        "cli-linux-x86_64" => "dskcopy-cli-linux-x86_64.tar.gz",
        "cli-linux-aarch64" => "dskcopy-cli-linux-aarch64.tar.gz",
        _ => return Redirect::temporary("/").into_response(),
    };
    let url = format!(
        "https://github.com/{}/releases/download/v{}/{}",
        state.repo, state.version, file
    );
    Redirect::temporary(&url).into_response()
}

async fn static_handler(uri: Uri) -> impl IntoResponse {
    let path = uri.path().trim_start_matches("/static/");
    if let Some(content) = Asset::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        ([(header::CONTENT_TYPE, mime.as_ref())], content.data).into_response()
    } else {
        (StatusCode::NOT_FOUND, "404 Not Found").into_response()
    }
}
