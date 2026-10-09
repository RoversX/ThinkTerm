//! What programs have told the terminal about themselves with OSC 7501.
//!
//! A program files records under ids of its choosing: the root record
//! (no id) for itself, `/`-separated paths for parts of its work such as
//! sub-tasks. The terminal keeps them until they are replaced, cleared, or
//! outlived by the program they describe, and answers questions about them;
//! deciding what they mean for the interface is the mux's business.

use std::collections::HashMap;
pub use wezterm_escape_parser::osc::{ProgramBlockedKind, ProgramState, ProgramStatusReport};

/// One record, as a program last reported it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgramStatusRecord {
    /// Local receipt identity; even identical reports are new emissions.
    pub serial: u64,
    /// Foreground process group observed when this report was received.
    pub process_group: Option<u32>,
    /// `None` for the root record.
    pub id: Option<String>,
    /// Never [`ProgramState::Clear`], which removes records instead.
    pub state: ProgramState,
    pub kind: Option<ProgramBlockedKind>,
    pub progress: Option<u8>,
    /// As reported. [`ProgramStatusRecords::snapshot`] fills it in from the
    /// nearest ancestor for a record that names none.
    pub app: Option<String>,
    pub title: Option<String>,
    pub msg: Option<String>,
    /// Kept after the program that filed it went away: a result nobody has
    /// seen yet, describing nothing that is running now.
    pub orphaned: bool,
}

/// The records as a consumer sees them: the root, and every other record
/// in id order with its `app` inherited.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProgramStatusSnapshot {
    pub root: Option<ProgramStatusRecord>,
    pub children: Vec<ProgramStatusRecord>,
    /// The receipt serial of the last report taken in by then: what a
    /// decision made on this snapshot may act on, and nothing filed after.
    pub latest_serial: u64,
}

/// The records a terminal keeps, least recently updated first.
#[derive(Clone, Debug, Default)]
pub struct ProgramStatusRecords {
    records: Vec<ProgramStatusRecord>,
    serial: u64,
}

impl ProgramStatusRecords {
    /// The protocol's floor for how many records a terminal keeps. Past it,
    /// the record updated longest ago makes room.
    pub const MAX_RECORDS: usize = 256;

    /// File a report: it replaces its record whole, or for `clear` removes
    /// the record and everything under it -- every record, without an id.
    pub fn apply(&mut self, report: ProgramStatusReport) {
        self.apply_from(report, None);
    }

    /// File a report with the process group observed at receipt, if available.
    pub fn apply_from(&mut self, report: ProgramStatusReport, process_group: Option<u32>) {
        self.serial = self.serial.wrapping_add(1);
        if report.state == ProgramState::Clear {
            match report.id {
                Some(id) => self.records.retain(|record| !is_within(record, &id)),
                None => self.records.clear(),
            }
            return;
        }
        if report.id.is_none()
            && self.find(None).is_some_and(|root| {
                matches!((root.process_group, process_group), (Some(old), Some(new)) if old != new)
            })
        {
            // Children keep the app of the root they were filed under,
            // even if a later command replaces it before a shell prompt.
            let apps = self.apps();
            let inherited: Vec<_> = self.records
                .iter()
                .map(|record| {
                    record.id.as_deref().filter(|_| record.app.is_none())
                        .and_then(|id| inherited_app(&apps, id))
                })
                .collect();
            for (record, app) in self.records.iter_mut().zip(inherited) {
                if app.is_some() {
                    record.app = app;
                }
            }
        }
        self.records.retain(|record| record.id != report.id);
        if self.records.len() >= Self::MAX_RECORDS {
            self.records.remove(0);
        }
        self.records.push(ProgramStatusRecord {
            serial: self.serial,
            process_group,
            id: report.id,
            state: report.state,
            kind: report.kind,
            progress: report.progress,
            app: report.app,
            title: report.title,
            msg: report.msg,
            orphaned: false,
        });
    }

    /// The program is gone, or a shell has put up a new prompt: nothing it
    /// was in the middle of is still going on. Only results nobody has seen
    /// yet -- `done` and `error` -- outlive it, marked as left behind.
    /// Returns whether anything changed.
    pub fn end_of_program(&mut self) -> bool {
        self.end_of_program_through(u64::MAX)
    }

    /// [`end_of_program`](Self::end_of_program) for the records received up
    /// to serial `through` only: a decision taken on a snapshot must not
    /// sweep away a report that arrived after it.
    pub fn end_of_program_through(&mut self, through: u64) -> bool {
        // A survivor keeps the name it had from a parent that is about to
        // go: it is still that program's result.
        let inherited: Vec<Option<String>> = {
            let apps = self.apps();
            self.records
                .iter()
                .map(|record| match (&record.app, &record.id) {
                    (None, Some(id)) if record.serial <= through => inherited_app(&apps, id),
                    _ => None,
                })
                .collect()
        };
        for (record, app) in self.records.iter_mut().zip(inherited) {
            if app.is_some() {
                record.app = app;
            }
        }
        let before = self.records.len();
        self.records.retain(|record| {
            record.serial > through
                || matches!(record.state, ProgramState::Done | ProgramState::Error)
        });
        let mut changed = self.records.len() != before;
        for record in self.records.iter_mut().filter(|record| record.serial <= through) {
            changed |= !record.orphaned;
            record.orphaned = true;
        }
        changed
    }

    /// Someone has looked: a `done` or `error` result has been seen and
    /// need not be announced any longer. Returns whether anything changed.
    pub fn seen(&mut self) -> bool {
        let before = self.records.len();
        self.records.retain(|record| {
            !matches!(record.state, ProgramState::Done | ProgramState::Error)
        });
        self.records.len() != before
    }

    pub fn clear(&mut self) {
        self.records.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// The root and, of the other records, the first `max_children` in id
    /// order. Taken under the terminal's lock, so every record is looked at
    /// once and only the ones handed out are copied.
    pub fn snapshot(&self, max_children: usize) -> ProgramStatusSnapshot {
        if self.records.is_empty() {
            return ProgramStatusSnapshot::default();
        }
        let apps = self.apps();
        let app_of = |id: &str| inherited_app(&apps, id);
        let mut children: Vec<&ProgramStatusRecord> =
            self.records.iter().filter(|record| record.id.is_some()).collect();
        children.sort_by(|a, b| a.id.cmp(&b.id));
        let children = children
            .into_iter()
            .take(max_children)
            .map(|record| {
                let mut record = record.clone();
                if record.app.is_none() {
                    record.app = app_of(record.id.as_deref().unwrap_or_default());
                }
                record
            })
            .collect();
        ProgramStatusSnapshot {
            root: self.find(None).cloned(),
            children,
            latest_serial: self.serial,
        }
    }

    /// Each record that names its program, by id.
    fn apps(&self) -> HashMap<Option<&str>, &str> {
        self.records
            .iter()
            .filter_map(|record| Some((record.id.as_deref(), record.app.as_deref()?)))
            .collect()
    }

    /// The `app` the nearest ancestor of `id` names.
    fn find(&self, id: Option<&str>) -> Option<&ProgramStatusRecord> {
        self.records.iter().find(|record| record.id.as_deref() == id)
    }
}

/// The `app` the nearest ancestor of `id` names, from [`ProgramStatusRecords::apps`].
fn inherited_app(apps: &HashMap<Option<&str>, &str>, id: &str) -> Option<String> {
    let mut path = id;
    while let Some((parent, _)) = path.rsplit_once('/') {
        path = parent;
        if let Some(app) = apps.get(&Some(path)) {
            return Some(app.to_string());
        }
    }
    apps.get(&None).map(|app| app.to_string())
}

/// Whether `record` is `id` itself or somewhere under it.
fn is_within(record: &ProgramStatusRecord, id: &str) -> bool {
    record.id.as_deref().is_some_and(|own| {
        own == id || (own.starts_with(id) && own.as_bytes().get(id.len()) == Some(&b'/'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(state: ProgramState, id: Option<&str>, app: Option<&str>) -> ProgramStatusReport {
        ProgramStatusReport {
            state,
            id: id.map(str::to_string),
            kind: None,
            progress: None,
            app: app.map(str::to_string),
            title: None,
            msg: None,
        }
    }

    #[test]
    fn a_report_replaces_its_record_whole() {
        let mut records = ProgramStatusRecords::default();
        let mut first = report(ProgramState::Working, None, Some("claude-code"));
        first.msg = Some("Running tests".to_string());
        first.progress = Some(40);
        records.apply(first);
        records.apply(report(ProgramState::Idle, None, None));
        let root = records.snapshot(usize::MAX).root.unwrap();
        assert_eq!(root.state, ProgramState::Idle);
        assert_eq!((root.msg, root.progress, root.app), (None, None, None));
    }

    #[test]
    fn identical_emissions_keep_distinct_receipts_even_after_a_clear() {
        let mut records = ProgramStatusRecords::default();
        let working = report(ProgramState::Working, None, Some("example"));
        records.apply_from(working.clone(), Some(9101));
        let first = records.snapshot(0).root.unwrap();
        records.apply_from(working.clone(), Some(9102));
        let second = records.snapshot(0).root.unwrap();
        assert_eq!(first.process_group, Some(9101));
        assert_eq!(second.process_group, Some(9102));
        assert_ne!(second.serial, first.serial);
        records.clear();
        records.apply_from(working, Some(9102));
        assert_ne!(records.snapshot(0).root.unwrap().serial, second.serial);
    }

    #[test]
    fn children_inherit_the_nearest_app_and_clear_with_their_parent() {
        let mut records = ProgramStatusRecords::default();
        records.apply(report(ProgramState::Working, None, Some("claude-code")));
        records.apply(report(ProgramState::Working, Some("task"), Some("tool")));
        records.apply(report(ProgramState::Blocked, Some("task/sub"), None));
        records.apply(report(ProgramState::Working, Some("taskforce"), None));
        let snapshot = records.snapshot(usize::MAX);
        let apps: Vec<_> = snapshot
            .children
            .iter()
            .map(|child| (child.id.clone().unwrap(), child.app.clone().unwrap()))
            .collect();
        assert_eq!(
            apps,
            [
                ("task".to_string(), "tool".to_string()),
                ("task/sub".to_string(), "tool".to_string()),
                ("taskforce".to_string(), "claude-code".to_string()),
            ]
        );

        // `taskforce` only shares a prefix with `task`; it is not under it.
        records.apply(report(ProgramState::Clear, Some("task"), None));
        let ids: Vec<_> = records
            .snapshot(usize::MAX)
            .children
            .into_iter()
            .map(|child| child.id.unwrap())
            .collect();
        assert_eq!(ids, ["taskforce"]);

        records.apply(report(ProgramState::Clear, None, None));
        assert!(records.is_empty());
    }

    #[test]
    fn only_unseen_results_outlive_their_program() {
        let mut records = ProgramStatusRecords::default();
        records.apply(report(ProgramState::Working, None, None));
        records.apply(report(ProgramState::Blocked, Some("a"), None));
        records.apply(report(ProgramState::Idle, Some("b"), None));
        records.apply(report(ProgramState::Done, Some("c"), None));
        records.apply(report(ProgramState::Error, Some("d"), None));
        assert!(records.end_of_program());
        let left: Vec<_> = records
            .snapshot(usize::MAX)
            .children
            .into_iter()
            .map(|child| (child.state, child.orphaned))
            .collect();
        assert_eq!(left, [(ProgramState::Done, true), (ProgramState::Error, true)]);
        assert!(!records.end_of_program());

        // A program reporting again files a record of its own, not one left
        // behind.
        records.apply(report(ProgramState::Done, Some("c"), None));
        assert!(!records.snapshot(usize::MAX).children[0].orphaned);
        assert!(records.seen());
        assert!(records.is_empty());
    }

    #[test]
    fn ending_a_program_spares_what_arrived_after_the_decision() {
        let mut records = ProgramStatusRecords::default();
        records.apply(report(ProgramState::Working, None, Some("old")));
        let decided_on = records.snapshot(0).latest_serial;
        // The next command reports before the decision is acted on.
        records.apply(report(ProgramState::Working, Some("next"), Some("new")));
        assert!(records.end_of_program_through(decided_on));
        let snapshot = records.snapshot(usize::MAX);
        assert_eq!(snapshot.root, None);
        assert_eq!(snapshot.children.len(), 1);
        assert!(!snapshot.children[0].orphaned);
    }

    #[test]
    fn children_keep_their_app_when_another_process_replaces_the_root() {
        let mut records = ProgramStatusRecords::default();
        records.apply_from(report(ProgramState::Working, None, Some("tool")), Some(9801));
        records.apply_from(report(ProgramState::Error, Some("fetch"), None), Some(9801));
        records.apply_from(report(ProgramState::Working, None, Some("example")), Some(9802));
        let snapshot = records.snapshot(usize::MAX);
        assert_eq!(snapshot.children[0].app.as_deref(), Some("tool"));
        records.end_of_program();
        assert_eq!(records.snapshot(usize::MAX).children[0].app.as_deref(), Some("tool"));
    }

    #[test]
    fn a_snapshot_hands_out_the_first_children_only() {
        let mut records = ProgramStatusRecords::default();
        records.apply(report(ProgramState::Working, None, Some("tool")));
        for id in ["c", "a", "b"] {
            records.apply(report(ProgramState::Working, Some(id), None));
        }
        let snapshot = records.snapshot(2);
        let ids: Vec<_> = snapshot.children.iter().map(|c| c.id.clone().unwrap()).collect();
        assert_eq!(ids, ["a", "b"]);
        assert!(snapshot.children.iter().all(|c| c.app.as_deref() == Some("tool")));
    }

    #[test]
    fn the_least_recently_updated_record_makes_room() {
        let mut records = ProgramStatusRecords::default();
        for n in 0..ProgramStatusRecords::MAX_RECORDS {
            records.apply(report(ProgramState::Working, Some(&n.to_string()), None));
        }
        // Updating "0" makes "1" the oldest.
        records.apply(report(ProgramState::Idle, Some("0"), None));
        records.apply(report(ProgramState::Working, Some("new"), None));
        let snapshot = records.snapshot(usize::MAX);
        assert_eq!(snapshot.children.len(), ProgramStatusRecords::MAX_RECORDS);
        assert!(snapshot.children.iter().all(|c| c.id.as_deref() != Some("1")));
        assert!(snapshot.children.iter().any(|c| c.id.as_deref() == Some("0")));
    }
}
