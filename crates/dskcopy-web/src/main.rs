use axum::{
    extract::Path,
    http::{header, StatusCode, Uri},
    response::{Html, IntoResponse, Redirect},
    routing::get,
    Router,
};
use rust_embed::RustEmbed;
use tera::{Context, Tera};
use std::sync::Arc;

#[derive(RustEmbed)]
#[folder = "static/"]
struct Asset;

#[derive(RustEmbed)]
#[folder = "templates/"]
struct TemplateAsset;

struct AppState {
    tera: Tera,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    // Initialize Tera templates from embedded files
    let mut tera = Tera::default();
    for file in TemplateAsset::iter() {
        if let Some(content) = TemplateAsset::get(&file) {
            let content_str = std::str::from_utf8(content.data.as_ref()).unwrap();
            tera.add_raw_template(&file, content_str).unwrap();
        }
    }
    
    let state = Arc::new(AppState { tera });

    let app = Router::new()
        .route("/", get(index))
        .route("/download/:platform", get(download))
        .route("/static/*file", get(static_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    tracing::info!("Listening on http://0.0.0.0:3000");
    axum::serve(listener, app).await.unwrap();
}

async fn index(axum::extract::State(state): axum::extract::State<Arc<AppState>>) -> impl IntoResponse {
    let context = Context::new();
    match state.tera.render("index.html", &context) {
        Ok(rendered) => Html(rendered).into_response(),
        Err(e) => {
            tracing::error!("Template rendering error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
        }
    }
}

async fn download(Path(platform): Path<String>) -> impl IntoResponse {
    // Redirect to actual GitHub releases or binary download links
    let url = match platform.as_str() {
        "windows" => "https://github.com/example/dskcopy/releases/latest/download/dskcopy-windows.exe",
        "macos" => "https://github.com/example/dskcopy/releases/latest/download/dskcopy-macos.tar.gz",
        "linux" => "https://github.com/example/dskcopy/releases/latest/download/dskcopy-linux.tar.gz",
        _ => "/",
    };
    Redirect::temporary(url)
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