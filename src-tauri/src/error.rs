use serde::Serialize;

pub type Result<T> = std::result::Result<T, AppError>;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppError {
    pub code: &'static str,
    pub message: String,
    pub scope: &'static str,
    pub retryable: bool,
}

impl AppError {
    pub fn new(
        code: &'static str,
        scope: &'static str,
        message: impl Into<String>,
        retryable: bool,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            scope,
            retryable,
        }
    }

    pub fn invalid(scope: &'static str, message: impl Into<String>) -> Self {
        Self::new("invalid_input", scope, message, false)
    }

    pub fn database(_: rusqlite::Error) -> Self {
        Self::new(
            "database",
            "storage",
            "The portable database could not be read or updated.",
            true,
        )
    }

    pub fn task(_: tokio::task::JoinError) -> Self {
        Self::new(
            "task_failed",
            "application",
            "The application could not complete this operation.",
            true,
        )
    }

    pub fn qobuz(error: crate::qobuz::QobuzError, scope: &'static str) -> Self {
        Self::new(error.code, scope, error.message, error.retryable)
    }

    pub fn media(error: crate::media::MediaError, scope: &'static str) -> Self {
        Self::new(error.code, scope, error.message, error.retryable)
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AppError {}
