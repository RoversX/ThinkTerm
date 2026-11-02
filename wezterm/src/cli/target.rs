//! `--pane NAME`: a pane named by its label or by its thread, or a pane on
//! a server as `MACHINE/NAME`.

use crate::cli::{printable, NoSuchTarget, UsageError};
use codec::PaneMachine;
use mux::pane::PaneId;
use std::collections::HashMap;
use wezterm_client::client::Client;

/// The pane `pane` names; without it, `pane_id`, or the current pane.
pub async fn resolve(
    client: &Client,
    pane_id: Option<PaneId>,
    pane: Option<&str>,
) -> anyhow::Result<PaneId> {
    match pane {
        Some(name) => find(client, name).await,
        None => client.resolve_pane_id(pane_id).await,
    }
}

/// The pane `pane` names, and the client that reaches it: a pane of this
/// machine's through `client`; one that is not, but on a server as
/// `MACHINE/NAME` names it, through the window this machine has open, which
/// is connected to the server -- as far as the window's Settings let it be.
pub async fn resolve_reaching_servers(
    client: Client,
    pane_id: Option<PaneId>,
    pane: Option<&str>,
) -> anyhow::Result<(Client, PaneId)> {
    let here = resolve(&client, pane_id, pane).await;
    let (why, name) = match (here, pane) {
        (Ok(pane_id), _) => return Ok((client, pane_id)),
        (Err(why), Some(name)) if name.contains('/') => (why, name),
        (Err(why), _) => return Err(why),
    };
    if !crate::cli::servers_terminals_reachable() {
        return Err(why.context(SERVER_PANES));
    }
    let asked = crate::cli::within_window_wait(async {
        let window = crate::cli::window_client().await?;
        let panes = candidates(&window).await.ok()?;
        Some((window, panes))
    })
    .await;
    let Some((window, panes)) = asked else {
        return Err(why.context(SERVER_PANES));
    };
    match on_machine(&panes, name.trim()) {
        Some(Ok(pane_id)) => Ok((window, pane_id)),
        Some(Err(why)) => Err(NoSuchTarget(why).into()),
        None => Err(why.context(SERVER_PANES)),
    }
}

/// How a pane on a server is named, said where a name names nothing here.
const SERVER_PANES: &str = "a pane on a server is named MACHINE/NAME, as `thinkterm cli list` \
     shows it, with ThinkTerm open and Settings › Agents letting programs here reach it";

/// The pane `name` names on a machine, as `MACHINE/ID` or `MACHINE/NAME`;
/// `None` when it starts with no machine's name.
fn on_machine(panes: &[Candidate], name: &str) -> Option<Result<PaneId, String>> {
    // Match the complete label, including any slashes in it. Prefer the
    // longest known label so that names printed by list round-trip.
    let (machine, rest) = name
        .match_indices('/')
        .rev()
        .map(|(at, _)| (&name[..at], &name[at + 1..]))
        .find(|(machine, _)| {
            panes.iter().any(|pane| {
                pane.machine
                    .as_ref()
                    .is_some_and(|on| same(&on.machine, machine))
            })
        })?;
    let there: Vec<Candidate> = panes
        .iter()
        .filter(|pane| {
            pane.machine
                .as_ref()
                .is_some_and(|on| same(&on.machine, machine))
        })
        .cloned()
        .collect();
    if there.is_empty() {
        return None;
    }
    // Its id there, as the panes on it know it.
    if let Some(id) = as_pane_id(rest) {
        let by_id: Vec<&Candidate> = there
            .iter()
            .filter(|pane| pane.machine.as_ref().is_some_and(|on| on.pane_id == id))
            .collect();
        return Some(match by_id.as_slice() {
            [pane] => Ok(pane.pane_id),
            [] => Err(format!(
                "there is no pane {id} on {machine}; `thinkterm cli list` shows them"
            )),
            _ => Err(format!(
                "{name:?} is ambiguous: multiple machines have the same label and pane id; \
                 give the machines distinct labels before targeting this pane"
            )),
        });
    }
    Some(choose(&there, rest, None))
}

/// A pane as a name can pick it.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub pane_id: PaneId,
    pub label: Option<String>,
    pub title: String,
    pub workspace: String,
    /// The project and the thread its workspace backs, if a thread does.
    pub thread: Option<(String, String)>,
    /// The server it is on, and its id there, for a pane of another machine.
    pub machine: Option<PaneMachine>,
}

async fn find(client: &Client, name: &str) -> anyhow::Result<PaneId> {
    let name = name.trim();
    if name.is_empty() {
        return Err(UsageError("--pane needs a name or an id".to_string()).into());
    }
    let panes = candidates(client).await?;
    // A name the caller's own thread holds once means that pane.
    let here = std::env::var("WEZTERM_PANE")
        .ok()
        .and_then(|pane| pane.parse::<PaneId>().ok())
        .and_then(|pane_id| panes.iter().find(|pane| pane.pane_id == pane_id))
        .map(|pane| pane.workspace.clone());
    choose(&panes, name, here.as_deref()).map_err(|why| NoSuchTarget(why).into())
}

/// Every pane, with its label and its thread.
pub async fn candidates(client: &Client) -> anyhow::Result<Vec<Candidate>> {
    let listing = client.list_panes().await?;
    let threads: HashMap<String, (String, String)> = client
        .get_workspace_names()
        .await?
        .names
        .into_iter()
        .map(|name| (name.workspace, (name.project, name.thread)))
        .collect();
    let mut panes = vec![];
    for tab in &listing.tabs {
        for entry in tab.entries() {
            panes.push(Candidate {
                pane_id: entry.pane_id,
                label: listing.pane_labels.get(&entry.pane_id).cloned(),
                title: entry.title.clone(),
                workspace: entry.workspace.clone(),
                thread: threads.get(&entry.workspace).cloned(),
                machine: listing.pane_machines.get(&entry.pane_id).cloned(),
            });
        }
    }
    Ok(panes)
}

/// The one pane `name` names. When it names several, the one of them in
/// the thread `here` -- the caller's -- if just one is; otherwise why not.
/// Digits alone are a pane's id, unless they are another pane's label too:
/// which of them is meant is not guessed.
pub fn choose(panes: &[Candidate], name: &str, here: Option<&str>) -> Result<PaneId, String> {
    let mut panes_in: HashMap<&str, usize> = HashMap::new();
    for pane in panes {
        *panes_in.entry(pane.workspace.as_str()).or_default() += 1;
    }
    let named: Vec<&Candidate> = panes
        .iter()
        .filter(|pane| names(pane, name, panes_in[pane.workspace.as_str()] == 1))
        .collect();
    let by_id = as_pane_id(name).and_then(|id| panes.iter().find(|pane| pane.pane_id == id));
    if let Some(pane) = by_id {
        if named.iter().all(|other| other.pane_id == pane.pane_id) {
            return Ok(pane.pane_id);
        }
        let mut both = vec![pane];
        both.extend(
            named
                .iter()
                .copied()
                .filter(|other| other.pane_id != pane.pane_id),
        );
        return Err(listing(
            format!(
                "{name:?} is pane {}'s id and another pane's name; name one by --pane-id:",
                pane.pane_id
            ),
            &both,
        ));
    }
    if let [pane] = named[..] {
        return Ok(pane.pane_id);
    }
    if let Some(here) = here {
        let mine: Vec<&Candidate> = named
            .iter()
            .copied()
            .filter(|pane| pane.workspace == here)
            .collect();
        if let [pane] = mine[..] {
            return Ok(pane.pane_id);
        }
    }
    if named.is_empty() {
        let threads: Vec<&Candidate> = panes.iter().filter(|pane| thread_is(pane, name)).collect();
        if !threads.is_empty() {
            return Err(listing(
                format!(
                    "the thread {name:?} has {} panes; name one as `thread/label` or by its id:",
                    threads.len()
                ),
                &threads,
            ));
        }
        if as_pane_id(name).is_some() {
            return Err(format!("there is no pane {name}"));
        }
        return Err(format!(
            "no pane is called {name:?}; `thinkterm cli list` shows the labels"
        ));
    }
    Err(listing(
        format!(
            "{name:?} could be any of {} panes; name one by its id:",
            named.len()
        ),
        &named,
    ))
}

/// `name` as a pane id: digits and nothing else.
fn as_pane_id(name: &str) -> Option<PaneId> {
    if name.bytes().all(|b| b.is_ascii_digit()) {
        name.parse().ok()
    } else {
        None
    }
}

/// `why`, then a line for each of `panes`.
fn listing(mut why: String, panes: &[&Candidate]) -> String {
    for pane in panes {
        why.push_str("\n  ");
        why.push_str(&describe(pane));
    }
    why
}

/// Whether `name` names `pane`: as its label, `thread/label`, or -- when
/// the pane is `alone` in its thread -- its thread's name, alone or after
/// its project's. Case does not matter.
fn names(pane: &Candidate, name: &str, alone: bool) -> bool {
    label_is(pane, name)
        || (alone && thread_is(pane, name))
        || name
            .match_indices('/')
            .any(|(at, _)| thread_is(pane, &name[..at]) && label_is(pane, &name[at + 1..]))
}

fn same(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

fn label_is(pane: &Candidate, name: &str) -> bool {
    pane.label.as_deref().is_some_and(|label| same(label, name))
}

fn thread_is(pane: &Candidate, name: &str) -> bool {
    pane.thread.as_ref().is_some_and(|(project, thread)| {
        same(thread, name) || same(&thread_display_name(project, thread), name)
    })
}

/// "Project · Thread", or the thread alone, as the sidebar names it.
pub fn thread_display_name(project: &str, thread: &str) -> String {
    if project.is_empty() || project == thread {
        thread.to_string()
    } else {
        format!("{project} \u{b7} {thread}")
    }
}

fn describe(pane: &Candidate) -> String {
    let place = match &pane.thread {
        Some((project, thread)) => format!("thread {}", thread_display_name(project, thread)),
        None => format!("workspace {}", pane.workspace),
    };
    let id = match &pane.machine {
        Some(on) => format!("{}/{}", on.machine, on.pane_id),
        None => pane.pane_id.to_string(),
    };
    printable(&format!(
        "{:>4}  {}  {}  ({place})",
        id,
        pane.label.as_deref().unwrap_or("-"),
        pane.title
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(
        pane_id: PaneId,
        label: Option<&str>,
        workspace: &str,
        thread: Option<&str>,
    ) -> Candidate {
        Candidate {
            pane_id,
            label: label.map(str::to_string),
            title: "zsh".to_string(),
            workspace: workspace.to_string(),
            thread: thread.map(|thread| ("project".to_string(), thread.to_string())),
            machine: None,
        }
    }

    /// Pane `pane_id` here, `there` on `machine`.
    fn on(pane_id: PaneId, machine: &str, there: PaneId, label: Option<&str>) -> Candidate {
        Candidate {
            machine: Some(PaneMachine {
                machine: machine.to_string(),
                pane_id: there,
            }),
            ..pane(pane_id, label, "w-server", None)
        }
    }

    #[test]
    fn a_pane_on_a_server_is_named_by_its_machine_and_its_id_or_label_there() {
        let mut panes = panes();
        panes.push(on(20, "devbox", 3, Some("logs")));
        panes.push(on(21, "devbox", 4, None));
        panes.push(on(22, "server-b", 3, Some("logs")));
        assert_eq!(on_machine(&panes, "devbox/3"), Some(Ok(20)));
        assert_eq!(on_machine(&panes, "DevBox/4"), Some(Ok(21)));
        assert_eq!(on_machine(&panes, "server-b/logs"), Some(Ok(22)));
        assert_eq!(on_machine(&panes, "devbox/logs"), Some(Ok(20)));
        let why = on_machine(&panes, "devbox/9").unwrap().unwrap_err();
        assert!(why.contains("no pane 9 on devbox"), "{}", why);
        // A thread of this machine's is no machine.
        assert_eq!(on_machine(&panes, "fix login/reviewer"), None);
        assert_eq!(on_machine(&panes, "devbox"), None);
    }

    #[test]
    fn duplicate_machine_labels_never_pick_the_first_pane() {
        for second in ["devbox", "DevBox"] {
            let panes = vec![on(20, "devbox", 3, None), on(21, second, 3, None)];
            assert!(on_machine(&panes, "devbox/3")
                .unwrap()
                .unwrap_err()
                .contains("ambiguous"));
        }
    }

    #[test]
    fn machine_labels_with_slashes_round_trip() {
        let panes = vec![
            on(20, "team/devbox", 3, Some("logs")),
            on(21, "team", 3, None),
        ];
        assert_eq!(on_machine(&panes, "team/devbox/3"), Some(Ok(20)));
        assert_eq!(on_machine(&panes, "TEAM/DEVBOX/logs"), Some(Ok(20)));
        assert_eq!(on_machine(&panes, "team/3"), Some(Ok(21)));
    }

    fn panes() -> Vec<Candidate> {
        vec![
            pane(1, Some("reviewer"), "w-login", Some("fix login")),
            pane(2, None, "w-login", Some("fix login")),
            pane(3, Some("reviewer"), "w-docs", Some("docs")),
            pane(4, Some("build"), "w-solo", Some("solo")),
            pane(5, None, "default", None),
        ]
    }

    #[test]
    fn a_label_held_once_names_its_pane() {
        assert_eq!(choose(&panes(), "build", None), Ok(4));
        assert_eq!(choose(&panes(), "BUILD", None), Ok(4));
    }

    #[test]
    fn a_shared_label_needs_the_callers_thread_or_an_id() {
        let why = choose(&panes(), "reviewer", None).unwrap_err();
        assert!(why.contains("any of 2 panes"), "{}", why);
        assert!(why.contains("\n     1  reviewer"), "{}", why);
        assert!(why.contains("\n     3  reviewer"), "{}", why);
        assert_eq!(choose(&panes(), "reviewer", Some("w-docs")), Ok(3));
        assert_eq!(choose(&panes(), "reviewer", Some("w-login")), Ok(1));
        assert!(choose(&panes(), "reviewer", Some("default")).is_err());
    }

    #[test]
    fn a_thread_names_its_pane_when_it_has_one() {
        assert_eq!(choose(&panes(), "solo", None), Ok(4));
        assert_eq!(choose(&panes(), "project \u{b7} solo", None), Ok(4));
        let why = choose(&panes(), "fix login", None).unwrap_err();
        assert!(why.contains("\"fix login\" has 2 panes"), "{}", why);
        assert!(why.contains("\n     2  -"), "{}", why);
    }

    #[test]
    fn a_thread_of_several_panes_leaves_a_label_of_its_name_alone() {
        let mut panes = panes();
        panes[4].label = Some("fix login".to_string());
        assert_eq!(choose(&panes, "fix login", None), Ok(5));
    }

    #[test]
    fn digits_are_an_id_unless_a_pane_is_called_that_too() {
        let mut panes = panes();
        assert_eq!(choose(&panes, "2", None), Ok(2));
        assert_eq!(
            choose(&panes, "9", None),
            Err("there is no pane 9".to_string())
        );
        let why = choose(&panes, "+2", None).unwrap_err();
        assert!(why.starts_with("no pane is called \"+2\""), "{}", why);

        panes[4].label = Some("2".to_string());
        let why = choose(&panes, "2", None).unwrap_err();
        assert!(
            why.contains("pane 2's id and another pane's name"),
            "{}",
            why
        );
        assert!(
            why.contains("\n     2  -") && why.contains("\n     5  2"),
            "{}",
            why
        );

        panes[4].label = Some("9".to_string());
        assert_eq!(
            choose(&panes, "9", None),
            Ok(5),
            "a name in digits, no such id"
        );
    }

    #[test]
    fn a_thread_and_a_label_name_one_pane() {
        assert_eq!(choose(&panes(), "fix login/reviewer", None), Ok(1));
        assert_eq!(choose(&panes(), "docs/reviewer", None), Ok(3));
        assert!(choose(&panes(), "docs/build", None).is_err());
    }

    #[test]
    fn a_name_nothing_has_says_so() {
        let why = choose(&panes(), "nobody", None).unwrap_err();
        assert!(why.starts_with("no pane is called \"nobody\""), "{}", why);
    }

    #[test]
    fn a_slash_in_a_thread_name_still_splits_at_the_label() {
        let panes = vec![pane(7, Some("tests"), "w", Some("feature/login"))];
        assert_eq!(choose(&panes, "feature/login/tests", None), Ok(7));
        assert_eq!(choose(&panes, "feature/login", None), Ok(7));
    }
}
