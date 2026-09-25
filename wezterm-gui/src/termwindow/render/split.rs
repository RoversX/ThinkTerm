use crate::termwindow::render::TripleLayerQuadAllocator;
use crate::termwindow::{UIItem, UIItemType};
use mux::pane::Pane;
use mux::tab::{PositionedSplit, SplitDirection};
use std::sync::Arc;

impl crate::TermWindow {
    pub fn paint_split(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        split: &PositionedSplit,
        pane: &Arc<dyn Pane>,
    ) -> anyhow::Result<()> {
        let palette = pane.palette();
        let foreground = palette.split.to_linear();
        let cell_width = self.render_metrics.cell_size.width as f32;
        let cell_height = self.render_metrics.cell_size.height as f32;

        let border = self.get_os_border();
        let first_row_offset = self.pane_area_insets().0 + border.top.get() as f32;

        let (padding_left, padding_top) = self.padding_left_top();

        let pos_y = split.top as f32 * cell_height + first_row_offset + padding_top;
        let pos_x = split.left as f32 * cell_width + padding_left + border.left.get() as f32;

        if split.direction == SplitDirection::Horizontal {
            let x = pos_x + (cell_width / 2.0);
            let width = self.render_metrics.underline_height as f32;
            let mut top = pos_y - (cell_height / 2.0);
            let mut bottom = top + (1. + split.size as f32) * cell_height;
            // Between the top panes the line reaches up beside their lifted
            // nav bars (see `pane_nav_lift`), straight through the terminal
            // bar under them; along the bottom it stops short of the bar.
            let mut lift = 0.;
            match self.terminal_bar_placement() {
                Some(bar) if !bar.at_bottom && split.top == 0 => lift = bar.height,
                Some(bar) if bar.at_bottom => bottom = bottom.min(self.pane_area_bottom()),
                _ => {}
            }
            // Never above the terminal area: with no top padding (tabline.wez
            // zeroes it) the half-cell overhang reached into the window tab
            // row.
            let area_top = border.top.get() as f32 + self.tab_bar_pixel_height().unwrap_or(0.);
            top = (top - lift).max(area_top);
            self.filled_rectangle(
                layers,
                2,
                euclid::rect(x, top, width, (bottom - top).max(0.)),
                foreground,
            )?;
            // The drag area follows it up.
            let item_top =
                padding_top as usize + first_row_offset as usize + split.top * cell_height as usize;
            self.ui_items.push(UIItem {
                x: border.left.get() as usize
                    + padding_left as usize
                    + (split.left * cell_width as usize),
                width: cell_width as usize,
                y: item_top.saturating_sub(lift as usize),
                height: split.size * cell_height as usize + lift as usize,
                item_type: UIItemType::Split(split.clone()),
            });
        } else {
            // Match the pane chrome span: nav bars extend to the sidebar
            // and window edges (pane_chrome_span), and this divider is
            // drawn over the lower pane's top strip row, so it must cover
            // the same extended width or the strip peeks out beside it.
            let mut line_left = pos_x - (cell_width / 2.0);
            let mut line_right = line_left + (1.0 + split.size as f32) * cell_width;
            if split.left == 0 {
                line_left = self.tab_bar_left_edge() as f32;
            }
            if split.left + split.size >= self.terminal_size.cols {
                line_right = (self.dimensions.pixel_width as f32
                    - border.right.get() as f32
                    - self.right_sidebar_width() as f32)
                    .max(line_right);
            }
            self.filled_rectangle(
                layers,
                2,
                euclid::rect(
                    line_left,
                    pos_y + (cell_height / 2.0),
                    (line_right - line_left).max(1.0),
                    self.render_metrics.underline_height as f32,
                ),
                foreground,
            )?;
            self.ui_items.push(UIItem {
                x: border.left.get() as usize
                    + padding_left as usize
                    + (split.left * cell_width as usize),
                width: split.size * cell_width as usize,
                y: padding_top as usize
                    + first_row_offset as usize
                    + split.top * cell_height as usize,
                height: cell_height as usize,
                item_type: UIItemType::Split(split.clone()),
            });
        }

        Ok(())
    }
}
