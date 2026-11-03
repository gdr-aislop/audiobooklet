//! TEMP(stall-hunt): environment switches for finding where the Library's stalls come from, to be
//! deleted once the numbers are in. Unset (the normal case) they change nothing.
//!
//!   ABS_LIB_LIMIT=N   build at most N Library entries
//!   ABS_LIB_LAZY=1    don't render the Library until its page is shown
//!
//! Read once at startup; when any is set, one log line says so. Removing this feature means
//! deleting this file, its `mod` line and call in `main.rs`, and the two places in
//! `screens/library.rs` tagged `TEMP(stall-hunt)`.

use std::sync::OnceLock;

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Switches {
    pub(crate) library_limit: Option<usize>,
    pub(crate) library_lazy: bool,
}

fn parse(limit: Option<&str>, lazy: Option<&str>) -> Switches {
    Switches {
        library_limit: limit.and_then(|value| value.trim().parse().ok()),
        library_lazy: lazy.is_some_and(|value| matches!(value.trim(), "1" | "true" | "yes")),
    }
}

/// The switches, read from the environment on first use (and said in the log if any is on).
pub(crate) fn get() -> &'static Switches {
    static SWITCHES: OnceLock<Switches> = OnceLock::new();
    SWITCHES.get_or_init(|| {
        let switches = parse(std::env::var("ABS_LIB_LIMIT").ok().as_deref(), std::env::var("ABS_LIB_LAZY").ok().as_deref());
        if switches != Switches::default() {
            tracing::info!(library_limit = ?switches.library_limit, library_lazy = switches.library_lazy, "debug switches active");
        }
        switches
    })
}

/// Cuts `entries` down to `limit` of them, when there is one.
pub(crate) fn limit_entries<T>(entries: &mut Vec<T>, limit: Option<usize>) {
    if let Some(limit) = limit {
        entries.truncate(limit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switches_parse_and_default_to_off() {
        assert_eq!(parse(None, None), Switches::default());
        assert_eq!(parse(Some("40"), Some("1")), Switches { library_limit: Some(40), library_lazy: true });
        assert_eq!(parse(Some(" 7 "), Some("true")), Switches { library_limit: Some(7), library_lazy: true });
        assert_eq!(parse(Some("many"), Some("0")), Switches::default(), "nonsense and 0 mean off");
    }

    #[test]
    fn a_limit_cuts_the_entries_and_no_limit_leaves_them() {
        let mut entries: Vec<u32> = (0..10).collect();
        limit_entries(&mut entries, None);
        assert_eq!(entries.len(), 10);
        limit_entries(&mut entries, Some(4));
        assert_eq!(entries, vec![0, 1, 2, 3]);
        limit_entries(&mut entries, Some(100));
        assert_eq!(entries.len(), 4);
    }
}
