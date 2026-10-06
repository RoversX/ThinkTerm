use super::*;

fn plan() -> ImportPlan {
    ImportPlan {
        mode: ImportMode::Layout,
        active: Selection::default(),
        projects: vec![Project {
            name: "example".into(),
            directory: "/home/user/example".into(),
            threads: vec![Thread {
                name: "development".into(),
                active_tab: 0,
                tabs: vec![Tab {
                    name: None,
                    layout: Layout::Stack {
                        panes: vec![1, 2],
                        active: 1,
                    },
                    focused: Some(2),
                    zoomed: false,
                    panes: [1, 2]
                        .into_iter()
                        .map(|id| {
                            (
                                id,
                                Pane {
                                    cwd: "/home/user/example".into(),
                                    title: None,
                                },
                            )
                        })
                        .collect(),
                }],
            }],
        }],
    }
}

#[test]
fn validates_visible_focus_and_unique_panes_across_threads() {
    let mut plan = plan();
    plan.validate().unwrap();
    assert_eq!(plan.projects[0].threads[0].tabs[0].layout.first_pane(), 2);
    plan.projects[0].threads[0].tabs[0].focused = Some(1);
    assert!(plan.validate().is_err());
    plan.projects[0].threads[0].tabs[0].focused = None;
    let duplicate = plan.projects[0].threads[0].clone();
    plan.projects[0].threads.push(duplicate);
    assert!(plan.validate().is_err());
}

#[test]
fn rejects_invalid_layouts_and_selections() {
    for layout in [
        Layout::Pane(9),
        Layout::Stack {
            panes: vec![],
            active: 0,
        },
        Layout::Stack {
            panes: vec![1, 2],
            active: 2,
        },
        Layout::Stack {
            panes: vec![1, 1],
            active: 0,
        },
        Layout::Split {
            direction: Direction::Vertical,
            ratio: f32::NAN,
            first: Box::new(Layout::Pane(1)),
            second: Box::new(Layout::Pane(2)),
        },
    ] {
        let mut plan = plan();
        plan.projects[0].threads[0].tabs[0].layout = layout;
        assert!(plan.validate().is_err());
    }
    let mut plan = plan();
    plan.active.thread = 1;
    assert!(plan.validate().is_err());
    plan.active.thread = 0;
    plan.projects[0].threads[0].active_tab = 1;
    assert!(plan.validate().is_err());
}

#[cfg(unix)]
mod native {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    struct Source;
    impl ImportSource for Source {
        fn info(&self) -> SourceInfo {
            SourceInfo {
                id: "example",
                name: "Example",
                icon: "terminal",
            }
        }
        fn discover(&self, _: &ImportContext) -> anyhow::Result<Vec<Session>> {
            Ok(vec![])
        }
        fn preview(&self, _: &ImportContext, _: &str) -> anyhow::Result<Preview> {
            anyhow::bail!("empty")
        }
        fn prepare(&self, _: &ImportContext, _: &ImportRequest) -> anyhow::Result<PreparedImport> {
            anyhow::bail!("empty")
        }
    }
    static SOURCE: Source = Source;

    #[test]
    fn registry_resolves_sources_and_rejects_duplicates() {
        let mut registry = Registry::new();
        registry.register(&SOURCE).unwrap();
        assert_eq!(registry.get("example").unwrap().info().name, "Example");
        assert!(registry.get("missing").is_none());
        assert!(registry.register(&SOURCE).is_err());
    }

    struct Transfer(Arc<AtomicBool>);
    impl Handoff for Transfer {
        fn commit(&mut self) -> anyhow::Result<()> {
            panic!("invalid resources must never commit")
        }
        fn finish(self: Box<Self>) {
            panic!("invalid resources must never finish")
        }
    }
    impl Drop for Transfer {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn incomplete_live_import_releases_transfer_without_commit() {
        let released = Arc::new(AtomicBool::new(false));
        let mut plan = plan();
        plan.mode = ImportMode::Live;
        let prepared = PreparedImport {
            plan,
            terminals: vec![],
            handoff: Some(Box::new(Transfer(released.clone()))),
        };
        assert!(prepared.validate().is_err());
        drop(prepared);
        assert!(released.load(Ordering::SeqCst));
    }
}
