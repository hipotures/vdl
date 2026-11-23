//! Small HTTP/JSON client of the filesystem repository; never a downloader.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path as AxumPath, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use url::Url;

use crate::config::Config;
use crate::repository::{RepoError, SourceRepository};
use crate::runner::AppLogger;
use crate::state::{apply_action, snapshot};

const INDEX_HTML: &[u8] = include_bytes!("../static/index.html");
const APP_CSS: &[u8] = include_bytes!("../static/app.css");
const APP_JS: &[u8] = include_bytes!("../static/app.js");
const SORT_JS: &[u8] = include_bytes!("../static/sort.js");
const URL_JS: &[u8] = include_bytes!("../static/url.js");

const CSP: &str = "default-src 'self'; img-src 'self' data:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'";

#[derive(Clone)]
pub struct WebState {
    pub repository: SourceRepository,
    pub logger: Arc<AppLogger>,
}

fn json_error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

fn repo_error_response(err: RepoError, logger: &AppLogger) -> Response {
    match err {
        RepoError::NotFound(msg) => json_error(StatusCode::NOT_FOUND, &msg),
        RepoError::AlreadyExists(msg) => json_error(StatusCode::CONFLICT, &msg),
        RepoError::Value(msg) => json_error(StatusCode::BAD_REQUEST, &msg),
        RepoError::Io(io_err) => {
            logger.error(&format!("HTTP filesystem operation failed: {io_err}"));
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Filesystem operation failed; check the vdl log",
            )
        }
    }
}

async fn security_boundary(request: Request, next: Next) -> Response {
    let mut response = if request.method() != Method::GET && request.method() != Method::HEAD {
        if let Some(origin_val) = request.headers().get(header::ORIGIN) {
            let origin_str = origin_val.to_str().unwrap_or("");
            let host_str = request
                .headers()
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if !origin_matches_host(origin_str, host_str) {
                json_error(
                    StatusCode::FORBIDDEN,
                    "Cross-origin requests are not allowed",
                )
            } else if !has_same_origin_json_headers(request.headers()) {
                json_error(
                    StatusCode::FORBIDDEN,
                    "A same-origin JSON request is required",
                )
            } else {
                next.run(request).await
            }
        } else if !has_same_origin_json_headers(request.headers()) {
            json_error(
                StatusCode::FORBIDDEN,
                "A same-origin JSON request is required",
            )
        } else {
            next.run(request).await
        }
    } else {
        next.run(request).await
    };

    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    response
}

fn origin_matches_host(origin: &str, host: &str) -> bool {
    let Ok(parsed) = Url::parse(origin) else {
        return false;
    };
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return false;
    }
    let Some(origin_host) = parsed.host_str() else {
        return false;
    };
    let origin_netloc = match parsed.port() {
        Some(port) => format!("{origin_host}:{port}"),
        None => origin_host.to_string(),
    };
    origin_netloc.eq_ignore_ascii_case(host)
}

fn has_same_origin_json_headers(headers: &axum::http::HeaderMap) -> bool {
    let req_hdr = headers
        .get("x-vdl-request")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let media_type = content_type.split(';').next().unwrap_or("").trim();
    req_hdr == "1" && media_type.eq_ignore_ascii_case("application/json")
}

async fn get_state(State(state): State<WebState>) -> Response {
    let repo = state.repository.clone();
    match tokio::task::spawn_blocking(move || snapshot(&repo)).await {
        Ok(Ok(snap)) => (StatusCode::OK, Json(snap)).into_response(),
        Ok(Err(err)) => repo_error_response(err, &state.logger),
        Err(_) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Filesystem operation failed; check the vdl log",
        ),
    }
}

async fn post_source(State(state): State<WebState>, body: axum::body::Bytes) -> Response {
    let Ok(data) = serde_json::from_slice::<Value>(&body) else {
        return json_error(StatusCode::BAD_REQUEST, "invalid JSON body");
    };
    let Some(obj) = data.as_object() else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "request body must be a JSON object",
        );
    };
    let Some(url_str) = obj.get("url").and_then(Value::as_str) else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "source URL must be a non-empty string",
        );
    };
    let url_owned = url_str.to_string();
    let repo = state.repository.clone();
    let res = tokio::task::spawn_blocking(move || {
        let added = repo.add_source(&url_owned)?;
        let snap = snapshot(&repo)?;
        Ok::<_, RepoError>((added, snap))
    })
    .await;

    match res {
        Ok(Ok((added, snap))) => {
            let status = if added.created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            (
                status,
                Json(json!({
                    "created": added.created,
                    "id": added.source.id(),
                    "url": added.source.url,
                    "active": added.source.active,
                    "state": snap,
                })),
            )
                .into_response()
        }
        Ok(Err(err)) => repo_error_response(err, &state.logger),
        Err(_) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Filesystem operation failed; check the vdl log",
        ),
    }
}

async fn post_action(
    State(state): State<WebState>,
    AxumPath(action): AxumPath<String>,
    body: axum::body::Bytes,
) -> Response {
    if action != "disable" && action != "download-now" {
        return json_error(StatusCode::NOT_FOUND, "Not Found");
    }
    let Ok(data) = serde_json::from_slice::<Value>(&body) else {
        return json_error(StatusCode::BAD_REQUEST, "invalid JSON body");
    };
    let Some(obj) = data.as_object() else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "request body must be a JSON object",
        );
    };
    let Some(source_id) = obj.get("id").and_then(Value::as_str) else {
        return json_error(StatusCode::BAD_REQUEST, "source ID must be a string");
    };
    let id_owned = source_id.to_string();
    let repo = state.repository.clone();
    let res = tokio::task::spawn_blocking(move || {
        apply_action(&repo, &action, &id_owned)?;
        snapshot(&repo)
    })
    .await;

    match res {
        Ok(Ok(snap)) => (StatusCode::OK, Json(json!({ "state": snap }))).into_response(),
        Ok(Err(err)) => repo_error_response(err, &state.logger),
        Err(_) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Filesystem operation failed; check the vdl log",
        ),
    }
}

fn asset_response(bytes: &'static [u8], content_type: &'static str) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

pub fn create_router(repository: SourceRepository, logger: Arc<AppLogger>) -> Router {
    let state = WebState { repository, logger };
    Router::new()
        .route(
            "/",
            get(|| async { asset_response(INDEX_HTML, "text/html; charset=utf-8") }),
        )
        .route(
            "/app.css",
            get(|| async { asset_response(APP_CSS, "text/css; charset=utf-8") }),
        )
        .route(
            "/app.js",
            get(|| async { asset_response(APP_JS, "text/javascript; charset=utf-8") }),
        )
        .route(
            "/sort.js",
            get(|| async { asset_response(SORT_JS, "text/javascript; charset=utf-8") }),
        )
        .route(
            "/url.js",
            get(|| async { asset_response(URL_JS, "text/javascript; charset=utf-8") }),
        )
        .route("/api/state", get(get_state))
        .route("/api/sources", post(post_source))
        .route("/api/sources/{action}", post(post_action))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(middleware::from_fn(security_boundary))
        .with_state(state)
}

pub fn serve(config: Config) -> Result<(), String> {
    let repository = SourceRepository::new(&config.download_root).map_err(|e| e.to_string())?;
    let logger = Arc::new(AppLogger::new(Some(&config.log_file)));
    let addr: SocketAddr = format!("{}:{}", config.web_host, config.web_port)
        .parse()
        .map_err(|e| {
            format!(
                "invalid bind address {}:{}: {e}",
                config.web_host, config.web_port
            )
        })?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;

    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| e.to_string())?;
        let app = create_router(repository, logger);
        axum::serve(listener, app).await.map_err(|e| e.to_string())
    })
}

#[cfg(test)]
mod tests {
    use http_body_util::BodyExt;
    use tempfile::TempDir;
    use tower::ServiceExt;

    use super::*;
    use crate::runtime::RuntimePaths;

    #[tokio::test]
    async fn api_endpoints_and_security_headers() {
        let tmp = TempDir::new().unwrap();
        let runtime = RuntimePaths::from_dir(tmp.path().join("rt"));
        let repo = SourceRepository::with_runtime(tmp.path().join("dl"), runtime);
        let logger = Arc::new(AppLogger::new(Some(&tmp.path().join("vdl.log"))));
        let app = create_router(repo, logger);

        // GET /
        let resp = app
            .clone()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        assert_eq!(
            resp.headers().get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
            "nosniff"
        );

        // POST without X-VDL-Request -> 403
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/sources")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"url":"https://www.tiktok.com/@alice"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // POST cross-origin -> 403
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/sources")
                    .header(header::HOST, "localhost:8780")
                    .header(header::ORIGIN, "https://evil.example")
                    .header("x-vdl-request", "1")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"url":"https://www.tiktok.com/@alice"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // POST valid -> 201
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/sources")
                    .header(header::HOST, "localhost:8780")
                    .header(header::ORIGIN, "http://localhost:8780")
                    .header("x-vdl-request", "1")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"url":"https://www.tiktok.com/@alice?tab=videos"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let val: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(val["id"], "tiktok/alice");
        assert_eq!(val["url"], "https://www.tiktok.com/@alice");

        // POST disable -> 200
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/sources/disable")
                    .header("x-vdl-request", "1")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"id":"tiktok/alice"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let val: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(val["state"]["sources"][0]["state"], "inactive");
    }
}
