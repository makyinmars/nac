use axum::{
    extract::{rejection::JsonRejection, Path as AxumPath, Query, State},
    http::StatusCode,
    Json,
};
use nac_core::{session_service::SessionFrontendSnapshot, sessions};

use crate::{
    ApiError, ApiErrorBody, ClaudeStatusQuery, ClaudeStatusResponse, CreateSessionRequest,
    ReplyClaudePermissionRequest, SessionManager, UpdateConfigRequest,
};

#[utoipa::path(
    get,
    path = "/claude/status",
    operation_id = "get_claude_status",
    tag = "sessions",
    params(ClaudeStatusQuery),
    responses((status = 200, description = "Claude Code availability on the selected host", body = ClaudeStatusResponse, content_type = "application/json"))
)]
pub(crate) async fn claude_status_handler(
    State(manager): State<SessionManager>,
    Query(query): Query<ClaudeStatusQuery>,
) -> std::result::Result<Json<ClaudeStatusResponse>, ApiError> {
    Ok(Json(manager.claude_status(query).await?))
}

#[utoipa::path(
    post,
    path = "/sessions",
    operation_id = "post_sessions",
    tag = "sessions",
    request_body(content = CreateSessionRequest, content_type = "application/json"),
    responses((status = 201, description = "Success", body = SessionFrontendSnapshot, content_type = "application/json"), (status = 400, description = "Request failed", body = ApiErrorBody, content_type = "application/json"), (status = 404, description = "Request failed", body = ApiErrorBody, content_type = "application/json"), (status = 409, description = "Request failed", body = ApiErrorBody, content_type = "application/json"), (status = 500, description = "Request failed", body = ApiErrorBody, content_type = "application/json"))
)]
pub(crate) async fn create_session(
    State(manager): State<SessionManager>,
    payload: std::result::Result<Json<CreateSessionRequest>, JsonRejection>,
) -> std::result::Result<(StatusCode, Json<SessionFrontendSnapshot>), ApiError> {
    let Json(request) = payload.map_err(ApiError::from)?;
    Ok((
        StatusCode::CREATED,
        Json(manager.create_session(request).await?),
    ))
}

#[utoipa::path(
    delete,
    path = "/sessions/{session_id}",
    operation_id = "delete_sessions_session_id",
    tag = "sessions",
    params(("session_id" = String, Path)),
    responses((status = 200, description = "Success with no response body"), (status = 400, description = "Path extraction failed", body = String, content_type = "text/plain"), (status = 404, description = "Request failed", body = ApiErrorBody, content_type = "application/json"), (status = 409, description = "Request failed", body = ApiErrorBody, content_type = "application/json"), (status = 500, description = "Request failed", body = ApiErrorBody, content_type = "application/json"))
)]
pub(crate) async fn delete_session_handler(
    State(manager): State<SessionManager>,
    AxumPath(session_id): AxumPath<String>,
) -> std::result::Result<StatusCode, ApiError> {
    manager.delete_session(&session_id).await?;
    Ok(StatusCode::OK)
}

#[utoipa::path(
    get,
    path = "/sessions/{session_id}/skills",
    operation_id = "get_sessions_session_id_skills",
    tag = "sessions",
    params(("session_id" = String, Path)),
    responses((status = 200, description = "Success", body = Vec<nac_core::skill_catalog::SkillCatalogEntry>, content_type = "application/json"), (status = 400, description = "Path extraction failed", body = String, content_type = "text/plain"), (status = 404, description = "Request failed", body = ApiErrorBody, content_type = "application/json"), (status = 500, description = "Request failed", body = ApiErrorBody, content_type = "application/json"))
)]
pub(crate) async fn session_skills_handler(
    State(manager): State<SessionManager>,
    AxumPath(session_id): AxumPath<String>,
) -> std::result::Result<Json<Vec<nac_core::skill_catalog::SkillCatalogEntry>>, ApiError> {
    Ok(Json(manager.session_skills(&session_id).await?))
}

#[utoipa::path(
    get,
    path = "/sessions/{session_id}/config",
    operation_id = "get_sessions_session_id_config",
    tag = "sessions",
    params(("session_id" = String, Path)),
    responses((status = 200, description = "Success", body = sessions::RawSessionConfig, content_type = "application/json"), (status = 400, description = "Path extraction failed", body = String, content_type = "text/plain"), (status = 404, description = "Request failed", body = ApiErrorBody, content_type = "application/json"), (status = 500, description = "Request failed", body = ApiErrorBody, content_type = "application/json"))
)]
pub(crate) async fn session_config_handler(
    State(manager): State<SessionManager>,
    AxumPath(session_id): AxumPath<String>,
) -> std::result::Result<Json<sessions::RawSessionConfig>, ApiError> {
    Ok(Json(manager.session_config(&session_id)?))
}

#[utoipa::path(
    patch,
    path = "/sessions/{session_id}/config",
    operation_id = "patch_sessions_session_id_config",
    tag = "sessions",
    params(("session_id" = String, Path)),
    request_body(content = UpdateConfigRequest, content_type = "application/json"),
    responses((status = 200, description = "Success with no response body"), (status = 400, description = "Bad request or rejected path/query/body extraction", content((ApiErrorBody = "application/json"), (String = "text/plain"))), (status = 404, description = "Request failed", body = ApiErrorBody, content_type = "application/json"), (status = 409, description = "Request failed", body = ApiErrorBody, content_type = "application/json"), (status = 500, description = "Request failed", body = ApiErrorBody, content_type = "application/json"))
)]
pub(crate) async fn update_config_handler(
    State(manager): State<SessionManager>,
    AxumPath(session_id): AxumPath<String>,
    payload: std::result::Result<Json<UpdateConfigRequest>, JsonRejection>,
) -> std::result::Result<StatusCode, ApiError> {
    let Json(request) = payload.map_err(ApiError::from)?;
    manager.update_session_config(&session_id, request).await?;
    Ok(StatusCode::OK)
}

#[utoipa::path(
    post,
    path = "/sessions/{session_id}/claude-worker-trust",
    operation_id = "post_sessions_session_id_claude_worker_trust",
    tag = "sessions",
    params(("session_id" = String, Path)),
    responses((status = 200, description = "Workspace trusted for Claude workers"), (status = 400, description = "Unsupported session", body = ApiErrorBody, content_type = "application/json"), (status = 404, description = "Session not found", body = ApiErrorBody, content_type = "application/json"), (status = 500, description = "Request failed", body = ApiErrorBody, content_type = "application/json"))
)]
pub(crate) async fn trust_claude_worker_workspace_handler(
    State(manager): State<SessionManager>,
    AxumPath(session_id): AxumPath<String>,
) -> std::result::Result<StatusCode, ApiError> {
    manager.trust_claude_worker_workspace(&session_id).await?;
    Ok(StatusCode::OK)
}

#[utoipa::path(
    get,
    path = "/sessions/{session_id}/claude-permissions",
    operation_id = "get_sessions_session_id_claude_permissions",
    tag = "sessions",
    params(("session_id" = String, Path)),
    responses((status = 200, description = "Pending Claude tool approvals", body = Vec<nac_core::claude_approval::ClaudePermissionRequest>, content_type = "application/json"), (status = 404, description = "Session not found", body = ApiErrorBody, content_type = "application/json"))
)]
pub(crate) async fn claude_permissions_handler(
    State(manager): State<SessionManager>,
    AxumPath(session_id): AxumPath<String>,
) -> std::result::Result<Json<Vec<nac_core::claude_approval::ClaudePermissionRequest>>, ApiError> {
    Ok(Json(manager.claude_permission_requests(&session_id).await?))
}

#[utoipa::path(
    post,
    path = "/sessions/{session_id}/claude-permissions/{request_id}/reply",
    operation_id = "post_sessions_session_id_claude_permissions_request_id_reply",
    tag = "sessions",
    params(("session_id" = String, Path), ("request_id" = String, Path)),
    request_body(content = ReplyClaudePermissionRequest, content_type = "application/json"),
    responses((status = 200, description = "Claude permission replied"), (status = 400, description = "Invalid reply", body = ApiErrorBody, content_type = "application/json"), (status = 404, description = "Session not found", body = ApiErrorBody, content_type = "application/json"), (status = 409, description = "Stale approval", body = ApiErrorBody, content_type = "application/json"))
)]
pub(crate) async fn reply_claude_permission_handler(
    State(manager): State<SessionManager>,
    AxumPath((session_id, request_id)): AxumPath<(String, String)>,
    payload: std::result::Result<Json<ReplyClaudePermissionRequest>, JsonRejection>,
) -> std::result::Result<StatusCode, ApiError> {
    let Json(request) = payload.map_err(ApiError::from)?;
    manager
        .reply_claude_permission_request(&session_id, &request_id, request)
        .await?;
    Ok(StatusCode::OK)
}
