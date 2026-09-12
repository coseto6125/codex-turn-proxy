pub mod cli;
pub mod store;
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, OriginalUri, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use headers::{Connection, HeaderMapExt};
use serde_json::json;
use std::{sync::Arc, time::Duration};

pub type Result<T> = std::result::Result<T, ProxyError>;
#[derive(Debug)]
pub struct ProxyError(pub StatusCode, pub &'static str);
impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        (self.0, axum::Json(json!({"error": {"message": self.1}}))).into_response()
    }
}
fn internal(_: impl std::fmt::Display) -> ProxyError {
    ProxyError(StatusCode::INTERNAL_SERVER_ERROR, "internal proxy error")
}
fn bad(message: &'static str) -> ProxyError {
    ProxyError(StatusCode::BAD_REQUEST, message)
}
#[derive(Clone)]
pub struct Config {
    pub upstream: String,
    pub token: String,
    pub max_request_bytes: usize,
}
impl Config {
    pub fn new(upstream: String, token: String) -> Self {
        Self {
            upstream,
            token,
            max_request_bytes: 32 * 1024 * 1024,
        }
    }
}
pub struct Proxy {
    config: Config,
    client: reqwest::Client,
}
impl Proxy {
    pub fn new(config: Config) -> Result<Arc<Self>> {
        let url = reqwest::Url::parse(&config.upstream).map_err(|_| bad("invalid upstream URL"))?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(bad(
                "upstream must be an HTTP(S) base URL without credentials or query",
            ));
        }
        if config.token.len() < 32 {
            return Err(bad("PROXY_TOKEN must contain at least 32 bytes"));
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(300))
            .build()
            .map_err(internal)?;
        Ok(Arc::new(Self { config, client }))
    }
    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .route("/v1/responses", post(forward))
            .route("/v1/models", get(models))
            .route("/v1/responses/compact", post(compact))
            .layer(DefaultBodyLimit::max(self.config.max_request_bytes))
            .with_state(self)
    }
    fn authorize(&self, headers: &HeaderMap) -> Result<()> {
        if headers.get("x-proxy-token").and_then(|v| v.to_str().ok()) != Some(&self.config.token) {
            return Err(ProxyError(StatusCode::UNAUTHORIZED, "invalid proxy token"));
        }
        Ok(())
    }
}
fn transport_headers(source: &HeaderMap) -> HeaderMap {
    let connection = source.typed_get::<Connection>();
    source
        .iter()
        .filter(|(name, _)| {
            !connection.as_ref().is_some_and(|c| c.contains(*name))
                && !matches!(
                    name.as_str(),
                    "host"
                        | "connection"
                        | "content-length"
                        | "transfer-encoding"
                        | "keep-alive"
                        | "te"
                        | "trailer"
                        | "upgrade"
                        | "proxy-authorization"
                        | "proxy-authenticate"
                        | "x-proxy-token"
                )
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}
async fn forward(
    State(proxy): State<Arc<Proxy>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    dispatch(proxy, "responses", headers, body).await
}
async fn compact(
    State(proxy): State<Arc<Proxy>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    dispatch(proxy, "responses/compact", headers, body).await
}
async fn models(
    State(proxy): State<Arc<Proxy>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Result<Response> {
    proxy.authorize(&headers)?;
    let mut url = reqwest::Url::parse(&format!(
        "{}/models",
        proxy.config.upstream.trim_end_matches('/')
    ))
    .map_err(internal)?;
    url.set_query(uri.query());
    let upstream = proxy
        .client
        .get(url)
        .headers(transport_headers(&headers))
        .send()
        .await
        .map_err(|_| ProxyError(StatusCode::BAD_GATEWAY, "upstream connection failed"))?;
    Ok(upstream_response(upstream))
}
async fn dispatch(
    proxy: Arc<Proxy>,
    path: &str,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    proxy.authorize(&headers)?;
    let mut headers = transport_headers(&headers);
    headers.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let upstream = proxy
        .client
        .post(format!(
            "{}/{path}",
            proxy.config.upstream.trim_end_matches('/')
        ))
        .headers(headers)
        .body(body)
        .send()
        .await
        .map_err(|_| ProxyError(StatusCode::BAD_GATEWAY, "upstream connection failed"))?;
    Ok(upstream_response(upstream))
}
fn upstream_response(upstream: reqwest::Response) -> Response {
    let status = upstream.status();
    let headers = transport_headers(upstream.headers());
    let mut response = Response::new(Body::from_stream(upstream.bytes_stream()));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
        .headers_mut()
        .insert("x-proxy-injected", HeaderValue::from_static("0"));
    response
}
