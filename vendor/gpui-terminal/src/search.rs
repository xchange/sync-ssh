//! Search the terminal's rendered cells, including scrollback and soft wraps.
//! Regex compilation and matching run on an independent grid snapshot, never
//! while holding the live terminal lock. Search never modifies the copy selection.
use alacritty_terminal::{
    event::VoidListener,
    grid::{Dimensions, Grid},
    index::{Column, Direction, Point},
    term::{
        Config, Term,
        cell::Cell,
        search::{Match, RegexIter, RegexSearch},
    },
};
use std::time::{Duration, Instant};

pub const MAX_SEARCH_MATCHES: usize = 10_000;
const MAX_QUERY_BYTES: usize = 1024;
const MATCH_BUDGET: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchOptions {
    pub query: String,
    pub regex: bool,
    pub case_sensitive: bool,
}

#[derive(Clone, Debug, Default)]
pub struct SearchSummary {
    pub matches: usize,
    /// One-based current result, or zero when there is no result.
    pub current: usize,
    pub searching: bool,
    /// The displayed count is a lower bound when the work/result limit was hit.
    pub limited: bool,
    pub error: Option<String>,
}

#[derive(Default)]
pub(crate) struct SearchResult {
    pub matches: Vec<Match>,
    pub limited: bool,
    pub error: Option<String>,
}

/// Alacritty accounts for double-width cells, continuation spacers and wrapped
/// rows, so offsets remain terminal coordinates instead of UTF-8 byte offsets.
pub(crate) fn search_grid(grid: Grid<Cell>, options: &SearchOptions) -> SearchResult {
    if options.query.is_empty() {
        return SearchResult::default();
    }
    if options.query.len() > MAX_QUERY_BYTES {
        return SearchResult {
            error: Some(format!("搜索内容不能超过 {MAX_QUERY_BYTES} 字节。")),
            ..Default::default()
        };
    }
    let pattern = if options.regex {
        options.query.clone()
    } else {
        literal_pattern(&options.query)
    };
    // Override Alacritty's default smart-case behavior explicitly.
    let pattern = format!(
        "(?{}i:{pattern})",
        if options.case_sensitive { "-" } else { "" }
    );
    let mut regex = match RegexSearch::new(&pattern) {
        Ok(regex) => regex,
        Err(_) => {
            return SearchResult {
                error: Some("正则表达式无效或过于复杂。".into()),
                ..Default::default()
            };
        }
    };
    let start = Point::new(grid.topmost_line(), Column(0));
    let end = Point::new(grid.bottommost_line(), grid.last_column());
    let mut term = Term::new(Config::default(), &grid, VoidListener);
    *term.grid_mut() = grid;
    let deadline = Instant::now() + MATCH_BUDGET;
    let mut matches = Vec::new();
    let mut limited = false;
    for found in RegexIter::new(start, end, Direction::Right, &term, &mut regex) {
        if matches.len() == MAX_SEARCH_MATCHES || Instant::now() >= deadline {
            limited = true;
            break;
        }
        // Include the continuation cell when the match ends in a wide glyph.
        // Matching still uses Alacritty's original range to advance the iterator.
        matches.push(*found.start()..=term.expand_wide(*found.end(), Direction::Right));
    }
    SearchResult {
        matches,
        limited,
        error: None,
    }
}

fn literal_pattern(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(
            ch,
            '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|'
        ) {
            result.push('\\');
        }
        result.push(ch);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{MAX_SEARCH_MATCHES, SearchOptions, search_grid};
    use crate::{GpuiEventProxy, TerminalState};
    use alacritty_terminal::{
        grid::Dimensions,
        index::{Column, Line, Point},
    };
    use std::sync::mpsc;

    fn terminal(cols: usize, rows: usize, bytes: &[u8]) -> TerminalState {
        let (tx, _) = mpsc::channel();
        let mut terminal =
            TerminalState::new_with_scrollback(cols, rows, 10_000, GpuiEventProxy::new(tx));
        terminal.process_bytes(bytes);
        terminal
    }

    fn matches(
        terminal: &TerminalState,
        query: &str,
        regex: bool,
        case_sensitive: bool,
    ) -> super::SearchResult {
        search_grid(
            terminal.with_term(|t| t.grid().clone()),
            &SearchOptions {
                query: query.into(),
                regex,
                case_sensitive,
            },
        )
    }

    #[test]
    fn literal_metacharacters_and_explicit_case_are_respected() {
        let terminal = terminal(40, 3, b"Foo foo FOO a.b axb [x]");
        assert_eq!(matches(&terminal, "foo", false, false).matches.len(), 3);
        assert_eq!(matches(&terminal, "foo", false, true).matches.len(), 1);
        assert_eq!(matches(&terminal, "a.b", false, true).matches.len(), 1);
        assert_eq!(matches(&terminal, "a.b", true, true).matches.len(), 2);
        assert_eq!(matches(&terminal, "[x]", false, true).matches.len(), 1);
    }

    #[test]
    fn unicode_wide_cells_soft_wraps_and_history_have_grid_coordinates() {
        let terminal = terminal(8, 2, "前中文abc\r\nsecond\r\nthird\r\nlast".as_bytes());
        let result = matches(&terminal, "中文abc", false, true);
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].start().column, Column(2));
        assert!(
            result.matches[0].start().line < Line(0),
            "search must include history"
        );
        assert_eq!(result.matches[0].end().column, Column(0));
        assert_eq!(
            result.matches[0].end().line.0,
            result.matches[0].start().line.0 + 1
        );
        assert_eq!(matches(&terminal, "last", false, true).matches.len(), 1);
        let wide = matches(&terminal, "中文", false, true);
        assert_eq!(wide.matches[0].start().column, Column(2));
        assert_eq!(wide.matches[0].end().column, Column(5));
    }

    #[test]
    fn hard_newlines_are_not_soft_wraps_and_search_does_not_change_selection() {
        let terminal = terminal(20, 3, b"abc\r\ndef");
        terminal.with_term_mut(|term| {
            use alacritty_terminal::{
                index::Side,
                selection::{Selection, SelectionType},
            };
            let mut selection = Selection::new(
                SelectionType::Simple,
                Point::new(Line(0), Column(0)),
                Side::Left,
            );
            selection.update(Point::new(Line(0), Column(2)), Side::Right);
            term.selection = Some(selection);
        });
        let selection_before = terminal.with_term(|term| term.selection_to_string());
        assert!(matches(&terminal, "abcdef", false, true).matches.is_empty());
        assert_eq!(
            matches(&terminal, "abc", false, true).matches[0],
            Point::new(Line(0), Column(0))..=Point::new(Line(0), Column(2))
        );
        assert_eq!(
            terminal.with_term(|term| term.selection_to_string()),
            selection_before
        );
        assert_eq!(selection_before.as_deref(), Some("abc"));
    }

    #[test]
    fn fresh_snapshot_tracks_new_output_without_mutating_an_in_flight_snapshot() {
        let mut terminal = terminal(20, 2, b"needle\r\nline\r\n");
        let old_snapshot = terminal.with_term(|term| term.grid().clone());
        let old = matches(&terminal, "needle", false, true);
        terminal.process_bytes(b"needle\r\nnew\r\n");
        let new = matches(&terminal, "needle", false, true);
        assert_eq!(old.matches.len(), 1);
        assert_eq!(new.matches.len(), 2);
        assert!(new.matches[0].start().line < old.matches[0].start().line);
        let isolated = search_grid(
            old_snapshot,
            &SearchOptions {
                query: "needle".into(),
                regex: false,
                case_sensitive: true,
            },
        );
        assert_eq!(isolated.matches, old.matches);
    }

    #[test]
    fn invalid_regex_empty_query_and_resource_limits_are_reported() {
        let terminal = terminal(80, 3, &vec![b'x'; 20_000]);
        assert!(matches(&terminal, "[", true, true).error.is_some());
        assert!(matches(&terminal, "", false, true).matches.is_empty());
        assert!(
            matches(&terminal, &"x".repeat(1025), false, true)
                .error
                .is_some()
        );
        let many = matches(&terminal, "x", false, true);
        assert!(many.limited);
        assert!(many.matches.len() <= MAX_SEARCH_MATCHES);
        assert!(terminal.with_term(|t| t.grid().total_lines()) > 3);
    }
}
