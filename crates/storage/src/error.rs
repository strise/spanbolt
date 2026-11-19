/// Status codes Spanner returns from the data and admin paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    InvalidArgument,
    NotFound,
    AlreadyExists,
    FailedPrecondition,
    OutOfRange,
    Aborted,
    Unimplemented,
    Internal,
    DeadlineExceeded,
    ResourceExhausted,
}

/// An error with a gRPC status code and a Spanner-style message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct Error {
    pub code: Code,
    pub message: String,
    /// Constraint violations carry `google.spanner.ConstraintError` in the emulator.
    pub constraint: bool,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(code: Code, message: impl Into<String>) -> Error {
        Error {
            code,
            message: message.into(),
            constraint: false,
        }
    }

    pub fn constraint(code: Code, message: impl Into<String>) -> Error {
        Error {
            code,
            message: message.into(),
            constraint: true,
        }
    }

    pub fn invalid_argument(m: impl Into<String>) -> Error {
        Error::new(Code::InvalidArgument, m)
    }

    pub fn not_found(m: impl Into<String>) -> Error {
        Error::new(Code::NotFound, m)
    }

    pub fn already_exists(m: impl Into<String>) -> Error {
        Error::new(Code::AlreadyExists, m)
    }

    pub fn failed_precondition(m: impl Into<String>) -> Error {
        Error::new(Code::FailedPrecondition, m)
    }

    pub fn aborted(m: impl Into<String>) -> Error {
        Error::new(Code::Aborted, m)
    }

    pub fn internal(m: impl Into<String>) -> Error {
        Error::new(Code::Internal, m)
    }
}

impl From<spanbolt_ddl::DdlError> for Error {
    fn from(e: spanbolt_ddl::DdlError) -> Error {
        use spanbolt_ddl::DdlError as D;
        match e {
            D::Syntax(m) => Error::invalid_argument(m),
            D::Precondition(m) => Error::failed_precondition(m),
            D::NotFound(m) => Error::not_found(m),
            // Spanner reports duplicate schema objects as FAILED_PRECONDITION.
            D::AlreadyExists(m) => Error::failed_precondition(m),
            D::Unimplemented(m) => Error::new(Code::Unimplemented, m),
        }
    }
}
