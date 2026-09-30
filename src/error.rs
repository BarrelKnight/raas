use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;
use tracing::{error, warn};

/// API 错误响应
#[derive(Debug, thiserror::Error)]
pub enum ArchiveApiError {
    #[error("{0}")]
    BadRequest(String),

    #[error("{0}")]
    InternalError(#[from] anyhow::Error),
}

impl From<std::io::Error> for ArchiveApiError {
    fn from(err: std::io::Error) -> Self {
        ArchiveApiError::InternalError(anyhow::Error::from(err))
    }
}

impl From<crate::archive::ArchiveError> for ArchiveApiError {
    fn from(err: crate::archive::ArchiveError) -> Self {
        ArchiveApiError::InternalError(anyhow::Error::from(err))
    }
}

impl IntoResponse for ArchiveApiError {
    fn into_response(self) -> Response {
        let error_msg = self.to_string();

        let (status_code, error_type) = match &self {
            ArchiveApiError::BadRequest(_) => {
                // 客户端错误属于预期情况,记 warn 而非 error
                warn!(error_type = "BadRequest", "{}", error_msg);
                (StatusCode::BAD_REQUEST, "BadRequest")
            }
            ArchiveApiError::InternalError(_) => {
                error!(error_type = "InternalError", "{}", error_msg);
                (StatusCode::INTERNAL_SERVER_ERROR, "InternalError")
            }
        };

        (
            status_code,
            Json(json!({
                "success": false,
                "error": {
                    "type": error_type,
                    "message": error_msg,
                },
            })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[test]
    fn test_bad_request_error() {
        let error = ArchiveApiError::BadRequest("测试错误".to_string());
        let response = error.into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_internal_error() {
        let io_error = std::io::Error::other("IO错误");
        let error = ArchiveApiError::from(io_error);
        let response = error.into_response();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn test_from_anyhow_error() {
        let anyhow_err = anyhow::anyhow!("自定义错误");
        let error = ArchiveApiError::InternalError(anyhow_err);
        let response = error.into_response();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn test_from_random_access_archive_error() {
        use crate::archive::ArchiveError;
        let io_error = std::io::Error::new(std::io::ErrorKind::NotFound, "文件不存在");
        let archive_error = ArchiveError::Io(io_error);
        let error = ArchiveApiError::from(archive_error);
        let response = error.into_response();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
