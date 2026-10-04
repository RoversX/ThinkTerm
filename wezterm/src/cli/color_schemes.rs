//! `thinkterm cli color-schemes`: the built-in colour schemes, resolved to
//! plain hex colours.
//!
//! Nothing here talks to a mux server: the table is compiled in. The browser
//! client is the consumer -- `ci/build-web.sh` runs this into
//! `thinkterm-web/www/schemes.json`, which the scheme picker fetches.

use clap::Parser;
use config::ColorSchemeFile;
use serde::Serialize;
use std::io::Write;
use wezterm_term::color::ColorPalette;

#[derive(Debug, Parser, Clone)]
pub struct ColorSchemesCommand {
    /// Print the schemes and their colours as a JSON array.
    #[arg(long = "json")]
    json: bool,
}

#[derive(Debug, Serialize)]
struct Scheme {
    name: String,
    foreground: String,
    background: String,
    cursor_bg: String,
    cursor_fg: String,
    cursor_border: String,
    selection_bg: String,
    selection_fg: String,
    ansi: Vec<String>,
    brights: Vec<String>,
}

/// The scheme's own colours over the stock palette, so that a scheme that
/// names only `ansi`/`foreground`/`background` still answers for the cursor
/// and the selection.
fn resolve(name: &str, toml: &str) -> anyhow::Result<Scheme> {
    let scheme = ColorSchemeFile::from_toml_str(toml)?;
    let p = ColorPalette::from(scheme.colors);
    Ok(Scheme {
        name: name.to_string(),
        foreground: p.foreground.to_rgb_string(),
        background: p.background.to_rgb_string(),
        cursor_bg: p.cursor_bg.to_rgb_string(),
        cursor_fg: p.cursor_fg.to_rgb_string(),
        cursor_border: p.cursor_border.to_rgb_string(),
        selection_bg: p.selection_bg.to_rgb_string(),
        selection_fg: p.selection_fg.to_rgb_string(),
        ansi: (0..8).map(|i| p.colors.0[i].to_rgb_string()).collect(),
        brights: (8..16).map(|i| p.colors.0[i].to_rgb_string()).collect(),
    })
}

impl ColorSchemesCommand {
    pub fn run(&self) -> anyhow::Result<()> {
        let mut schemes = Vec::with_capacity(config::scheme_data::SCHEMES.len());
        for (name, toml) in config::scheme_data::SCHEMES.iter() {
            match resolve(name, toml) {
                Ok(scheme) => schemes.push(scheme),
                Err(err) => log::warn!("color scheme {name:?} did not parse: {err:#}"),
            }
        }
        self.print(&schemes).map_err(crate::stdout_error)
    }

    fn print(&self, schemes: &[Scheme]) -> std::io::Result<()> {
        // Buffered: the plain list is a thousand short lines.
        let mut out = std::io::BufWriter::new(std::io::stdout().lock());
        if self.json {
            serde_json::to_writer(&mut out, schemes)?;
            writeln!(out)?;
        } else {
            for scheme in schemes {
                writeln!(out, "{}", scheme.name)?;
            }
        }
        out.flush()
    }
}
