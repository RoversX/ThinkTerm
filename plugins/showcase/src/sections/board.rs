//! Board: a kanban board of four lanes. A card is picked with a click, and
//! moved between lanes with its arrows or with Left and Right while the
//! page has the keyboard; Up and Down pick the card above or below. Every
//! view of the plugin shows the same board, which is kept in the plugin's
//! data directory so it outlives the program.

use crate::chrome;
use crate::ui::{self, Kind, Look, Rgn, GAP, PAD, RADIUS};
use crate::Section;
use serde_json::{json, Value};
use std::path::PathBuf;
use thinkterm_plugin_sdk::panel::{Align, Frame, Hit, Item, Rect, Scroll, Size, Text, Token};

pub const LANES: [&str; 4] = ["Backlog", "In progress", "Review", "Done"];
/// The most cards a board holds.
pub const CARDS: usize = 48;
const TAGS: [(&str, Token, Token); 4] = [
    ("bug", Token::NegativeBg, Token::Negative),
    ("feature", Token::PositiveBg, Token::Positive),
    ("docs", Token::BgSelected, Token::TextMuted),
    ("perf", Token::BgHover, Token::Warning),
];
const PEOPLE: [&str; 5] = ["AK", "SR", "KN", "RL", "JT"];

#[derive(Debug, Clone, PartialEq)]
pub struct Card {
    pub id: u32,
    pub title: String,
    pub tag: usize,
    pub who: usize,
    pub lane: usize,
}

#[derive(Debug)]
pub struct Board {
    /// In the order each lane shows them.
    pub cards: Vec<Card>,
    next: u32,
    /// Where it is kept; none in the tests.
    file: Option<PathBuf>,
}

/// The card a view has picked.
#[derive(Debug, Default)]
pub struct Pick {
    pub card: Option<u32>,
}

impl Board {
    /// The board kept in `dir`, or a new one.
    pub fn load(dir: Option<PathBuf>) -> Self {
        let file = dir.map(|dir| dir.join("board.json"));
        let kept = file
            .as_ref()
            .and_then(|file| std::fs::read(file).ok())
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|value| Self::read(&value));
        let cards = kept.unwrap_or_else(Self::sample);
        let next = cards.iter().map(|card| card.id).max().unwrap_or(0) + 1;
        Self { cards, next, file }
    }

    fn sample() -> Vec<Card> {
        [
            ("Rate-limit the upload endpoint", 3, 0, 0),
            ("Dark theme for the settings page", 1, 1, 0),
            ("Document the page API", 2, 2, 0),
            ("Retry failed webhooks with backoff", 1, 3, 1),
            ("Crash when a field holds 16k characters", 0, 4, 1),
            ("Faster startup on cold cache", 3, 0, 2),
            ("Export the board as CSV", 1, 1, 2),
            ("Fix typo in the install guide", 2, 2, 3),
            ("Search across all hosts", 1, 3, 3),
        ]
        .into_iter()
        .enumerate()
        .map(|(n, (title, tag, who, lane))| Card {
            id: n as u32 + 1,
            title: title.to_string(),
            tag,
            who,
            lane,
        })
        .collect()
    }

    fn read(value: &Value) -> Option<Vec<Card>> {
        let cards = value["cards"].as_array()?;
        let read = cards
            .iter()
            .take(CARDS)
            .filter_map(|card| {
                Some(Card {
                    id: card["id"].as_u64()? as u32,
                    title: ui::cut(card["title"].as_str()?, 80),
                    tag: (card["tag"].as_u64()? as usize).min(TAGS.len() - 1),
                    who: (card["who"].as_u64()? as usize).min(PEOPLE.len() - 1),
                    lane: (card["lane"].as_u64()? as usize).min(LANES.len() - 1),
                })
            })
            .collect();
        Some(read)
    }

    fn save(&self) {
        let Some(file) = &self.file else {
            return;
        };
        let cards: Vec<Value> = self
            .cards
            .iter()
            .map(|card| json!({"id": card.id, "title": card.title, "tag": card.tag, "who": card.who, "lane": card.lane}))
            .collect();
        if let Err(err) = std::fs::write(file, json!({"cards": cards}).to_string()) {
            eprintln!("showcase: cannot keep the board: {err}");
        }
    }

    fn find(&self, id: u32) -> Option<usize> {
        self.cards.iter().position(|card| card.id == id)
    }

    /// Moves card `id` a lane `by`, to the end of that lane; true when it
    /// moved.
    fn shift(&mut self, id: u32, by: isize) -> bool {
        let Some(at) = self.find(id) else {
            return false;
        };
        let lane = self.cards[at].lane as isize + by;
        if !(0..LANES.len() as isize).contains(&lane) {
            return false;
        }
        let mut card = self.cards.remove(at);
        card.lane = lane as usize;
        self.cards.push(card);
        self.save();
        true
    }

    fn in_lane(&self, lane: usize) -> impl Iterator<Item = &Card> {
        self.cards.iter().filter(move |card| card.lane == lane)
    }
}

/// A click on `id`, past "board:"; true when the board changed, which
/// every view is to show.
pub fn click(board: &mut Board, pick: &mut Pick, id: &str) -> bool {
    let picked = pick.card.filter(|card| board.find(*card).is_some());
    if let Some(card) = id.strip_prefix("card:").and_then(|n| n.parse().ok()) {
        pick.card = match picked == Some(card) {
            true => None,
            false => Some(card),
        };
        return false;
    }
    if let Some(lane) = id
        .strip_prefix("add:")
        .and_then(|n| n.parse::<usize>().ok())
    {
        if board.cards.len() >= CARDS || lane >= LANES.len() {
            return false;
        }
        let id = board.next;
        board.next += 1;
        board.cards.push(Card {
            id,
            title: format!("New card {id}"),
            tag: id as usize % TAGS.len(),
            who: id as usize % PEOPLE.len(),
            lane,
        });
        pick.card = Some(id);
        board.save();
        return true;
    }
    let Some(card) = picked else {
        return false;
    };
    match id {
        "left" => board.shift(card, -1),
        "right" => board.shift(card, 1),
        "remove" => {
            if let Some(at) = board.find(card) {
                board.cards.remove(at);
                pick.card = None;
                board.save();
            }
            true
        }
        _ => false,
    }
}

/// A key the page takes: Left and Right move the card picked, Up and Down
/// pick the one above or below it. True when the board changed.
pub fn key(board: &mut Board, pick: &mut Pick, key: &str) -> bool {
    let Some(at) = pick.card.and_then(|card| board.find(card)) else {
        // Nothing picked: the first card is.
        pick.card = board.cards.first().map(|card| card.id);
        return false;
    };
    let card = board.cards[at].id;
    match key {
        "ArrowLeft" => board.shift(card, -1),
        "ArrowRight" => board.shift(card, 1),
        "ArrowUp" | "ArrowDown" => {
            let lane: Vec<u32> = board
                .in_lane(board.cards[at].lane)
                .map(|card| card.id)
                .collect();
            let here = lane.iter().position(|id| *id == card).unwrap_or(0);
            let there = match key {
                "ArrowUp" => here.saturating_sub(1),
                _ => (here + 1).min(lane.len() - 1),
            };
            pick.card = Some(lane[there]);
            false
        }
        _ => false,
    }
}

pub fn draw(frame: &mut Frame, look: &Look, r: Rgn, board: &Board, pick: &mut Pick) {
    let items = &mut frame.items;
    let top = chrome::header(items, look, r, Section::Board, 0.0);
    let w = r.w - PAD * 2.0;
    let view_h = (r.bottom() - top).max(0.0);
    let side_by_side = w >= 640.0;
    if pick.card.is_some_and(|card| board.find(card).is_none()) {
        pick.card = None;
    }
    let mut inside = Vec::new();
    let height = if side_by_side {
        let lane_w = (w - GAP * 3.0) / 4.0;
        let mut tallest: f32 = 0.0;
        let mut lanes = Vec::new();
        for lane in 0..LANES.len() {
            let mut cards = Vec::new();
            let x = PAD + lane as f32 * (lane_w + GAP);
            let h = lane_cards(
                &mut cards,
                look,
                Rgn::new(x, 0.0, lane_w, 0.0),
                board,
                lane,
                pick,
            );
            tallest = tallest.max(h);
            lanes.push((x, cards));
        }
        // Every lane as tall as the tallest, or the page.
        let lane_h = tallest.max(view_h - PAD);
        for (x, mut cards) in lanes {
            inside.push(
                Rect::new(x, 0.0, lane_w, lane_h)
                    .fill(Token::BgRaised)
                    .radius(RADIUS + 2.0)
                    .into(),
            );
            inside.append(&mut cards);
        }
        lane_h + PAD
    } else {
        let mut y = 0.0;
        for lane in 0..LANES.len() {
            let mut cards = Vec::new();
            let h = lane_cards(
                &mut cards,
                look,
                Rgn::new(PAD, y, w, 0.0),
                board,
                lane,
                pick,
            );
            inside.push(
                Rect::new(PAD, y, w, h)
                    .fill(Token::BgRaised)
                    .radius(RADIUS + 2.0)
                    .into(),
            );
            inside.append(&mut cards);
            y += h + GAP;
        }
        y + PAD
    };
    items.push(Scroll::new("board", r.x, top, r.w, view_h, height, inside).into());
}

/// A lane: its name, how many cards it has, a button that adds one, and
/// its cards. Says how tall it came out.
fn lane_cards(
    items: &mut Vec<Item>,
    look: &Look,
    r: Rgn,
    board: &Board,
    lane: usize,
    pick: &Pick,
) -> f32 {
    let env = look.env;
    let head = look.control();
    let x = r.x + GAP;
    let w = r.w - GAP * 2.0;
    let mut y = r.y + GAP;
    let count = board.in_lane(lane).count();
    let name_w = look.text_w(LANES[lane], Size::Body) + 8.0;
    items.push(
        Text::new(x + 4.0, y, name_w.min(w - head - 36.0), head, LANES[lane])
            .bold()
            .into(),
    );
    let chip_h = env.small.line + 2.0;
    ui::chip(
        items,
        look,
        x + 4.0 + name_w.min(w - head - 36.0) + 4.0,
        y + (head - chip_h) / 2.0,
        &count.to_string(),
        Token::BgHover,
        Token::TextMuted,
        chip_h,
    );
    let full = board.cards.len() >= CARDS;
    let add = Rect::new(x + w - head, y, head, head).radius(RADIUS);
    items.push(add.into());
    if !full {
        items.push(
            Hit::new(format!("board:add:{lane}"), x + w - head, y, head, head)
                .hover(Token::BgHover)
                .radius(RADIUS)
                .into(),
        );
    }
    let color = if full {
        Token::TextFaint
    } else {
        Token::TextMuted
    };
    items.push(
        Text::new(x + w - head, y, head, head, "+")
            .size(Size::Title)
            .color(color)
            .align(Align::Center)
            .into(),
    );
    y += head + GAP;
    for card in board.in_lane(lane) {
        y += card_item(
            items,
            look,
            Rgn::new(x, y, w, 0.0),
            card,
            pick.card == Some(card.id),
        ) + GAP;
    }
    if count == 0 {
        let empty = look.row();
        items.push(
            Rect::new(x, y, w, empty)
                .border(Token::Border)
                .radius(RADIUS)
                .into(),
        );
        items.push(
            Text::new(x, y, w, empty, "No cards")
                .size(Size::Small)
                .color(Token::TextFaint)
                .align(Align::Center)
                .into(),
        );
        y += empty + GAP;
    }
    y - r.y
}

/// A card: its title, tag, who has it and its number; picked, its moves.
fn card_item(items: &mut Vec<Item>, look: &Look, r: Rgn, card: &Card, picked: bool) -> f32 {
    let env = look.env;
    let inner_w = r.w - 24.0;
    let lines = look.lines_for(&card.title, inner_w, Size::Body).min(3);
    let chip_h = env.small.line + 4.0;
    let mut h = 10.0 + lines as f32 * env.body.line + GAP + chip_h + 10.0;
    let moves = look.control();
    if picked {
        h += moves + GAP;
    }
    items.push(
        Rect::new(r.x, r.y, r.w, h)
            .fill(Token::Bg)
            .border(if picked { Token::Accent } else { Token::Border })
            .radius(RADIUS)
            .into(),
    );
    items.push(
        Hit::new(format!("board:card:{}", card.id), r.x, r.y, r.w, h)
            .hover(Token::BgHover)
            .radius(RADIUS)
            .into(),
    );
    let mut title = Text::new(
        r.x + 12.0,
        r.y + 10.0,
        inner_w,
        lines as f32 * env.body.line,
        card.title.clone(),
    );
    if look.wraps() {
        title = title.wrap();
    }
    items.push(title.into());
    let my = r.y + 10.0 + lines as f32 * env.body.line + GAP;
    let (tag, ground, color) = TAGS[card.tag];
    ui::chip(items, look, r.x + 12.0, my, tag, ground, color, chip_h);
    let side = chip_h + 2.0;
    let ground = if card.who == 0 {
        Token::Accent
    } else {
        Token::BgSelected
    };
    ui::avatar(
        items,
        r.right() - 12.0 - side,
        my - 1.0,
        side,
        PEOPLE[card.who],
        ground,
    );
    items.push(
        Text::new(
            r.right() - 12.0 - side - 52.0,
            my,
            44.0,
            chip_h,
            format!("#{}", card.id),
        )
        .size(Size::Small)
        .color(Token::TextFaint)
        .align(Align::Right)
        .into(),
    );
    if picked {
        let y = my + chip_h + GAP + 2.0;
        let arrow = moves.max(36.0);
        let left = if card.lane > 0 {
            Kind::Plain
        } else {
            Kind::Disabled
        };
        let right = if card.lane + 1 < LANES.len() {
            Kind::Plain
        } else {
            Kind::Disabled
        };
        ui::button(
            items,
            look,
            "board:left",
            r.x + 12.0,
            y,
            arrow,
            "\u{2190}",
            left,
        );
        ui::button(
            items,
            look,
            "board:right",
            r.x + 12.0 + arrow + 6.0,
            y,
            arrow,
            "\u{2192}",
            right,
        );
        let rx = r.x + 12.0 + arrow * 2.0 + 12.0;
        let rw = (r.right() - 12.0 - rx).max(0.0);
        if rw >= 48.0 {
            ui::button(
                items,
                look,
                "board:remove",
                rx,
                y,
                rw,
                "Remove",
                Kind::Danger,
            );
        }
    }
    h
}
