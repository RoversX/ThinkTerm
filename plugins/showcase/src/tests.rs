//! Every section drawn at the sizes a page comes in -- a wide tab, a narrow
//! split beside a terminal, a phone held upright with a finger -- and
//! checked: nothing outside the view, nothing near the limits a frame has,
//! the rows of its lists answered; and what the page is opened with comes
//! back as it went.

use super::*;
use crate::sections::lists::Tab;
use thinkterm_plugin_sdk::panel::{
    feature, Bounds, Button, CloseButton, Env, Item, MonoMetrics, Player, Pointer, TextMetrics,
};
use thinkterm_plugin_sdk::Page;

fn env(width: f32, height: f32, touch: bool) -> Env {
    let text = |size, line| TextMetrics { size, line };
    Env {
        width,
        height,
        scale: 2.0,
        dark: true,
        // What the browser says: larger than the desktop's, so the
        // tighter fit.
        small: text(12.0, 16.0),
        body: text(14.0, 20.0),
        title: text(16.0, 22.0),
        mono: MonoMetrics {
            size: 13.0,
            line: 18.0,
            advance: 7.8,
        },
        locale: "en-US".into(),
        cwd: Some("/home/user/project".into()),
        remote: None,
        terminal: Some(thinkterm_plugin_sdk::panel::Terminal {
            pane: 12,
            socket: Some("/home/user/.local/share/thinkterm/sock".into()),
        }),
        can_extend: false,
        close: None,
        features: vec![
            feature::FIELDS.into(),
            feature::KEYS.into(),
            feature::WRAP.into(),
        ],
        pointer: if touch {
            Pointer::Touch
        } else {
            Pointer::Mouse
        },
    }
}

/// The three rooms a page is drawn for.
fn sizes() -> [Env; 3] {
    [
        env(1100.0, 640.0, false),
        env(420.0, 600.0, false),
        env(390.0, 800.0, true),
    ]
}

fn showcase() -> Showcase {
    let live: SharedLive = Arc::new((
        Mutex::new(Live {
            metrics: Metrics::new(42),
            wanted: 0,
            paused: false,
        }),
        Condvar::new(),
    ));
    let ticker: SharedTicker = Arc::new((Mutex::new(Ticker { rate: 0 }), Condvar::new()));
    let mut showcase = Showcase::new(None, live, ticker);
    // Not the one in a data directory: the tests' own.
    showcase.board = board::Board::load(None);
    showcase
}

fn page(id: u64, env: Env, args: Value) -> View {
    let mut view = View::new(id, env);
    view.page = Some(Page {
        id: format!("{id:016x}"),
        args,
    });
    view
}

fn click(id: &str) -> Input {
    Input::Click(Click {
        id: id.into(),
        x: 1.0,
        y: 1.0,
        button: Button::Left,
        count: 1,
        mods: Default::default(),
    })
}

/// What a frame holds: how many items, scroll areas' included.
struct Checked {
    items: usize,
    lists: Vec<thinkterm_plugin_sdk::panel::List>,
}

const SLACK: f32 = 0.5;

fn within(b: &Bounds, w: f32, h: f32) -> bool {
    b.left >= -SLACK && b.top >= -SLACK && b.right <= w + SLACK && b.bottom <= h + SLACK
}

fn check_item(item: &Item, w: f32, h: f32, what: &str) {
    // A number that is not finite goes as null, and the client drops the
    // item.
    let sent = serde_json::to_string(item).unwrap();
    assert!(!sent.contains("null"), "{what}: {sent}");
    let b = item.bounds();
    assert!(
        within(&b, w, h),
        "{what}: {item:?} is outside {w} x {h}: {b:?}"
    );
    match item {
        Item::Text(text) => {
            assert!(text.text.chars().count() < 2_000, "{what}: a text too long");
            assert!(text.w >= 0.0 && text.h >= 0.0, "{what}: {item:?}");
        }
        Item::Line(line) => assert!(
            line.points.len() <= 8_192 && line.points.len() % 2 == 0,
            "{what}"
        ),
        Item::Area(area) => assert!(
            area.points.len() <= 8_192 && area.points.len() % 2 == 0,
            "{what}"
        ),
        Item::Rect(rect) => assert!(rect.w >= 0.0 && rect.h >= 0.0, "{what}: {item:?}"),
        _ => {}
    }
}

/// Checks a frame drawn for `env`: everything within the view, a scroll
/// area's items within it, no field or list in one, nothing near the
/// limits.
fn check(frame: &Frame, env: &Env, what: &str) -> Checked {
    let mut count = 0;
    let mut lists = Vec::new();
    let mut fields = std::collections::HashSet::new();
    for item in &frame.items {
        count += 1;
        check_item(item, env.width, env.height, what);
        match item {
            Item::Scroll(scroll) => {
                assert!(scroll.height >= 0.0, "{what}");
                for inside in &scroll.items {
                    count += 1;
                    assert!(
                        !matches!(inside, Item::Scroll(_) | Item::List(_) | Item::Field(_)),
                        "{what}: {inside:?} in a scroll area"
                    );
                    check_item(inside, scroll.w, scroll.height.max(scroll.h), what);
                }
            }
            Item::List(list) => lists.push(list.clone()),
            Item::Field(field) => assert!(
                fields.insert(field.id.clone()),
                "{what}: two fields {}",
                field.id
            ),
            _ => {}
        }
    }
    assert!(count < 3_000, "{what}: {count} items, too near the limit");
    let size = serde_json::to_string(frame).unwrap().len();
    assert!(size < 1 << 20, "{what}: a frame of {size} bytes");
    Checked {
        items: count,
        lists,
    }
}

/// The rows of each list in a frame, as the player would ask for the first
/// of them: answered, and each within its row.
fn check_rows(
    showcase: &mut Showcase,
    view: &View,
    lists: &[thinkterm_plugin_sdk::panel::List],
    what: &str,
) {
    for list in lists {
        let wanted = RowsWanted {
            list: list.id.clone(),
            key: list.key.clone(),
            version: list.version,
            layout: 1,
            from: 0,
            to: 48,
        };
        let rows = showcase.rows(view, &wanted);
        assert_eq!(
            rows.len() as u32,
            list.count.min(48),
            "{what}: rows of {}",
            list.id
        );
        let width = if list.width > 0.0 { list.width } else { list.w };
        for row in &rows {
            assert!(!row.is_empty(), "{what}: an empty row of {}", list.id);
            for item in row {
                assert!(!matches!(
                    item,
                    Item::Scroll(_) | Item::List(_) | Item::Field(_)
                ));
                check_item(
                    item,
                    width,
                    list.row,
                    &format!("{what}, a row of {}", list.id),
                );
            }
        }
    }
}

#[test]
fn every_section_fits_every_room() {
    let mut drawn = 0;
    for env in sizes() {
        for section in Section::ALL {
            let tabs: Vec<Option<Tab>> = match section {
                Section::Lists => Tab::ALL.into_iter().map(Some).collect(),
                _ => vec![None],
            };
            for tab in tabs {
                let mut showcase = showcase();
                let what = format!(
                    "{} {:?} at {} x {}",
                    section.name(),
                    tab,
                    env.width,
                    env.height
                );
                let view = page(1, env.clone(), args_of(section, tab));
                let mut frame = Frame::default();
                showcase.draw(&view, &mut frame);
                let checked = check(&frame, &env, &what);
                assert!(checked.items > 20, "{what}: drew only {}", checked.items);
                // What each section must have room for, at any size.
                let wanted: &[&str] = match section {
                    Section::Forms => &["form:create", "form:reset"],
                    Section::Motion => &["mo:play", "mo:rate:3"],
                    Section::Dashboard => &["dash:pause"],
                    _ => &[],
                };
                let sent = serde_json::to_string(&frame).unwrap();
                for id in wanted {
                    assert!(sent.contains(&format!("\"{id}\"")), "{what}: no {id}");
                }
                check_rows(&mut showcase, &view, &checked.lists, &what);
                drawn += 1;
            }
        }
        // The narrow page's list of sections.
        if env.width < 760.0 {
            let mut showcase = showcase();
            let view = page(1, env.clone(), json!({}));
            showcase.draw(&view, &mut Frame::default());
            showcase.input(&view, click("menu"), &mut Cx::new());
            let mut frame = Frame::default();
            showcase.draw(&view, &mut frame);
            check(&frame, &env, "the menu");
            let names = serde_json::to_string(&frame).unwrap();
            assert!(
                Section::ALL
                    .iter()
                    .all(|s| names.contains(&format!("nav:{}", s.name()))),
                "{names}"
            );
        }
    }
    assert_eq!(drawn, 30);
}

#[test]
fn sections_fit_with_more_on_them_too() {
    // A picked card, labels and a picked event take more room.
    for env in sizes() {
        let mut showcase = showcase();
        let view = page(1, env.clone(), args_of(Section::Board, None));
        showcase.draw(&view, &mut Frame::default());
        showcase.input(&view, click("board:card:5"), &mut Cx::new());
        let mut frame = Frame::default();
        showcase.draw(&view, &mut frame);
        check(&frame, &env, "a picked card");

        let view = page(2, env.clone(), args_of(Section::Forms, None));
        showcase.draw(&view, &mut Frame::default());
        // Each typed over the field as the last label emptied it.
        for (seq, label) in ["bug", "ui", "a-longer-label"].into_iter().enumerate() {
            let typed = thinkterm_plugin_sdk::panel::Typed {
                id: "form:label".into(),
                text: label.into(),
                seq: seq as u32,
            };
            showcase.input(&view, Input::Submit(typed), &mut Cx::new());
        }
        let typed = thinkterm_plugin_sdk::panel::Typed {
            id: "form:assignee".into(),
            text: "a".into(),
            seq: 0,
        };
        showcase.input(&view, Input::Text(typed), &mut Cx::new());
        let mut frame = Frame::default();
        showcase.draw(&view, &mut frame);
        check(&frame, &env, "a form filled in");
        assert_eq!(
            showcase.views[&2].forms.labels,
            ["bug", "ui", "a-longer-label"]
        );

        let view = page(3, env.clone(), args_of(Section::Lists, Some(Tab::Events)));
        showcase.draw(&view, &mut Frame::default());
        showcase.input(&view, click("list:ev:7"), &mut Cx::new());
        let mut frame = Frame::default();
        showcase.draw(&view, &mut frame);
        let checked = check(&frame, &env, "an event picked");
        check_rows(&mut showcase, &view, &checked.lists, "an event picked");

        let view = page(4, env.clone(), args_of(Section::Page, None));
        showcase.draw(&view, &mut Frame::default());
        let ran = sections::page::Ran {
            what: sections::page::Command::Read,
            result: Ok((0..12)
                .map(|n| format!("user@devbox:~/project$ line {n}"))
                .collect()),
        };
        showcase.views.get_mut(&4).unwrap().page.heard(ran);
        let mut frame = Frame::default();
        showcase.draw(&view, &mut frame);
        check(&frame, &env, "the screen read");
    }
}

#[test]
fn the_panel_and_its_extended_view_fit() {
    let mut showcase = showcase();
    let mut panel_env = env(320.0, 720.0, false);
    panel_env.can_extend = true;
    let panel = View::new(1, panel_env.clone());
    let mut frame = Frame::default();
    showcase.draw(&panel, &mut frame);
    check(&frame, &panel_env, "the panel");
    assert!(!frame.extend, "not until asked");
    showcase.input(&panel, click("extend"), &mut Cx::new());
    let mut frame = Frame::default();
    showcase.draw(&panel, &mut frame);
    assert!(frame.extend, "asked");

    let mut wide = env(560.0, 900.0, false);
    wide.close = Some(CloseButton {
        x: 5.0,
        y: 2.0,
        size: 22.0,
    });
    let mut extended = View::new(2, wide.clone());
    extended.extends = Some(1);
    let mut frame = Frame::default();
    showcase.draw(&extended, &mut frame);
    check(&frame, &wide, "the extended view");
    // Nothing of it in the close button's square.
    for item in &frame.items {
        let b = item.bounds();
        assert!(
            b.bottom <= 24.0 + SLACK || b.top >= 24.0 - SLACK || b.left >= 27.0,
            "{item:?}"
        );
    }
    showcase.input(&extended, Input::Close, &mut Cx::new());
    let mut frame = Frame::default();
    showcase.draw(&panel, &mut frame);
    assert!(!frame.extend, "closed, it stops asking");

    // A phone's panel, narrow and pressed with a finger.
    let small = env(300.0, 560.0, true);
    let view = View::new(3, small.clone());
    let mut frame = Frame::default();
    showcase.draw(&view, &mut frame);
    check(&frame, &small, "a small panel");
}

#[test]
fn what_a_page_is_opened_with_comes_back_as_it_went() {
    for section in Section::ALL {
        assert_eq!(parse_args(&args_of(section, None)), (section, None));
    }
    for tab in Tab::ALL {
        let args = args_of(Section::Lists, Some(tab));
        assert_eq!(parse_args(&args), (Section::Lists, Some(tab)));
    }
    assert_eq!(parse_args(&Value::Null).0, Section::Dashboard);
    assert_eq!(
        parse_args(&json!({"section": "nope"})).0,
        Section::Dashboard
    );

    // Opened on a section, the page says so: its tab's name, its icon and
    // what it opens with.
    let mut showcase = showcase();
    let view = page(
        1,
        env(1100.0, 640.0, false),
        json!({"section": "lists", "tab": "table"}),
    );
    let mut frame = Frame::default();
    showcase.draw(&view, &mut frame);
    let change = frame.page_change.clone().expect("the page names itself");
    assert_eq!(change.title.as_deref(), Some("Showcase \u{00b7} Lists"));
    assert_eq!(change.icon.as_deref(), Some("database"));
    assert_eq!(
        change.args,
        Some(json!({"section": "lists", "tab": "table"}))
    );
    let mut again = Frame::default();
    showcase.draw(&view, &mut again);
    assert_eq!(
        again.page_change.map(|c| c.seq),
        Some(change.seq),
        "said once"
    );

    // Moved to another section, it opens on that from then on.
    showcase.input(&view, click("nav:motion"), &mut Cx::new());
    let mut frame = Frame::default();
    showcase.draw(&view, &mut frame);
    let moved = frame.page_change.expect("changed");
    assert!(moved.seq > change.seq);
    assert_eq!(moved.args, Some(json!({"section": "motion"})));
    assert_eq!(moved.icon.as_deref(), Some("activity"));
    // Opened anew with that -- after a restart, on another device -- it
    // is on the same section.
    let reopened = page(2, env(420.0, 600.0, false), moved.args.unwrap());
    showcase.draw(&reopened, &mut Frame::default());
    assert_eq!(showcase.views[&2].section, Section::Motion);

    // The narrow bar's arrows go round.
    let narrow = page(3, env(420.0, 600.0, false), json!({"section": "dashboard"}));
    showcase.draw(&narrow, &mut Frame::default());
    showcase.input(&narrow, click("prev"), &mut Cx::new());
    assert_eq!(showcase.views[&3].section, Section::Page);
    showcase.input(&narrow, click("next"), &mut Cx::new());
    assert_eq!(showcase.views[&3].section, Section::Dashboard);
}

#[test]
fn the_panel_opens_the_page_on_a_section_when_pressed() {
    let mut showcase = showcase();
    let panel = View::new(1, env(320.0, 720.0, false));
    let mut frame = Frame::default();
    showcase.draw(&panel, &mut frame);
    assert!(frame.page.is_none(), "nothing opens by itself");
    showcase.input(&panel, click("open:board"), &mut Cx::new());
    let mut frame = Frame::default();
    showcase.draw(&panel, &mut frame);
    let asked = frame.page.expect("asked");
    assert_eq!(asked.args, json!({"section": "board"}));
    showcase.input(&panel, click("open:"), &mut Cx::new());
    let mut frame = Frame::default();
    showcase.draw(&panel, &mut frame);
    let again = frame.page.expect("asked again");
    assert!(again.seq > asked.seq);
    assert_eq!(again.args, Value::Null, "the tour from its start");
}

#[test]
fn the_threads_run_only_while_something_shows_what_they_move() {
    let mut showcase = showcase();
    let rate = |showcase: &Showcase| lock(&showcase.ticker.0).rate;
    let wanted = |showcase: &Showcase| lock(&showcase.live.0).wanted;
    let motion = page(1, env(1100.0, 640.0, false), args_of(Section::Motion, None));
    showcase.draw(&motion, &mut Frame::default());
    assert_eq!(rate(&showcase), 60, "playing, at the rate picked");
    assert_eq!(wanted(&showcase), 0, "no numbers on show");
    showcase.input(&motion, click("mo:rate:3"), &mut Cx::new());
    showcase.draw(&motion, &mut Frame::default());
    assert_eq!(rate(&showcase), 120);
    showcase.input(&motion, click("mo:play"), &mut Cx::new());
    showcase.draw(&motion, &mut Frame::default());
    assert_eq!(rate(&showcase), 0, "paused");
    showcase.input(&motion, click("mo:play"), &mut Cx::new());
    showcase.draw(&motion, &mut Frame::default());
    assert_eq!(rate(&showcase), 120);
    showcase.input(&motion, click("nav:dashboard"), &mut Cx::new());
    showcase.draw(&motion, &mut Frame::default());
    assert_eq!(rate(&showcase), 0, "gone off show");
    assert_eq!(wanted(&showcase), 1, "the dashboard is on");
    showcase.input(&motion, click("nav:motion"), &mut Cx::new());
    showcase.draw(&motion, &mut Frame::default());
    assert_eq!(rate(&showcase), 120);
    showcase.closed(&motion);
    assert_eq!(
        (rate(&showcase), wanted(&showcase)),
        (0, 0),
        "closed, all stops"
    );
    assert!(showcase.views.is_empty() && showcase.wants.is_empty());

    let panel = View::new(2, env(320.0, 720.0, false));
    showcase.draw(&panel, &mut Frame::default());
    assert_eq!(wanted(&showcase), 1, "the panel's summary moves");
    showcase.closed(&panel);
    assert_eq!(wanted(&showcase), 0);
}

#[test]
fn the_events_filter_and_the_keys_pick() {
    let mut showcase = showcase();
    let view = page(1, env(1100.0, 640.0, false), args_of(Section::Lists, None));
    let mut frame = Frame::default();
    showcase.draw(&view, &mut frame);
    assert!(frame.keys.iter().any(|key| key == "ArrowDown"));
    let key = |name: &str| {
        Input::Key(Key {
            key: name.into(),
            mods: Default::default(),
        })
    };
    showcase.input(&view, key("ArrowDown"), &mut Cx::new());
    assert_eq!(showcase.views[&1].lists.picked, Some(0));
    showcase.input(&view, key("End"), &mut Cx::new());
    assert_eq!(
        showcase.views[&1].lists.picked,
        Some(sections::lists::EVENTS - 1)
    );
    let mut frame = Frame::default();
    showcase.draw(&view, &mut frame);
    let list = check(&frame, &view.env, "picked").lists.remove(0);
    let jump = list.top.expect("kept in view");
    assert_eq!(
        (jump.to, jump.through),
        (
            (sections::lists::EVENTS - 1) as f32,
            Some(sections::lists::EVENTS as f32)
        )
    );

    // Filtered, the list is other rows, every one of them an error.
    let typed = thinkterm_plugin_sdk::panel::Typed {
        id: "list:filter".into(),
        text: "error".into(),
        seq: 0,
    };
    showcase.input(&view, Input::Submit(typed), &mut Cx::new());
    let mut frame = Frame::default();
    showcase.draw(&view, &mut frame);
    let filtered = check(&frame, &view.env, "filtered").lists.remove(0);
    assert_ne!(filtered.key, list.key);
    assert!(
        filtered.count > 100 && filtered.count < sections::lists::EVENTS / 10,
        "{}",
        filtered.count
    );
    let picked = showcase.views[&1].lists.picked.unwrap();
    assert!(!sections::lists::event(picked).message.is_empty());
    let wanted = RowsWanted {
        list: "events".into(),
        key: filtered.key.clone(),
        version: 0,
        layout: 0,
        from: 0,
        to: 30,
    };
    let rows = showcase.rows(&view, &wanted);
    assert_eq!(rows.len(), 30);
    let drawn = serde_json::to_string(&rows).unwrap();
    assert_eq!(drawn.matches("\"ERROR\"").count(), 30, "{drawn}");
    // Rows asked for under the old key are not this list's.
    let stale = RowsWanted {
        key: list.key,
        ..wanted
    };
    assert!(showcase.rows(&view, &stale).is_empty());
}

#[test]
fn a_card_moves_between_lanes_and_every_view_is_told() {
    let mut showcase = showcase();
    let view = page(1, env(1100.0, 640.0, false), args_of(Section::Board, None));
    showcase.draw(&view, &mut Frame::default());
    let lane = |showcase: &Showcase, id: u32| {
        showcase
            .board
            .cards
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.lane)
    };
    assert_eq!(lane(&showcase, 1), Some(0));
    showcase.input(&view, click("board:card:1"), &mut Cx::new());
    let mut cx = Cx::new();
    showcase.input(&view, click("board:right"), &mut cx);
    assert_eq!(lane(&showcase, 1), Some(1));
    assert!(cx.finish().redraw, "the other views show it");
    let key = |name: &str| {
        Input::Key(Key {
            key: name.into(),
            mods: Default::default(),
        })
    };
    showcase.input(&view, key("ArrowRight"), &mut Cx::new());
    showcase.input(&view, key("ArrowRight"), &mut Cx::new());
    showcase.input(&view, key("ArrowRight"), &mut Cx::new());
    assert_eq!(lane(&showcase, 1), Some(3), "no further than the last lane");
    showcase.input(&view, click("board:add:0"), &mut Cx::new());
    assert_eq!(showcase.board.cards.len(), 10);
    showcase.input(&view, click("board:remove"), &mut Cx::new());
    assert_eq!(showcase.board.cards.len(), 9);
}

#[test]
fn a_press_where_a_nav_row_is_drawn_moves_to_its_section() {
    let wide = env(1100.0, 640.0, false);
    let mut showcase = showcase();
    let view = page(1, wide.clone(), json!({}));
    let mut frame = Frame::default();
    showcase.draw(&view, &mut frame);
    let hit = frame
        .items
        .iter()
        .find_map(|item| match item {
            Item::Hit(hit) if hit.id == "nav:forms" => Some(hit.clone()),
            _ => None,
        })
        .expect("a row for Forms");
    let mut player = Player::new(wide);
    player.frame(frame);
    let input = player
        .click(
            hit.x + 4.0,
            hit.y + 4.0,
            Button::Left,
            1,
            Default::default(),
        )
        .expect("the press reaches the plugin");
    showcase.input(&view, input, &mut Cx::new());
    assert_eq!(showcase.views[&1].section, Section::Forms);
}

#[test]
fn a_press_on_a_button_with_no_terminal_to_run_in_starts_nothing() {
    let mut showcase = showcase();
    // Opened from no terminal, as a page restored is.
    let mut nowhere = env(1100.0, 640.0, false);
    nowhere.terminal = None;
    let view = page(1, nowhere, args_of(Section::Page, None));
    showcase.draw(&view, &mut Frame::default());
    showcase.input(&view, click("page:greet"), &mut Cx::new());
    let mut frame = Frame::default();
    showcase.draw(&view, &mut frame);
    let drawn = serde_json::to_string(&frame).unwrap();
    assert!(drawn.contains("Type a greeting"));
    assert!(!drawn.contains("Typing"), "nothing is on its way");
}

#[test]
fn a_press_means_what_was_pressed_however_late_it_lands() {
    let mut showcase = showcase();
    let wide = env(1100.0, 640.0, false);
    let typed = |id: &str, text: &str, seq: u32| thinkterm_plugin_sdk::panel::Typed {
        id: id.into(),
        text: text.into(),
        seq,
    };
    let view = page(1, wide.clone(), args_of(Section::Forms, None));
    showcase.draw(&view, &mut Frame::default());
    for (seq, label) in ["bug", "ui", "docs"].into_iter().enumerate() {
        let label = typed("form:label", label, seq as u32);
        showcase.input(&view, Input::Submit(label), &mut Cx::new());
    }
    // Pressed twice before the chips were drawn again: the label pressed
    // goes, and not the one that moved into its place.
    showcase.input(&view, click("form:unlabel:bug"), &mut Cx::new());
    showcase.input(&view, click("form:unlabel:bug"), &mut Cx::new());
    assert_eq!(showcase.views[&1].forms.labels, ["ui", "docs"]);
    // The person pressed, whoever the typing since puts first.
    let name = typed("form:assignee", "a", 0);
    showcase.input(&view, Input::Text(name), &mut Cx::new());
    let mut frame = Frame::default();
    showcase.draw(&view, &mut frame);
    assert!(serde_json::to_string(&frame).unwrap().contains("form:suggest:sam"));
    let name = typed("form:assignee", "al", 1);
    showcase.input(&view, Input::Text(name), &mut Cx::new());
    showcase.input(&view, click("form:suggest:sam"), &mut Cx::new());
    assert_eq!(showcase.views[&1].forms.assignee.text(), "Sam Rivera");

    // The disabled button is not the plain one.
    let view = page(2, wide, args_of(Section::Components, None));
    showcase.draw(&view, &mut Frame::default());
    showcase.input(&view, click("comp:button:3"), &mut Cx::new());
    assert_eq!(showcase.views[&2].components.pressed, None);
    showcase.input(&view, click("comp:button:1"), &mut Cx::new());
    assert_eq!(showcase.views[&2].components.pressed, Some(("Accent", 1)));
}

#[test]
fn the_narrow_pages_menu_does_not_outlast_the_page_growing_wide() {
    let mut showcase = showcase();
    let view = page(1, env(420.0, 600.0, false), json!({}));
    showcase.draw(&view, &mut Frame::default());
    showcase.input(&view, click("menu"), &mut Cx::new());
    showcase.draw(&view, &mut Frame::default());
    assert!(showcase.views[&1].menu);
    assert_eq!(showcase.wants.get(&1), None, "the list covers the numbers");
    // Wide, the rail is there and the dashboard under no list: its
    // numbers move again.
    let view = page(1, env(1100.0, 640.0, false), json!({}));
    showcase.draw(&view, &mut Frame::default());
    assert!(!showcase.views[&1].menu);
    assert_eq!(showcase.wants.get(&1), Some(&(true, 0)));
}

#[test]
fn the_greeting_is_typed_only_at_a_terminal() {
    let mut showcase = showcase();
    let err = showcase
        .call(json!({"op": "type-greeting", "at": {}}), &mut Cx::new())
        .unwrap_err();
    assert!(format!("{err:#}").contains("at a terminal"));
    assert_eq!(
        showcase
            .call(json!({"op": "sections"}), &mut Cx::new())
            .unwrap()[7],
        json!("page")
    );
    assert!(showcase
        .call(json!({"op": "nope"}), &mut Cx::new())
        .is_err());
}

#[test]
fn the_heat_map_counts_days_and_the_keys_walk_it() {
    let mut state = sections::activity::State::default();
    let look_env = env(1100.0, 640.0, false);
    let look = Look::of(&look_env);
    let mut frame = Frame::default();
    let today = 20_770;
    sections::activity::draw_on(
        &mut frame,
        &look,
        ui::Rgn::new(213.0, 0.0, 887.0, 640.0),
        &mut state,
        today,
    );
    check(&frame, &look_env, "the heat map");
    sections::activity::key(&mut state, "ArrowUp");
    assert_eq!(state.picked, Some(today), "nothing picked: today first");
    sections::activity::key(&mut state, "ArrowDown");
    assert_eq!(state.picked, Some(today), "no further than today");
    sections::activity::key(&mut state, "ArrowLeft");
    assert_eq!(state.picked, Some(today - 7));
}
