use crate::spawn::SpawnWhere;
use config::keyassignment::{SpawnCommand, SpawnTabDomain};
use config::TermConfig;
use std::sync::Arc;

impl super::TermWindow {
    pub fn spawn_command(&mut self, spawn: &SpawnCommand, spawn_where: SpawnWhere) {
        if matches!(spawn_where, SpawnWhere::NewTab | SpawnWhere::NewTabAt(_))
            && self.active_content_view_is_remote_thread()
        {
            return;
        }

        if matches!(spawn_where, SpawnWhere::NewTab | SpawnWhere::NewTabAt(_))
            && spawn.domain == SpawnTabDomain::CurrentPaneDomain
        {
            if let Some(pane_id) = self.get_active_pane_or_overlay().map(|pane| pane.pane_id()) {
                if self.redirect_failed_remote_tab_spawn_to_thread_view(pane_id) {
                    return;
                }
            }
        }

        let deactivate_content_view_after_spawn = matches!(
            spawn_where,
            SpawnWhere::NewTab | SpawnWhere::NewTabAt(_) | SpawnWhere::SplitPane(_)
        ) && self.content_view_foreground();

        let size = if spawn_where == SpawnWhere::NewWindow {
            self.config.initial_size(
                self.dimensions.dpi as u32,
                crate::cell_pixel_dims(&self.config, self.dimensions.dpi as f64).ok(),
            )
        } else {
            self.terminal_size
        };
        let term_config = Arc::new(TermConfig::with_config(self.config.clone()));
        let layout_mutation_reason = match spawn_where {
            SpawnWhere::NewWindow => None,
            SpawnWhere::NewTab | SpawnWhere::NewTabAt(_) => Some("tab spawned"),
            SpawnWhere::SplitPane(_) => Some("pane split"),
        };

        crate::spawn::spawn_command_impl(
            spawn,
            spawn_where,
            size,
            Some(self.mux_window_id),
            term_config,
            self.window.clone(),
            layout_mutation_reason,
            deactivate_content_view_after_spawn.then(|| {
                Box::new(|term_window: &mut crate::termwindow::TermWindow| {
                    term_window.set_content_view_active(false);
                }) as crate::spawn::SpawnSuccessAction
            }),
        )
    }

    pub fn spawn_tab(&mut self, domain: &SpawnTabDomain) {
        self.spawn_command(
            &SpawnCommand {
                domain: domain.clone(),
                ..Default::default()
            },
            SpawnWhere::NewTab,
        );
    }

    pub fn spawn_tab_to_right(&mut self, domain: &SpawnTabDomain) {
        let insert_idx = mux::Mux::get()
            .get_window(self.mux_window_id)
            .map(|window| window.get_active_idx().saturating_add(1))
            .unwrap_or(0);

        self.spawn_command(
            &SpawnCommand {
                domain: domain.clone(),
                ..Default::default()
            },
            SpawnWhere::NewTabAt(insert_idx),
        );
    }
}
