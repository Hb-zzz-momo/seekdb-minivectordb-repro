use std::fmt;

/// Error categories, reported to clients by `name`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Syntax,
    Unsupported,
    TableExists,
    NoSuchTable,
    NoSuchColumn,
    DuplicateKey,
    /// INSERT row has a different number of values than target columns.
    ColumnCount,
    /// INSERT column list leaves out a column (there are no defaults).
    NoDefault,
    TypeMismatch,
    OutOfRange,
    DataTooLong,
    VectorDim,
    InvalidValue,
    Schema,
    TxnState,
    Io,
    Corrupt,
    Internal,
}

impl ErrorKind {
    /// Semantic error name, in the style of the public cases' `error`
    /// assertions (`duplicate_key`, `unsupported_sql`, `dimension_mismatch`;
    /// the other names are ours until the official mapping is frozen).
    pub fn name(self) -> &'static str {
        match self {
            ErrorKind::Syntax => "syntax_error",
            ErrorKind::Unsupported => "unsupported_sql",
            ErrorKind::TableExists => "table_exists",
            ErrorKind::NoSuchTable => "table_not_found",
            ErrorKind::NoSuchColumn => "column_not_found",
            ErrorKind::DuplicateKey => "duplicate_key",
            ErrorKind::ColumnCount => "column_count_mismatch",
            ErrorKind::NoDefault => "missing_value",
            ErrorKind::TypeMismatch => "type_mismatch",
            ErrorKind::OutOfRange => "out_of_range",
            ErrorKind::DataTooLong => "data_too_long",
            ErrorKind::VectorDim => "dimension_mismatch",
            ErrorKind::InvalidValue => "invalid_value",
            ErrorKind::Schema => "invalid_schema",
            ErrorKind::TxnState => "transaction_state",
            ErrorKind::Io => "io_error",
            ErrorKind::Corrupt => "corrupt_data",
            ErrorKind::Internal => "internal_error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DbError {
    pub kind: ErrorKind,
    pub msg: String,
}

impl DbError {
    pub fn new(kind: ErrorKind, msg: impl Into<String>) -> Self {
        DbError { kind, msg: msg.into() }
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind.name(), self.msg)
    }
}

impl std::error::Error for DbError {}

impl From<std::io::Error> for DbError {
    fn from(e: std::io::Error) -> Self {
        DbError::new(ErrorKind::Io, e.to_string())
    }
}

pub type DbResult<T> = Result<T, DbError>;

/// Builds a `DbError` of the given kind with a formatted message.
#[macro_export]
macro_rules! db_err {
    ($kind:ident, $($arg:tt)*) => {
        $crate::error::DbError::new($crate::error::ErrorKind::$kind, format!($($arg)*))
    };
}

/// Returns early with a `DbError` of the given kind.
#[macro_export]
macro_rules! bail {
    ($kind:ident, $($arg:tt)*) => {
        return Err($crate::db_err!($kind, $($arg)*))
    };
}
