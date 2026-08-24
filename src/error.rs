/// The error type used throughout Sheetql.
///
/// It wraps a single human-readable message so that existing error text, CLI
/// output and tests are preserved verbatim. A dedicated type (instead of
/// `String`) makes error-returning signatures self-describing and gives us a
/// single place to grow structured variants later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    message: String,
}

impl Error {
    /// Build an error from a message.
    pub fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Whether the message contains `needle` (convenient for assertions).
    #[cfg(test)]
    pub fn contains(&self, needle: &str) -> bool {
        self.message.contains(needle)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<String> for Error {
    fn from(message: String) -> Self {
        Self { message }
    }
}

impl From<&str> for Error {
    fn from(message: &str) -> Self {
        Self {
            message: message.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_prints_the_message() {
        let error = Error::message("boom");
        assert_eq!(error.to_string(), "boom");
    }

    #[test]
    fn converts_from_string_and_str() {
        let error: Error = "boom".to_string().into();
        assert_eq!(error.to_string(), "boom");
        let error: Error = "boom".into();
        assert_eq!(error.to_string(), "boom");
    }

    #[test]
    fn contains_checks_the_message() {
        let error = Error::message("Unknown database `foo`");
        assert!(error.contains("foo"));
        assert!(!error.contains("bar"));
    }

    #[test]
    fn errors_convert_through_the_question_mark_operator() {
        fn inner() -> Result<u8, String> {
            Err("inner failure".to_string())
        }
        fn outer() -> Result<u8, Error> {
            let value = inner()?;
            Ok(value)
        }
        assert_eq!(outer().unwrap_err().to_string(), "inner failure");
    }
}
