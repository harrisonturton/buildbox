
pub type Result<T> = std::result::Result<T, Error>; 

#[derive(thiserror::Error, Debug)]
pub enum Error {
  #[error("invalid argument: {0}")]
  InvalidArgument(String),
  #[error("io error: {0}")]
  Io(std::io::Error),
  #[error("error: {0}")]
  Boxed(Box<dyn std::error::Error>),
}

impl Error {
  #[must_use]
  pub fn invalid(msg: &str) -> Error {
    Error::InvalidArgument(msg.to_string())
  }

  #[must_use]
  pub fn io(err: std::io::Error) -> Error {
    Error::Io(err)
  }

  #[must_use]
  pub fn boxed(err: impl std::error::Error + 'static) -> Error {
    Error::Boxed(Box::new(err))
  }
}