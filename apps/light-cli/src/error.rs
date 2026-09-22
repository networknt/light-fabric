//! Errors, and the process exit codes scripts branch on.

/// Exit codes. Stable: the daily gate and shell scripts depend on them.
pub mod exit {
    pub const OK: u8 = 0;
    /// Bad configuration or an unexpected internal failure.
    pub const FAILED: u8 = 1;
    /// A credential or proof was refused (HTTP 401/403).
    pub const DENIED: u8 = 3;
    // 4 was "not enrolled" while the CLI held a certificate. It is retired, not reused.
    /// A service could not be reached, so the outcome is uncertain.
    pub const UNREACHABLE: u8 = 5;
    /// There is no valid user login: run `/login`.
    pub const LOGIN_REQUIRED: u8 = 6;
}

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("{0}")]
    Config(String),
    #[error("denied: {0}")]
    Denied(String),
    #[error("unreachable: {0}")]
    Unreachable(String),
    #[error("sign-in required: {0}")]
    LoginRequired(String),
    #[error("{0}")]
    Failed(String),
}

impl CliError {
    pub fn exit_code(&self) -> u8 {
        match self {
            CliError::Config(_) | CliError::Failed(_) => exit::FAILED,
            CliError::Denied(_) => exit::DENIED,
            CliError::Unreachable(_) => exit::UNREACHABLE,
            CliError::LoginRequired(_) => exit::LOGIN_REQUIRED,
        }
    }
}

impl From<std::io::Error> for CliError {
    fn from(error: std::io::Error) -> Self {
        CliError::Failed(format!("i/o error: {error}"))
    }
}
