//! Forms: a new issue, typed in every kind of field -- one line with an
//! icon, a limit and a clear button; lines; a secret -- with labels added
//! on Return, people suggested as a name is typed, the keyboard moved on
//! from field to field, and a preview of what was typed beside the form.

use crate::chrome;
use crate::ui::{self, Kind, Look, Rgn, GAP, PAD, RADIUS};
use crate::Section;
use thinkterm_plugin_sdk::panel::{
    feature, Align, FieldText, Focus, Frame, Hit, Item, Rect, Size, Text, Token, Typed,
};
use thinkterm_plugin_sdk::View;

const TITLE: &str = "form:title";
const LABEL: &str = "form:label";
const ASSIGNEE: &str = "form:assignee";
const BODY: &str = "form:body";
const TOKEN: &str = "form:token";
const TITLE_MAX: u32 = 80;
/// The most labels an issue takes.
const LABELS: usize = 6;
const PEOPLE: [(&str, &str); 6] = [
    ("alex", "Alex Kim"),
    ("sam", "Sam Rivera"),
    ("kai", "Kai Novak"),
    ("robin", "Robin Lee"),
    ("jordan", "Jordan Tan"),
    ("taylor", "Taylor Quinn"),
];

/// An issue made with the form.
#[derive(Debug, Clone, PartialEq)]
pub struct Issue {
    pub number: u32,
    pub title: String,
}

#[derive(Debug, Default)]
pub struct State {
    pub title: FieldText,
    pub label: FieldText,
    pub assignee: FieldText,
    pub body: FieldText,
    pub token: FieldText,
    pub labels: Vec<String>,
    /// How long the token was, once submitted: the token itself is never
    /// kept.
    pub token_len: Option<usize>,
    /// The field the keyboard is to move to, once for each seq.
    focus: Option<(&'static str, u32)>,
    pub made: Option<Issue>,
    made_count: u32,
    /// Why the last press of Create made nothing.
    refused: Option<&'static str>,
}

impl State {
    fn move_to(&mut self, field: &'static str) {
        let seq = self.focus.map_or(0, |(_, seq)| seq) + 1;
        self.focus = Some((field, seq));
    }

    /// The people whose names start with what is typed, or have it in them.
    fn suggestions(&self) -> Vec<(&'static str, &'static str)> {
        let typed = self.assignee.text().trim().to_lowercase();
        if typed.is_empty() {
            return Vec::new();
        }
        PEOPLE
            .iter()
            .copied()
            .filter(|(handle, name)| {
                handle.contains(&typed) || name.to_lowercase().contains(&typed)
            })
            .take(4)
            .collect()
    }

    fn create(&mut self) {
        let title = self.title.text().trim();
        if title.is_empty() {
            self.refused = Some("An issue needs a title");
            self.move_to(TITLE);
            return;
        }
        self.made_count += 1;
        self.made = Some(Issue {
            number: 100 + self.made_count,
            title: title.to_string(),
        });
        self.refused = None;
    }

    fn reset(&mut self) {
        for field in [
            &mut self.title,
            &mut self.label,
            &mut self.assignee,
            &mut self.body,
            &mut self.token,
        ] {
            field.clear();
        }
        self.labels.clear();
        self.token_len = None;
        self.refused = None;
        self.made = None;
        self.move_to(TITLE);
    }
}

/// What a field holds, as it changes, or when Return is pressed in it
/// (`submit`): Command-Return in the description.
pub fn typed(state: &mut State, typed: &Typed, submit: bool) {
    match typed.id.as_str() {
        TITLE => {
            if state.title.heard(typed) {
                state.refused = None;
            }
            if submit {
                state.move_to(LABEL);
            }
        }
        LABEL => {
            state.label.heard(typed);
            let label = state.label.text().trim().to_lowercase();
            if submit && !label.is_empty() {
                if !state.labels.contains(&label) && state.labels.len() < LABELS {
                    state.labels.push(label);
                }
                state.label.clear();
            } else if submit {
                state.move_to(ASSIGNEE);
            }
        }
        ASSIGNEE => {
            state.assignee.heard(typed);
            if submit {
                if let Some((_, name)) = state.suggestions().first() {
                    state.assignee.set(*name);
                }
                state.move_to(BODY);
            }
        }
        BODY => {
            state.body.heard(typed);
            if submit {
                state.create();
            }
        }
        // A secret field says what it holds only when submitted: its length
        // is kept, and the field emptied.
        TOKEN if submit => {
            state.token_len = Some(typed.text.chars().count());
            state.token.clear();
        }
        _ => {}
    }
}

/// A click on `id`, past "form:". A person and a label are named by what
/// they are, not where they were drawn: a press that was on its way while
/// the chips moved still means the one pressed.
pub fn click(state: &mut State, id: &str) {
    if let Some(handle) = id.strip_prefix("suggest:") {
        if let Some((_, name)) = PEOPLE.iter().find(|(known, _)| *known == handle) {
            state.assignee.set(*name);
            state.move_to(BODY);
        }
    } else if let Some(label) = id.strip_prefix("unlabel:") {
        state.labels.retain(|kept| kept != label);
    } else {
        match id {
            "create" => state.create(),
            "reset" => state.reset(),
            _ => {}
        }
    }
}

pub fn draw(frame: &mut Frame, look: &Look, r: Rgn, state: &mut State, view: &View) {
    let items = &mut frame.items;
    let top = chrome::header(items, look, r, Section::Forms, 0.0);
    let w = r.w - PAD * 2.0;
    if !look.env.has(feature::FIELDS) {
        let said = "This ThinkTerm draws no fields, so there is no form to show.";
        ui::paragraph(
            items,
            look,
            r.x + PAD,
            top,
            w,
            said,
            Size::Body,
            Token::TextMuted,
            2,
        );
        return;
    }
    let side = w >= 700.0;
    let form_w = if side { (w * 0.56).min(560.0) } else { w };
    let bottom = form(
        items,
        look,
        Rgn::new(r.x + PAD, top, form_w, r.bottom() - top - PAD),
        state,
        view,
    );
    if side {
        let px = r.x + PAD + form_w + PAD * 1.5;
        let pr = Rgn::new(px, top, r.right() - PAD - px, r.bottom() - top - PAD);
        preview(items, look, pr, state, false);
    } else {
        let room = r.bottom() - PAD - bottom - PAD;
        if room >= look.env.body.line * 2.0 + look.env.small.line + 32.0 {
            let pr = Rgn::new(r.x + PAD, bottom + PAD, w, room);
            preview(items, look, pr, state, true);
        }
    }
}

/// The form's fields and buttons, from the top of `r` down, as many as
/// fit; says where they end.
fn form(items: &mut Vec<Item>, look: &Look, r: Rgn, state: &State, view: &View) -> f32 {
    let env = look.env;
    let h = look.control();
    let mut y = r.y;
    let focus = |id: &str| {
        state
            .focus
            .filter(|(field, _)| *field == id)
            .map(|(_, seq)| seq)
    };
    let chip_h = if look.touch {
        32.0
    } else {
        env.small.line + 8.0
    };
    // The description takes what room the rest leaves, two lines to four,
    // so the buttons fit below it on a short page.
    let caption = env.small.line + 4.0;
    let rest = caption * 5.0
        + h * 5.0
        + (GAP + 4.0) * 5.0
        + (GAP + chip_h + GAP + 4.0 - (GAP + 4.0)) * 2.0;
    let inset = thinkterm_plugin_sdk::panel::player::FIELD_INSET_LINES * 2.0;
    let lines = ((r.h - rest - inset) / env.body.line)
        .floor()
        .clamp(2.0, 4.0);
    let lines_h = env.body.line * lines + inset;

    // The title, with how much of its limit it has used.
    let fits = |y: f32, tall: f32| y + env.small.line + 4.0 + tall <= r.bottom();
    if !fits(y, h) {
        return y;
    }
    let used = state.title.text().chars().count();
    ui::caption(items, look, r.x, y, r.w - 64.0, "Title");
    let counter = if used as u32 >= TITLE_MAX {
        Token::Warning
    } else {
        Token::TextFaint
    };
    items.push(
        Text::new(
            r.right() - 64.0,
            y,
            64.0,
            env.small.line,
            format!("{used} / {TITLE_MAX}"),
        )
        .size(Size::Small)
        .color(counter)
        .align(Align::Right)
        .into(),
    );
    y += env.small.line + 4.0;
    let mut title = state
        .title
        .field(TITLE, r.x, y, r.w, h)
        .placeholder("A short summary, then Return")
        .icon("pencil")
        .max(TITLE_MAX)
        .clear();
    if let Some(seq) = focus(TITLE) {
        title = title.focus(seq);
    }
    items.push(title.into());
    y += h + GAP + 4.0;

    // Labels: Return adds one, as a chip below, and a press on one takes it
    // off again.
    if !fits(y, h + chip_h + GAP) {
        return y;
    }
    ui::caption(
        items,
        look,
        r.x,
        y,
        r.w,
        format!("Labels \u{00b7} {} of {LABELS}", state.labels.len()),
    );
    y += env.small.line + 4.0;
    let mut label = state
        .label
        .field(LABEL, r.x, y, r.w, h)
        .placeholder("Add a label, then Return")
        .icon("plus")
        .max(24);
    if let Some(seq) = focus(LABEL) {
        label = label.focus(seq);
    }
    items.push(label.into());
    y += h + GAP;
    if state.labels.is_empty() {
        ui::caption(
            items,
            look,
            r.x,
            y + (chip_h - env.small.line) / 2.0,
            r.w,
            "No labels yet",
        );
    }
    let mut x = r.x;
    for said in &state.labels {
        let w = look.text_w(said, Size::Small) + 36.0;
        if x + w > r.right() {
            break;
        }
        items.push(
            Rect::new(x, y, w, chip_h)
                .fill(Token::BgSelected)
                .radius(chip_h / 2.0)
                .into(),
        );
        items.push(
            Hit::new(format!("form:unlabel:{said}"), x, y, w, chip_h)
                .hover(Token::BgHover)
                .radius(chip_h / 2.0)
                .into(),
        );
        items.push(
            Text::new(x + 10.0, y, w - 30.0, chip_h, said.clone())
                .size(Size::Small)
                .into(),
        );
        items.push(
            Text::new(x + w - 22.0, y, 14.0, chip_h, "\u{00d7}")
                .color(Token::TextMuted)
                .align(Align::Center)
                .into(),
        );
        x += w + GAP;
    }
    y += chip_h + GAP + 4.0;

    // The assignee: people whose names have what is typed in them, below.
    if !fits(y, h + chip_h + GAP) {
        return y;
    }
    ui::caption(items, look, r.x, y, r.w, "Assignee");
    y += env.small.line + 4.0;
    let mut assignee = state
        .assignee
        .field(ASSIGNEE, r.x, y, r.w, h)
        .placeholder("Search people: try a or ro")
        .icon("search")
        .max(40)
        .clear();
    if let Some(seq) = focus(ASSIGNEE) {
        assignee = assignee.focus(seq);
    }
    items.push(assignee.into());
    y += h + GAP;
    let suggestions = state.suggestions();
    let typed = state.assignee.text().trim();
    if suggestions.is_empty() {
        let said = match typed.is_empty() {
            true => "Names show here as you type",
            false if PEOPLE.iter().any(|(_, name)| *name == typed) => "\u{2713} Assigned",
            false => "Nobody by that name",
        };
        ui::caption(
            items,
            look,
            r.x,
            y + (chip_h - env.small.line) / 2.0,
            r.w,
            said,
        );
    }
    let mut x = r.x;
    for (handle, name) in &suggestions {
        let w = look.text_w(name, Size::Small) + 40.0;
        if x + w > r.right() {
            break;
        }
        items.push(
            Rect::new(x, y, w, chip_h)
                .border(Token::Border)
                .radius(chip_h / 2.0)
                .into(),
        );
        items.push(
            Hit::new(format!("form:suggest:{handle}"), x, y, w, chip_h)
                .hover(Token::BgHover)
                .radius(chip_h / 2.0)
                .into(),
        );
        let initials: String = name
            .split(' ')
            .filter_map(|part| part.chars().next())
            .collect();
        let side = chip_h - 6.0;
        ui::avatar(items, x + 3.0, y + 3.0, side, &initials, Token::BgSelected);
        items.push(
            Text::new(x + side + 8.0, y, w - side - 12.0, chip_h, *name)
                .size(Size::Small)
                .into(),
        );
        let _ = handle;
        x += w + GAP;
    }
    y += chip_h + GAP + 4.0;

    // The description, on lines: Command-Return creates the issue.
    if !fits(y, lines_h) {
        return y;
    }
    ui::caption(
        items,
        look,
        r.x,
        y,
        r.w,
        "Description \u{00b7} \u{2318}\u{21a9} creates",
    );
    y += env.small.line + 4.0;
    let mut body = state
        .body
        .field(BODY, r.x, y, r.w, lines_h)
        .lines()
        .placeholder("What happened, and what did you expect?");
    if let Some(seq) = focus(BODY) {
        body = body.focus(seq);
    }
    items.push(body.into());
    y += lines_h + GAP + 4.0;

    // The token: a secret, sent only when Return is pressed in it.
    if !fits(y, h) {
        return y;
    }
    ui::caption(items, look, r.x, y, r.w, "API token \u{00b7} secret");
    y += env.small.line + 4.0;
    let mut token = state
        .token
        .field(TOKEN, r.x, y, r.w, h)
        .secret()
        .placeholder("Paste a token, then Return")
        .mono();
    if let Some(seq) = focus(TOKEN) {
        token = token.focus(seq);
    }
    items.push(token.into());
    y += h + GAP + 4.0;

    if y + h > r.bottom() {
        return y;
    }
    let create_w = ui::button_w(look, "Create issue");
    ui::button(
        items,
        look,
        "form:create",
        r.x,
        y,
        create_w,
        "Create issue",
        Kind::Accent,
    );
    let reset_w = ui::button_w(look, "Reset");
    ui::button(
        items,
        look,
        "form:reset",
        r.x + create_w + GAP,
        y,
        reset_w,
        "Reset",
        Kind::Plain,
    );
    // Where the keyboard is, as ThinkTerm says.
    let sx = r.x + create_w + reset_w + GAP * 3.0;
    let (said, color) = match (state.refused, &view.focus) {
        (Some(why), _) => (why.to_string(), Token::Negative),
        (None, Some(Focus { id: Some(id) })) => {
            (format!("Keyboard in {}", field_name(id)), Token::Accent)
        }
        (None, Some(Focus { id: None })) => ("Keyboard in the page".to_string(), Token::TextMuted),
        (None, None) => ("Press a field to type".to_string(), Token::TextFaint),
    };
    items.push(
        Text::new(sx, y, (r.right() - sx).max(0.0), h, said)
            .size(Size::Small)
            .color(color)
            .into(),
    );
    y + h
}

fn field_name(id: &str) -> &'static str {
    match id {
        TITLE => "Title",
        LABEL => "Labels",
        ASSIGNEE => "Assignee",
        BODY => "Description",
        TOKEN => "API token",
        _ => "a field",
    }
}

/// What was typed, as the issue would show: in full beside the form, or in
/// a few lines under it (`compact`).
fn preview(items: &mut Vec<Item>, look: &Look, r: Rgn, state: &State, compact: bool) {
    let env = look.env;
    let mut tall = r.h;
    if compact {
        tall = tall.min(env.body.line * 2.0 + env.small.line * 2.0 + 40.0);
    }
    ui::card(items, Rgn::new(r.x, r.y, r.w, tall));
    let inner = Rgn::new(r.x + PAD, r.y + 12.0, r.w - PAD * 2.0, tall - 24.0);
    let mut y = inner.y;
    // The issue made last, or that this is a draft.
    let (said, ground, color) = match &state.made {
        Some(issue) => (
            format!("Created #{}", issue.number),
            Token::PositiveBg,
            Token::Positive,
        ),
        None => ("Draft".to_string(), Token::BgHover, Token::TextMuted),
    };
    let chip_h = env.small.line + 4.0;
    let chip_w = look.text_w(&said, Size::Small) + 16.0;
    items.push(
        Text::new(inner.x, y, inner.w - chip_w - GAP, chip_h, "Preview")
            .size(Size::Small)
            .bold()
            .color(Token::TextMuted)
            .into(),
    );
    ui::chip(
        items,
        look,
        inner.right() - chip_w,
        y,
        &said,
        ground,
        color,
        chip_h,
    );
    y += chip_h + GAP;
    let title = state.title.text().trim();
    let (title, color) = match title.is_empty() {
        true => ("Untitled issue", Token::TextFaint),
        false => (title, Token::Text),
    };
    let most = if compact { 1 } else { 2 };
    let lines = look.lines_for(title, inner.w, Size::Title).min(most);
    let mut heading = Text::new(inner.x, y, inner.w, env.title.line * lines as f32, title)
        .size(Size::Title)
        .bold()
        .color(color);
    if look.wraps() {
        heading = heading.wrap();
    }
    items.push(heading.into());
    y += env.title.line * lines as f32 + 4.0;
    let assignee = match state.assignee.text().trim() {
        "" => "Unassigned".to_string(),
        name => name.to_string(),
    };
    let token = match state.token_len {
        None => "no token".to_string(),
        Some(len) => format!("token of {len} characters, not kept"),
    };
    let meta = format!(
        "{assignee} \u{00b7} {} labels \u{00b7} {token}",
        state.labels.len()
    );
    ui::caption(items, look, inner.x, y, inner.w, meta);
    y += env.small.line + GAP;
    if compact {
        return;
    }
    let mut x = inner.x;
    for said in &state.labels {
        let w = look.text_w(said, Size::Small) + 16.0;
        if x + w > inner.right() {
            break;
        }
        ui::chip(
            items,
            look,
            x,
            y,
            said,
            Token::BgSelected,
            Token::Text,
            chip_h,
        );
        x += w + 6.0;
    }
    if !state.labels.is_empty() {
        y += chip_h + GAP;
    }
    ui::rule(items, inner.x, y, inner.w);
    y += GAP + 4.0;
    let body = state.body.text().trim();
    let (body, color) = match body.is_empty() {
        true => ("Nothing described yet. The preview follows the fields as they change: the client sends what each holds as it is typed.", Token::TextFaint),
        false => (body, Token::Text),
    };
    let room = ((inner.bottom() - y) / env.body.line).floor().max(1.0) as usize;
    if y + env.body.line <= inner.bottom() {
        ui::paragraph(
            items,
            look,
            inner.x,
            y,
            inner.w,
            body,
            Size::Body,
            color,
            room,
        );
    }
    let _ = RADIUS;
}
