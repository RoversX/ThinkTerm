use clap::Parser;
use codec::PagePlace;
use config::ConfigHandle;
use mux::pane::PaneId;
use mux::tab::{SplitDirection, SplitRequest, SplitSize};
use thinkterm_proto::PageSpec;
use wezterm_client::client::Client;

#[derive(Debug, Parser, Clone)]
pub struct OpenPage {
    /// The plugin whose page to open, by id: one on the machine the pane
    /// is on, whose manifest has a `[page]`.
    plugin: String,

    /// The pane the page is opened by: in a tab of its window, or with
    /// `--stack` or a direction in its place or beside it.
    /// The default is to use the current pane based on the
    /// environment variable WEZTERM_PANE.
    #[arg(long)]
    pane_id: Option<PaneId>,

    /// The pane by name rather than id: its label, its thread's name (a
    /// thread with one pane), or `thread/label`.
    #[arg(long, conflicts_with = "pane_id")]
    pane: Option<String>,

    /// What the page is opened with, as JSON: the plugin's to make sense
    /// of.
    #[arg(long)]
    args: Option<String>,

    /// What the page's tab is called. The plugin's id when omitted.
    #[arg(long)]
    title: Option<String>,

    /// The icon the page's tab shows, by the name a plugin's manifest
    /// gives one. A puzzle piece when omitted.
    #[arg(long)]
    icon: Option<String>,

    /// Open it in the pane's place, as another of the tabs there, rather
    /// than in a tab of the window.
    #[arg(long, conflicts_with_all=&["left", "right", "top", "bottom"])]
    stack: bool,

    /// Open it in a split, on the pane's left
    #[arg(long, conflicts_with_all=&["right", "top", "bottom"])]
    left: bool,

    /// Open it in a split, on the pane's right
    #[arg(long, conflicts_with_all=&["left", "top", "bottom"])]
    right: bool,

    /// Open it in a split, above the pane
    #[arg(long, conflicts_with_all=&["left", "right", "bottom"])]
    top: bool,

    /// Open it in a split, below the pane
    #[arg(long, conflicts_with_all=&["left", "right", "top"])]
    bottom: bool,

    /// Label the page's pane, so that `--pane LABEL` finds it.
    #[arg(long)]
    label: Option<String>,
}

impl OpenPage {
    /// The page asked for, held to what a page may be before anything is
    /// asked of the server.
    fn page(&self) -> anyhow::Result<PageSpec> {
        let args = match self.args.as_deref().map(str::trim) {
            None | Some("") => String::new(),
            Some(text) => {
                let value: serde_json::Value = serde_json::from_str(text)
                    .map_err(|err| anyhow::anyhow!("--args is not JSON: {err}"))?;
                value.to_string()
            }
        };
        // Its id, and where it was opened from, the server gives it.
        let page = PageSpec {
            plugin: self.plugin.clone(),
            args,
            title: self.title.clone().unwrap_or_default(),
            icon: self.icon.clone().unwrap_or_default(),
            ..Default::default()
        };
        match page.refusal() {
            Some(why) => anyhow::bail!("{why}"),
            None => Ok(page),
        }
    }

    pub async fn run(self, client: Client, config: &ConfigHandle) -> anyhow::Result<()> {
        // Checked before anything is opened.
        let page = self.page()?;
        let label = match &self.label {
            Some(text) => crate::cli::set_pane_label::checked_label(text)?,
            None => None,
        };
        let pane_id =
            crate::cli::target::resolve(&client, self.pane_id, self.pane.as_deref()).await?;

        let split = |direction, target_is_second| PagePlace::Split {
            pane_id,
            split_request: SplitRequest {
                direction,
                target_is_second,
                size: SplitSize::Percent(50),
                top_level: false,
            },
        };
        let place = if self.stack {
            PagePlace::Stack { pane_id }
        } else if self.left || self.right {
            split(SplitDirection::Horizontal, self.right)
        } else if self.top || self.bottom {
            split(SplitDirection::Vertical, self.bottom)
        } else {
            let panes = client.list_panes().await?;
            let window_id = panes
                .tabs
                .iter()
                .flat_map(|tab| tab.entries())
                .find(|entry| entry.pane_id == pane_id)
                .map(|entry| entry.window_id);
            PagePlace::Tab {
                window_id,
                workspace: config
                    .default_workspace
                    .as_deref()
                    .unwrap_or(mux::DEFAULT_WORKSPACE)
                    .to_string(),
                pane_id: Some(pane_id),
            }
        };

        let opened = client
            .spawn_page(codec::SpawnPage {
                page,
                place,
                size: config.initial_size(0, None),
            })
            .await?;

        log::debug!("{:?}", opened);
        if let Some(label) = label {
            let labelled = client
                .set_pane_label(codec::PaneLabelChanged {
                    pane_id: opened.pane_id,
                    label: Some(label),
                })
                .await;
            if let Err(err) = labelled {
                eprintln!("pane {} was not labelled: {err:#}", opened.pane_id);
            }
        }
        println!("{}", opened.pane_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asked(arguments: &[&str]) -> OpenPage {
        OpenPage::parse_from(std::iter::once("open-page").chain(arguments.iter().copied()))
    }

    #[test]
    fn a_page_is_asked_for_by_its_plugin_and_what_to_open_it_with() {
        let page = asked(&["board"]).page().unwrap();
        assert_eq!((page.plugin.as_str(), page.args.as_str()), ("board", ""));
        assert_eq!(page.title, "", "the plugin's name, which a client knows");
        let page = asked(&["board", "--args", " { \"board\" : \"release\" } ", "--title", "Plan"])
            .page()
            .unwrap();
        assert_eq!(page.args, "{\"board\":\"release\"}");
        assert_eq!(page.title, "Plan");
        assert!(asked(&["board", "--args", "{release"]).page().is_err());
        assert!(asked(&["Not A Plugin"]).page().is_err());
        let big = format!("\"{}\"", "x".repeat(thinkterm_proto::PAGE_ARGS_LIMIT));
        assert!(asked(&["board", "--args", &big]).page().is_err());
    }
}
