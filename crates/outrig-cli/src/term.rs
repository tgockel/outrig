//! The one answer to "can this terminal take a rich interface".
//!
//! `outrig run` asks it before opening the line editor and `outrig init` (and
//! `config init`, `image add`) before opening dialoguer's arrow-key pickers.

/// Terminals that cannot take a rich interface. On any of these rustyline
/// takes its `readline_direct` path, which writes the prompt to *stdout* -- so
/// the check has to happen out here, before an editor exists, or
/// `TERM=dumb outrig run > out.txt` would put `> ` in the captured output.
/// They also cannot honor the cursor movement dialoguer's `FuzzySelect` and
/// `MultiSelect` redraw with, so the wizards read plain lines instead.
///
/// This mirrors `UNSUPPORTED_TERM` in rustyline's `src/tty/mod.rs`, which is
/// private, as is the `Term::is_unsupported` that reads it. There is no public
/// way to ask -- `Editor::dimensions()` keys off `is_output_tty`, which under
/// `PreferTerm` answers for the `/dev/tty` rustyline opened and so says "fine"
/// on `TERM=dumb`. Predicting a private list is the only option available, so
/// **re-check it on every rustyline upgrade**: if upstream adds a name, outrig
/// hands that terminal an editor whose prompt lands in redirected stdout, and
/// no test here can see it.
const UNSUPPORTED_TERMS: [&str; 3] = ["dumb", "cons25", "emacs"];

/// Whether a terminal named by `$TERM` can take a rich interface. An unset
/// `TERM` is assumed capable.
pub(crate) fn is_rich(term: Option<&str>) -> bool {
    !term.is_some_and(|term| {
        UNSUPPORTED_TERMS
            .iter()
            .any(|unsupported| unsupported.eq_ignore_ascii_case(term))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_terminals_are_rejected_whatever_their_case() {
        for term in ["dumb", "DUMB", "Dumb", "cons25", "CONS25", "emacs", "Emacs"] {
            assert!(!is_rich(Some(term)), "TERM={term} must not be rich");
        }
    }

    #[test]
    fn a_usable_terminal_is_accepted() {
        for term in [Some("xterm-256color"), Some("screen"), Some("dumber"), None] {
            assert!(is_rich(term), "TERM={term:?} must be rich");
        }
    }
}
