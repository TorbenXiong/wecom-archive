use crate::{AppState, ErrorResponse};
use archive_store::StoreError;
use axum::Json;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;

#[derive(Debug)]
pub(super) struct ApiError {
    pub(super) status: StatusCode,
    pub(super) body: ErrorResponse,
}

pub(super) fn is_local_source_not_found(message: &str) -> bool {
    message == "未发现可支持的本机企业微信数据，请确认客户端已登录。"
        || message == "这个文件夹里没找到企业微信数据库，请填企业微信实际存放聊天记录的那一层目录。"
        || message
            == "数据目录无效或无法访问，请填写本机磁盘上的企业微信存储目录（不能是盘符根目录）。"
}

pub(super) fn local_collection_error(message: String) -> ApiError {
    if is_local_source_not_found(&message) {
        ApiError::local_source_not_found(message)
    } else {
        ApiError::local_collection_failed(message)
    }
}

impl ApiError {
    pub(super) fn forbidden() -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            body: ErrorResponse {
                code: "SUPER_ADMIN_REQUIRED",
                message: "当前未启用超管模式，请在设置中开启后再访问会话内容。".into(),
                recoverable: true,
            },
        }
    }

    pub(super) fn not_found() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            body: ErrorResponse {
                code: "MEDIA_NOT_FOUND",
                message: "媒体内容不存在。".into(),
                recoverable: true,
            },
        }
    }

    pub(super) fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            body: ErrorResponse {
                code: "UNAUTHORIZED",
                message: "需要有效的服务端访问令牌。".into(),
                recoverable: true,
            },
        }
    }

    pub(super) fn invalid_export() -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            body: ErrorResponse {
                code: "INVALID_CLIENT_EXPORT",
                message: "客户端 JSON 格式、关系或校验值无效。".into(),
                recoverable: true,
            },
        }
    }

    pub(super) fn too_large() -> Self {
        Self {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            body: ErrorResponse {
                code: "CLIENT_EXPORT_TOO_LARGE",
                message: "客户端 JSON 超过服务端单次导入上限。".into(),
                recoverable: true,
            },
        }
    }

    pub(super) fn invalid_query() -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            body: ErrorResponse {
                code: "INVALID_QUERY",
                message: "检索参数无效。".into(),
                recoverable: true,
            },
        }
    }

    pub(super) fn local_collection_unavailable() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            body: ErrorResponse {
                code: "LOCAL_COLLECTION_UNAVAILABLE",
                message: "当前访问方式不支持采集本机数据，请在桌面工作台中使用。".into(),
                recoverable: true,
            },
        }
    }

    pub(super) fn local_collection_running() -> Self {
        Self {
            status: StatusCode::CONFLICT,
            body: ErrorResponse {
                code: "LOCAL_COLLECTION_RUNNING",
                message: "已有本机采集任务正在运行，请稍后重试。".into(),
                recoverable: true,
            },
        }
    }

    pub(super) fn local_collection_failed(message: String) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            body: ErrorResponse {
                code: "LOCAL_COLLECTION_FAILED",
                message,
                recoverable: true,
            },
        }
    }

    pub(super) fn local_source_not_found(message: String) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            body: ErrorResponse {
                code: "LOCAL_SOURCE_NOT_FOUND",
                message,
                recoverable: true,
            },
        }
    }

    pub(super) fn store() -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: ErrorResponse {
                code: "ARCHIVE_STORE_FAILED",
                message: "服务端归档写入失败，诊断信息已脱敏。".into(),
                recoverable: true,
            },
        }
    }

    pub(super) fn collector_directory_missing() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            body: ErrorResponse {
                code: "COLLECTOR_DIRECTORY_MISSING",
                message: "暂未生成过采集端，请先生成采集端。".into(),
                recoverable: true,
            },
        }
    }

    pub(super) fn from_store(error: StoreError) -> Self {
        if matches!(error, StoreError::BatchConflict) {
            Self {
                status: StatusCode::CONFLICT,
                body: ErrorResponse {
                    code: "BATCH_CONFLICT",
                    message: "同一采集批次标识已存在，但内容校验值不同。".into(),
                    recoverable: false,
                },
            }
        } else {
            Self::store()
        }
    }

    pub(super) fn from_export(error: archive_export::ExportError) -> Self {
        let _ = error;
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: ErrorResponse {
                code: "ARCHIVE_EXPORT_FAILED",
                message: "服务端导出失败；未完成文件已清理。".into(),
                recoverable: true,
            },
        }
    }
}

pub(super) fn require_super_admin(state: &AppState) -> Result<(), ApiError> {
    let enabled = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?
        .super_admin_enabled;
    enabled.then_some(()).ok_or_else(ApiError::forbidden)
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}
