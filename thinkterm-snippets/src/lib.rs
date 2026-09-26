//! ThinkTerm's command snippets: the list, the changes made to it, and how
//! two copies of it come back together.
//!
//! Snippets are ordinary user data rather than secrets: command templates,
//! not a password vault. The store is plain data and builds anywhere,
//! WebAssembly included; keeping it in a file and minting ids need the
//! `native` feature.

use memchr::memmem;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt;
use std::sync::OnceLock;

#[cfg(feature = "native")]
pub mod file;

pub type SnippetId = String;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnippetRecord {
    pub id: SnippetId,
    pub title: String,
    pub body: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// A deletion, kept so that a copy which still has the snippet cannot bring
/// it back when the two are merged.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Tombstone {
    pub id: SnippetId,
    pub deleted_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnippetStore {
    #[serde(default = "store_version")]
    version: u32,
    /// Newest first.
    #[serde(default)]
    snippets: Vec<SnippetRecord>,
    /// Apart from `snippets`, so a build from before deletions were kept
    /// reads the same file and sees only the live snippets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    deleted: Vec<Tombstone>,
    #[serde(skip)]
    folded: Folded,
}

impl Default for SnippetStore {
    fn default() -> Self {
        Self {
            version: store_version(),
            snippets: vec![],
            deleted: vec![],
            folded: Folded::default(),
        }
    }
}

fn store_version() -> u32 {
    1
}

/// A fresh snippet id. Random, so two machines creating a snippet in the
/// same millisecond cannot collide when their copies meet.
#[cfg(feature = "native")]
pub fn new_id(now_ms: u64) -> SnippetId {
    format!("snippet-{now_ms}-{}", uuid::Uuid::new_v4().simple())
}

/// The first non-empty line of `body`, shortened: the title of a snippet
/// saved without one.
pub fn title_from_body(body: &str) -> String {
    body.lines()
        .find_map(|line| {
            let trimmed = line.trim();
            (!trimmed.is_empty()).then_some(trimmed)
        })
        .unwrap_or("Untitled snippet")
        .chars()
        .take(48)
        .collect()
}

fn normalized_title(title: &str, body: &str) -> String {
    let title = title.trim();
    if title.is_empty() {
        title_from_body(body)
    } else {
        title.chars().take(80).collect()
    }
}

impl SnippetStore {
    /// The live snippets, newest first.
    pub fn snippets(&self) -> &[SnippetRecord] {
        &self.snippets
    }

    pub fn get(&self, id: &str) -> Option<&SnippetRecord> {
        self.snippets.iter().find(|snippet| snippet.id == id)
    }

    /// The snippets whose title or body contains `query`, ignoring ASCII
    /// case, in list order; all of them for a blank query.
    pub fn matching<'a>(&'a self, query: &str) -> impl Iterator<Item = &'a SnippetRecord> + 'a {
        let query = query.trim().to_ascii_lowercase();
        let finder =
            (!query.is_empty()).then(|| memmem::Finder::new(query.as_bytes()).into_owned());
        let folded = finder.as_ref().map(|_| self.folded.of(&self.snippets));
        self.snippets
            .iter()
            .enumerate()
            .filter(move |(index, _)| match (&finder, folded) {
                (Some(finder), Some(folded)) => {
                    let (title, body) = &folded[*index];
                    finder.find(title.as_bytes()).is_some()
                        || finder.find(body.as_bytes()).is_some()
                }
                _ => true,
            })
            .map(|(_, snippet)| snippet)
    }

    /// Adds a snippet at the top of the list.
    pub fn create(
        &mut self,
        id: SnippetId,
        title: &str,
        body: String,
        now_ms: u64,
    ) -> SnippetRecord {
        let record = SnippetRecord {
            id,
            title: normalized_title(title, &body),
            body,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        };
        self.snippets.insert(0, record.clone());
        self.folded = Folded::default();
        record
    }

    pub fn update(
        &mut self,
        id: &str,
        title: &str,
        body: String,
        now_ms: u64,
    ) -> Option<SnippetRecord> {
        let record = self.snippets.iter_mut().find(|snippet| snippet.id == id)?;
        self.folded = Folded::default();
        record.title = normalized_title(title, &body);
        record.body = body;
        // Later than the change it replaces even if the clock went back, so
        // it wins a merge against the copy it was made from.
        record.updated_at_ms = now_ms.max(record.updated_at_ms.saturating_add(1));
        Some(record.clone())
    }

    pub fn delete(&mut self, id: &str, now_ms: u64) -> bool {
        let Some(index) = self.snippets.iter().position(|snippet| snippet.id == id) else {
            return false;
        };
        let record = self.snippets.remove(index);
        self.folded = Folded::default();
        let deleted_at_ms = now_ms.max(record.updated_at_ms.saturating_add(1));
        match self
            .deleted
            .iter_mut()
            .find(|tombstone| tombstone.id == record.id)
        {
            Some(tombstone) => {
                tombstone.deleted_at_ms = tombstone.deleted_at_ms.max(deleted_at_ms);
            }
            None => self.deleted.push(Tombstone {
                id: record.id,
                deleted_at_ms,
            }),
        }
        true
    }

    /// Brings another copy's changes in. The result depends only on what the
    /// two copies hold, not on which merges into which or how often: per
    /// snippet the later change wins, and a deletion wins over any change it
    /// is not older than. A snippet missing from one copy is never taken to
    /// be deleted -- only a tombstone deletes.
    pub fn merge(&mut self, other: &SnippetStore) {
        let mut deleted: HashMap<&str, u64> = HashMap::new();
        for tombstone in self.deleted.iter().chain(&other.deleted) {
            let at = deleted.entry(&tombstone.id).or_insert(0);
            *at = (*at).max(tombstone.deleted_at_ms);
        }

        let mut latest: HashMap<&str, &SnippetRecord> = HashMap::new();
        for snippet in self.snippets.iter().chain(&other.snippets) {
            let keep = latest
                .get(snippet.id.as_str())
                .map_or(true, |kept| wins_over(snippet, kept));
            if keep {
                latest.insert(&snippet.id, snippet);
            }
        }

        let mut snippets: Vec<SnippetRecord> = latest
            .into_values()
            .filter(|snippet| {
                deleted
                    .get(snippet.id.as_str())
                    .map_or(true, |&at| at < snippet.updated_at_ms)
            })
            .cloned()
            .collect();
        snippets.sort_by(newest_first);

        let mut tombstones: Vec<Tombstone> = deleted
            .into_iter()
            .map(|(id, deleted_at_ms)| Tombstone {
                id: id.to_owned(),
                deleted_at_ms,
            })
            .collect();
        tombstones.sort_by(|a, b| a.id.cmp(&b.id));

        self.snippets = snippets;
        self.deleted = tombstones;
        self.folded = Folded::default();
    }
}

/// Which of two versions of one snippet a merge keeps: the later change, and
/// on a tie the greater content, so both sides of a merge keep the same one.
fn wins_over(candidate: &SnippetRecord, kept: &SnippetRecord) -> bool {
    (
        candidate.updated_at_ms,
        &candidate.title,
        &candidate.body,
        candidate.created_at_ms,
    ) > (
        kept.updated_at_ms,
        &kept.title,
        &kept.body,
        kept.created_at_ms,
    )
}

fn newest_first(a: &SnippetRecord, b: &SnippetRecord) -> Ordering {
    b.created_at_ms
        .cmp(&a.created_at_ms)
        .then_with(|| a.id.cmp(&b.id))
}

/// Each snippet's title and body ASCII-lowercased, which is how `matching`
/// has always compared them. Built by the first search after a change and
/// dropped by the next change: it never holds more than the snippets' own
/// text, and a repaint neither lowercases nor copies anything to search.
#[derive(Default)]
struct Folded(OnceLock<Vec<(String, String)>>);

impl Folded {
    fn of(&self, snippets: &[SnippetRecord]) -> &[(String, String)] {
        self.0.get_or_init(|| {
            snippets
                .iter()
                .map(|snippet| {
                    (
                        snippet.title.to_ascii_lowercase(),
                        snippet.body.to_ascii_lowercase(),
                    )
                })
                .collect()
        })
    }
}

/// A cache, not part of the store's value.
impl Clone for Folded {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl PartialEq for Folded {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for Folded {}

impl fmt::Debug for Folded {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("Folded")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, body: &str, created_at_ms: u64, updated_at_ms: u64) -> SnippetRecord {
        SnippetRecord {
            id: id.to_string(),
            title: title_from_body(body),
            body: body.to_string(),
            created_at_ms,
            updated_at_ms,
        }
    }

    fn store(snippets: Vec<SnippetRecord>, deleted: Vec<(&str, u64)>) -> SnippetStore {
        let mut store = SnippetStore {
            version: 1,
            snippets,
            deleted: deleted
                .into_iter()
                .map(|(id, deleted_at_ms)| Tombstone {
                    id: id.to_string(),
                    deleted_at_ms,
                })
                .collect(),
            folded: Folded::default(),
        };
        // Canonical order, as a merge leaves it.
        store.merge(&SnippetStore::default());
        store
    }

    fn merged(a: &SnippetStore, b: &SnippetStore) -> SnippetStore {
        let mut out = a.clone();
        out.merge(b);
        out
    }

    fn ids(store: &SnippetStore) -> Vec<&str> {
        store.snippets().iter().map(|s| s.id.as_str()).collect()
    }

    #[test]
    fn title_falls_back_to_first_non_empty_body_line() {
        assert_eq!(title_from_body("\n  ls -la\npwd"), "ls -la");
    }

    #[test]
    fn create_puts_the_new_snippet_first() {
        let mut store = SnippetStore::default();
        store.create("a".into(), "", "ls".into(), 1);
        store.create("b".into(), "  Build  ", "make".into(), 2);
        assert_eq!(ids(&store), ["b", "a"]);
        assert_eq!(store.get("b").unwrap().title, "Build");
        assert_eq!(store.get("a").unwrap().title, "ls");
    }

    #[test]
    fn an_edit_is_later_than_the_change_it_replaces_even_if_the_clock_went_back() {
        let mut store = SnippetStore::default();
        store.create("a".into(), "", "ls".into(), 100);
        let edited = store.update("a", "", "ls -la".into(), 50).unwrap();
        assert_eq!(edited.updated_at_ms, 101);
        assert!(store.update("missing", "", "pwd".into(), 200).is_none());
    }

    #[test]
    fn delete_leaves_a_tombstone() {
        let mut store = SnippetStore::default();
        store.create("a".into(), "", "ls".into(), 1);
        assert!(store.delete("a", 5));
        assert!(!store.delete("a", 6));
        assert!(store.snippets().is_empty());
        assert_eq!(
            store.deleted,
            [Tombstone {
                id: "a".into(),
                deleted_at_ms: 5
            }]
        );
    }

    #[test]
    fn matching_ignores_ascii_case_and_blank_queries() {
        let store = store(
            vec![
                record("a", "git PULL --rebase", 3, 3),
                record("b", "ls -la", 2, 2),
                record("c", "echo Ünïcode", 1, 1),
            ],
            vec![],
        );
        let found =
            |query: &str| -> Vec<&str> { store.matching(query).map(|s| s.id.as_str()).collect() };
        assert_eq!(found("pull"), ["a"]);
        assert_eq!(found("  Pull  "), ["a"]);
        assert_eq!(found(""), ["a", "b", "c"]);
        assert_eq!(found("   "), ["a", "b", "c"]);
        // Only ASCII folds, as before: `Ü` is not `ü`.
        assert_eq!(found("Ünï"), ["c"]);
        assert!(found("ünï").is_empty());
        assert!(found("nothing").is_empty());
    }

    #[test]
    fn matching_answers_as_lowercasing_both_sides_did() {
        let long = "a".repeat(4096);
        let bodies = ["git PULL --rebase", "echo Ünïcode", "", "ab", long.as_str()];
        let mut snippets = SnippetStore::default();
        for (index, body) in bodies.iter().enumerate() {
            snippets.create(format!("s{index}"), "", body.to_string(), index as u64);
        }
        let near_miss = format!("{}b", "a".repeat(63));
        let upper = "A".repeat(64);
        for query in [
            "pull", "PuLL --", "Ünï", "ünï", "b", "a", &near_miss, &upper,
        ] {
            let needle = query.trim().to_ascii_lowercase();
            let expected: Vec<&str> = snippets
                .snippets()
                .iter()
                .filter(|s| {
                    s.title.to_ascii_lowercase().contains(&needle)
                        || s.body.to_ascii_lowercase().contains(&needle)
                })
                .map(|s| s.id.as_str())
                .collect();
            let found: Vec<&str> = snippets.matching(query).map(|s| s.id.as_str()).collect();
            assert_eq!(found, expected, "{query:?}");
        }
    }

    #[test]
    fn a_change_is_searchable_at_once() {
        let mut store = SnippetStore::default();
        store.create("a".into(), "", "ls".into(), 1);
        assert_eq!(store.matching("ls").count(), 1);
        store.update("a", "", "pwd".into(), 2);
        assert_eq!(store.matching("ls").count(), 0);
        assert_eq!(store.matching("pwd").count(), 1);
        store.create("b".into(), "", "ls -la".into(), 3);
        assert_eq!(store.matching("ls").count(), 1);
        store.delete("b", 4);
        assert_eq!(store.matching("ls").count(), 0);
        let mut other = SnippetStore::default();
        other.create("c".into(), "", "make".into(), 5);
        store.merge(&other);
        assert_eq!(store.matching("make").count(), 1);
    }

    #[test]
    fn the_later_edit_wins() {
        let ours = store(vec![record("a", "ls", 1, 10)], vec![]);
        let theirs = store(vec![record("a", "ls -la", 1, 20)], vec![]);
        assert_eq!(merged(&ours, &theirs).get("a").unwrap().body, "ls -la");
        assert_eq!(merged(&theirs, &ours).get("a").unwrap().body, "ls -la");
    }

    #[test]
    fn a_deletion_beats_an_older_edit_and_loses_to_a_newer_one() {
        let deleted = store(vec![], vec![("a", 15)]);
        let older_edit = store(vec![record("a", "ls", 1, 10)], vec![]);
        let newer_edit = store(vec![record("a", "ls -la", 1, 20)], vec![]);
        assert!(merged(&older_edit, &deleted).snippets().is_empty());
        assert_eq!(ids(&merged(&newer_edit, &deleted)), ["a"]);
        // A tie goes to the deletion.
        let same_moment = store(vec![record("a", "ls", 1, 15)], vec![]);
        assert!(merged(&same_moment, &deleted).snippets().is_empty());
    }

    #[test]
    fn a_snippet_missing_from_one_copy_is_kept() {
        let ours = store(vec![record("a", "ls", 1, 1)], vec![]);
        let theirs = store(vec![record("b", "pwd", 2, 2)], vec![]);
        assert_eq!(ids(&merged(&ours, &theirs)), ["b", "a"]);
        assert_eq!(ids(&merged(&ours, &SnippetStore::default())), ["a"]);
    }

    #[test]
    fn merging_is_order_independent_and_idempotent() {
        let a = store(
            vec![record("x", "ls", 1, 5), record("y", "pwd", 2, 2)],
            vec![("z", 9)],
        );
        let b = store(
            vec![record("x", "ls -la", 1, 7), record("z", "top", 3, 3)],
            vec![("y", 1)],
        );
        let c = store(
            vec![record("x", "ls -lh", 1, 7), record("w", "df -h", 4, 4)],
            vec![("x", 6)],
        );

        assert_eq!(merged(&a, &b), merged(&b, &a));
        assert_eq!(merged(&merged(&a, &b), &c), merged(&a, &merged(&b, &c)));
        let all = merged(&merged(&a, &b), &c);
        assert_eq!(merged(&all, &all), all);
        assert_eq!(merged(&all, &a), all);

        // x: the two edits at 7 tie on time, and the greater content wins
        // on both sides; the deletion at 6 is older. y: the deletion at 1 is
        // older than y's creation at 2. z: deleted at 9, after it was made.
        assert_eq!(ids(&all), ["w", "y", "x"]);
        assert_eq!(all.get("x").unwrap().body, "ls -lh");
    }

    #[test]
    fn a_file_without_tombstones_still_reads() {
        let store: SnippetStore = serde_json::from_str(
            r#"{"version":1,"snippets":[{"id":"a","title":"List","body":"ls","created_at_ms":1,"updated_at_ms":2}]}"#,
        )
        .unwrap();
        assert_eq!(ids(&store), ["a"]);
        assert!(store.deleted.is_empty());
        assert!(!serde_json::to_string(&store).unwrap().contains("deleted"));
    }

    #[test]
    fn a_build_from_before_tombstones_sees_only_live_snippets() {
        // The store as those builds declared it.
        #[derive(Deserialize)]
        struct Earlier {
            #[serde(default)]
            snippets: Vec<SnippetRecord>,
        }
        let mut current = SnippetStore::default();
        current.create("a".into(), "", "ls".into(), 1);
        current.create("b".into(), "", "pwd".into(), 2);
        current.delete("a", 3);
        let earlier: Earlier =
            serde_json::from_str(&serde_json::to_string(&current).unwrap()).unwrap();
        assert_eq!(earlier.snippets, current.snippets);
    }
}
