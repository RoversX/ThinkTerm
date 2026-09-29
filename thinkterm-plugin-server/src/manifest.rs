//! A plugin's manifest, `plugin.toml`: who the plugin is, and the program
//! it runs. docs/thinkterm/plugins.md describes it.
//! A built-in plugin has one too, without the program.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use thinkterm_plugin_channel::registry::{self, Background, Panel};
use thinkterm_plugin_sdk::protocol::{API, PANEL_API};

pub const FILE: &str = "plugin.toml";
/// The longest id: it names a directory, a switch, and every event.
const ID_LIMIT: usize = 64;
/// A file this long is not a manifest.
const SIZE_LIMIT: u64 = 256 * 1024;

#[derive(Debug, Deserialize)]
struct Raw {
    id: String,
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    description: String,
    api: u32,
    #[serde(default)]
    platforms: Vec<String>,
    #[serde(default)]
    run: Option<RawRun>,
    #[serde(default)]
    panel: Option<RawPanel>,
    #[serde(default)]
    locales: BTreeMap<String, RawLocale>,
}

#[derive(Debug, Default, Deserialize)]
struct RawPanel {
    #[serde(default)]
    icon: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawRun {
    #[serde(default)]
    program: Option<String>,
    #[serde(default)]
    args: Option<Vec<String>>,
    /// How long the program runs unused: `[run]`'s own, the same on every
    /// system.
    #[serde(default)]
    background: Option<Background>,
    #[serde(default)]
    macos: Option<Box<RawRun>>,
    #[serde(default)]
    linux: Option<Box<RawRun>>,
    #[serde(default)]
    windows: Option<Box<RawRun>>,
}

#[derive(Debug, Default, Deserialize)]
struct RawLocale {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub platforms: Vec<String>,
    /// The panel it adds to the right sidebar.
    pub panel: Option<Panel>,
    /// How long its program runs unused, unless the user chose otherwise.
    pub background: Background,
    /// The program for this system, and its arguments. None for a built-in
    /// plugin, and for one that does not run on this system.
    run: Option<(String, Vec<String>)>,
    /// By language tag, lowercased.
    locales: BTreeMap<String, Locale>,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Locale {
    name: Option<String>,
    description: Option<String>,
}

/// A plugin's name and description in one language.
pub struct Localized<'a> {
    pub name: &'a str,
    pub description: &'a str,
}

/// Whether `id` can name a plugin: a-z, 0-9 and "-", starting with a letter
/// or digit.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= ID_LIMIT
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn id_rule() -> String {
    format!("a-z, 0-9 and \"-\", at most {ID_LIMIT} characters")
}

/// The icon a panel that names none, or one no client has, gets.
const PANEL_ICON: &str = thinkterm_plugin_sdk::panel::ICONS[0];

impl Manifest {
    /// The manifest in an installed plugin's directory.
    pub fn load(dir: &Path) -> Result<Self, String> {
        let path = dir.join(FILE);
        let meta = std::fs::metadata(&path).map_err(|err| match err.kind() {
            ErrorKind::NotFound => format!("there is no {FILE}"),
            _ => format!("{FILE}: {err}"),
        })?;
        if meta.len() > SIZE_LIMIT {
            return Err(format!("{FILE} is larger than {SIZE_LIMIT} bytes"));
        }
        let text = std::fs::read_to_string(&path).map_err(|err| format!("{FILE}: {err}"))?;
        Self::parse(&text, false)
    }

    /// A manifest's text. A built-in plugin's names no program.
    pub fn parse(text: &str, builtin: bool) -> Result<Self, String> {
        let raw: Raw = toml::from_str(text).map_err(|err| {
            let line = err.span().map(|span| {
                let before = &text.as_bytes()[..span.start.min(text.len())];
                before.iter().filter(|byte| **byte == b'\n').count() + 1
            });
            match line {
                Some(line) => format!("{FILE}, line {line}: {}", err.message()),
                None => format!("{FILE}: {}", err.message()),
            }
        })?;
        if !valid_id(&raw.id) {
            return Err(format!("the id {:?} is not {}", raw.id, id_rule()));
        }
        if raw.id == registry::PLUGIN {
            return Err(format!("the id {:?} is ThinkTerm's own", raw.id));
        }
        if raw.name.trim().is_empty() {
            return Err("the name is empty".into());
        }
        if raw.api > API {
            return Err(format!(
                "it needs a newer ThinkTerm: it speaks plugin API {}, this one speaks {API}",
                raw.api
            ));
        }
        if raw.api == 0 {
            return Err(format!("api must be {API}"));
        }
        let platforms: Vec<String> = raw
            .platforms
            .iter()
            .map(|platform| platform.to_ascii_lowercase())
            .collect();
        let supported = supports(&platforms);
        let background = raw
            .run
            .as_ref()
            .and_then(|run| run.background)
            .unwrap_or_default();
        let run = match (builtin, raw.run) {
            (true, None) => None,
            (true, Some(_)) => return Err("a built-in plugin runs no program".into()),
            (false, None) => return Err("there is no [run] section".into()),
            (false, Some(_)) if !supported => None,
            (false, Some(run)) => Some(run.for_this_system()?),
        };
        if !builtin && raw.version.trim().is_empty() {
            return Err("the version is empty".into());
        }
        let panel = match raw.panel {
            None => None,
            Some(_) if builtin => return Err("a built-in plugin's panel is ThinkTerm's own".into()),
            Some(_) if raw.api < PANEL_API => {
                return Err(format!("a [panel] needs api = {PANEL_API} or later"))
            }
            Some(panel) => Some(Panel {
                icon: panel
                    .icon
                    .filter(|icon| thinkterm_plugin_sdk::panel::ICONS.contains(&icon.as_str()))
                    .unwrap_or_else(|| PANEL_ICON.to_string()),
            }),
        };
        let locales = raw
            .locales
            .into_iter()
            .map(|(tag, locale)| {
                let locale = Locale {
                    name: locale.name.filter(|name| !name.trim().is_empty()),
                    description: locale.description,
                };
                (tag.to_ascii_lowercase(), locale)
            })
            .collect();
        Ok(Self {
            id: raw.id,
            name: raw.name,
            version: raw.version,
            description: raw.description,
            platforms,
            panel,
            background,
            run,
            locales,
        })
    }

    /// Whether it runs on this system.
    pub fn supported(&self) -> bool {
        supports(&self.platforms)
    }

    /// Its name and description in `locale`, where it has them: the tag
    /// itself (`zh-CN`), else its language (`zh`), else another tag of the
    /// same language.
    pub fn localized(&self, locale: &str) -> Localized<'_> {
        let found = self.locale_for(locale);
        Localized {
            name: found
                .and_then(|found| found.name.as_deref())
                .unwrap_or(&self.name),
            description: found
                .and_then(|found| found.description.as_deref())
                .unwrap_or(&self.description),
        }
    }

    fn locale_for(&self, tag: &str) -> Option<&Locale> {
        let tag = tag.to_ascii_lowercase();
        let language = tag
            .split(['-', '_'])
            .next()
            .filter(|lang| !lang.is_empty())?;
        self.locales
            .get(&tag)
            .or_else(|| self.locales.get(language))
            .or_else(|| {
                self.locales
                    .iter()
                    .find(|(key, _)| key.split(['-', '_']).next() == Some(language))
                    .map(|(_, found)| found)
            })
    }

    /// The program to start for the plugin in `dir`, and its arguments: the
    /// file beside the manifest when there is one, else, for a bare name, a
    /// program on PATH.
    pub fn program(&self, dir: &Path) -> Result<(PathBuf, Vec<String>), String> {
        let Some((program, args)) = &self.run else {
            return Err("it runs no program on this system".into());
        };
        let named = Path::new(program);
        let found = if named.is_absolute() {
            existing(named)
        } else {
            existing(&dir.join(named)).or_else(|| {
                // A bare name, such as an interpreter.
                (named.components().count() == 1)
                    .then(|| on_path(named))
                    .flatten()
            })
        };
        match found {
            Some(found) => Ok((found, args.clone())),
            None => Err(format!("cannot find its program {program:?}")),
        }
    }
}

fn supports(platforms: &[String]) -> bool {
    platforms.is_empty() || platforms.iter().any(|os| os == std::env::consts::OS)
}

impl RawRun {
    /// The program and arguments for this system: its own section's, where
    /// it has one, over the shared ones.
    fn for_this_system(self) -> Result<(String, Vec<String>), String> {
        let own = match std::env::consts::OS {
            "macos" => self.macos,
            "linux" => self.linux,
            "windows" => self.windows,
            _ => None,
        };
        let (program, args) = match own {
            Some(own) => (own.program.or(self.program), own.args.or(self.args)),
            None => (self.program, self.args),
        };
        let program = program
            .filter(|program| !program.trim().is_empty())
            .ok_or_else(|| format!("[run] names no program for {}", std::env::consts::OS))?;
        Ok((program, args.unwrap_or_default()))
    }
}

/// `path`, if it is a file; on Windows, with ".exe" added when that is.
fn existing(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    if cfg!(windows) && path.extension().is_none() {
        let exe = path.with_extension("exe");
        if exe.is_file() {
            return Some(exe);
        }
    }
    None
}

fn on_path(name: &Path) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| existing(&dir.join(name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
        id = "text-tools"
        name = "Text Tools"
        version = "0.1.0"
        description = "Decodes things."
        api = 1

        [run]
        program = "tool"
        args = ["--stdio"]

        [run.windows]
        program = "tool.exe"

        [locales.zh-CN]
        name = "文本工具"

        [locales.ja]
        description = "デコード"
    "#;

    #[test]
    fn a_manifest_reads_and_localizes() {
        let manifest = Manifest::parse(EXAMPLE, false).unwrap();
        assert_eq!(manifest.id, "text-tools");
        let (program, args) = manifest.run.clone().unwrap();
        assert_eq!(program, if cfg!(windows) { "tool.exe" } else { "tool" });
        assert_eq!(args, ["--stdio"]);
        assert!(manifest.supported());

        let chinese = manifest.localized("zh-CN");
        assert_eq!(chinese.name, "文本工具");
        assert_eq!(chinese.description, "Decodes things.");
        assert_eq!(manifest.localized("zh").name, "文本工具", "same language");
        assert_eq!(manifest.localized("ja-JP").description, "デコード");
        assert_eq!(manifest.localized("").name, "Text Tools");
        assert_eq!(manifest.localized("de-DE").name, "Text Tools");
        assert_eq!(manifest.background, Background::Briefly, "unless it says");
    }

    #[test]
    fn a_manifest_says_how_long_its_program_runs_unused() {
        let with =
            |line: &str| Manifest::parse(&EXAMPLE.replacen("args = [\"--stdio\"]", line, 1), false);
        assert_eq!(
            with("background = \"always\"").unwrap().background,
            Background::Always
        );
        assert_eq!(
            with("background = \"never\"").unwrap().background,
            Background::Never
        );
        assert!(with("background = \"sometimes\"")
            .unwrap_err()
            .contains("line"));
    }

    #[test]
    fn what_a_manifest_must_not_be() {
        let broken = |from: &str, to: &str| {
            let text = EXAMPLE.replacen(from, to, 1);
            Manifest::parse(&text, false).unwrap_err()
        };
        assert!(broken("text-tools", "Text Tools").contains("is not a-z"));
        assert!(broken("text-tools", "plugins").contains("ThinkTerm's own"));
        assert!(broken("api = 1", "api = 3").contains("newer ThinkTerm"));
        assert!(broken("api = 1", "api = 0").contains("api must be"));
        assert!(broken("version = \"0.1.0\"", "").contains("version"));
        let unreadable = broken("[run]", "[run");
        assert!(unreadable.starts_with("plugin.toml, line "), "{unreadable}");
        assert!(Manifest::parse(EXAMPLE, true)
            .unwrap_err()
            .contains("runs no program"));
    }

    #[test]
    fn a_panel_needs_the_api_that_draws_one() {
        let with_panel = |api: u32, panel: &str| {
            let text = EXAMPLE.replace("api = 1", &format!("api = {api}"));
            Manifest::parse(&format!("{text}\n{panel}\n"), false)
        };
        let manifest = with_panel(2, "[panel]\nicon = \"git-compare\"").unwrap();
        assert_eq!(manifest.panel.unwrap().icon, "git-compare");
        let manifest = with_panel(2, "[panel]\nicon = \"Not An Icon\"").unwrap();
        assert_eq!(manifest.panel.unwrap().icon, PANEL_ICON);
        let manifest = with_panel(2, "[panel]\nicon = \"house\"").unwrap();
        assert_eq!(
            manifest.panel.unwrap().icon,
            PANEL_ICON,
            "a Lucide icon no client has"
        );
        assert_eq!(
            with_panel(2, "[panel]").unwrap().panel.unwrap().icon,
            PANEL_ICON
        );
        assert!(with_panel(2, "").unwrap().panel.is_none());
        assert!(with_panel(1, "[panel]")
            .unwrap_err()
            .contains("needs api = 2"));
    }

    #[test]
    fn an_error_says_the_line_it_is_on() {
        // The second id starts its line.
        let twice = Manifest::parse("id = \"a\"\nid = \"b\"\n", false).unwrap_err();
        assert!(twice.starts_with("plugin.toml, line 2:"), "{twice}");
        let first = Manifest::parse("id = \n", false).unwrap_err();
        assert!(first.starts_with("plugin.toml, line 1:"), "{first}");
    }

    #[test]
    fn a_plugin_for_another_system_needs_no_program_here() {
        let elsewhere = if cfg!(windows) { "linux" } else { "windows" };
        let text = format!(
            "id = \"a\"\nname = \"A\"\nversion = \"1\"\napi = 1\nplatforms = [\"{elsewhere}\"]\n[run.{elsewhere}]\nprogram = \"a\"\n"
        );
        let manifest = Manifest::parse(&text, false).unwrap();
        assert!(!manifest.supported());
        assert!(manifest.program(Path::new(".")).is_err());
    }

    #[test]
    fn the_program_is_found_beside_the_manifest_or_on_path() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = Manifest::parse(EXAMPLE, false).unwrap();
        assert!(manifest
            .program(dir.path())
            .unwrap_err()
            .contains("cannot find"));
        let name = if cfg!(windows) { "tool.exe" } else { "tool" };
        std::fs::write(dir.path().join(name), "").unwrap();
        let (found, _) = manifest.program(dir.path()).unwrap();
        assert_eq!(found, dir.path().join(name));

        let interpreter = if cfg!(windows) { "cmd" } else { "sh" };
        let text = EXAMPLE
            .replace(
                "program = \"tool\"",
                &format!("program = \"{interpreter}\""),
            )
            .replace(
                "program = \"tool.exe\"",
                &format!("program = \"{interpreter}\""),
            );
        let manifest = Manifest::parse(&text, false).unwrap();
        let (found, _) = manifest.program(dir.path()).unwrap();
        assert!(found.is_absolute(), "{found:?} is from PATH");
    }

    #[test]
    fn ids() {
        for id in ["a", "text-tools", "x2"] {
            assert!(valid_id(id), "{id}");
        }
        for id in ["", "-a", "A", "a_b", "a b", &"a".repeat(65)] {
            assert!(!valid_id(id), "{id}");
        }
    }

    #[test]
    fn a_manifest_nested_as_deep_as_toml_goes_is_read_on_a_hosts_stack() {
        // The parser takes 80 levels, and refuses more.
        let deepest = |open: &str, close: &str| {
            format!(
                "id = \"a\"\nname = \"A\"\napi = 1\nx = {}1{}\n[run]\nprogram = \"a\"\n",
                open.repeat(79),
                close.repeat(79)
            )
        };
        for text in [deepest("[", "]"), deepest("{a=", "}")] {
            // Read or refused, as long as it is not the host aborted.
            std::thread::Builder::new()
                .stack_size(crate::process::STACK)
                .spawn(move || Manifest::parse(&text, false).is_ok())
                .unwrap()
                .join()
                .unwrap();
        }
    }
}
