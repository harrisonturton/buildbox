use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Runtime(String),
    NotFound(String),
    InvalidArgument(String),
    Io(Option<String>, std::io::Error),
    Boxed(Option<String>, Box<dyn std::error::Error>),
}

impl Error {
    #[must_use]
    pub fn runtime(msg: &str) -> Error {
        Error::Runtime(msg.to_string())
    }

    #[must_use]
    pub fn not_found(msg: &str) -> Error {
        Error::NotFound(msg.to_string())
    }

    #[must_use]
    pub fn invalid(msg: &str) -> Error {
        Error::InvalidArgument(msg.to_string())
    }

    #[must_use]
    pub fn io(err: std::io::Error) -> Error {
        Error::Io(None, err)
    }

    #[must_use]
    pub fn io_msg(msg: &str) -> impl Fn(std::io::Error) -> Error + '_ {
        |err| Error::Io(Some(msg.to_owned()), err)
    }

    #[must_use]
    pub fn boxed(err: impl std::error::Error + 'static) -> Error {
        Error::Boxed(None, Box::new(err))
    }

    #[must_use]
    pub fn boxed_msg<E: std::error::Error + 'static>(msg: &str) -> impl Fn(E) -> Error + '_ {
        |err| Error::Boxed(Some(msg.to_owned()), Box::new(err))
    }
}

impl std::error::Error for Error {}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Runtime(msg) => write!(f, "runtime error: {msg}"),
            Error::NotFound(msg) => write!(f, "not found: {msg}"),
            Error::InvalidArgument(msg) => write!(f, "invalid argument: {msg}"),
            Error::Io(msg, err) => write!(f, "io error: {}{err}", format_msg(msg)),
            Error::Boxed(msg, err) => write!(f, "boxed error: {}{err}", format_msg(msg)),
        }
    }
}

fn format_msg(msg: &Option<String>) -> String {
    msg.clone().map(|msg| format!("{msg}: ")).unwrap_or_default()
}
