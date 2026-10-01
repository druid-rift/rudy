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

/// Renders a fatal failure the way the user sees it, in the CLI and the GUI
/// alike (the GUI's `main` returned `Box<dyn Error>` and printed neither the
/// prefix nor the causes until PRV-09).
///
/// The prefix is `rudy: ` and deliberately **not** `rudy: error:` — that belongs
/// to the boot payload's serial console (`CONTEXT.md` §4) and is scanned as a
/// fatal signature by `boot_evidence.py` and `SerialLogAnalyzer`. A host-side
/// error wearing it would forge boot evidence.
///
/// The `source()` chain is walked because `Box<dyn Error>`'s default rendering
/// through `Termination` drops it, which is how "cannot open" used to reach the
/// user without the `errno` that said why.
pub fn render_fatal(error: &dyn std::error::Error) -> String {
    // One line per message in the chain, and one per line of a message that
    // spans several: an install failure is a kind with its cause beneath it, and
    // a cause is often external text — a D-Bus reply, a path — that can carry
    // newlines. Every line is stripped, not just the first, or a later line could
    // open with the payload's prefix and forge boot evidence (AR-17).
    let mut lines = Vec::new();
    for (depth, message) in error_chain(error).iter().enumerate() {
        for (index, line) in message.split('\n').enumerate() {
            let lead = match (depth, index) {
                (0, 0) => "rudy: ",
                (_, 0) => "  caused by: ",
                _ => "    ",
            };
            lines.push(format!("{lead}{}", without_payload_prefix(line)));
        }
    }
    lines.join("\n")
}

/// `line` with any leading `error:` or payload prefix removed, as often as it
/// repeats.
///
/// A message that already opens with "error:" would otherwise render as exactly
/// `rudy: error:` — the payload's signature, forged by accident — and stripping
/// once still leaves one behind for "error: error: x", which is what a
/// wrapped-then-rewrapped message looks like. `get` rather than indexing, so a
/// multibyte message is never sliced mid-character.
fn without_payload_prefix(line: &str) -> &str {
    let mut head = line.trim_start();
    loop {
        let stripped = ["error:", rudy_core::diagnostics::payload_error_prefix()]
            .into_iter()
            .find_map(|prefix| {
                head.get(..prefix.len())
                    .filter(|start| start.eq_ignore_ascii_case(prefix))
                    .and_then(|_| head.get(prefix.len()..))
                    .map(str::trim_start)
            });
        match stripped {
            Some(rest) => head = rest,
            None => return head,
        }
    }
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
