//! The local listening screen: plays presented renders and stores what the
//! person marks. It offers no editing; music changes stay with the agent.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::extract::{Path as UrlPath, State};
use axum::http::{Request, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tower_http::services::ServeFile;

use crate::review::Mark;
use crate::service::{Service, Status, ToolResponse};
use crate::store::Store;

const PAGE: &str = include_str!("review_page.html");
/// Written while a standalone screen runs, so other processes can link to it.
pub const SERVER_FILE: &str = "review-server.json";

pub type SharedService = Arc<Mutex<Service>>;

#[derive(Deserialize)]
struct DraftBody {
    #[serde(default)]
    marks: Vec<Mark>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
struct DeliverBody {
    delivery_id: String,
}

async fn with_service<T: Send + 'static>(
    service: SharedService,
    work: impl FnOnce(&mut Service) -> T + Send + 'static,
) -> T {
    tokio::task::spawn_blocking(move || work(&mut service.lock().expect("service lock")))
        .await
        .expect("service task")
}

fn reply(response: ToolResponse) -> Response {
    let code = match response.status {
        Status::Ok => StatusCode::OK,
        Status::Blocked => StatusCode::UNPROCESSABLE_ENTITY,
        Status::Error => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (code, Json(response)).into_response()
}

pub fn router(service: SharedService) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route(
            "/review/{project}/{presentation}",
            get(|| async { Html(PAGE) }),
        )
        .route(
            "/api/review/{project}/{presentation}",
            get(
                |State(service): State<SharedService>,
                 UrlPath((project, presentation)): UrlPath<(String, String)>| async move {
                    with_service(service, move |s| {
                        match s.review_page(&project, &presentation) {
                            Ok(page) => {
                                let draft = s.draft(&project, &presentation);
                                Json(json!({"page": page, "draft": draft})).into_response()
                            }
                            Err(response) => reply(response),
                        }
                    })
                    .await
                },
            ),
        )
        .route(
            "/api/review/{project}/{presentation}/draft",
            axum::routing::put(
                |State(service): State<SharedService>,
                 UrlPath((project, presentation)): UrlPath<(String, String)>,
                 Json(body): Json<DraftBody>| async move {
                    reply(
                        with_service(service, move |s| {
                            s.save_draft(&project, &presentation, body.marks, body.text)
                        })
                        .await,
                    )
                },
            ),
        )
        .route(
            "/api/review/{project}/{presentation}/deliver",
            axum::routing::post(
                |State(service): State<SharedService>,
                 UrlPath((project, presentation)): UrlPath<(String, String)>,
                 Json(body): Json<DeliverBody>| async move {
                    reply(
                        with_service(service, move |s| {
                            s.deliver_draft(&project, &presentation, &body.delivery_id)
                        })
                        .await,
                    )
                },
            ),
        )
        .route(
            "/audio/{object}",
            get(
                |State(service): State<SharedService>,
                 UrlPath(object): UrlPath<String>,
                 request: Request<axum::body::Body>| async move {
                    let path = with_service(service, move |s| s.object_file(&object)).await;
                    match path {
                        // ServeFile answers range requests, which audio seeking needs.
                        Some(path) => match tower::ServiceExt::oneshot(
                            ServeFile::new_with_mime(path, &"audio/wav".parse().unwrap()),
                            request,
                        )
                        .await
                        {
                            Ok(response) => response.into_response(),
                            Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
                                .into_response(),
                        },
                        None => StatusCode::NOT_FOUND.into_response(),
                    }
                },
            ),
        )
        .with_state(service)
}

/// Binds to `127.0.0.1:port` (0 picks a free port) and serves in the
/// background. Returns the base URL.
pub async fn start(service: SharedService, port: u16) -> std::io::Result<String> {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await?;
    let address = listener.local_addr()?;
    let app = router(service);
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(format!("http://{address}"))
}

/// URL of a standalone screen started for this workspace, if it still answers.
pub fn running_server_url(workspace: &Path) -> Option<String> {
    let text = std::fs::read_to_string(Store::directory_for(workspace).join(SERVER_FILE)).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let url = value.get("url")?.as_str()?.to_owned();
    let address: SocketAddr = url.trim_start_matches("http://").parse().ok()?;
    std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_millis(300)).ok()?;
    Some(url)
}
