//! The icon heading each pane tab: a coloured circle carrying a one-colour
//! glyph, chosen by what the pane runs. The cards, what a user's settings
//! make of them, and which card a pane gets -- shared by the desktop, which
//! draws and edits them (`wezterm-gui/src/tab_icons.rs`), and the browser
//! client, which draws the cards its server sends (`WireCatalog`).
//!
//! The facts come from the mux that owns the pane -- the agent it runs and
//! the program leading its terminal -- so a remote pane is dressed exactly
//! like a local one. What a program looks like is decided here, by cards:
//! built-in ones the user may restyle, and cards of their own.
//!
//! A card's glyph is only ever a shape. Built-in marks and imported SVGs are
//! drawn in the card's glyph colour, so every icon in a row belongs to the
//! same family whatever it was drawn in.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use thinkterm_proto::ForegroundProgram;

/// How many cards of their own a user may keep. Each can carry an SVG, and
/// every one of them may be rasterized into the glyph atlas.
pub const MAX_CUSTOM_CARDS: usize = 64;
/// The card that dresses shells and anything no other card claims. It is
/// fixed: the settings page leaves it out and a saved change to it is
/// ignored. Any other card may still claim a shell's name.
pub const TERMINAL_CARD: &str = "terminal";

/// A glyph's shape, from the tree or from an SVG the user imported.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum GlyphKey {
    Builtin(BuiltinGlyph),
    /// Hex SHA-256 of the SVG's bytes, which is also its file name.
    Svg(Arc<str>),
}

/// The marks the built-in cards use. Brand marks come from simple-icons and
/// lobe-icons, generic ones from lucide; all are single-colour shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltinGlyph {
    Activity,
    Ansible,
    Bot,
    Bun,
    Claude,
    Copilot,
    Cursor,
    Deno,
    Docker,
    Emacs,
    FileText,
    FolderOpen,
    Gemini,
    Git,
    GitHub,
    Go,
    Hammer,
    Java,
    Kimi,
    Kubernetes,
    Lua,
    MongoDb,
    MySql,
    Neovim,
    Node,
    Ollama,
    OpenAi,
    OpenCode,
    Package,
    Php,
    Pi,
    PostgreSql,
    Python,
    Redis,
    Ruby,
    Rust,
    Server,
    Sqlite,
    Swift,
    Terminal,
    Terraform,
    Tmux,
    Vim,
    Zig,
}

impl BuiltinGlyph {
    /// The SVG, as the file in the tree has it.
    pub fn svg(self) -> &'static str {
        std::str::from_utf8(self.bytes()).unwrap_or_default()
    }

    pub fn bytes(self) -> &'static [u8] {
        macro_rules! lucide {
            ($name:literal) => {
                include_bytes!(concat!("../../third_party/lucide/icons/", $name, ".svg"))
            };
        }
        macro_rules! simple {
            ($name:literal) => {
                include_bytes!(concat!(
                    "../../third_party/simple-icons/icons/",
                    $name,
                    ".svg"
                ))
            };
        }
        macro_rules! lobe {
            ($name:literal) => {
                include_bytes!(concat!("../../third_party/lobe-icons/", $name, ".svg"))
            };
        }
        match self {
            Self::Activity => lucide!("activity"),
            Self::Ansible => simple!("ansible"),
            Self::Bot => lucide!("bot"),
            Self::Bun => simple!("bun"),
            Self::Claude => simple!("claude"),
            Self::Copilot => simple!("githubcopilot"),
            Self::Cursor => lobe!("cursor"),
            Self::Deno => simple!("deno"),
            Self::Docker => simple!("docker"),
            Self::Emacs => simple!("gnuemacs"),
            Self::FileText => lucide!("file-text"),
            Self::FolderOpen => lucide!("folder-open"),
            Self::Gemini => simple!("googlegemini"),
            Self::Git => simple!("git"),
            Self::GitHub => simple!("github"),
            Self::Go => simple!("go"),
            Self::Hammer => lucide!("hammer"),
            Self::Java => simple!("openjdk"),
            Self::Kimi => lobe!("kimi"),
            Self::Kubernetes => simple!("kubernetes"),
            Self::Lua => simple!("lua"),
            Self::MongoDb => simple!("mongodb"),
            Self::MySql => simple!("mysql"),
            Self::Neovim => simple!("neovim"),
            Self::Node => simple!("nodedotjs"),
            Self::Ollama => simple!("ollama"),
            Self::OpenAi => lobe!("openai"),
            Self::OpenCode => lobe!("opencode"),
            Self::Package => lucide!("package"),
            Self::Php => simple!("php"),
            Self::Pi => lobe!("pi"),
            Self::PostgreSql => simple!("postgresql"),
            Self::Python => simple!("python"),
            Self::Redis => simple!("redis"),
            Self::Ruby => simple!("ruby"),
            Self::Rust => simple!("rust"),
            Self::Server => lucide!("server"),
            Self::Sqlite => simple!("sqlite"),
            Self::Swift => simple!("swift"),
            Self::Terminal => lucide!("terminal"),
            Self::Terraform => simple!("terraform"),
            Self::Tmux => simple!("tmux"),
            Self::Vim => simple!("vim"),
            Self::Zig => simple!("zig"),
        }
    }
}

/// An sRGB colour as the settings file spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub fn parse(text: &str) -> Option<Self> {
        let hex = text.trim().strip_prefix('#')?;
        if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
        Some(Self(channel(0)?, channel(2)?, channel(4)?))
    }

    pub fn to_hex(self) -> String {
        format!("#{:02X}{:02X}{:02X}", self.0, self.1, self.2)
    }

    /// Perceived brightness, 0..=255.
    fn luma(self) -> f32 {
        0.2126 * self.0 as f32 + 0.7152 * self.1 as f32 + 0.0722 * self.2 as f32
    }

    /// The glyph colour that reads on this circle: white on dark, near
    /// black on light.
    pub fn legible_glyph(self) -> Self {
        if self.luma() > 165.0 {
            Self(0x11, 0x11, 0x11)
        } else {
            Self(0xFF, 0xFF, 0xFF)
        }
    }
}

pub const WHITE: Rgb = Rgb(0xFF, 0xFF, 0xFF);
pub const INK: Rgb = Rgb(0x11, 0x11, 0x11);
/// Brands whose own colour is black sit on a light circle instead: a black
/// circle would sink into the dark chrome.
pub const PAPER: Rgb = Rgb(0xF2, 0xF2, 0xF2);

pub struct BuiltinCard {
    pub id: &'static str,
    /// A brand's own name, or an i18n key (`tab-icons-card-…`) for the
    /// generic cards, which are translated.
    pub name: &'static str,
    pub glyph: BuiltinGlyph,
    pub circle: Rgb,
    pub glyph_color: Rgb,
    /// Agents this card dresses, by agent id. `*` takes every agent no
    /// other card names.
    pub agents: &'static [&'static str],
    pub programs: &'static [&'static str],
}

/// The cards every user starts with, in the order the settings page shows
/// them. Agents first: they are what this product is about. Program names
/// are matched lower-case against what the pane runs; see `candidates`.
/// Brands wear their own colours; the generic cards wear quiet greys, a
/// little warm or cool so neighbours still differ, and the terminal the
/// darkest, as the one seen most.
pub const BUILTIN_CARDS: &[BuiltinCard] = &[
    BuiltinCard {
        id: TERMINAL_CARD,
        name: "tab-icons-card-terminal",
        glyph: BuiltinGlyph::Terminal,
        circle: Rgb(0x26, 0x26, 0x2B),
        glyph_color: WHITE,
        agents: &[],
        programs: &[
            "zsh",
            "bash",
            "fish",
            "sh",
            "dash",
            "ksh",
            "mksh",
            "tcsh",
            "csh",
            "nu",
            "pwsh",
            "powershell",
            "cmd",
            "login",
        ],
    },
    BuiltinCard {
        id: "claude",
        name: "Claude",
        glyph: BuiltinGlyph::Claude,
        circle: Rgb(0xD9, 0x77, 0x57),
        glyph_color: WHITE,
        agents: &["claude"],
        programs: &["claude"],
    },
    BuiltinCard {
        id: "codex",
        name: "Codex",
        glyph: BuiltinGlyph::OpenAi,
        circle: PAPER,
        glyph_color: INK,
        agents: &["codex"],
        programs: &["codex"],
    },
    BuiltinCard {
        id: "copilot",
        name: "Copilot",
        glyph: BuiltinGlyph::Copilot,
        circle: PAPER,
        glyph_color: INK,
        agents: &["copilot"],
        programs: &["copilot"],
    },
    BuiltinCard {
        id: "cursor",
        name: "Cursor",
        glyph: BuiltinGlyph::Cursor,
        circle: PAPER,
        glyph_color: INK,
        agents: &["cursor"],
        programs: &["cursor-agent"],
    },
    BuiltinCard {
        id: "gemini",
        name: "Gemini",
        glyph: BuiltinGlyph::Gemini,
        circle: Rgb(0x8E, 0x75, 0xB2),
        glyph_color: WHITE,
        agents: &["gemini"],
        programs: &["gemini"],
    },
    BuiltinCard {
        id: "kimi",
        name: "Kimi",
        glyph: BuiltinGlyph::Kimi,
        circle: Rgb(0x17, 0x83, 0xFF),
        glyph_color: WHITE,
        agents: &["kimi"],
        programs: &["kimi"],
    },
    BuiltinCard {
        id: "opencode",
        name: "OpenCode",
        glyph: BuiltinGlyph::OpenCode,
        circle: PAPER,
        glyph_color: INK,
        agents: &["opencode"],
        programs: &["opencode"],
    },
    BuiltinCard {
        id: "pi",
        name: "Pi",
        glyph: BuiltinGlyph::Pi,
        circle: PAPER,
        glyph_color: INK,
        agents: &["pi"],
        programs: &["pi"],
    },
    BuiltinCard {
        id: "agent",
        name: "tab-icons-card-agent",
        glyph: BuiltinGlyph::Bot,
        circle: Rgb(0x3F, 0x3F, 0x46),
        glyph_color: WHITE,
        agents: &["*"],
        programs: &[],
    },
    BuiltinCard {
        id: "python",
        name: "Python",
        glyph: BuiltinGlyph::Python,
        circle: Rgb(0x37, 0x76, 0xAB),
        glyph_color: WHITE,
        agents: &[],
        programs: &[
            "python", "ipython", "bpython", "pip", "uv", "uvx", "poetry", "pdm", "pytest",
            "jupyter",
        ],
    },
    BuiltinCard {
        id: "node",
        name: "Node.js",
        glyph: BuiltinGlyph::Node,
        circle: Rgb(0x5F, 0xA0, 0x4E),
        glyph_color: WHITE,
        agents: &[],
        programs: &[
            "node", "nodejs", "npm", "npx", "pnpm", "yarn", "tsx", "ts-node",
        ],
    },
    BuiltinCard {
        id: "deno",
        name: "Deno",
        glyph: BuiltinGlyph::Deno,
        circle: PAPER,
        glyph_color: INK,
        agents: &[],
        programs: &["deno"],
    },
    BuiltinCard {
        id: "bun",
        name: "Bun",
        glyph: BuiltinGlyph::Bun,
        circle: PAPER,
        glyph_color: INK,
        agents: &[],
        programs: &["bun", "bunx"],
    },
    BuiltinCard {
        id: "ruby",
        name: "Ruby",
        glyph: BuiltinGlyph::Ruby,
        circle: Rgb(0xCC, 0x34, 0x2D),
        glyph_color: WHITE,
        agents: &[],
        programs: &["ruby", "irb", "bundle", "rails", "rake"],
    },
    BuiltinCard {
        id: "rust",
        name: "Rust",
        glyph: BuiltinGlyph::Rust,
        circle: PAPER,
        glyph_color: INK,
        agents: &[],
        programs: &["cargo", "rustc", "rustup"],
    },
    BuiltinCard {
        id: "go",
        name: "Go",
        glyph: BuiltinGlyph::Go,
        circle: Rgb(0x00, 0xAD, 0xD8),
        glyph_color: WHITE,
        agents: &[],
        programs: &["go"],
    },
    BuiltinCard {
        id: "java",
        name: "Java",
        glyph: BuiltinGlyph::Java,
        circle: PAPER,
        glyph_color: INK,
        agents: &[],
        programs: &["java", "jshell", "gradle", "mvn"],
    },
    BuiltinCard {
        id: "php",
        name: "PHP",
        glyph: BuiltinGlyph::Php,
        circle: Rgb(0x77, 0x7B, 0xB4),
        glyph_color: WHITE,
        agents: &[],
        programs: &["php", "composer"],
    },
    BuiltinCard {
        id: "lua",
        name: "Lua",
        glyph: BuiltinGlyph::Lua,
        circle: Rgb(0x00, 0x00, 0x80),
        glyph_color: WHITE,
        agents: &[],
        programs: &["lua", "luajit"],
    },
    BuiltinCard {
        id: "swift",
        name: "Swift",
        glyph: BuiltinGlyph::Swift,
        circle: Rgb(0xF0, 0x51, 0x38),
        glyph_color: WHITE,
        agents: &[],
        programs: &["swift"],
    },
    BuiltinCard {
        id: "zig",
        name: "Zig",
        glyph: BuiltinGlyph::Zig,
        circle: Rgb(0xF7, 0xA4, 0x1D),
        glyph_color: INK,
        agents: &[],
        programs: &["zig"],
    },
    BuiltinCard {
        id: "vim",
        name: "Vim",
        glyph: BuiltinGlyph::Vim,
        circle: Rgb(0x01, 0x97, 0x33),
        glyph_color: WHITE,
        agents: &[],
        programs: &["vim", "vi", "view", "vimdiff"],
    },
    BuiltinCard {
        id: "neovim",
        name: "Neovim",
        glyph: BuiltinGlyph::Neovim,
        circle: Rgb(0x57, 0xA1, 0x43),
        glyph_color: WHITE,
        agents: &[],
        programs: &["nvim"],
    },
    BuiltinCard {
        id: "emacs",
        name: "Emacs",
        glyph: BuiltinGlyph::Emacs,
        circle: Rgb(0x7F, 0x5A, 0xB6),
        glyph_color: WHITE,
        agents: &[],
        programs: &["emacs", "emacsclient"],
    },
    BuiltinCard {
        id: "git",
        name: "Git",
        glyph: BuiltinGlyph::Git,
        circle: Rgb(0xF0, 0x3C, 0x2E),
        glyph_color: WHITE,
        agents: &[],
        programs: &["git", "lazygit", "tig", "gitui"],
    },
    BuiltinCard {
        id: "github",
        name: "GitHub CLI",
        glyph: BuiltinGlyph::GitHub,
        circle: PAPER,
        glyph_color: INK,
        agents: &[],
        programs: &["gh"],
    },
    BuiltinCard {
        id: "docker",
        name: "Docker",
        glyph: BuiltinGlyph::Docker,
        circle: Rgb(0x24, 0x96, 0xED),
        glyph_color: WHITE,
        agents: &[],
        programs: &["docker", "docker-compose", "lazydocker", "podman"],
    },
    BuiltinCard {
        id: "kubernetes",
        name: "Kubernetes",
        glyph: BuiltinGlyph::Kubernetes,
        circle: Rgb(0x32, 0x6C, 0xE5),
        glyph_color: WHITE,
        agents: &[],
        programs: &["kubectl", "k9s", "helm", "minikube", "kind"],
    },
    BuiltinCard {
        id: "terraform",
        name: "Terraform",
        glyph: BuiltinGlyph::Terraform,
        circle: Rgb(0x84, 0x4F, 0xBA),
        glyph_color: WHITE,
        agents: &[],
        programs: &["terraform", "tofu"],
    },
    BuiltinCard {
        id: "ansible",
        name: "Ansible",
        glyph: BuiltinGlyph::Ansible,
        circle: Rgb(0xEE, 0x00, 0x00),
        glyph_color: WHITE,
        agents: &[],
        programs: &["ansible", "ansible-playbook"],
    },
    BuiltinCard {
        id: "ssh",
        name: "SSH",
        glyph: BuiltinGlyph::Server,
        circle: Rgb(0x33, 0x41, 0x55),
        glyph_color: WHITE,
        agents: &[],
        // mosh execs mosh-client once connected.
        programs: &["ssh", "mosh", "mosh-client", "autossh", "et", "telnet"],
    },
    BuiltinCard {
        id: "tmux",
        name: "tmux",
        glyph: BuiltinGlyph::Tmux,
        circle: Rgb(0x1B, 0xB9, 0x1F),
        glyph_color: WHITE,
        agents: &[],
        programs: &["tmux", "screen", "zellij"],
    },
    BuiltinCard {
        id: "monitor",
        name: "tab-icons-card-monitor",
        glyph: BuiltinGlyph::Activity,
        circle: Rgb(0x3A, 0x3A, 0x40),
        glyph_color: WHITE,
        agents: &[],
        programs: &[
            "btop", "htop", "top", "atop", "glances", "btm", "bottom", "nvtop", "gtop",
        ],
    },
    BuiltinCard {
        id: "postgresql",
        name: "PostgreSQL",
        glyph: BuiltinGlyph::PostgreSql,
        circle: Rgb(0x41, 0x69, 0xE1),
        glyph_color: WHITE,
        agents: &[],
        programs: &["psql", "pgcli", "postgres"],
    },
    BuiltinCard {
        id: "mysql",
        name: "MySQL",
        glyph: BuiltinGlyph::MySql,
        circle: Rgb(0x44, 0x79, 0xA1),
        glyph_color: WHITE,
        agents: &[],
        programs: &["mysql", "mycli", "mariadb"],
    },
    BuiltinCard {
        id: "redis",
        name: "Redis",
        glyph: BuiltinGlyph::Redis,
        circle: Rgb(0xFF, 0x44, 0x38),
        glyph_color: WHITE,
        agents: &[],
        programs: &["redis-cli", "redis-server", "valkey-cli"],
    },
    BuiltinCard {
        id: "sqlite",
        name: "SQLite",
        glyph: BuiltinGlyph::Sqlite,
        circle: Rgb(0x00, 0x3B, 0x57),
        glyph_color: WHITE,
        agents: &[],
        programs: &["sqlite3", "litecli"],
    },
    BuiltinCard {
        id: "mongodb",
        name: "MongoDB",
        glyph: BuiltinGlyph::MongoDb,
        circle: Rgb(0x47, 0xA2, 0x48),
        glyph_color: WHITE,
        agents: &[],
        programs: &["mongosh", "mongo", "mongod"],
    },
    BuiltinCard {
        id: "ollama",
        name: "Ollama",
        glyph: BuiltinGlyph::Ollama,
        circle: PAPER,
        glyph_color: INK,
        agents: &[],
        programs: &["ollama"],
    },
    BuiltinCard {
        id: "pager",
        name: "tab-icons-card-pager",
        glyph: BuiltinGlyph::FileText,
        circle: Rgb(0x40, 0x40, 0x40),
        glyph_color: WHITE,
        agents: &[],
        programs: &["less", "more", "man", "bat", "most", "glow"],
    },
    BuiltinCard {
        id: "files",
        name: "tab-icons-card-files",
        glyph: BuiltinGlyph::FolderOpen,
        circle: Rgb(0x44, 0x40, 0x3C),
        glyph_color: WHITE,
        agents: &[],
        programs: &["ranger", "lf", "yazi", "nnn", "mc", "broot", "vifm"],
    },
    BuiltinCard {
        id: "build",
        name: "tab-icons-card-build",
        glyph: BuiltinGlyph::Hammer,
        circle: Rgb(0x37, 0x41, 0x51),
        glyph_color: WHITE,
        agents: &[],
        programs: &["make", "gmake", "cmake", "ninja", "just", "bazel"],
    },
];

/// Circles offered for a new card, in turn, so two new cards made in a row
/// are told apart before the user gets to colours.
pub const NEW_CARD_CIRCLES: &[Rgb] = &[
    Rgb(0xEC, 0x48, 0x99),
    Rgb(0x8B, 0x5C, 0xF6),
    Rgb(0x14, 0xB8, 0xA6),
    Rgb(0xF9, 0x73, 0x16),
    Rgb(0x0E, 0xA5, 0xE9),
    Rgb(0x84, 0xCC, 0x16),
];

/// Circle colours offered on the settings page, besides typing one.
pub const CIRCLE_PRESETS: &[Rgb] = &[
    Rgb(0x3A, 0x3A, 0x40),
    PAPER,
    Rgb(0xEF, 0x44, 0x44),
    Rgb(0xF9, 0x73, 0x16),
    Rgb(0xEA, 0xB3, 0x08),
    Rgb(0x22, 0xC5, 0x5E),
    Rgb(0x14, 0xB8, 0xA6),
    Rgb(0x3B, 0x82, 0xF6),
    Rgb(0x8B, 0x5C, 0xF6),
    Rgb(0xEC, 0x48, 0x99),
];

/// Glyph colours offered the same way.
pub const GLYPH_PRESETS: &[Rgb] = &[
    WHITE,
    INK,
    Rgb(0x4A, 0xDE, 0x80),
    Rgb(0xFA, 0xCC, 0x15),
    Rgb(0x60, 0xA5, 0xFA),
    Rgb(0xF4, 0x72, 0xB6),
];

/// One card as it stands after the user's changes.
#[derive(Debug, Clone)]
pub struct Card {
    pub id: String,
    pub name: String,
    /// Built in (may be reset, never deleted) or the user's own.
    pub builtin: bool,
    /// A built-in card the user changed, so "reset" means something.
    pub changed: bool,
    pub glyph: GlyphKey,
    pub circle: Rgb,
    pub glyph_color: Rgb,
    pub programs: Vec<String>,
    /// Agents it dresses, by agent id; `*` takes every agent no other card
    /// names.
    pub agents: Vec<String>,
}

/// What a tab's icon is drawn with.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedIcon {
    pub glyph: GlyphKey,
    pub circle: Rgb,
    pub glyph_color: Rgb,
}

impl Card {
    pub fn resolved(&self) -> ResolvedIcon {
        ResolvedIcon {
            glyph: self.glyph.clone(),
            circle: self.circle,
            glyph_color: self.glyph_color,
        }
    }

    /// Whether the settings page's search for `query` (already trimmed and
    /// lower case) finds this card: by its name or by a program it claims.
    pub fn matches(&self, query: &str) -> bool {
        query.is_empty()
            || self.name.to_lowercase().contains(query)
            || self
                .programs
                .iter()
                .any(|program| program.to_lowercase().contains(query))
    }
}

/// Every card, and the tables that find one for a pane.
pub struct Catalog {
    pub enabled: bool,
    pub cards: Vec<Card>,
    matcher: Matcher,
}

impl Catalog {
    /// The built-in cards with the user's changes, then the user's own.
    /// `tr` translates the generic cards' names, which are catalogue keys.
    pub fn build(settings: &TabIconSettings, tr: impl Fn(&str) -> String) -> Self {
        let overrides: HashMap<&str, &TabIconCard> = settings
            .cards
            .iter()
            .map(|card| (card.id.as_str(), card))
            .collect();
        let mut cards = Vec::with_capacity(BUILTIN_CARDS.len() + settings.cards.len());
        for builtin in BUILTIN_CARDS {
            let over = overrides
                .get(builtin.id)
                .copied()
                .filter(|_| builtin.id != TERMINAL_CARD);
            let name = if builtin.name.starts_with("tab-icons-card-") {
                tr(builtin.name)
            } else {
                builtin.name.to_string()
            };
            let glyph = over
                .and_then(|over| svg_key(over.svg.as_deref()))
                .unwrap_or(GlyphKey::Builtin(builtin.glyph));
            let circle = over
                .and_then(|over| over.circle.as_deref().and_then(Rgb::parse))
                .unwrap_or(builtin.circle);
            let glyph_color = over
                .and_then(|over| over.glyph.as_deref().and_then(Rgb::parse))
                .unwrap_or(builtin.glyph_color);
            let programs: Vec<String> = over
                .and_then(|over| over.programs.clone())
                .unwrap_or_else(|| builtin.programs.iter().map(|p| p.to_string()).collect());
            // Changed is judged by what the card looks like, not by whether
            // settings mention it: a colour picked that happens to be the
            // default (or became it) leaves nothing to reset.
            let changed = glyph != GlyphKey::Builtin(builtin.glyph)
                || circle != builtin.circle
                || glyph_color != builtin.glyph_color
                || programs
                    .iter()
                    .map(String::as_str)
                    .ne(builtin.programs.iter().copied());
            cards.push(Card {
                id: builtin.id.to_string(),
                name,
                builtin: true,
                changed,
                glyph,
                circle,
                glyph_color,
                programs,
                agents: builtin.agents.iter().map(|agent| agent.to_string()).collect(),
            });
        }
        for custom in settings
            .cards
            .iter()
            .filter(|card| is_custom_id(&card.id))
            .take(MAX_CUSTOM_CARDS)
        {
            let circle = custom
                .circle
                .as_deref()
                .and_then(Rgb::parse)
                .unwrap_or(NEW_CARD_CIRCLES[0]);
            cards.push(Card {
                id: custom.id.clone(),
                name: custom
                    .name
                    .clone()
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| tr("tab-icons-card-untitled")),
                builtin: false,
                changed: true,
                glyph: svg_key(custom.svg.as_deref())
                    .unwrap_or(GlyphKey::Builtin(BuiltinGlyph::Package)),
                circle,
                glyph_color: custom
                    .glyph
                    .as_deref()
                    .and_then(Rgb::parse)
                    .unwrap_or_else(|| circle.legible_glyph()),
                programs: custom.programs.clone().unwrap_or_default(),
                agents: vec![],
            });
        }

        let matcher = Matcher::new(
            cards
                .iter()
                .map(|card| (card.id.as_str(), &card.programs[..], &card.agents[..])),
        );
        Self {
            enabled: settings.enabled.unwrap_or(true),
            cards,
            matcher,
        }
    }

    pub fn card(&self, id: &str) -> Option<&Card> {
        self.cards.iter().find(|card| card.id == id)
    }

    /// The card for a tab whose pane runs the agent `agent` (its id, while
    /// it runs) and is led by `program`: the agent's card, else the
    /// program's, else the terminal's.
    pub fn card_for(&self, agent: Option<&str>, program: Option<&ForegroundProgram>) -> &Card {
        &self.cards[self.matcher.find(agent, program)]
    }

    /// The card that dresses shells and whatever no other card claims.
    pub fn terminal_card(&self) -> &Card {
        &self.cards[self.matcher.terminal]
    }

    /// The cards as a page receives them: what each looks like, with its
    /// glyph's SVG, and what it is matched by. `svg` reads an imported
    /// SVG by its hash; a card whose SVG cannot be read is sent with none,
    /// and drawn as the desktop draws it: its circle, without a glyph.
    pub fn wire(&self, svg: impl Fn(&str) -> Option<String>) -> WireCatalog {
        let cards = self
            .cards
            .iter()
            .map(|card| WireCard {
                id: card.id.clone(),
                circle: card.circle.to_hex(),
                glyph: card.glyph_color.to_hex(),
                svg: match &card.glyph {
                    GlyphKey::Builtin(glyph) => glyph.svg().to_string(),
                    GlyphKey::Svg(hash) => svg(hash).unwrap_or_default(),
                },
                programs: card.programs.clone(),
                agents: card.agents.clone(),
            })
            .collect();
        WireCatalog {
            enabled: self.enabled,
            terminal: self.terminal_card().id.clone(),
            cards,
        }
    }
}

/// The tables that find a card for a pane, by index into a list of cards.
#[derive(Debug, Clone, Default)]
pub struct Matcher {
    by_program: HashMap<String, usize>,
    by_agent: HashMap<String, usize>,
    any_agent: Option<usize>,
    terminal: usize,
}

impl Matcher {
    /// From `(id, programs, agents)` per card, in order. Built-ins come
    /// first, then the user's own: a name the user gave one of their cards
    /// wins even where a built-in still lists it.
    pub fn new<'a>(cards: impl IntoIterator<Item = (&'a str, &'a [String], &'a [String])>) -> Self {
        let mut matcher = Self::default();
        let mut terminal = None;
        for (index, (id, programs, agents)) in cards.into_iter().enumerate() {
            if id == TERMINAL_CARD {
                terminal.get_or_insert(index);
            }
            for program in programs {
                if let Some(name) = normalize_program_name(program) {
                    matcher.by_program.insert(name, index);
                }
            }
            for agent in agents {
                if agent == "*" {
                    matcher.any_agent.get_or_insert(index);
                } else {
                    matcher.by_agent.entry(agent.clone()).or_insert(index);
                }
            }
        }
        matcher.terminal = terminal.unwrap_or(0);
        matcher
    }

    /// The index of the card for a pane running `agent` and led by
    /// `program`; see `Catalog::card_for`.
    pub fn find(&self, agent: Option<&str>, program: Option<&ForegroundProgram>) -> usize {
        if let Some(agent) = agent {
            if let Some(index) = self.by_agent.get(agent).copied().or(self.any_agent) {
                return index;
            }
        }
        program
            .and_then(|program| {
                candidates(program)
                    .into_iter()
                    .find_map(|name| self.by_program.get(&name).copied())
            })
            .unwrap_or(self.terminal)
    }

    /// The terminal card's index.
    pub fn terminal(&self) -> usize {
        self.terminal
    }
}

/// One card as a page draws it; see `Catalog::wire`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireCard {
    pub id: String,
    /// `#RRGGBB`.
    pub circle: String,
    /// `#RRGGBB`.
    pub glyph: String,
    /// The glyph's shape, drawn in `glyph` over `circle`.
    pub svg: String,
    pub programs: Vec<String>,
    pub agents: Vec<String>,
}

/// Every card a page needs, and which one is the terminal's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireCatalog {
    /// The machine's own switch; a page has its own as well.
    pub enabled: bool,
    pub terminal: String,
    pub cards: Vec<WireCard>,
}

impl WireCatalog {
    /// The tables for matching panes against these cards.
    pub fn matcher(&self) -> Matcher {
        Matcher::new(
            self.cards
                .iter()
                .map(|card| (card.id.as_str(), &card.programs[..], &card.agents[..])),
        )
    }
}

/// Names a program answers to, most specific first: the script or command
/// an interpreter or launcher runs before the executable itself, and each
/// of those also without an extension (`deploy.sh` → `deploy`), past a
/// dotted suffix (`vim.basic` → `vim`) and without a version (`python3.12`
/// → `python3` → `python`).
fn candidates(program: &ForegroundProgram) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut push = |name: &str| {
        if !name.is_empty() && !names.iter().any(|existing| existing == name) {
            names.push(name.to_string());
        }
    };
    for raw in program
        .runs
        .iter()
        .chain(std::iter::once(&program.executable))
    {
        let Some(name) = normalize_program_name(raw) else {
            continue;
        };
        push(&name);
        let stem = strip_extension(&name);
        push(stem);
        let base = stem.split('.').next().unwrap_or(stem);
        push(base);
        push(base.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.' || c == '-'));
    }
    names
}

/// Extensions a program's file may carry that say nothing about which
/// program it is.
const PROGRAM_EXTENSIONS: &[&str] = &[
    ".exe", ".cmd", ".bat", ".ps1", ".js", ".mjs", ".cjs", ".ts", ".py", ".rb", ".pl", ".php",
    ".lua", ".sh", ".bash", ".zsh", ".fish",
];

fn strip_extension(name: &str) -> &str {
    PROGRAM_EXTENSIONS
        .iter()
        .find_map(|ext| name.strip_suffix(ext).filter(|stem| !stem.is_empty()))
        .unwrap_or(name)
}

/// How a program name is written into, and looked up in, the tables:
/// trimmed and lower case. `None` for nothing, or for a path.
pub fn normalize_program_name(name: &str) -> Option<String> {
    let name = name.trim().to_lowercase();
    (!name.is_empty() && !name.contains(['/', '\\'])).then_some(name)
}

pub fn is_custom_id(id: &str) -> bool {
    id.strip_prefix("custom-")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// An imported SVG's id is its hash and its file name, so a hand-edited
/// settings file must not be able to name any other path.
pub fn is_svg_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

pub fn svg_key(id: Option<&str>) -> Option<GlyphKey> {
    id.filter(|id| is_svg_id(id))
        .map(|id| GlyphKey::Svg(Arc::from(id)))
}

/// The tab icon settings as the desktop's settings file keeps them.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct TabIconSettings {
    /// Absent means on.
    pub enabled: Option<bool>,
    /// Built-in cards the user changed, and the user's own cards, in the
    /// order they are shown.
    pub cards: Vec<TabIconCard>,
}

/// One card's settings. On a built-in card every field left `None` keeps
/// the built-in value, so a future default still reaches whoever never
/// touched it.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct TabIconCard {
    /// A built-in card's id (`python`), or `custom-<n>` for the user's own.
    pub id: String,
    /// The user's own cards only; a built-in keeps its own name.
    pub name: Option<String>,
    /// Replaces the card's program list.
    pub programs: Option<Vec<String>>,
    /// An imported SVG, by the hex SHA-256 of its bytes, replacing the glyph.
    pub svg: Option<String>,
    /// `#RRGGBB`.
    pub circle: Option<String>,
    /// `#RRGGBB`.
    pub glyph: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program(executable: &str, runs: Option<&str>) -> ForegroundProgram {
        ForegroundProgram {
            executable: executable.to_string(),
            runs: runs.map(str::to_string),
        }
    }

    fn build(settings: &TabIconSettings) -> Catalog {
        Catalog::build(settings, |key| key.to_string())
    }

    fn card_id(catalog: &Catalog, program: &ForegroundProgram) -> String {
        catalog.card_for(None, Some(program)).id.clone()
    }

    #[test]
    fn programs_are_found_by_their_most_specific_name() {
        let catalog = build(&TabIconSettings::default());
        assert_eq!(card_id(&catalog, &program("node", Some("npm"))), "node");
        assert_eq!(
            card_id(&catalog, &program("Python", Some("manage.py"))),
            "python"
        );
        assert_eq!(card_id(&catalog, &program("python3.12", None)), "python");
        assert_eq!(card_id(&catalog, &program("vim.basic", None)), "vim");
        assert_eq!(card_id(&catalog, &program("nvim", None)), "neovim");
        assert_eq!(card_id(&catalog, &program("zsh", None)), TERMINAL_CARD);
        assert_eq!(
            card_id(&catalog, &program("bash", Some("deploy.sh"))),
            TERMINAL_CARD
        );
        assert_eq!(card_id(&catalog, &program("sudo", Some("htop"))), "monitor");
        assert_eq!(card_id(&catalog, &program("ffmpeg", None)), TERMINAL_CARD);
    }

    #[test]
    fn an_agent_is_dressed_before_the_program_it_runs_in() {
        let catalog = build(&TabIconSettings::default());
        let node = program("node", None);
        assert_eq!(catalog.card_for(Some("claude"), Some(&node)).id, "claude");
        // An agent no card names takes the catch-all agent card.
        let other = catalog.card_for(Some("no-such-agent"), Some(&node));
        assert!(other.agents.iter().any(|agent| agent == "*"));
        assert_eq!(catalog.card_for(None, None).id, TERMINAL_CARD);
        assert_eq!(catalog.terminal_card().id, TERMINAL_CARD);
    }

    #[test]
    fn a_card_of_the_users_own_wins_over_a_built_in() {
        let settings = TabIconSettings {
            enabled: None,
            cards: vec![TabIconCard {
                id: "custom-1".to_string(),
                programs: Some(vec!["deploy.sh".to_string(), "python".to_string()]),
                ..Default::default()
            }],
        };
        let catalog = build(&settings);
        assert_eq!(
            card_id(&catalog, &program("bash", Some("deploy.sh"))),
            "custom-1"
        );
        assert_eq!(card_id(&catalog, &program("python3", None)), "custom-1");
    }

    #[test]
    fn a_built_in_card_keeps_its_defaults_where_unchanged() {
        let settings = TabIconSettings {
            enabled: Some(false),
            cards: vec![TabIconCard {
                id: "python".to_string(),
                circle: Some("#112233".to_string()),
                svg: Some("../../outside".to_string()),
                ..Default::default()
            }],
        };
        let catalog = build(&settings);
        assert!(!catalog.enabled);
        let python = catalog.card("python").unwrap();
        assert_eq!(python.circle, Rgb(0x11, 0x22, 0x33));
        assert_eq!(python.glyph_color, WHITE);
        // A settings file cannot point a card at an arbitrary path.
        assert_eq!(python.glyph, GlyphKey::Builtin(BuiltinGlyph::Python));
        assert!(python.changed);
        assert!(!catalog.card("node").unwrap().changed);
    }

    #[test]
    fn the_terminal_card_ignores_saved_changes() {
        let settings = TabIconSettings {
            enabled: None,
            cards: vec![TabIconCard {
                id: TERMINAL_CARD.to_string(),
                circle: Some("#112233".to_string()),
                programs: Some(Vec::new()),
                ..Default::default()
            }],
        };
        let catalog = build(&settings);
        let terminal = catalog.card(TERMINAL_CARD).unwrap();
        assert_eq!(terminal.circle, BUILTIN_CARDS[0].circle);
        assert!(terminal.programs.iter().any(|name| name == "zsh"));
        assert!(!terminal.changed);
    }

    #[test]
    fn a_default_saved_as_a_change_is_no_change() {
        let settings = TabIconSettings {
            enabled: None,
            cards: vec![TabIconCard {
                id: "monitor".to_string(),
                circle: Some(
                    BUILTIN_CARDS
                        .iter()
                        .find(|card| card.id == "monitor")
                        .unwrap()
                        .circle
                        .to_hex(),
                ),
                ..Default::default()
            }],
        };
        assert!(!build(&settings).card("monitor").unwrap().changed);
    }

    #[test]
    fn the_search_finds_cards_by_name_or_program() {
        let catalog = build(&TabIconSettings::default());
        let found = |query: &str| -> Vec<&str> {
            catalog
                .cards
                .iter()
                .filter(|card| card.matches(query))
                .map(|card| card.id.as_str())
                .collect()
        };
        assert!(found("pyth").contains(&"python"));
        assert_eq!(found("kubectl"), vec!["kubernetes"]);
        assert!(found("no such program").is_empty());
        assert_eq!(found("").len(), catalog.cards.len());
    }

    #[test]
    fn colours_round_trip_through_the_settings_spelling() {
        assert_eq!(Rgb::parse("#3776ab"), Some(Rgb(0x37, 0x76, 0xAB)));
        assert_eq!(Rgb::parse(" #3776AB "), Some(Rgb(0x37, 0x76, 0xAB)));
        assert_eq!(Rgb::parse("3776AB"), None);
        assert_eq!(Rgb::parse("#37 6AB"), None);
        assert_eq!(Rgb(0x37, 0x76, 0xAB).to_hex(), "#3776AB");
        assert_eq!(PAPER.legible_glyph(), INK);
        assert_eq!(Rgb(0x37, 0x76, 0xAB).legible_glyph(), WHITE);
    }

    #[test]
    fn only_hash_named_svgs_and_numbered_custom_cards_are_recognized() {
        assert!(is_svg_id(&"a".repeat(64)));
        assert!(!is_svg_id(&"A".repeat(64)));
        assert!(!is_svg_id("../../etc/passwd"));
        assert!(is_custom_id("custom-12"));
        assert!(!is_custom_id("custom-"));
        assert!(!is_custom_id("python"));
    }

    #[test]
    fn a_page_matches_the_cards_it_is_sent_as_the_desktop_does() {
        let settings = TabIconSettings {
            enabled: None,
            cards: vec![TabIconCard {
                id: "custom-1".to_string(),
                programs: Some(vec!["deploy.sh".to_string()]),
                svg: Some("b".repeat(64)),
                ..Default::default()
            }],
        };
        let catalog = build(&settings);
        let wire = catalog.wire(|hash| (hash == "b".repeat(64)).then(|| "<svg/>".to_string()));
        let text = serde_json::to_string(&wire).unwrap();
        let wire: WireCatalog = serde_json::from_str(&text).unwrap();
        let matcher = wire.matcher();
        for (agent, prog) in [
            (None, program("bash", Some("deploy.sh"))),
            (None, program("python3.12", None)),
            (Some("claude"), program("node", None)),
            (None, program("zsh", None)),
        ] {
            let page = &wire.cards[matcher.find(agent, Some(&prog))];
            let desktop = catalog.card_for(agent, Some(&prog));
            assert_eq!(page.id, desktop.id);
            assert_eq!(page.circle, desktop.circle.to_hex());
        }
        let custom = wire.cards.iter().find(|card| card.id == "custom-1").unwrap();
        assert_eq!(custom.svg, "<svg/>");
        // One whose SVG is gone has no glyph, as on the desktop.
        let gone = catalog.wire(|_| None);
        let custom = gone.cards.iter().find(|card| card.id == "custom-1").unwrap();
        assert_eq!(custom.svg, "");
        assert_eq!(wire.terminal, TERMINAL_CARD);
        // A built-in glyph travels as its SVG.
        assert!(wire.cards[matcher.terminal()].svg.contains("<svg"));
    }
}
