use ratatui::crossterm::event::KeyEvent;
use ratatui::prelude::*;
use ratatui::text::Line;
use ratatui_textarea::TextArea;

/// Reusable single-field search/filter state for list views.
///
/// Owns the query text plus the insert/committed modes; *what* is matched is
/// supplied by the caller (chat names today, message text in the future) via
/// [`SearchState::set_visible`] + [`SearchState::map_to_source`].
#[derive(Clone, Default)]
pub struct SearchState {
    query: TextArea<'static>,
    /// `/` was pressed and the user is typing. Enter commits; Esc clears.
    inserting: bool,
    /// Source-list positions that currently match the query. Only meaningful
    /// while a query is set (otherwise the view is the full source list).
    visible_indices: Vec<usize>,
    /// Source index of the chat that should be (re)selected when the filter is
    /// cancelled. Remembered even while an empty result set clears the visible
    /// selection, so Esc lands back on a useful chat.
    anchor_source: Option<usize>,
}

impl SearchState {
    /// Start a fresh search: clear any previous query and enter insert mode.
    pub fn begin(&mut self) {
        self.query = TextArea::default();
        self.inserting = true;
        self.visible_indices.clear();
        self.anchor_source = None;
    }

    /// Exit insert mode, keeping the current filter applied.
    pub fn commit(&mut self) {
        self.inserting = false;
    }

    /// Cancel the search entirely: exit insert mode and drop the filter.
    /// The selection anchor is preserved so the caller can restore the chat
    /// the user was looking at.
    pub fn clear(&mut self) {
        self.query = TextArea::default();
        self.inserting = false;
        self.visible_indices.clear();
    }

    /// True while the user is typing (insert mode).
    pub fn is_inserting(&self) -> bool {
        self.inserting
    }

    /// True when a query is set, whether live (typing) or committed.
    pub fn is_filtered(&self) -> bool {
        !self.is_empty_query()
    }

    /// True when the search input should be rendered (typing or a filter held).
    pub fn is_active(&self) -> bool {
        self.inserting || self.is_filtered()
    }

    /// True when no query text is present.
    pub fn is_empty_query(&self) -> bool {
        self.query_text().is_empty()
    }

    /// The current query text (single-line).
    pub fn query_text(&self) -> &str {
        self.query.lines().first().map(String::as_str).unwrap_or("")
    }

    /// Cursor column of the underlying input, in characters.
    pub fn cursor_col(&self) -> usize {
        self.query.cursor().1
    }

    /// Feed a key event to the underlying input (characters, backspace,
    /// cursor movement). Enter/Esc are handled by the caller.
    pub fn input(&mut self, key: KeyEvent) {
        self.query.input(key);
    }

    /// Record the source indices that currently match the query.
    pub fn set_visible(&mut self, indices: Vec<usize>) {
        self.visible_indices = indices;
    }

    /// The source indices currently visible after filtering, in display order.
    pub fn indices(&self) -> &[usize] {
        &self.visible_indices
    }

    /// Source index to restore the selection to when the filter clears.
    pub fn anchor_source(&self) -> Option<usize> {
        self.anchor_source
    }

    /// Remember which source chat the selection should land back on.
    pub fn set_anchor(&mut self, source: Option<usize>) {
        self.anchor_source = source;
    }

    /// Translate a visible selection index into its source index. When no
    /// filter is applied the view is the full source list, so this is identity.
    pub fn map_to_source(&self, idx: usize) -> Option<usize> {
        if self.is_filtered() {
            self.visible_indices.get(idx).copied()
        } else {
            Some(idx)
        }
    }

    /// Bottom-title line for the surrounding block: the search input when a
    /// search is active, otherwise empty. The cursor is drawn as a highlighted
    /// cell only while the user is actually typing.
    pub fn title_line(&self) -> Line<'static> {
        if !self.is_active() {
            return Line::default();
        }

        let label = Span::styled(" Search: ", Style::default().fg(Color::DarkGray));
        let text = self.query_text().to_string();
        let body = Style::default().fg(Color::Gray);

        if self.is_inserting() {
            let mut chars: Vec<char> = text.chars().collect();
            let col = self.cursor_col().min(chars.len());
            let before: String = chars[..col].iter().collect();
            let cursor = if col < chars.len() {
                chars.remove(col)
            } else {
                ' '
            };

            Line::from(vec![
                label,
                Span::styled(before, body),
                Span::styled(
                    cursor.to_string(),
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
            ])
        } else {
            Line::from(vec![label, Span::styled(text, body)])
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::{KeyCode, KeyEvent};

    use super::*;

    fn key(c: char) -> KeyEvent {
        KeyEvent::from(KeyCode::Char(c))
    }

    #[test]
    fn begin_clears_query_and_enables_insert_mode() {
        let mut state = SearchState::default();
        state.begin();
        assert!(state.is_inserting());
        assert!(state.is_empty_query());

        state.input(key('a'));
        state.input(key('b'));
        assert_eq!(state.query_text(), "ab");

        state.clear();
        assert!(!state.is_inserting());
        assert!(state.is_empty_query());
        assert!(!state.is_active());
    }

    #[test]
    fn commit_keeps_filter_and_exits_insert_mode() {
        let mut state = SearchState::default();
        state.begin();
        state.input(key('h'));
        state.commit();

        assert!(!state.is_inserting());
        assert!(state.is_filtered());
        assert!(state.is_active());
    }

    #[test]
    fn backspace_edits_the_query() {
        let mut state = SearchState::default();
        state.begin();
        state.input(key('h'));
        state.input(key('i'));
        state.input(KeyEvent::from(KeyCode::Backspace));

        assert_eq!(state.query_text(), "h");

        while !state.is_empty_query() {
            state.input(KeyEvent::from(KeyCode::Backspace));
        }
        assert!(!state.is_filtered());
    }

    #[test]
    fn map_to_source_is_identity_when_unfiltered() {
        let state = SearchState::default();
        assert_eq!(state.map_to_source(0), Some(0));
        assert_eq!(state.map_to_source(7), Some(7));
        assert_eq!(state.indices().len(), 0);
    }

    #[test]
    fn map_to_source_uses_visible_indices_when_filtered() {
        let mut state = SearchState::default();
        state.begin();
        state.input(key('x'));
        state.set_visible(vec![4, 9]);
        assert_eq!(state.indices().len(), 2);
        assert_eq!(state.map_to_source(0), Some(4));
        assert_eq!(state.map_to_source(1), Some(9));
        assert_eq!(state.map_to_source(2), None);
    }

    #[test]
    fn cursor_col_tracks_the_insert_position() {
        let mut state = SearchState::default();
        state.begin();
        assert_eq!(state.cursor_col(), 0);

        state.input(key('a'));
        state.input(key('b'));
        state.input(key('c'));
        assert_eq!(state.cursor_col(), 3);

        state.input(KeyEvent::from(KeyCode::Left));
        state.input(key('X'));
        assert_eq!(state.query_text(), "abXc");
        assert_eq!(state.cursor_col(), 3);
    }

    #[test]
    fn title_line_is_empty_when_inactive() {
        let state = SearchState::default();
        assert_eq!(state.title_line(), Line::default());
    }

    #[test]
    fn title_line_shows_query_while_inserting() {
        let mut state = SearchState::default();
        state.begin();
        state.input(key('a'));
        state.input(key('b'));
        let line = state.title_line();
        let text: String = line.spans.iter().map(|s| s.to_string()).collect();
        assert!(text.contains("Search:"));
        assert!(text.contains("ab"));
    }

    #[test]
    fn title_line_shows_committed_query_without_cursor() {
        let mut state = SearchState::default();
        state.begin();
        state.input(key('a'));
        state.input(key('b'));
        state.commit();
        let line = state.title_line();
        let text: String = line.spans.iter().map(|s| s.to_string()).collect();
        assert!(text.contains("ab"));
        assert!(
            line.spans
                .iter()
                .all(|s| s.style.bg != Some(Color::Yellow)),
            "no cursor cell while not typing"
        );
    }
}
