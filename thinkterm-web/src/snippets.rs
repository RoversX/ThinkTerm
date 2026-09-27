//! The right panel's Snippets tab, as data. The snippets are the plugin
//! host's, on the server's machine, reached through the server
//! (`PluginFrame`): it searches, saves, deletes and says what a pane is sent
//! (thinkterm-snippets `wire`). This holds only what the tab shows -- the
//! search and the rows sent for it, asked for as every client asks
//! (thinkterm-snippets `view`) -- and whether it is followed.

use crate::plugins::Answer;
use serde::Serialize;
use thinkterm_snippets::view::Listing;
use thinkterm_snippets::wire::Row;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum Phase {
    /// Not followed: the tab is not on show, or the connection went.
    #[default]
    Closed,
    /// Asked for; the host has not answered.
    Opening,
    Open,
    /// The host cannot be reached through this server; why.
    Unavailable(String),
}

#[derive(Debug, Default)]
pub struct SnippetsModel {
    phase: Phase,
    /// The tab is on show, so the snippets are wanted.
    wanted: bool,
    listing: Listing,
    /// Counts changes to what [`view`](Self::view) shows, so the page
    /// re-reads it only when it moved.
    revision: u64,
    /// Times in a row the host went away before it answered.
    gone_in_a_row: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SnippetsView {
    /// `loading`, `ready` or `unavailable`.
    pub state: &'static str,
    /// Why they are unavailable.
    pub reason: Option<String>,
    pub rows: Vec<Row>,
    pub query: String,
    /// What an empty list says.
    pub empty: String,
    /// The search the rows answer: `query`, once they have caught up.
    pub answers: Option<String>,
    /// How many times the rows were asked for, for probes.
    pub asked: u64,
    pub revision: u64,
}

impl SnippetsModel {
    pub fn revision(&self) -> u64 {
        self.revision
    }

    fn changed(&mut self) {
        self.revision += 1;
    }

    /// Whether the rows follow the host, so a change there asks again.
    pub fn followed(&self) -> bool {
        matches!(self.phase, Phase::Opening | Phase::Open)
    }

    /// The tab is on show and the page connected: the rows are asked for
    /// afresh, unless they are followed already.
    pub fn open(&mut self) {
        self.wanted = true;
        if self.followed() {
            return;
        }
        self.phase = Phase::Opening;
        self.listing.changed();
        self.changed();
    }

    /// The tab was put away, and the rows with it. True when the server's
    /// connection to the host is to be closed.
    pub fn close(&mut self) -> bool {
        let followed = self.followed();
        self.wanted = false;
        self.phase = Phase::Closed;
        self.listing.clear();
        self.changed();
        followed
    }

    pub fn set_query(&mut self, query: &str) {
        if self.listing.query() != query {
            self.listing.set_query(query);
            self.changed();
        }
    }

    /// The host said the snippets changed.
    pub fn snippets_changed(&mut self) {
        self.listing.changed();
    }

    /// The search to ask the host about now, if any: one request at a time,
    /// and none while the tab is not followed.
    pub fn next_list(&mut self) -> Option<String> {
        if !self.followed() {
            return None;
        }
        let next = self.listing.next();
        if next.is_some() {
            self.changed();
        }
        next
    }

    /// The host's answer to a list for `query`.
    pub fn listed(&mut self, query: String, answer: Answer) {
        let rows = answer.and_then(|body| {
            serde_json::from_value::<Vec<Row>>(body).map_err(|err| err.to_string())
        });
        let answered = self.listing.answered(query, rows);
        if !self.wanted {
            return;
        }
        match answered {
            Ok(()) => {
                self.phase = Phase::Open;
                self.gone_in_a_row = 0;
            }
            // Rows already on show stay; only an empty tab has news.
            Err(why) if self.listing.rows().is_some() => log::warn!("snippets: {why}"),
            Err(why) => self.phase = Phase::Unavailable(why),
        }
        self.changed();
    }

    /// The connection to the server went, and with it the one to the host:
    /// the rows are asked for again once it is back, if still wanted.
    pub fn connection_lost(&mut self) {
        if self.followed() {
            self.phase = Phase::Closed;
        }
    }

    /// The server's connection to the host closed: the host went away, and
    /// is asked again. True when it is still wanted.
    pub fn host_gone(&mut self) -> bool {
        if self.phase != Phase::Open {
            self.gone_in_a_row += 1;
        }
        self.connection_lost();
        self.wanted
    }

    /// How long to wait before asking again after the host went away: a
    /// second, doubling while it keeps going before it answers, to half a
    /// minute.
    pub fn retry_delay_ms(&self) -> f64 {
        1000.0 * f64::from(1u32 << self.gone_in_a_row.min(5)).min(30.0)
    }

    pub fn view(&self) -> SnippetsView {
        let rows = self.listing.rows();
        let (state, reason) = match (&self.phase, rows) {
            (_, Some(_)) => ("ready", None),
            (Phase::Unavailable(reason), None) => ("unavailable", Some(reason.clone())),
            _ => ("loading", None),
        };
        let query = self.listing.query();
        let empty = match state {
            "unavailable" => thinkterm_i18n::tr("right-snippets-unavailable"),
            _ if query.trim().is_empty() => thinkterm_i18n::tr("right-no-snippets"),
            _ => thinkterm_i18n::tr("right-no-matching-snippets"),
        };
        SnippetsView {
            state,
            reason,
            rows: rows.map(<[_]>::to_vec).unwrap_or_default(),
            query: query.to_string(),
            empty,
            answers: self.listing.answers().map(str::to_string),
            asked: self.listing.asked(),
            revision: self.revision,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(id: &str) -> serde_json::Value {
        json!({"id": id, "title": id, "preview": "ls"})
    }

    fn ids(model: &SnippetsModel) -> Vec<String> {
        model.view().rows.into_iter().map(|r| r.id).collect()
    }

    fn opened(model: &mut SnippetsModel, rows: serde_json::Value) {
        model.open();
        let query = model.next_list().expect("asked for");
        model.listed(query, Ok(rows));
    }

    #[test]
    fn opening_asks_once_and_shows_what_arrives() {
        let mut model = SnippetsModel::default();
        assert_eq!(model.view().state, "loading");
        assert_eq!(model.next_list(), None, "not followed yet");
        model.open();
        assert_eq!(model.next_list().as_deref(), Some(""));
        model.open();
        assert_eq!(model.next_list(), None, "already asked");
        model.listed(String::new(), Ok(json!([row("a")])));
        assert_eq!(model.view().state, "ready");
        assert_eq!(ids(&model), ["a"]);
    }

    #[test]
    fn typing_has_one_request_out_and_the_newest_search_wins() {
        let mut model = SnippetsModel::default();
        opened(&mut model, json!([row("a"), row("b")]));
        model.set_query("p");
        assert_eq!(model.next_list().as_deref(), Some("p"));
        for query in ["pu", "pul", "pull"] {
            model.set_query(query);
            assert_eq!(model.next_list(), None, "one request at a time");
        }
        model.listed("p".into(), Ok(json!([row("a")])));
        assert_eq!(model.next_list().as_deref(), Some("pull"));
        model.listed("pull".into(), Ok(json!([])));
        assert!(ids(&model).is_empty());
        assert_eq!(model.view().empty, "No matching snippets");
        assert_eq!(model.view().asked, 3);
    }

    #[test]
    fn a_change_asks_again() {
        let mut model = SnippetsModel::default();
        opened(&mut model, json!([row("a")]));
        assert_eq!(model.next_list(), None);
        model.snippets_changed();
        assert_eq!(model.next_list().as_deref(), Some(""));
    }

    #[test]
    fn an_unreachable_host_says_so_and_rows_on_show_stay() {
        let mut model = SnippetsModel::default();
        model.open();
        let query = model.next_list().unwrap();
        model.listed(query, Err("no plugin host here".into()));
        let view = model.view();
        assert_eq!((view.state, view.reason.as_deref()), ("unavailable", Some("no plugin host here")));
        assert_eq!(view.empty, "Snippets are unavailable right now");
        assert_eq!(model.next_list(), None, "not asked again in a loop");

        let mut model = SnippetsModel::default();
        opened(&mut model, json!([row("a")]));
        model.snippets_changed();
        let query = model.next_list().unwrap();
        model.listed(query, Err("the plugin host went away".into()));
        assert_eq!(ids(&model), ["a"]);
    }

    #[test]
    fn closing_forgets_the_rows_and_ignores_what_comes_after() {
        let mut model = SnippetsModel::default();
        assert!(!model.close(), "nothing was followed");
        opened(&mut model, json!([row("a")]));
        assert!(model.close());
        assert_eq!(model.view().state, "loading");
        assert_eq!(model.next_list(), None);
        assert!(!model.host_gone(), "not wanted, so not asked again");
    }

    #[test]
    fn a_host_that_keeps_going_away_is_asked_less_often() {
        let mut model = SnippetsModel::default();
        model.open();
        assert!(model.host_gone());
        assert!(!model.followed());
        assert_eq!(model.retry_delay_ms(), 2000.0);
        for _ in 0..10 {
            model.open();
            model.host_gone();
        }
        assert_eq!(model.retry_delay_ms(), 30000.0);
        model.open();
        let query = model.next_list().unwrap();
        model.listed(query, Ok(json!([])));
        assert!(model.host_gone());
        assert_eq!(model.retry_delay_ms(), 1000.0);
    }

    #[test]
    fn a_lost_connection_asks_again_once_back() {
        let mut model = SnippetsModel::default();
        opened(&mut model, json!([row("a")]));
        model.connection_lost();
        model.open();
        assert_eq!(model.next_list().as_deref(), Some(""));
        assert_eq!(ids(&model), ["a"], "shown meanwhile");
    }
}
