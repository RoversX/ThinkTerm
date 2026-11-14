use crate::cli::CliOutputFormatKind;
use clap::Parser;
use codec::ListPanesResponse;
use mux::pane::PaneId;
use tabout::{tabulate_output, Alignment, Column};
use wezterm_client::client::Client;
use wezterm_term::TerminalSize;

#[derive(Debug, Parser, Clone, Copy)]
pub struct ListCommand {
    /// Controls the output format.
    /// "table" and "json" are possible formats.
    #[arg(long = "format", default_value = "table")]
    format: CliOutputFormatKind,

    /// The same as `--format json`.
    #[arg(long, conflicts_with = "format")]
    json: bool,
}

impl ListCommand {
    pub async fn run(&self, client: Client) -> anyhow::Result<()> {
        let panes = client.list_panes().await?;
        let mut output_items = items(panes, |_| true);
        // Then the panes on servers, as the window this machine has open
        // is connected to them and its Settings let programs here have
        // them: asked of it unless it was asked already, and not waited
        // on for long.
        if crate::cli::servers_terminals_reachable()
            && !output_items.iter().any(|item| item.machine.is_some())
        {
            let theirs = crate::cli::within_window_wait(async {
                crate::cli::window_client().await?.list_panes().await.ok()
            })
            .await;
            if let Some(theirs) = theirs {
                let on_servers: Vec<PaneId> = theirs.pane_machines.keys().copied().collect();
                output_items.extend(items(theirs, |pane_id| on_servers.contains(&pane_id)));
            }
        }
        let format = if self.json {
            CliOutputFormatKind::Json
        } else {
            self.format
        };
        let reverse = crate::cli::reverse::list(&client).await.unwrap_or_default();
        match format {
            CliOutputFormatKind::Json => {
                #[derive(serde::Serialize)]
                #[serde(untagged)]
                enum Item<'a> {
                    Local(&'a CliListResultItem),
                    Reverse(&'a crate::cli::reverse::Listed),
                }
                let items: Vec<_> = output_items
                    .iter()
                    .map(Item::Local)
                    .chain(reverse.iter().map(Item::Reverse))
                    .collect();
                crate::cli::print_json(&items)?;
            }
            CliOutputFormatKind::Table => {
                let cols = vec![
                    Column {
                        name: "WINID".to_string(),
                        alignment: Alignment::Right,
                    },
                    Column {
                        name: "TABID".to_string(),
                        alignment: Alignment::Right,
                    },
                    Column {
                        name: "PANEID".to_string(),
                        alignment: Alignment::Right,
                    },
                    Column {
                        name: "WORKSPACE".to_string(),
                        alignment: Alignment::Left,
                    },
                    Column {
                        name: "SIZE".to_string(),
                        alignment: Alignment::Left,
                    },
                    Column {
                        name: "TITLE".to_string(),
                        alignment: Alignment::Left,
                    },
                    Column {
                        name: "CWD".to_string(),
                        alignment: Alignment::Left,
                    },
                    // Last, so scripts that pick columns by place still
                    // find the others where they were.
                    Column {
                        name: "LABEL".to_string(),
                        alignment: Alignment::Left,
                    },
                ];
                let mut data = output_items
                    .iter()
                    .map(|output_item| {
                        vec![
                            output_item.window_id.to_string(),
                            output_item.tab_id.to_string(),
                            output_item.pane_id.to_string(),
                            output_item.workspace.to_string(),
                            format!("{}x{}", output_item.size.cols, output_item.size.rows),
                            match &output_item.page {
                                Some(_) => format!("{} [page]", output_item.title),
                                None => output_item.title.to_string(),
                            },
                            output_item.cwd.to_string(),
                            output_item.label.clone().unwrap_or_default(),
                        ]
                    })
                    .collect::<Vec<_>>();
                data.extend(reverse.iter().map(|item| {
                    vec![
                        String::new(),
                        String::new(),
                        item.pane_id.clone(),
                        String::new(),
                        String::new(),
                        item.title.clone(),
                        String::new(),
                        item.label.clone(),
                    ]
                }));
                tabulate_output(&cols, &data, &mut std::io::stdout().lock())
                    .map_err(crate::stdout_error)?;
            }
        }
        Ok(())
    }
}

/// The panes `panes` lists, those `keep` keeps, as `list` prints them.
fn items(panes: ListPanesResponse, keep: impl Fn(PaneId) -> bool) -> Vec<CliListResultItem> {
    let mut output_items = vec![];
    for (tabroot, tab_title) in panes.tabs.into_iter().zip(panes.tab_titles.iter()) {
        let mut cursor = tabroot.into_tree().cursor();

        loop {
            if let Some(stack) = cursor.leaf_mut() {
                for entry in &stack.panes {
                    if !keep(entry.pane_id) {
                        continue;
                    }
                    let window_title = panes
                        .window_titles
                        .get(&entry.window_id)
                        .map(|s| s.as_str())
                        .unwrap_or("");
                    let mut item = CliListResultItem::from(
                        entry.clone(),
                        tab_title,
                        window_title,
                        panes.pane_labels.get(&entry.pane_id).cloned(),
                    );
                    // A plugin's page rather than a terminal: said, since
                    // none of what is asked of a terminal is of it.
                    if let Some(page) = panes.pane_pages.get(&entry.pane_id) {
                        item.page = Some(CliListPage {
                            plugin: page.plugin.clone(),
                            args: serde_json::from_str(&page.args).ok(),
                            id: page.id.clone(),
                            cwd: (!page.cwd.is_empty()).then(|| page.cwd.clone()),
                            terminal: page.terminal,
                        });
                    }
                    // Named by its machine and its id there.
                    if let Some(on) = panes.pane_machines.get(&entry.pane_id) {
                        item.machine = Some(on.machine.clone());
                        item.pane_id =
                            ListedPaneId::There(format!("{}/{}", on.machine, on.pane_id));
                    }
                    output_items.push(item);
                }
            }
            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(_) => break,
            }
        }
    }
    output_items
}

/// A pane's id as `list` gives it: a number for one of this machine's, and
/// `MACHINE/ID` for one on a server -- a string, so that it is never taken
/// for one of this machine's and passed to `--pane-id`, but to `--pane`.
#[derive(serde::Serialize)]
#[serde(untagged)]
enum ListedPaneId {
    Here(PaneId),
    There(String),
}

impl std::fmt::Display for ListedPaneId {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::Here(pane_id) => write!(f, "{pane_id}"),
            Self::There(target) => f.write_str(target),
        }
    }
}

#[derive(serde::Serialize)]
struct CliListResultPtySize {
    rows: usize,
    cols: usize,
    /// Pixel width of the pane, if known (can be zero)
    pixel_width: usize,
    /// Pixel height of the pane, if known (can be zero)
    pixel_height: usize,
    /// dpi of the pane, if known (can be zero)
    dpi: u32,
}

/// A plugin's page in a pane, as `list` gives it.
#[derive(serde::Serialize)]
struct CliListPage {
    plugin: String,
    /// What it opens with: what it was opened with, or what its plugin
    /// changed that to.
    #[serde(skip_serializing_if = "Option::is_none")]
    args: Option<serde_json::Value>,
    /// Which page it is, to its plugin.
    id: String,
    /// The directory of the terminal it was opened from.
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    /// That terminal, by its pane's id.
    #[serde(skip_serializing_if = "Option::is_none")]
    terminal: Option<u64>,
}

// This will be serialized to JSON via the 'List' command.
// As such it is intended to be a stable output format,
// Thus we need to be careful about both the fields and their types,
// herein as they are directly reflected in the output.
#[derive(serde::Serialize)]
struct CliListResultItem {
    window_id: mux::window::WindowId,
    tab_id: mux::tab::TabId,
    pane_id: ListedPaneId,
    /// The label the pane was given, if any.
    label: Option<String>,
    /// The server the pane is on, when it is not this machine's: then
    /// `pane_id` is `MACHINE/ID`, for `--pane`.
    machine: Option<String>,
    /// The plugin's page the pane holds in place of a terminal, for one
    /// that does: it has no text, and takes none.
    #[serde(skip_serializing_if = "Option::is_none")]
    page: Option<CliListPage>,
    workspace: String,
    size: CliListResultPtySize,
    title: String,
    cwd: String,
    /// Cursor x coordinate from top left of non-scrollback pane area
    cursor_x: usize,
    /// Cursor y coordinate from top left of non-scrollback pane area
    cursor_y: usize,
    cursor_shape: termwiz::surface::CursorShape,
    cursor_visibility: termwiz::surface::CursorVisibility,
    /// Number of cols from the left of the tab area to the left of this pane
    left_col: usize,
    /// Number of rows from the top of the tab area to the top of this pane
    top_row: usize,
    tab_title: String,
    window_title: String,
    is_active: bool,
    is_zoomed: bool,
    tty_name: Option<String>,
}

impl CliListResultItem {
    fn from(
        pane: mux::tab::PaneEntry,
        tab_title: &str,
        window_title: &str,
        label: Option<String>,
    ) -> CliListResultItem {
        let mux::tab::PaneEntry {
            window_id,
            tab_id,
            pane_id,
            workspace,
            title,
            working_dir,
            cursor_pos,
            physical_top,
            left_col,
            top_row,
            is_active_pane,
            is_zoomed_pane,
            tty_name,
            size:
                TerminalSize {
                    rows,
                    cols,
                    pixel_width,
                    pixel_height,
                    dpi,
                },
            ..
        } = pane;

        CliListResultItem {
            window_id,
            tab_id,
            pane_id: ListedPaneId::Here(pane_id),
            label,
            machine: None,
            page: None,
            workspace,
            size: CliListResultPtySize {
                rows,
                cols,
                pixel_width,
                pixel_height,
                dpi,
            },
            title,
            cwd: working_dir
                .as_ref()
                .map(|url| url.url.as_str())
                .unwrap_or("")
                .to_string(),
            cursor_x: cursor_pos.x,
            cursor_y: cursor_pos.y.saturating_sub(physical_top) as usize,
            cursor_shape: cursor_pos.shape,
            cursor_visibility: cursor_pos.visibility,
            left_col,
            top_row,
            tab_title: tab_title.to_string(),
            window_title: window_title.to_string(),
            is_active: is_active_pane,
            is_zoomed: is_zoomed_pane,
            tty_name,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pane_on_a_server_is_listed_by_a_name_no_id_here_could_be() {
        let here = serde_json::to_string(&ListedPaneId::Here(3)).unwrap();
        let there = serde_json::to_string(&ListedPaneId::There("devbox/3".into())).unwrap();
        assert_eq!(here, "3");
        assert_eq!(there, "\"devbox/3\"");
        assert_eq!(
            ListedPaneId::There("devbox/3".into()).to_string(),
            "devbox/3"
        );
    }
}
