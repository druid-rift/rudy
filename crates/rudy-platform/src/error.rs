use thiserror::Error;

#[derive(Error, Debug)]
pub enum PlatformError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Authorization for a privileged operation was not obtained.
    ///
    /// Carries a sentence, not a D-Bus error name. Dismissing the polkit prompt
    /// is the most common non-success outcome of an install since ADR 0003 made
    /// udisks2 the thing that asks, and rendering
    /// `org.freedesktop.UDisks2.Error.NotAuthorizedDismissed` at a user is not
    /// reporting it.
    #[error("{0}")]
    AuthorizationUnavailable(String),

    #[error("Platform specific error: {0}")]
    Other(String),
}

/// The message of `error` and of every cause beneath it, outermost first.
///
/// A cause whose text its wrapper already ends with is dropped. Many errors in
/// this tree render their source into their own message *and* return it from
/// `source()` — [`PlatformError::Io`] is one — so a plain walk printed the same
/// cause twice. Rewriting every such type is a larger change than the duplicate
/// is worth; this keeps a chain readable whichever convention a type follows.
///
/// Presentation stays with each client: the CLI prints a line per message, the
/// GUI joins them into one sentence (AR-17).
pub fn error_chain(error: &dyn std::error::Error) -> Vec<String> {
    let mut chain = vec![error.to_string()];
    let mut next = error.source();
    while let Some(cause) = next {
        let message = cause.to_string();
        if !chain
            .last()
            .is_some_and(|previous| previous.ends_with(&message))
        {
            chain.push(message);
        }
        next = cause.source();
    }
    chain
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cause_its_wrapper_already_quotes_appears_once() {
        let error = PlatformError::Io(std::io::Error::other("permission denied"));
        assert_eq!(error_chain(&error), ["I/O error: permission denied"]);
    }
}
