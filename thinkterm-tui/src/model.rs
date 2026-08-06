use codec::{
    ThinkTermSessionProject, ThinkTermSessionSpace, ThinkTermSessionState, ThinkTermSessionTab,
    ThinkTermSessionThread, ThinkTermSessionWorkStatus,
};
use mux::tab::TabId;
use std::collections::{BTreeMap, HashMap};

/// A thread is only unique inside one running server.  Keep both the configured
/// transport name and the server runtime id in every UI key so that a restart
/// or two servers with identical tree ids can never cross-select each other.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ThreadKey {
    pub domain_name: String,
    pub server_id: String,
    pub thread_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TreeNodeKey {
    Domain(String),
    Space {
        domain_name: String,
        space_id: String,
    },
    Project {
        domain_name: String,
        project_id: String,
    },
    Thread(ThreadKey),
}

#[derive(Clone, Debug)]
pub struct ThreadRow {
    pub key: ThreadKey,
    pub space: ThinkTermSessionSpace,
    pub project: ThinkTermSessionProject,
    pub thread: ThinkTermSessionThread,
}

impl ThreadRow {
    pub fn status_marker(&self) -> &'static str {
        if self.thread.is_unread {
            return "!";
        }
        match self.thread.work_status {
            ThinkTermSessionWorkStatus::Idle => " ",
            ThinkTermSessionWorkStatus::Running => "*",
            ThinkTermSessionWorkStatus::NeedsAttention => "!",
            ThinkTermSessionWorkStatus::FinishedUnseen => "+",
        }
    }

    /// How much this thread wants to be looked at, lowest first.
    ///
    /// Waiting on a person outranks having finished, which outranks still
    /// working — the order in which ignoring one costs something. There is no
    /// timestamp on a thread, so anything idle is left to tree order rather
    /// than guessing at recency.
    pub fn attention_rank(&self) -> u8 {
        if self.thread.is_unread {
            return 0;
        }
        match self.thread.work_status {
            ThinkTermSessionWorkStatus::NeedsAttention => 1,
            ThinkTermSessionWorkStatus::FinishedUnseen => 2,
            ThinkTermSessionWorkStatus::Running => 3,
            ThinkTermSessionWorkStatus::Idle => 4,
        }
    }

    pub fn has_attention(&self) -> bool {
        self.thread.is_unread
            || matches!(
                self.thread.work_status,
                ThinkTermSessionWorkStatus::NeedsAttention
                    | ThinkTermSessionWorkStatus::FinishedUnseen
            )
    }
}

/// Server snapshots are the shared truth.  `rows` is a derived, normalized
/// index rebuilt only when a snapshot changes; rendering and hit testing never
/// clone and re-walk the complete tree.
#[derive(Default)]
pub struct AppModel {
    domains: BTreeMap<String, ThinkTermSessionState>,
    rows: Vec<ThreadRow>,
    selected_thread: Option<ThreadKey>,
    selected_tabs: HashMap<ThreadKey, TabId>,
}

impl AppModel {
    /// Generations are only comparable during one server connection.
    pub fn begin_connection_generation(&mut self, domain_name: &str) {
        if let Some(state) = self.domains.get_mut(domain_name) {
            state.generation = 0;
        }
    }

    pub fn apply_snapshot(
        &mut self,
        domain_name: impl Into<String>,
        state: ThinkTermSessionState,
    ) -> bool {
        let domain_name = domain_name.into();
        if self.domains.get(&domain_name).is_some_and(|prior| {
            prior.server_id == state.server_id && state.generation <= prior.generation
        }) {
            return false;
        }

        let server_changed = self
            .domains
            .get(&domain_name)
            .is_some_and(|prior| prior.server_id != state.server_id);
        if server_changed {
            self.selected_tabs
                .retain(|key, _| key.domain_name != domain_name);
            if self
                .selected_thread
                .as_ref()
                .is_some_and(|key| key.domain_name == domain_name)
            {
                self.selected_thread = None;
            }
        }

        self.domains.insert(domain_name, state);
        self.rebuild_index();
        self.normalize_selection();
        true
    }

    pub fn remove_domain(&mut self, domain_name: &str) -> bool {
        if self.domains.remove(domain_name).is_none() {
            return false;
        }
        self.selected_tabs
            .retain(|key, _| key.domain_name != domain_name);
        if self
            .selected_thread
            .as_ref()
            .is_some_and(|key| key.domain_name == domain_name)
        {
            self.selected_thread = None;
        }
        self.rebuild_index();
        self.normalize_selection();
        true
    }

    fn rebuild_index(&mut self) {
        self.rows.clear();
        for (domain_name, state) in &self.domains {
            let spaces: HashMap<&str, &ThinkTermSessionSpace> = state
                .spaces
                .iter()
                .map(|space| (space.id.as_str(), space))
                .collect();
            for project in &state.projects {
                let Some(space) = spaces.get(project.space_id.as_str()) else {
                    continue;
                };
                for thread in &project.threads {
                    self.rows.push(ThreadRow {
                        key: ThreadKey {
                            domain_name: domain_name.clone(),
                            server_id: state.server_id.clone(),
                            thread_id: thread.id.clone(),
                        },
                        space: (*space).clone(),
                        project: project.clone(),
                        thread: thread.clone(),
                    });
                }
            }
        }
    }

    fn normalize_selection(&mut self) {
        let selected_still_exists = self
            .selected_thread
            .as_ref()
            .is_some_and(|selected| self.rows.iter().any(|row| &row.key == selected));
        if !selected_still_exists {
            // Tree order says where a thread lives, not whether it wants
            // attention, and opening on whichever happened to sort first meant
            // arriving at the least interesting one. Rank by what the thread is
            // doing instead; tree order only breaks ties.
            self.selected_thread = self
                .rows
                .iter()
                .filter(|row| !row.thread.tabs.is_empty())
                .min_by_key(|row| row.attention_rank())
                .or_else(|| self.rows.first())
                .map(|row| row.key.clone());
        }
        self.normalize_selected_tab();
    }

    fn normalize_selected_tab(&mut self) {
        let Some(key) = self.selected_thread.clone() else {
            return;
        };
        let Some(row) = self.rows.iter().find(|row| row.key == key) else {
            self.selected_tabs.remove(&key);
            return;
        };
        if row.thread.tabs.is_empty() {
            self.selected_tabs.remove(&key);
            return;
        }
        if self
            .selected_tabs
            .get(&key)
            .is_some_and(|id| row.thread.tabs.iter().any(|tab| tab.tab_id == *id))
        {
            return;
        }
        let tab = row
            .thread
            .tabs
            .iter()
            .find(|tab| tab.is_active)
            .unwrap_or(&row.thread.tabs[0]);
        self.selected_tabs.insert(key, tab.tab_id);
    }

    pub fn domain(&self, domain_name: &str) -> Option<&ThinkTermSessionState> {
        self.domains.get(domain_name)
    }

    pub fn domains(&self) -> impl Iterator<Item = (&str, &ThinkTermSessionState)> {
        self.domains
            .iter()
            .map(|(name, state)| (name.as_str(), state))
    }

    pub fn rows(&self) -> &[ThreadRow] {
        &self.rows
    }

    pub fn selected_key(&self) -> Option<&ThreadKey> {
        self.selected_thread.as_ref()
    }

    pub fn selected_row(&self) -> Option<&ThreadRow> {
        let selected = self.selected_thread.as_ref()?;
        self.rows.iter().find(|row| &row.key == selected)
    }

    pub fn row(&self, key: &ThreadKey) -> Option<&ThreadRow> {
        self.rows.iter().find(|row| &row.key == key)
    }

    pub fn generation(&self) -> u64 {
        self.selected_row()
            .and_then(|row| self.domains.get(&row.key.domain_name))
            .map_or(0, |state| state.generation)
    }

    pub fn tree_revision(&self) -> u64 {
        self.selected_row()
            .and_then(|row| self.domains.get(&row.key.domain_name))
            .map_or(0, |state| state.tree_revision)
    }

    pub fn select_thread(&mut self, key: &ThreadKey) -> bool {
        if self.selected_thread.as_ref() == Some(key)
            || !self.rows.iter().any(|row| &row.key == key)
        {
            return false;
        }
        self.selected_thread = Some(key.clone());
        self.normalize_selected_tab();
        true
    }

    pub fn select_relative_thread(&mut self, delta: isize) -> bool {
        if self.rows.is_empty() {
            return false;
        }
        let current = self
            .selected_thread
            .as_ref()
            .and_then(|key| self.rows.iter().position(|row| &row.key == key))
            .unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(self.rows.len() as isize) as usize;
        let key = self.rows[next].key.clone();
        self.select_thread(&key)
    }

    pub fn tabs_for_selected_thread(&self) -> &[ThinkTermSessionTab] {
        self.selected_row()
            .map(|row| row.thread.tabs.as_slice())
            .unwrap_or_default()
    }

    /// Read-only by design: render code must never repair selection.
    pub fn selected_tab(&self) -> Option<&ThinkTermSessionTab> {
        let key = self.selected_thread.as_ref()?;
        let selected = self.selected_tabs.get(key)?;
        self.tabs_for_selected_thread()
            .iter()
            .find(|tab| tab.tab_id == *selected)
    }

    pub fn select_tab(&mut self, tab_id: TabId) -> bool {
        let Some(key) = self.selected_thread.clone() else {
            return false;
        };
        if !self
            .tabs_for_selected_thread()
            .iter()
            .any(|tab| tab.tab_id == tab_id)
        {
            return false;
        }
        self.selected_tabs.insert(key, tab_id) != Some(tab_id)
    }

    pub fn select_relative_tab(&mut self, delta: isize) -> bool {
        let tabs = self.tabs_for_selected_thread();
        if tabs.is_empty() {
            return false;
        }
        let current_id = self.selected_tab().map(|tab| tab.tab_id);
        let current = current_id
            .and_then(|id| tabs.iter().position(|tab| tab.tab_id == id))
            .unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(tabs.len() as isize) as usize;
        let tab_id = tabs[next].tab_id;
        self.select_tab(tab_id)
    }

    pub fn attention_count(&self) -> usize {
        self.rows.iter().filter(|row| row.has_attention()).count()
    }

    pub fn next_attention(&self, after: Option<&ThreadKey>) -> Option<ThreadKey> {
        if self.rows.is_empty() {
            return None;
        }
        let start = after
            .and_then(|key| self.rows.iter().position(|row| &row.key == key))
            .map_or(0, |index| index + 1);
        (0..self.rows.len())
            .map(|offset| (start + offset) % self.rows.len())
            .find_map(|index| {
                self.rows[index]
                    .has_attention()
                    .then(|| self.rows[index].key.clone())
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(server: &str, generation: u64, thread_ids: &[&str]) -> ThinkTermSessionState {
        ThinkTermSessionState {
            server_id: server.into(),
            tree_revision: generation,
            generation,
            spaces: vec![ThinkTermSessionSpace {
                id: "space".into(),
                name: "Space".into(),
                ..Default::default()
            }],
            projects: vec![ThinkTermSessionProject {
                id: "project".into(),
                space_id: "space".into(),
                name: "Project".into(),
                threads: thread_ids
                    .iter()
                    .map(|id| ThinkTermSessionThread {
                        id: (*id).into(),
                        project_id: "project".into(),
                        name: (*id).into(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }],
        }
    }

    /// Opening on whichever thread happened to sort first meant arriving at
    /// the least interesting one while another was waiting to be answered.
    #[test]
    fn the_thread_that_wants_attention_is_the_one_opened() {
        let mut snapshot = state("server", 1, &["quiet", "running", "done", "asking"]);
        let threads = &mut snapshot.projects[0].threads;
        // Everything has a live tab, so tree order alone would pick "quiet".
        for thread in threads.iter_mut() {
            thread.tabs = vec![ThinkTermSessionTab::default()];
        }
        threads[1].work_status = ThinkTermSessionWorkStatus::Running;
        threads[2].work_status = ThinkTermSessionWorkStatus::FinishedUnseen;
        threads[3].work_status = ThinkTermSessionWorkStatus::NeedsAttention;

        let mut model = AppModel::default();
        model.apply_snapshot("a", snapshot.clone());
        assert_eq!(model.selected_row().unwrap().thread.id, "asking");

        // With nobody waiting, finishing unseen outranks still working.
        snapshot.projects[0].threads[3].work_status = ThinkTermSessionWorkStatus::Idle;
        let mut model = AppModel::default();
        model.apply_snapshot("a", snapshot.clone());
        assert_eq!(model.selected_row().unwrap().thread.id, "done");

        // ...and with nothing to report at all, tree order decides.
        for thread in snapshot.projects[0].threads.iter_mut() {
            thread.work_status = ThinkTermSessionWorkStatus::Idle;
        }
        let mut model = AppModel::default();
        model.apply_snapshot("a", snapshot);
        assert_eq!(model.selected_row().unwrap().thread.id, "quiet");
    }

    #[test]
    fn ignores_stale_snapshots_from_the_same_server() {
        let mut model = AppModel::default();
        assert!(model.apply_snapshot("a", state("server", 2, &["a", "b"])));
        let key = model.rows()[1].key.clone();
        assert!(model.select_thread(&key));
        assert!(!model.apply_snapshot("a", state("server", 1, &["a"])));
        assert_eq!(model.selected_row().unwrap().thread.id, "b");
    }

    #[test]
    fn accepts_lower_generation_from_a_restarted_server() {
        let mut model = AppModel::default();
        model.apply_snapshot("a", state("old", 20, &["old"]));
        assert!(model.apply_snapshot("a", state("new", 1, &["new"])));
        assert_eq!(model.selected_row().unwrap().thread.id, "new");
    }

    #[test]
    fn identical_thread_ids_on_two_servers_do_not_collide() {
        let mut model = AppModel::default();
        model.apply_snapshot("a", state("one", 1, &["main"]));
        model.apply_snapshot("b", state("two", 1, &["main"]));
        assert_eq!(model.rows().len(), 2);
        assert_ne!(model.rows()[0].key, model.rows()[1].key);
        let key = model.rows()[1].key.clone();
        assert!(model.select_thread(&key));
        assert_eq!(model.selected_row().unwrap().key.domain_name, "b");
    }

    #[test]
    fn selected_tab_is_pure_after_snapshot_normalization() {
        let mut snapshot = state("server", 1, &["main"]);
        snapshot.projects[0].threads[0].tabs = vec![ThinkTermSessionTab {
            tab_id: 7,
            is_active: true,
            ..Default::default()
        }];
        let mut model = AppModel::default();
        model.apply_snapshot("a", snapshot);
        assert_eq!(model.selected_tab().map(|tab| tab.tab_id), Some(7));
        assert_eq!(model.selected_tab().map(|tab| tab.tab_id), Some(7));
    }
}
