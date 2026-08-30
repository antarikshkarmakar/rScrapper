use url::Url;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("policy violation: {0}")]
    Policy(String),
    #[error("DNS error: {0}")]
    Dns(String),
    #[error("{operation} timed out")]
    Timeout { operation: &'static str },
    #[error("response body exceeded {limit} byte limit")]
    BodyLimit { limit: usize },
    #[error("HTTP status {status} for {url}")]
    HttpStatus { status: u16, url: Url },
    #[error("browser error: {0}")]
    Browser(String),
    #[error("{kind} parse error: {message}")]
    Parse { kind: &'static str, message: String },
    #[error("authentication error: {0}")]
    Authentication(String),
    #[error("rate limited")]
    RateLimited { retry_after_secs: Option<u64> },
    #[error("robots policy denied {0}")]
    RobotsDenied(Url),
    #[error("operation cancelled")]
    Cancelled,
    #[error("upstream layout changed for {service}")]
    UpstreamLayout { service: &'static str },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
}
