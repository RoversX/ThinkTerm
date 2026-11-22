//! A plugin's manifest, `plugin.toml`: who the plugin is, and the program
//! it runs -- or the Python script, which the Python found on this machine
//! runs (`python`). docs/thinkterm/plugins.md describes it.
//! A built-in plugin has one too, without the program.

use crate::python;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
pub use thinkterm_plugin_channel::registry::valid_id;
use thinkterm_plugin_channel::registry::{self, id_rule, Action, Background, Does, Page, Panel};
use thinkterm_plugin_sdk::protocol::{API, PAGE_API, PANEL_API};

pub const FILE: &str = "plugin.toml";
/// A file this long is not a manifest.
const SIZE_LIMIT: u64 = 256 * 1024;
/// The most actions a plugin lists: every one is a row in the command
/// palette.
const ACTION_LIMIT: usize = 64;
/// What ThinkTerm adds to an action's call: where the user was.
const AT: &str = "at";

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
    page: Option<RawPanel>,
    #[serde(default, rename = "action")]
    actions: Vec<RawAction>,
    #[serde(default)]
    locales: BTreeMap<String, RawLocale>,
}

/// An `[[action]]`: what it is called, and the one thing it does.
#[derive(Debug, Deserialize)]
struct RawAction {
    id: String,
    title: String,
    #[serde(default)]
    call: Option<toml::Table>,
    #[serde(default)]
    page: Option<toml::Table>,
}

/// A `[panel]`, and a `[page]`, which says the same of itself.
#[derive(Debug, Default, Deserialize)]
struct RawPanel {
    #[serde(default)]
    icon: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawRun {
    #[serde(default)]
    program: Option<String>,
    /// A Python script in the plugin's directory, which the Python found on
    /// this machine runs: in place of `program`.
    #[serde(default)]
    python: Option<String>,
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
    /// Its actions' titles, by id.
    #[serde(default)]
    actions: BTreeMap<String, String>,
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
    /// The page it can be opened as, in a pane's place.
    pub page: Option<Page>,
    /// What it does when asked by name, titled as the manifest itself
    /// titles them: [`Manifest::actions`] has them in a language.
    actions: Vec<Action>,
    /// How long its program runs unused, unless the user chose otherwise.
    pub background: Background,
    /// What it runs on this system. None for a built-in plugin, and for one
    /// that does not run on this system.
    run: Option<Run>,
    /// By language tag, lowercased.
    locales: BTreeMap<String, Locale>,
}

/// What `[run]` says to run on this system.
#[derive(Debug, Clone, PartialEq)]
pub enum Run {
    /// A program, and its arguments.
    Program { program: String, args: Vec<String> },
    /// A Python script in the plugin's directory, and its arguments.
    Python { script: String, args: Vec<String> },
}

/// What starting a plugin's program comes to on this machine.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    /// The program started: for a Python script, the Python found here.
    pub program: PathBuf,
    pub args: Vec<String>,
    /// What it is given in its environment besides what every plugin is.
    pub env: Vec<(String, String)>,
    /// What of the host's environment it is not given.
    pub unset: Vec<String>,
    /// What a change to means that it is started again: its program file,
    /// or a script's directory too.
    pub source: Source,
}

/// What a plugin's program is made of, as far as a change to it means it
/// is started again ([`Source::stamp`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A program file of its own, rebuilt in place.
    Program(PathBuf),
    /// A script run by an interpreter: the interpreter, and every file of
    /// the plugin's directory, which the script may load any of.
    Script { interpreter: PathBuf, dir: PathBuf },
}

/// Why a plugin's program cannot be started now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cannot {
    /// Not yet: the Python it runs on is being looked for.
    Yet,
    /// Not until something changes, and why.
    Because(String),
}

impl Source {
    /// What it is made of, as it is now. A script's directory counts its
    /// Python files alone: what a plugin writes beside them as it runs --
    /// a state file, a log -- is not a change to the plugin.
    pub fn stamp(&self) -> Option<crate::stamp::Stamp> {
        match self {
            Self::Program(program) => crate::stamp::stamp(program),
            Self::Script { interpreter, dir } => {
                let tree = crate::stamp::tree_stamp(dir, Some("py"))?;
                Some(tree.with(crate::stamp::stamp(interpreter)))
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Locale {
    name: Option<String>,
    description: Option<String>,
    actions: BTreeMap<String, String>,
}

/// A plugin's name and description in one language.
pub struct Localized<'a> {
    pub name: &'a str,
    pub description: &'a str,
}

/// The icon a panel that names none, or one no client has, gets.
const PANEL_ICON: &str = thinkterm_plugin_sdk::panel::ICONS[0];

/// The icon a manifest names for a panel or a page, when every client has
/// it.
fn known_icon(icon: Option<String>) -> String {
    icon.filter(|icon| thinkterm_plugin_sdk::panel::ICONS.contains(&icon.as_str()))
        .unwrap_or_else(|| PANEL_ICON.to_string())
}

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
                icon: known_icon(panel.icon),
            }),
        };
        let page = match raw.page {
            None => None,
            Some(_) if builtin => return Err("a built-in plugin has no page".into()),
            Some(_) if raw.api < PAGE_API => {
                return Err(format!("a [page] needs api = {PAGE_API} or later"))
            }
            Some(page) => Some(Page {
                icon: known_icon(page.icon),
            }),
        };
        if !raw.actions.is_empty() && builtin {
            return Err("a built-in plugin's actions are ThinkTerm's own".into());
        }
        if raw.actions.len() > ACTION_LIMIT {
            return Err(format!("it lists more than {ACTION_LIMIT} actions"));
        }
        let mut actions: Vec<Action> = Vec::with_capacity(raw.actions.len());
        for action in raw.actions {
            let id = &action.id;
            if !valid_id(id) {
                return Err(format!("the action id {id:?} is not {}", id_rule()));
            }
            if actions.iter().any(|known| known.id == *id) {
                return Err(format!("it lists the action {id:?} twice"));
            }
            if action.title.trim().is_empty() {
                return Err(format!("the action {id:?} has no title"));
            }
            let does = match (action.call, action.page) {
                (Some(call), None) if call.contains_key(AT) => {
                    return Err(format!(
                        "the action {id:?} calls with {AT:?}, which ThinkTerm fills in"
                    ))
                }
                (Some(call), None) => Does::Call(json(call)),
                (None, Some(_)) if page.is_none() => {
                    return Err(format!("the action {id:?} opens a page, and there is no [page]"))
                }
                (None, Some(args)) => Does::Page(json(args)),
                _ => {
                    return Err(format!(
                        "the action {id:?} needs one of call and page, each a table"
                    ))
                }
            };
            actions.push(Action {
                id: action.id,
                title: action.title,
                does,
            });
        }
        let locales = raw
            .locales
            .into_iter()
            .map(|(tag, locale)| {
                let locale = Locale {
                    name: locale.name.filter(|name| !name.trim().is_empty()),
                    description: locale.description,
                    actions: locale.actions,
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
            page,
            actions,
            background,
            run,
            locales,
        })
    }

    /// Its actions, titled in `locale` where it titles them there.
    pub fn actions(&self, locale: &str) -> Vec<Action> {
        let found = self.locale_for(locale);
        self.actions
            .iter()
            .map(|action| {
                let title = found
                    .and_then(|found| found.actions.get(&action.id))
                    .filter(|title| !title.trim().is_empty())
                    .unwrap_or(&action.title);
                Action {
                    title: title.clone(),
                    ..action.clone()
                }
            })
            .collect()
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

    /// What to start for the plugin in `dir`, which is where it really is,
    /// links followed. A program is the file beside the manifest when there
    /// is one, else, for a bare name, a program on PATH. A Python script is
    /// a file in `dir`, run by the Python [`python::lookup`] found: while that is
    /// being looked for, [`Cannot::Yet`].
    pub fn program(&self, dir: &Path) -> Result<Resolved, Cannot> {
        match &self.run {
            None => Err(Cannot::Because("it runs no program on this system".into())),
            Some(Run::Program { program, args }) => {
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
                let program = found.ok_or_else(|| {
                    Cannot::Because(format!("cannot find its program {program:?}"))
                })?;
                Ok(Resolved {
                    source: Source::Program(program.clone()),
                    program,
                    args: args.clone(),
                    env: Vec::new(),
                    unset: Vec::new(),
                })
            }
            Some(Run::Python { script, args }) => {
                let script = script_in(dir, script).map_err(Cannot::Because)?;
                let python = match python::lookup() {
                    python::Lookup::Found(found) => found,
                    python::Lookup::Missing(why) => return Err(Cannot::Because(why)),
                    python::Lookup::Looking => return Err(Cannot::Yet),
                };
                // The file as it was found: what a link in the directory
                // leads to is not looked up again, and a name that starts
                // with "-" is not taken for an option.
                let mut given = vec![script.display().to_string()];
                given.extend(args.iter().cloned());
                Ok(Resolved {
                    source: Source::Script {
                        interpreter: python.program.clone(),
                        dir: dir.to_path_buf(),
                    },
                    program: python.program,
                    args: given,
                    env: python::ENV
                        .iter()
                        .map(|(name, value)| (name.to_string(), value.to_string()))
                        .collect(),
                    unset: python::UNSET.iter().map(|name| name.to_string()).collect(),
                })
            }
        }
    }
}

/// Where `script`, as a manifest names it, really is: a file in `dir`,
/// which is where the plugin really is. A script is the plugin's own, and
/// runs from there.
fn script_in(dir: &Path, script: &str) -> Result<PathBuf, String> {
    let named = Path::new(script);
    use std::path::Component;
    if named.is_absolute()
        || named.components().any(|part| {
            matches!(
                part,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(format!(
            "its python script {script:?} is not a path within its directory"
        ));
    }
    let path = dir.join(named);
    let real = std::fs::canonicalize(&path).map_err(|_| format!("there is no {script}"))?;
    let home = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    if !real.starts_with(&home) {
        return Err(format!(
            "its python script {script:?} leads out of its directory"
        ));
    }
    if !real.is_file() {
        return Err(format!("{script} is not a file"));
    }
    Ok(real)
}

/// A table of the manifest as the JSON it stands for. A date, which JSON
/// has none of, goes as the text it was written as.
fn json(table: toml::Table) -> serde_json::Value {
    fn one(value: toml::Value) -> serde_json::Value {
        match value {
            toml::Value::String(text) => text.into(),
            toml::Value::Integer(number) => number.into(),
            toml::Value::Float(number) => number.into(),
            toml::Value::Boolean(flag) => flag.into(),
            toml::Value::Datetime(when) => when.to_string().into(),
            toml::Value::Array(items) => items.into_iter().map(one).collect(),
            toml::Value::Table(table) => json(table),
        }
    }
    serde_json::Value::Object(
        table
            .into_iter()
            .map(|(key, item)| (key, one(item)))
            .collect(),
    )
}

fn supports(platforms: &[String]) -> bool {
    platforms.is_empty() || platforms.iter().any(|os| os == std::env::consts::OS)
}

impl RawRun {
    /// What runs on this system, with its arguments: its own section's,
    /// where it has one, over the shared ones. A section naming a program
    /// or a script names what runs, whichever the other says.
    fn for_this_system(mut self) -> Result<Run, String> {
        let own = match std::env::consts::OS {
            "macos" => self.macos.take(),
            "linux" => self.linux.take(),
            "windows" => self.windows.take(),
            _ => None,
        };
        fn given(text: Option<String>) -> Option<String> {
            text.filter(|text| !text.trim().is_empty())
        }
        let names = |run: &RawRun| {
            given(run.program.clone()).is_some() || given(run.python.clone()).is_some()
        };
        for section in std::iter::once(&self).chain(own.as_deref()) {
            if given(section.program.clone()).is_some() && given(section.python.clone()).is_some() {
                return Err("[run] names both a program and a python script".into());
            }
        }
        let (program, python, args) = match own {
            Some(own) if names(&own) => (own.program, own.python, own.args.or(self.args)),
            Some(own) => (self.program, self.python, own.args.or(self.args)),
            None => (self.program, self.python, self.args),
        };
        let args = args.unwrap_or_default();
        match (given(program), given(python)) {
            (Some(program), _) => Ok(Run::Program { program, args }),
            (None, Some(script)) => Ok(Run::Python { script, args }),
            (None, None) => Err(format!(
                "[run] names no program for {}",
                std::env::consts::OS
            )),
        }
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
        let Some(Run::Program { program, args }) = manifest.run.clone() else {
            panic!("it runs a program: {:?}", manifest.run);
        };
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
        assert!(broken("api = 1", &format!("api = {}", API + 1)).contains("newer ThinkTerm"));
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
    fn a_page_needs_the_api_that_draws_one() {
        let with = |api: u32, more: &str| {
            let text = EXAMPLE.replace("api = 1", &format!("api = {api}"));
            Manifest::parse(&format!("{text}\n{more}\n"), false)
        };
        let manifest = with(3, "[page]\nicon = \"list-todo\"").unwrap();
        assert_eq!(manifest.page.unwrap().icon, "list-todo");
        assert!(manifest.panel.is_none(), "a page needs no panel");
        assert_eq!(with(3, "[page]").unwrap().page.unwrap().icon, PANEL_ICON);
        let both = with(3, "[panel]\n[page]").unwrap();
        assert!(both.panel.is_some() && both.page.is_some());
        assert!(with(3, "").unwrap().page.is_none());
        assert!(with(2, "[page]").unwrap_err().contains("needs api = 3"));
        let builtin = EXAMPLE.replace("[run]", "[page]\n[run]");
        assert!(Manifest::parse(&builtin, true).is_err());
    }

    #[test]
    fn actions_are_listed_once_each_with_the_one_thing_they_do() {
        let with = |more: &str| {
            let text = EXAMPLE.replace("api = 1", "api = 3");
            Manifest::parse(&format!("{text}\n{more}\n"), false)
        };
        let manifest = with(
            "[page]\n\
             [[action]]\nid = \"new-card\"\ntitle = \"New card\"\ncall = { op = \"new\", column = 2 }\n\
             [[action]]\nid = \"open\"\ntitle = \"Open the board\"\npage = { board = \"release\" }\n\
             [locales.zh-CN.actions]\nnew-card = \"新卡片\"\n",
        )
        .unwrap();
        let actions = manifest.actions("en-US");
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].title, "New card");
        assert_eq!(
            actions[0].does,
            Does::Call(serde_json::json!({"op": "new", "column": 2}))
        );
        assert_eq!(
            actions[1].does,
            Does::Page(serde_json::json!({"board": "release"}))
        );
        // Titled in a language where it has the title, and as written where not.
        let chinese = manifest.actions("zh-CN");
        assert_eq!(chinese[0].title, "新卡片");
        assert_eq!(chinese[1].title, "Open the board");
        assert!(with("").unwrap().actions("en-US").is_empty());

        let action = |body: &str| with(&format!("[[action]]\nid = \"a\"\ntitle = \"A\"\n{body}"));
        assert!(action("call = {}").is_ok());
        assert!(action("").unwrap_err().contains("one of call and page"));
        assert!(action("call = {}\npage = {}").unwrap_err().contains("one of"));
        assert!(action("page = {}").unwrap_err().contains("no [page]"));
        assert!(action("call = { at = 1 }").unwrap_err().contains("fills in"));
        assert!(action("call = \"new\"").is_err(), "a table, not a word");
        let twice = with("[[action]]\nid = \"a\"\ntitle = \"A\"\ncall = {}\n[[action]]\nid = \"a\"\ntitle = \"B\"\ncall = {}");
        assert!(twice.unwrap_err().contains("twice"));
        let named = |id: &str, title: &str| {
            with(&format!("[[action]]\nid = \"{id}\"\ntitle = \"{title}\"\ncall = {{}}"))
        };
        assert!(named("a.b", "A").unwrap_err().contains("action id"));
        assert!(named("a", " ").unwrap_err().contains("no title"));
        let many: String = (0..=ACTION_LIMIT)
            .map(|n| format!("[[action]]\nid = \"a{n}\"\ntitle = \"A\"\ncall = {{}}\n"))
            .collect();
        assert!(with(&many).unwrap_err().contains("more than"));
        let builtin = EXAMPLE.replace("[run]", "[[action]]\nid = \"a\"\ntitle = \"A\"\ncall = {}\n[run]");
        assert!(Manifest::parse(&builtin, true).is_err());
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
        assert!(matches!(
            manifest.program(dir.path()),
            Err(Cannot::Because(why)) if why.contains("cannot find")
        ));
        let name = if cfg!(windows) { "tool.exe" } else { "tool" };
        std::fs::write(dir.path().join(name), "").unwrap();
        let found = manifest.program(dir.path()).unwrap();
        assert_eq!(found.program, dir.path().join(name));
        assert_eq!(found.source, Source::Program(dir.path().join(name)));
        assert!(found.env.is_empty());

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
        let found = manifest.program(dir.path()).unwrap().program;
        assert!(found.is_absolute(), "{found:?} is from PATH");
    }

    const SCRIPT: &str = r#"
        id = "hello"
        name = "Hello"
        version = "0.1.0"
        api = 3

        [run]
        python = "main.py"
        args = ["--quiet"]
    "#;

    #[test]
    fn a_python_script_runs_in_place_of_a_program() {
        let manifest = Manifest::parse(SCRIPT, false).unwrap();
        assert_eq!(
            manifest.run,
            Some(Run::Python {
                script: "main.py".into(),
                args: vec!["--quiet".into()]
            })
        );
        // A section of this system's naming a program runs that instead,
        // with the shared arguments.
        let own = match std::env::consts::OS {
            "macos" => "macos",
            "windows" => "windows",
            _ => "linux",
        };
        let text = format!("{SCRIPT}\n[run.{own}]\nprogram = \"tool\"\n");
        let manifest = Manifest::parse(&text, false).unwrap();
        assert_eq!(
            manifest.run,
            Some(Run::Program {
                program: "tool".into(),
                args: vec!["--quiet".into()]
            })
        );
        // One that names only arguments keeps the shared script.
        let text = format!("{SCRIPT}\n[run.{own}]\nargs = [\"-v\"]\n");
        let manifest = Manifest::parse(&text, false).unwrap();
        assert_eq!(
            manifest.run,
            Some(Run::Python {
                script: "main.py".into(),
                args: vec!["-v".into()]
            })
        );
    }

    #[test]
    fn a_section_names_a_program_or_a_script_not_both() {
        let text = SCRIPT.replace(
            "python = \"main.py\"",
            "python = \"main.py\"\nprogram = \"x\"",
        );
        assert!(Manifest::parse(&text, false)
            .unwrap_err()
            .contains("both a program and a python script"));
        let text = SCRIPT.replace("python = \"main.py\"", "python = \"  \"");
        assert!(Manifest::parse(&text, false)
            .unwrap_err()
            .contains("names no program"));
    }

    #[test]
    fn a_python_script_is_a_file_within_the_plugins_directory() {
        let dir = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(dir.path()).unwrap();
        assert!(script_in(&home, "main.py")
            .unwrap_err()
            .contains("there is no main.py"));
        std::fs::write(home.join("main.py"), "").unwrap();
        assert_eq!(script_in(&home, "main.py"), Ok(home.join("main.py")));
        std::fs::create_dir(home.join("src")).unwrap();
        std::fs::write(home.join("src").join("run.py"), "").unwrap();
        assert_eq!(
            script_in(&home, "src/run.py"),
            Ok(home.join("src").join("run.py"))
        );
        assert_eq!(script_in(&home, "./main.py"), Ok(home.join("main.py")));
        for outside in ["../main.py", "/etc/passwd", "src/../main.py"] {
            assert!(
                script_in(&home, outside)
                    .unwrap_err()
                    .contains("not a path within"),
                "{outside}"
            );
        }
        assert!(script_in(&home, "src").unwrap_err().contains("not a file"));
        #[cfg(unix)]
        {
            let elsewhere = tempfile::tempdir().unwrap();
            std::fs::write(elsewhere.path().join("x.py"), "").unwrap();
            std::os::unix::fs::symlink(elsewhere.path().join("x.py"), home.join("link.py"))
                .unwrap();
            assert!(script_in(&home, "link.py")
                .unwrap_err()
                .contains("leads out"));
        }
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
