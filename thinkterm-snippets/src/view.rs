//! What a client shows of the snippets: the rows the plugin host sent for
//! its search, and when to ask for them again. Every client follows the
//! same rules -- one request at a time, the newest search asked for once
//! it lands -- so a desktop window and a browser page behave alike, and
//! typing in a search box never has a request per keystroke in flight.

use crate::wire::Row;

#[derive(Debug, Default)]
pub struct Listing {
    /// What the search box holds.
    query: String,
    /// The rows on show; `None` until the host first answers.
    rows: Option<Vec<Row>>,
    /// The search the rows on show answer; `None` once a change made them
    /// out of date.
    answers: Option<String>,
    /// A request is on its way.
    asking: bool,
    /// Requests sent, for probes.
    asked: u64,
}

impl Listing {
    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn set_query(&mut self, query: &str) {
        if self.query != query {
            self.query = query.to_string();
        }
    }

    /// The rows on show; `None` until the host first answers.
    pub fn rows(&self) -> Option<&[Row]> {
        self.rows.as_deref()
    }

    /// How many times the rows were asked for.
    pub fn asked(&self) -> u64 {
        self.asked
    }

    /// The search the rows on show answer: the search box's, once they
    /// have caught up with it.
    pub fn answers(&self) -> Option<&str> {
        self.answers.as_deref()
    }

    /// The search to ask the host about now, if any: none while a request
    /// is on its way, nor while the rows on show answer the search and
    /// nothing changed since.
    pub fn next(&mut self) -> Option<String> {
        if self.asking || self.answers.as_deref() == Some(self.query.as_str()) {
            return None;
        }
        self.asking = true;
        self.asked += 1;
        Some(self.query.clone())
    }

    /// The host's answer to the request for `query`. Rows answering an
    /// older search are shown until the newest one's arrive. An error
    /// leaves the rows on show, is handed back, and is not asked about
    /// again until something changes: an unreachable host is not asked in
    /// a loop.
    pub fn answered(
        &mut self,
        query: String,
        answer: Result<Vec<Row>, String>,
    ) -> Result<(), String> {
        self.asking = false;
        self.answers = Some(query);
        self.rows = Some(answer?);
        Ok(())
    }

    /// The snippets changed, or the host is back: the rows are asked for
    /// again.
    pub fn changed(&mut self) {
        self.answers = None;
    }

    /// Lets the rows go; the search stays, and is asked about afresh.
    pub fn clear(&mut self) {
        self.rows = None;
        self.answers = None;
        self.asking = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(ids: &[&str]) -> Vec<Row> {
        ids.iter()
            .map(|id| Row {
                id: id.to_string(),
                title: id.to_string(),
                preview: String::new(),
            })
            .collect()
    }

    fn ids(listing: &Listing) -> Vec<&str> {
        listing
            .rows()
            .unwrap_or_default()
            .iter()
            .map(|row| row.id.as_str())
            .collect()
    }

    #[test]
    fn one_request_at_a_time_and_the_newest_search_wins() {
        let mut listing = Listing::default();
        assert_eq!(listing.next().as_deref(), Some(""));
        // Typing while the first request is out asks for nothing more...
        for query in ["g", "gi", "git"] {
            listing.set_query(query);
            assert_eq!(listing.next(), None);
        }
        // ...until it lands; then only the newest search is asked for.
        listing
            .answered(String::new(), Ok(rows(&["a", "b"])))
            .unwrap();
        assert_eq!(ids(&listing), ["a", "b"], "shown meanwhile");
        assert_eq!(listing.next().as_deref(), Some("git"));
        listing.answered("git".into(), Ok(rows(&["a"]))).unwrap();
        assert_eq!(listing.next(), None, "answered, and nothing changed");
        assert_eq!(ids(&listing), ["a"]);
        assert_eq!(listing.asked(), 2);
    }

    #[test]
    fn a_change_asks_again() {
        let mut listing = Listing::default();
        listing.next();
        listing.answered(String::new(), Ok(rows(&["a"]))).unwrap();
        listing.changed();
        assert_eq!(listing.next().as_deref(), Some(""));
    }

    #[test]
    fn an_error_keeps_the_rows_and_is_not_asked_about_in_a_loop() {
        let mut listing = Listing::default();
        listing.next();
        assert_eq!(
            listing.answered(String::new(), Err("no host".into())),
            Err("no host".into())
        );
        assert!(listing.rows().is_none());
        assert_eq!(listing.next(), None);
        listing.changed();
        listing.next();
        listing.answered(String::new(), Ok(rows(&["a"]))).unwrap();
        listing.changed();
        listing.next();
        assert!(listing.answered(String::new(), Err("gone".into())).is_err());
        assert_eq!(ids(&listing), ["a"]);
    }

    #[test]
    fn clearing_lets_the_rows_go_and_asks_afresh() {
        let mut listing = Listing::default();
        listing.set_query("git");
        listing.next();
        listing.answered("git".into(), Ok(rows(&["a"]))).unwrap();
        listing.clear();
        assert!(listing.rows().is_none());
        assert_eq!(listing.next().as_deref(), Some("git"));
    }
}
