//! The cards heading tabs and pane capsules (`thinkterm_tab_icons`), and
//! the pictures the shell shows of them and of a thread's mark: how they
//! look is said once for every client (`thinkterm_tab_icons::look`), and
//! drawn here into pixels, so the shell draws nothing of its own. A glyph
//! on its own is still a coverage mask the shell tints, as the desktop's
//! atlas does. The built-in cards are alike on every machine; a machine's
//! restyled and imported ones live in its desktop's settings, which the
//! network thread reads over the connection's own ssh session (`ssh`), as
//! a page gets them from its server. The commands it runs and what they
//! print are here.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use thinkterm_tab_icons::{Catalog, GlyphKey, TabIconSettings, WireCard, WireCatalog};

/// The largest glyph a shell may ask for, in pixels.
pub const MAX_SIZE: u32 = 512;

/// The line the commands' answer starts with, on a line of its own:
/// whatever a login profile says comes before it.
const MARKER: &[u8] = b"thinkterm-tab-icons\n";
/// The line it ends with: whatever the shell says on its way out comes
/// after, and an answer cut short has none.
const END: &[u8] = b"thinkterm-tab-icons-end\n";

/// The most of a settings file read; one larger does not parse, and is no
/// settings.
pub const SETTINGS_LIMIT: usize = 4 << 20;
/// The largest imported glyph the desktop keeps, and a page is sent.
const SVG_LIMIT: usize = 256 * 1024;

/// The most `svgs_command` may print for `count` glyphs.
pub fn svgs_limit(count: usize) -> usize {
    count * (SVG_LIMIT + 128) + 1024
}

/// The built-in cards, as a page receives them.
pub fn catalog() -> WireCatalog {
    Catalog::build(&TabIconSettings::default(), |key| key.to_string()).wire(|_| None)
}

/// Prints the desktop's settings file, where the desktop keeps it under
/// the login's home (`config::native_settings_path`), or nothing on a
/// machine without one. `/bin/sh` runs it, whatever the login shell is.
pub fn settings_command() -> String {
    format!(
        r#"/bin/sh -c 'printf "\nthinkterm-tab-icons\n"; f="$HOME/.config/thinkterm/settings.json"; [ -f "$f" ] && head -c {SETTINGS_LIMIT} "$f"; printf "\nthinkterm-tab-icons-end\n"; exit 0'"#
    )
}

/// Prints the imported glyphs `hashes` names, each as a line with its hash
/// and length and then the file as it is (a gzipped `.svgz` has NULs in
/// it). One gone, or larger than the desktop keeps, is left out.
pub fn svgs_command(hashes: &[String]) -> String {
    // Hashes are hex (`is_svg_id`): nothing in them for a shell to read.
    let names = hashes.join(" ");
    format!(
        r#"/bin/sh -c 'printf "\nthinkterm-tab-icons\n"; cd "$HOME/.config/thinkterm/tab-icons" 2>/dev/null && for h in {names}; do [ -f "$h.svg" ] || continue; n=$(wc -c < "$h.svg" | tr -d " "); [ "$n" -le {SVG_LIMIT} ] || continue; printf "%s %s\n" "$h" "$n"; head -c "$n" "$h.svg"; done; printf "thinkterm-tab-icons-end\n"; exit 0'"#
    )
}

/// What follows the marker in a command's output; None if it never came
/// (no `/bin/sh` ran it). The commands print a newline first, so the
/// marker starts a line whatever a profile printed before it.
fn payload(out: &[u8]) -> Option<&[u8]> {
    let mut from = 0;
    loop {
        let at = from + out[from..].windows(MARKER.len()).position(|w| w == MARKER)?;
        if at == 0 || out[at - 1] == b'\n' {
            return Some(&out[at + MARKER.len()..]);
        }
        from = at + 1;
    }
}

/// The cards a desktop's settings make, from what `settings_command`
/// printed: the built-in ones where there are no settings or they cannot
/// be read, as its server has it (`web_relay::tab_icons_at`). None if the
/// command never ran.
pub fn desktop_cards(out: &[u8]) -> Option<Catalog> {
    #[derive(Default, serde::Deserialize)]
    struct Settings {
        #[serde(default)]
        tab_icons: TabIconSettings,
    }
    let text = payload(out)?;
    // Up to the end line: an answer without one was cut short.
    let end = [&b"\n"[..], END].concat();
    let file = &text[..text.windows(end.len()).rposition(|w| w == end.as_slice())?];
    let settings = serde_json::from_slice::<Settings>(file).unwrap_or_default();
    // Names are the desktop's to show; the phone draws only the icons.
    Some(Catalog::build(&settings.tab_icons, |key| key.to_string()))
}

/// The imported glyphs `cards` show, by hash.
pub fn imported(cards: &Catalog) -> Vec<String> {
    let mut hashes: Vec<String> = cards
        .cards
        .iter()
        .filter_map(|card| match &card.glyph {
            GlyphKey::Svg(hash) => Some(hash.to_string()),
            GlyphKey::Builtin(_) => None,
        })
        .collect();
    hashes.sort();
    hashes.dedup();
    hashes
}

/// The glyphs `svgs_command` printed for `hashes`, by hash: those that
/// came whole. One that is not text (a gzipped `.svgz`) is left out, as
/// the host's server leaves it out of a page's.
pub fn imported_svgs(out: &[u8], hashes: &[String]) -> HashMap<String, String> {
    let mut svgs = HashMap::new();
    let Some(mut rest) = payload(out) else {
        return svgs;
    };
    while !rest.starts_with(END) {
        // A line of hash and length, then that many bytes; anything else
        // is an answer cut short, and what came before it stands.
        let Some(eol) = rest.iter().position(|&b| b == b'\n') else {
            break;
        };
        let Some((hash, len)) = std::str::from_utf8(&rest[..eol]).ok().and_then(|line| line.split_once(' ')) else {
            break;
        };
        let Some(len) = len.parse::<usize>().ok().filter(|&len| len <= SVG_LIMIT && eol + 1 + len <= rest.len()) else {
            break;
        };
        if !hashes.iter().any(|h| h == hash) {
            break;
        }
        if let Ok(svg) = std::str::from_utf8(&rest[eol + 1..eol + 1 + len]) {
            if !svg.is_empty() {
                svgs.insert(hash.to_string(), svg.to_string());
            }
        }
        rest = &rest[eol + 1 + len..];
    }
    svgs
}

/// The cards on show: the core thread puts in each connection's, the
/// shell's thread draws them (`Core::tab_icon`).
pub struct Shown {
    /// Taken by the shell's thread to draw: held only to swap or clone the
    /// pointer, never while the cards are compared or written out.
    cards: Mutex<Arc<WireCatalog>>,
    /// Which cards these are, for a shell to drop what it drew before.
    revision: AtomicU64,
}

/// Revisions are numbered across every `Shown` in the process: a shell's
/// drawings are kept for one, whatever core they came from.
fn next_revision() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

impl Shown {
    pub fn new(cards: WireCatalog) -> Self {
        Self {
            cards: Mutex::new(Arc::new(cards)),
            revision: AtomicU64::new(next_revision()),
        }
    }

    fn current(&self) -> Arc<WireCatalog> {
        Arc::clone(&self.cards.lock().unwrap())
    }

    /// Put `cards` on show; false if they are the ones there. Only the core
    /// thread puts cards on show.
    pub fn set(&self, cards: WireCatalog) -> bool {
        if *self.current() == cards {
            return false;
        }
        *self.cards.lock().unwrap() = Arc::new(cards);
        self.revision.store(next_revision(), Ordering::Release);
        true
    }

    pub fn card(&self, id: &str) -> Option<WireCard> {
        self.current().cards.iter().find(|card| card.id == id).cloned()
    }

    /// The cards as an App takes them (`App::set_tab_icons`).
    pub fn json(&self) -> String {
        serde_json::to_string(&*self.current()).unwrap_or_default()
    }

    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }
}

/// The glyph's shape, `size` pixels square and centred: one byte a pixel,
/// row by row, how much of it the shape covers. Its colours are dropped,
/// as the desktop drops them. None for a card with no glyph.
pub fn mask(svg: &str, size: u32) -> Option<Vec<u8>> {
    if svg.is_empty() {
        return None;
    }
    // A host's own SVG is anyone's: a renderer that panics on it costs
    // the glyph, not the app (a panic would cross into the shell).
    std::panic::catch_unwind(|| draw(svg, size)).ok().flatten()
}

fn draw(svg: &str, size: u32) -> Option<Vec<u8>> {
    let size = size.clamp(1, MAX_SIZE);
    let mut options = resvg::usvg::Options::default();
    // An icon is a shape: nothing it names outside itself is ever fetched.
    options.image_href_resolver.resolve_string = Box::new(|_, _| None);
    let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &options).ok()?;
    let svg_size = tree.size();
    let scale = (size as f32 / svg_size.width()).min(size as f32 / svg_size.height());
    let translate_x = (size as f32 - svg_size.width() * scale) / 2.0;
    let translate_y = (size as f32 - svg_size.height() * scale) / 2.0;
    let transform = resvg::tiny_skia::Transform::from_translate(translate_x, translate_y).pre_scale(scale, scale);
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size)?;
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Some(pixmap.data().chunks_exact(4).map(|pixel| pixel[3]).collect())
}

/// `svg` drawn as it is, `width` by `height` pixels: four bytes a pixel,
/// red first, the colours already multiplied by how opaque they are. None
/// for one that does not draw.
pub fn picture(svg: &str, width: u32, height: u32) -> Option<Vec<u8>> {
    // A card's glyph is a host's own SVG; see `mask`.
    std::panic::catch_unwind(|| paint(svg, width, height)).ok().flatten()
}

fn paint(svg: &str, width: u32, height: u32) -> Option<Vec<u8>> {
    let mut options = resvg::usvg::Options::default();
    options.image_href_resolver.resolve_string = Box::new(|_, _| None);
    let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &options).ok()?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)?;
    resvg::render(&tree, resvg::tiny_skia::Transform::identity(), &mut pixmap.as_mut());
    Some(pixmap.take())
}

/// The largest picture of a row's icons or of a mark, in pixels a side.
pub const MAX_PICTURE: f32 = 1024.0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_s_icons_and_a_mark_draw_as_the_look_says() {
        use thinkterm_tab_icons::look;
        let catalog = catalog();
        let terminal = catalog.cards.iter().find(|card| card.id == catalog.terminal).unwrap();
        let cards = look::cards_svg(&[terminal], 40.0, 2.0, true, true).unwrap();
        let (width, height) = (cards.width as u32, cards.height as u32);
        let pixels = picture(&cards.svg, width, height).expect("a picture");
        assert_eq!(pixels.len(), (width * height * 4) as usize);
        let at = |x: f32, y: f32| {
            let i = ((y as u32 * width + x as u32) * 4) as usize;
            [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
        };
        let circle = thinkterm_tab_icons::Rgb::parse(&terminal.circle).unwrap();
        // Just inside the rim at the top the fill is at its lightest, and
        // at the foot at its darkest; both are whole.
        let top = at(cards.inset + 20.0, cards.inset + 4.0);
        let foot = at(cards.inset + 20.0, cards.inset + 35.0);
        let near = |pixel: [u8; 4], color: thinkterm_tab_icons::Rgb| {
            pixel[3] == 255
                && (pixel[0] as i32 - color.0 as i32).abs() <= 6
                && (pixel[1] as i32 - color.1 as i32).abs() <= 6
                && (pixel[2] as i32 - color.2 as i32).abs() <= 6
        };
        assert!(near(top, look::lighten(circle, look::PLATE.fill_top_lighten)), "{top:?}");
        assert!(near(foot, look::darken(circle, look::PLATE.fill_bottom_darken)), "{foot:?}");
        // The glyph is on it, in its own colour, and the shadow under it:
        // some of it below the circle, none above.
        let glyph = thinkterm_tab_icons::Rgb::parse(&terminal.glyph).unwrap();
        let on_glyph = (0..40).flat_map(|y| (0..40).map(move |x| (x, y))).any(|(x, y)| {
            let pixel = at(cards.inset + x as f32, cards.inset + y as f32);
            pixel[..3] == [glyph.0, glyph.1, glyph.2]
        });
        assert!(on_glyph, "the glyph is drawn in the card's glyph colour");
        let below = at(cards.inset + 20.0, cards.inset + 44.0)[3];
        assert!(below > 0 && below < 255, "{below}");
        assert_eq!(at(cards.inset + 20.0, 0.0)[3], 0);

        // A working mark: nine pixels, the run's head whole and the rest
        // fainter; a finished one a tick in the same square.
        let color = look::srgb(look::WORKING);
        let side = look::mark_side(20.0) as u32;
        let mark = picture(&look::mark_svg(look::Mark::Working, Some(0), 20.0, color, 1.0), side, side).unwrap();
        let alphas: Vec<u8> = mark.chunks_exact(4).map(|pixel| pixel[3]).collect();
        assert!(alphas.contains(&255) && alphas.contains(&0));
        assert!(alphas.iter().any(|&a| (40..=50).contains(&a)), "an unlit pixel is 0.18 there");
        let tick = picture(&look::mark_svg(look::Mark::Finished, None, 20.0, look::srgb(look::FINISHED), 1.0), side, side).unwrap();
        assert!(tick.chunks_exact(4).any(|pixel| pixel[3] > 200), "the tick is drawn");
        assert!(tick.chunks_exact(4).any(|pixel| pixel[3] == 0));
    }

    #[test]
    fn the_built_in_cards_carry_their_glyphs() {
        let catalog = catalog();
        let terminal = catalog.cards.iter().find(|card| card.id == catalog.terminal).expect("the terminal's card");
        assert!(terminal.circle.starts_with('#') && terminal.glyph.starts_with('#'));
        let mask = mask(&terminal.svg, 48).expect("a glyph");
        assert_eq!(mask.len(), 48 * 48);
        assert!(mask.iter().any(|&a| a > 0), "the shape covers something");
        assert!(mask.iter().any(|&a| a == 0), "and not everything");
    }

    #[test]
    fn no_glyph_and_bad_shapes_draw_nothing() {
        assert_eq!(mask("", 32), None);
        assert_eq!(mask("not an svg", 32), None);
    }

    #[test]
    fn a_desktops_settings_make_its_cards() {
        let hash = "c".repeat(64);
        let settings = format!(
            r##"{{"theme":"x","tab_icons":{{"cards":[{{"id":"python","circle":"#E8772E"}},{{"id":"custom-1","programs":["deploy"],"svg":"{hash}","circle":"#112233"}}]}}}}"##
        );
        // A login profile may say something first, without a newline, and
        // the shell something on its way out.
        let out = format!("Welcome!\nthinkterm-tab-icons\n{settings}\nthinkterm-tab-icons-end\nbye\n");
        let cards = desktop_cards(out.as_bytes()).expect("the command ran");
        let hashes = imported(&cards);
        assert_eq!(hashes, vec![hash.clone()]);
        let svg = "<svg id='mine'/>";
        let out = format!("\nthinkterm-tab-icons\n{hash} {}\n{svg}thinkterm-tab-icons-end\n", svg.len());
        let svgs = imported_svgs(out.as_bytes(), &hashes);
        let wire = cards.wire(|hash| svgs.get(hash).cloned());
        let custom = wire.cards.iter().find(|card| card.id == "custom-1").unwrap();
        assert_eq!(custom.svg, svg);
        assert_eq!(custom.circle, "#112233");
        assert_eq!(custom.programs, vec!["deploy".to_string()]);
        assert_eq!(wire.cards.iter().find(|card| card.id == "python").unwrap().circle, "#E8772E");
    }

    #[test]
    fn a_machine_without_settings_has_the_built_in_cards() {
        for out in [
            &b"\nthinkterm-tab-icons\n\nthinkterm-tab-icons-end\n"[..],
            b"\nthinkterm-tab-icons\n{not json\nthinkterm-tab-icons-end\n",
        ] {
            let cards = desktop_cards(out).expect("the command ran");
            assert_eq!(cards.wire(|_| None), catalog());
        }
        // Nothing ran the command, or its answer was cut short: not the
        // same as no settings.
        assert!(desktop_cards(b"sh: not found\n").is_none());
        assert!(desktop_cards(b"xthinkterm-tab-icons\n{}\nthinkterm-tab-icons-end\n").is_none(), "the marker starts a line");
        assert!(desktop_cards(b"\nthinkterm-tab-icons\n{\"tab_icons\":{\"enab").is_none(), "no end line");
    }

    #[test]
    fn glyphs_that_are_not_text_or_cut_short_are_left_out() {
        let (a, b, c) = ("a".repeat(64), "b".repeat(64), "c".repeat(64));
        let hashes = vec![a.clone(), b.clone(), c.clone()];
        // A gzipped glyph (NULs and all) is skipped, the one after it read.
        let mut out = format!("\nthinkterm-tab-icons\n{a} 4\n").into_bytes();
        out.extend_from_slice(&[0x1f, 0x8b, 0x08, 0x00]);
        out.extend_from_slice(format!("{b} 6\n<svg/>thinkterm-tab-icons-end\n").as_bytes());
        let svgs = imported_svgs(&out, &hashes);
        assert_eq!(svgs.len(), 1);
        assert_eq!(svgs[&b], "<svg/>");
        // Cut short in a glyph: the ones before it stand.
        let out = format!("\nthinkterm-tab-icons\n{a} 6\n<svg/>{c} 100\n<svg");
        let svgs = imported_svgs(out.as_bytes(), &hashes);
        assert_eq!(svgs.keys().collect::<Vec<_>>(), vec![&a]);
        // A length past the desktop's limit is not believed.
        let out = format!("\nthinkterm-tab-icons\n{a} {}\n", SVG_LIMIT + 1);
        assert!(imported_svgs(out.as_bytes(), &hashes).is_empty());
    }

    #[test]
    fn the_commands_name_only_hex_inside_their_quotes() {
        let hash = "0123456789abcdef".repeat(4);
        let command = svgs_command(std::slice::from_ref(&hash));
        assert!(command.contains(&format!("for h in {hash};")));
        for command in [settings_command(), command] {
            assert!(command.starts_with("/bin/sh -c '") && command.ends_with('\''));
            assert_eq!(command.matches('\'').count(), 2, "{command}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_commands_read_a_desktops_files() {
        // As a login shell runs them, with the home the files are under.
        let run = |home: &std::path::Path, command: String| {
            std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(command)
                .env("HOME", home)
                .output()
                .expect("running /bin/sh")
                .stdout
        };
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".config").join("thinkterm");
        std::fs::create_dir_all(dir.join("tab-icons")).unwrap();
        // A gzipped glyph sorts first; the text one after it still comes.
        let (gz, text) = ("a".repeat(64), "c".repeat(64));
        std::fs::write(
            dir.join("settings.json"),
            format!(r#"{{"tab_icons":{{"cards":[{{"id":"custom-1","svg":"{gz}"}},{{"id":"custom-2","svg":"{text}"}}]}}}}"#),
        )
        .unwrap();
        std::fs::write(dir.join("tab-icons").join(format!("{gz}.svg")), [0x1f, 0x8b, 0x08, 0x00, 0x00]).unwrap();
        std::fs::write(dir.join("tab-icons").join(format!("{text}.svg")), "<svg id='mine'/>").unwrap();
        let cards = desktop_cards(&run(home.path(), settings_command())).expect("the command ran");
        let hashes = imported(&cards);
        assert_eq!(hashes, vec![gz.clone(), text.clone()]);
        let out = run(home.path(), svgs_command(&hashes));
        assert!(out.len() <= svgs_limit(hashes.len()));
        let svgs = imported_svgs(&out, &hashes);
        assert_eq!(svgs.keys().collect::<Vec<_>>(), vec![&text]);
        assert_eq!(svgs[&text], "<svg id='mine'/>");

        // A home with no desktop: the built-in cards, and no glyphs to read.
        let bare = tempfile::tempdir().unwrap();
        let cards = desktop_cards(&run(bare.path(), settings_command())).expect("the command ran");
        assert_eq!(cards.wire(|_| None), catalog());
        assert!(imported_svgs(&run(bare.path(), svgs_command(&hashes)), &hashes).is_empty());
    }

    #[test]
    fn the_cards_on_show_change_only_when_they_differ() {
        let shown = Shown::new(catalog());
        let first = shown.revision();
        assert!(!shown.set(catalog()));
        assert_eq!(shown.revision(), first);
        let mut other = catalog();
        other.cards[0].circle = "#000000".into();
        let id = other.cards[0].id.clone();
        assert!(shown.set(other.clone()));
        assert_ne!(shown.revision(), first);
        assert_eq!(shown.card(&id).unwrap().circle, "#000000");
        assert!(shown.json().contains("#000000"));
        // Another core's cards, changed as often, are never taken for these.
        let elsewhere = Shown::new(catalog());
        assert!(elsewhere.set(other));
        assert_ne!(elsewhere.revision(), shown.revision());
    }
}
