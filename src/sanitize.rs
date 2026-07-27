//! The crate's single control-character sanitization pass.
//!
//! Every string rendered ultimately comes from somewhere the process does not
//! control — worker log tails, run event JSON, service stdout and stderr — so
//! untrusted bytes are stripped of terminal control sequences exactly once,
//! here, rather than re-implemented per adapter.
//!
//! Feature-independent by design (matches `crate::process`'s own doc note):
//! `CommandMusterrollClient` in `musterroll.rs` needs sanitized failure
//! detail in a `--no-default-features` build, so this cannot live under the
//! `tui`-gated `dashboard` subtree even though the dashboard is its heaviest
//! user.
//!
//! Stripping every control character subsumes leading partial escape removal:
//! a tail that starts mid-sequence loses its `ESC`, so the remainder renders as
//! inert text instead of steering the terminal.

/// Strips every control character, including newlines, from a value rendered
/// on one line: a provider name, timestamp, commit, path, or summary.
pub(crate) fn sanitize_single_line(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_control())
        .collect()
}

/// Strips every control character except newline, which is preserved so
/// multi-line text (log tails, stderr coverage summaries) keeps its line
/// structure. Used only by the `tui`-gated dashboard today (musterroll's
/// own failure detail is single-line), hence the dead-code allow in a
/// `--no-default-features` build.
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
pub(crate) fn sanitize_text(text: &str) -> String {
    text.chars()
        .filter(|&character| character == '\n' || !character.is_control())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two passes differ only on newline; both must defuse an escape
    /// sequence by removing the `ESC` that arms it.
    #[test]
    fn both_passes_defuse_escapes_and_differ_only_on_newline() {
        let hostile = "red\u{1b}[31m\u{7}alert\nnext\r\u{0}";

        assert_eq!(sanitize_single_line(hostile), "red[31malertnext");
        assert_eq!(sanitize_text(hostile), "red[31malert\nnext");
    }

    /// A tail cut mid-escape leaves an orphaned prefix; dropping the `ESC`
    /// renders the remainder as text rather than a partial command.
    #[test]
    fn leading_partial_escape_is_defused() {
        assert_eq!(sanitize_text("\u{1b}[2Jcleared"), "[2Jcleared");
    }
}
