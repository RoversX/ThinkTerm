use super::*;

pub(super) fn minimum_size(layout: &Layout) -> (usize, usize) {
    match layout {
        Layout::Pane(_) | Layout::Stack { .. } => (2, 2),
        Layout::Split {
            direction,
            first,
            second,
            ..
        } => {
            let (a, b) = (minimum_size(first), minimum_size(second));
            match direction {
                Direction::Horizontal => (a.0 + b.0 + 1, a.1.max(b.1)),
                Direction::Vertical => (a.0.max(b.0), a.1 + b.1 + 1),
            }
        }
    }
}

pub(super) fn layout_tree(
    layout: &Layout,
    size: TerminalSize,
    entries: &HashMap<u32, PaneEntry>,
) -> PaneNode {
    match layout {
        Layout::Pane(id) => {
            let mut entry = entries[id].clone();
            entry.size = size;
            PaneNode::Leaf(entry)
        }
        Layout::Stack { panes, active } => PaneNode::Stack(thinkterm_proto::PaneStackEntry {
            active: *active,
            panes: panes
                .iter()
                .map(|id| {
                    let mut entry = entries[id].clone();
                    entry.size = size;
                    entry
                })
                .collect(),
            pane_stack_id: None,
        }),
        Layout::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let (min_a, min_b) = (minimum_size(first), minimum_size(second));
            let (mut a, mut b) = (size, size);
            let horizontal = matches!(direction, Direction::Horizontal);
            let extent = if horizontal { size.cols } else { size.rows };
            let (min_a, min_b) = if horizontal {
                (min_a.0, min_b.0)
            } else {
                (min_a.1, min_b.1)
            };
            let share =
                (((extent - 1) as f32 * ratio).round() as usize).clamp(min_a, extent - 1 - min_b);
            if horizontal {
                a.cols = share;
                b.cols = extent - 1 - share;
            } else {
                a.rows = share;
                b.rows = extent - 1 - share;
            }
            for child in [&mut a, &mut b] {
                child.pixel_width = size.pixel_width / size.cols * child.cols;
                child.pixel_height = size.pixel_height / size.rows * child.rows;
            }
            PaneNode::Split {
                left: Box::new(layout_tree(first, a, entries)),
                right: Box::new(layout_tree(second, b, entries)),
                node: SplitDirectionAndSize {
                    direction: if horizontal {
                        SplitDirection::Horizontal
                    } else {
                        SplitDirection::Vertical
                    },
                    first: a,
                    second: b,
                },
            }
        }
    }
}
