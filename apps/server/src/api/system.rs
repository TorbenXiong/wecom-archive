use crate::{
    AccessTokenRequest, AccessTokenResponse, ApiError, AppState, HealthResponse,
    RegeneratedAccessTokenResponse, authorize, embedded_asset, persist_access_token,
    random_access_token,
};
use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::http::header;
use axum::response::IntoResponse;
use axum::response::Response;
use sha2::Digest;
use sha2::Sha256;

pub(crate) async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        schema: "server-api.v1",
        network_mode: state.network_mode,
    })
}

pub(crate) async fn update_access_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<AccessTokenRequest>,
) -> Result<Json<AccessTokenResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let token = request.access_token.trim();
    if token.len() < 24 {
        return Err(ApiError::invalid_query());
    }
    persist_access_token(&state.access_token_path, token.as_bytes())
        .map_err(|_| ApiError::store())?;
    *state.token_sha256.lock().map_err(|_| ApiError::store())? =
        Sha256::digest(token.as_bytes()).into();
    Ok(Json(AccessTokenResponse { updated: true }))
}

pub(crate) async fn regenerate_access_token(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<RegeneratedAccessTokenResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let token = random_access_token();
    persist_access_token(&state.access_token_path, token.as_bytes())
        .map_err(|_| ApiError::store())?;
    *state.token_sha256.lock().map_err(|_| ApiError::store())? =
        Sha256::digest(token.as_bytes()).into();
    Ok(Json(RegeneratedAccessTokenResponse {
        access_token: token,
    }))
}

pub(crate) async fn embedded_web(uri: Uri) -> Response {
    if uri.path().starts_with("/api/") {
        return (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
            Body::from(
                r#"{"code":"API_NOT_FOUND","message":"请求的接口不存在。","recoverable":false}"#,
            ),
        )
            .into_response();
    }
    let requested = if uri.path() == "/" {
        "/index.html"
    } else {
        uri.path()
    };
    let asset = embedded_asset(requested).or_else(|| embedded_asset("/index.html"));
    match asset {
        Some((bytes, content_type)) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, content_type)],
            Body::from(bytes),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
