//! The icon heading each pane tab: a coloured circle carrying a one-colour
//! glyph, chosen by what the pane runs.
//!
//! The facts come from the mux that owns the pane -- the agent it runs
//! (`Pane::agent_status`) and the program leading its terminal
//! (`Pane::foreground_program`) -- so a remote pane is dressed exactly like a
//! local one. What a program looks like is decided here, by cards: built-in
//! ones the user may restyle, and cards of their own. Both live in settings.
//!
//! A card's glyph is only ever a shape. Built-in marks and imported SVGs are
//! rasterized to their coverage and tinted with the card's glyph colour, so
//! every icon in the row belongs to the same family whatever it was drawn in.

use crate::native_settings::{self, NativeTabIconCard, NativeTabIconSettings};
use crate::quad::{QuadTrait, TripleLayerQuadAllocator, TripleLayerQuadAllocatorTrait};
use crate::ui::draw::DrawContext;
use crate::ui::tile::{draw_tile, TileStyle};
use anyhow::{anyhow, bail, Context, Result};
use mux::pane::Pane;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use thinkterm_proto::ForegroundProgram;
use window::color::LinearRgba;
use window::{BitmapImage, Image};

/// Largest SVG accepted for a card. Icons are a few kilobytes; anything
/// much bigger is a drawing, not an icon, and would be parsed on every
/// rasterization.
pub(crate) const MAX_SVG_BYTES: u64 = 256 * 1024;
/// How many cards of their own a user may keep. Each can carry an SVG, and
/// every one of them may be rasterized into the glyph atlas.
pub(crate) const MAX_CUSTOM_CARDS: usize = 64;
/// The card that dresses shells and anything no other card claims. It is
/// fixed: the settings page leaves it out and a saved change to it is
/// ignored. Any other card may still claim a shell's name.
pub(crate) const TERMINAL_CARD: &str = "terminal";

/// A glyph's shape, from the tree or from an SVG the user imported.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum GlyphKey {
    Builtin(BuiltinGlyph),
    /// Hex SHA-256 of the SVG's bytes, which is also its file name.
    Svg(Arc<str>),
}

/// The marks the built-in cards use. Brand marks come from simple-icons and
/// lobe-icons, generic ones from lucide; all are single-colour shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum BuiltinGlyph {
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
    fn bytes(self) -> &'static [u8] {
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
pub(crate) struct Rgb(pub(crate) u8, pub(crate) u8, pub(crate) u8);

impl Rgb {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let hex = text.trim().strip_prefix('#')?;
        if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
        Some(Self(channel(0)?, channel(2)?, channel(4)?))
    }

    pub(crate) fn to_hex(self) -> String {
        format!("#{:02X}{:02X}{:02X}", self.0, self.1, self.2)
    }

    pub(crate) fn linear(self) -> LinearRgba {
        LinearRgba::with_srgba(self.0, self.1, self.2, 255)
    }

    /// Perceived brightness, 0..=255.
    fn luma(self) -> f32 {
        0.2126 * self.0 as f32 + 0.7152 * self.1 as f32 + 0.0722 * self.2 as f32
    }

    /// The glyph colour that reads on this circle: white on dark, near
    /// black on light.
    pub(crate) fn legible_glyph(self) -> Self {
        if self.luma() > 165.0 {
            Self(0x11, 0x11, 0x11)
        } else {
            Self(0xFF, 0xFF, 0xFF)
        }
    }
}

const WHITE: Rgb = Rgb(0xFF, 0xFF, 0xFF);
const INK: Rgb = Rgb(0x11, 0x11, 0x11);
/// Brands whose own colour is black sit on a light circle instead: a black
/// circle would sink into the dark chrome.
const PAPER: Rgb = Rgb(0xF2, 0xF2, 0xF2);

struct BuiltinCard {
    id: &'static str,
    /// A brand's own name, or an i18n key (`tab-icons-card-…`) for the
    /// generic cards, which are translated.
    name: &'static str,
    glyph: BuiltinGlyph,
    circle: Rgb,
    glyph_color: Rgb,
    /// Agents this card dresses, by agent id. `*` takes every agent no
    /// other card names.
    agents: &'static [&'static str],
    programs: &'static [&'static str],
}

/// The cards every user starts with, in the order the settings page shows
/// them. Agents first: they are what this product is about. Program names
/// are matched lower-case against what the pane runs; see `candidates`.
/// Brands wear their own colours; the generic cards wear quiet greys, a
/// little warm or cool so neighbours still differ, and the terminal the
/// darkest, as the one seen most.
const BUILTIN_CARDS: &[BuiltinCard] = &[
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
const NEW_CARD_CIRCLES: &[Rgb] = &[
    Rgb(0xEC, 0x48, 0x99),
    Rgb(0x8B, 0x5C, 0xF6),
    Rgb(0x14, 0xB8, 0xA6),
    Rgb(0xF9, 0x73, 0x16),
    Rgb(0x0E, 0xA5, 0xE9),
    Rgb(0x84, 0xCC, 0x16),
];

/// Circle colours offered on the settings page, besides typing one.
pub(crate) const CIRCLE_PRESETS: &[Rgb] = &[
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
pub(crate) const GLYPH_PRESETS: &[Rgb] = &[
    WHITE,
    INK,
    Rgb(0x4A, 0xDE, 0x80),
    Rgb(0xFA, 0xCC, 0x15),
    Rgb(0x60, 0xA5, 0xFA),
    Rgb(0xF4, 0x72, 0xB6),
];

/// One card as it stands after the user's changes.
#[derive(Debug, Clone)]
pub(crate) struct Card {
    pub(crate) id: String,
    pub(crate) name: String,
    /// Built in (may be reset, never deleted) or the user's own.
    pub(crate) builtin: bool,
    /// A built-in card the user changed, so "reset" means something.
    pub(crate) changed: bool,
    pub(crate) glyph: GlyphKey,
    pub(crate) circle: Rgb,
    pub(crate) glyph_color: Rgb,
    pub(crate) programs: Vec<String>,
    agents: &'static [&'static str],
}

/// What a tab's icon is drawn with.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResolvedIcon {
    pub(crate) glyph: GlyphKey,
    pub(crate) circle: Rgb,
    pub(crate) glyph_color: Rgb,
}

impl Card {
    pub(crate) fn resolved(&self) -> ResolvedIcon {
        ResolvedIcon {
            glyph: self.glyph.clone(),
            circle: self.circle,
            glyph_color: self.glyph_color,
        }
    }

    /// Whether the settings page's search for `query` (already trimmed and
    /// lower case) finds this card: by its name or by a program it claims.
    pub(crate) fn matches(&self, query: &str) -> bool {
        query.is_empty()
            || self.name.to_lowercase().contains(query)
            || self
                .programs
                .iter()
                .any(|program| program.to_lowercase().contains(query))
    }
}

/// Every card, and the tables that find one for a pane.
pub(crate) struct Catalog {
    pub(crate) enabled: bool,
    pub(crate) cards: Vec<Card>,
    by_program: HashMap<String, usize>,
    by_agent: HashMap<&'static str, usize>,
    any_agent: Option<usize>,
    terminal: usize,
}

impl Catalog {
    fn build(settings: &NativeTabIconSettings) -> Self {
        let overrides: HashMap<&str, &NativeTabIconCard> = settings
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
                crate::i18n::tr(builtin.name)
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
                agents: builtin.agents,
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
                    .unwrap_or_else(|| crate::i18n::tr("tab-icons-card-untitled")),
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
                agents: &[],
            });
        }

        // Built-ins first, then the user's own: a name the user gave one of
        // their cards wins even where a built-in still lists it.
        let mut by_program = HashMap::new();
        let mut by_agent = HashMap::new();
        let mut any_agent = None;
        for (index, card) in cards.iter().enumerate() {
            for program in &card.programs {
                if let Some(name) = normalize_program_name(program) {
                    by_program.insert(name, index);
                }
            }
            for agent in card.agents {
                if *agent == "*" {
                    any_agent.get_or_insert(index);
                } else {
                    by_agent.entry(*agent).or_insert(index);
                }
            }
        }
        let terminal = cards
            .iter()
            .position(|card| card.id == TERMINAL_CARD)
            .unwrap_or(0);
        Self {
            enabled: settings.enabled.unwrap_or(true),
            cards,
            by_program,
            by_agent,
            any_agent,
            terminal,
        }
    }

    pub(crate) fn card(&self, id: &str) -> Option<&Card> {
        self.cards.iter().find(|card| card.id == id)
    }

    fn card_for_pane(&self, pane: &dyn Pane) -> &Card {
        if let Some(status) = pane.agent_status().filter(|status| !status.ended) {
            let index = self
                .by_agent
                .get(status.agent_id.as_str())
                .copied()
                .or(self.any_agent);
            if let Some(index) = index {
                return &self.cards[index];
            }
        }
        if let Some(program) = pane.foreground_program() {
            if let Some(index) = self.card_for_program(&program) {
                return &self.cards[index];
            }
        }
        &self.cards[self.terminal]
    }

    fn card_for_program(&self, program: &ForegroundProgram) -> Option<usize> {
        candidates(program)
            .into_iter()
            .find_map(|name| self.by_program.get(&name).copied())
    }
}

thread_local! {
    /// The catalog built from the settings last read, rebuilt when they are
    /// replaced (every save swaps the shared settings for a new `Arc`).
    static CATALOG: RefCell<Option<(Arc<native_settings::ThinkTermNativeSettings>, Rc<Catalog>)>> =
        RefCell::new(None);
}

/// The catalog as the settings currently describe it.
pub(crate) fn catalog() -> Rc<Catalog> {
    let settings = native_settings::load_shared();
    let (catalog, rebuilt) = CATALOG.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some((built_from, catalog)) = slot.as_ref() {
            if Arc::ptr_eq(built_from, &settings) {
                return (Rc::clone(catalog), false);
            }
        }
        let catalog = Rc::new(Catalog::build(&settings.tab_icons));
        *slot = Some((settings, Rc::clone(&catalog)));
        (catalog, true)
    });
    if rebuilt {
        // The settings can change without `set_enabled` -- edited by hand,
        // reloaded from disk, restored from a backup -- and the observer's
        // switch has to follow the one the painters read.
        mux::foreground_program::refresh_enabled();
    }
    catalog
}

/// How to draw the icon of the tab showing `pane`, or `None` when the user
/// turned tab icons off and the tab keeps its plain terminal mark.
pub(crate) fn resolve(pane: &dyn Pane) -> Option<ResolvedIcon> {
    let catalog = catalog();
    catalog
        .enabled
        .then(|| catalog.card_for_pane(pane).resolved())
}

/// How to draw a window tab's icon: always the terminal card, as a window
/// tab holds panes that may each run something else. `None` when the user
/// turned tab icons off.
pub(crate) fn resolve_window_tab() -> Option<ResolvedIcon> {
    let catalog = catalog();
    catalog
        .enabled
        .then(|| catalog.cards[catalog.terminal].resolved())
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
pub(crate) fn normalize_program_name(name: &str) -> Option<String> {
    let name = name.trim().to_lowercase();
    (!name.is_empty() && !name.contains(['/', '\\'])).then_some(name)
}

fn is_custom_id(id: &str) -> bool {
    id.strip_prefix("custom-")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// An imported SVG's id is its hash and its file name, so a hand-edited
/// settings file must not be able to name any other path.
fn is_svg_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn svg_key(id: Option<&str>) -> Option<GlyphKey> {
    id.filter(|id| is_svg_id(id))
        .map(|id| GlyphKey::Svg(Arc::from(id)))
}

// ---------------------------------------------------------------------------
// Glyphs.

/// Where imported SVGs are kept, next to the settings that name them.
pub(crate) fn icon_dir() -> PathBuf {
    native_settings::settings_path().with_file_name("tab-icons")
}

fn svg_path(id: &str) -> PathBuf {
    icon_dir().join(format!("{id}.svg"))
}

/// The glyph as a white mask `size` pixels square, for the painter to tint.
pub(crate) fn rasterize_glyph(key: &GlyphKey, size: usize) -> Result<Image> {
    match key {
        GlyphKey::Builtin(glyph) => rasterize_mask(glyph.bytes(), size),
        GlyphKey::Svg(id) => {
            let path = svg_path(id);
            let bytes = read_bounded(&path)?;
            rasterize_mask(&bytes, size)
        }
    }
}

fn read_bounded(path: &Path) -> Result<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_SVG_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {}", path.display()))?;
    if bytes.len() as u64 > MAX_SVG_BYTES {
        bail!("{} is larger than {} bytes", path.display(), MAX_SVG_BYTES);
    }
    Ok(bytes)
}

/// Keep the shape, drop the colours: every pixel becomes white at its own
/// coverage (the atlas is premultiplied, so that is all four channels set to
/// alpha), and the painter tints it.
fn rasterize_mask(svg: &[u8], size: usize) -> Result<Image> {
    let size = size.max(1);
    let mut options = resvg::usvg::Options::default();
    // An icon is a shape: nothing it names outside itself is ever fetched.
    options.image_href_resolver.resolve_string = Box::new(|_, _| None);
    let tree = resvg::usvg::Tree::from_data(svg, &options).context("parsing SVG")?;
    let svg_size = tree.size();
    let scale = (size as f32 / svg_size.width()).min(size as f32 / svg_size.height());
    let translate_x = (size as f32 - svg_size.width() * scale) / 2.0;
    let translate_y = (size as f32 - svg_size.height() * scale) / 2.0;
    let transform = resvg::tiny_skia::Transform::from_translate(translate_x, translate_y)
        .pre_scale(scale, scale);
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size as u32, size as u32)
        .ok_or_else(|| anyhow!("allocating a {size}px glyph"))?;
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    let mut data = pixmap.take();
    for pixel in data.chunks_exact_mut(4) {
        let alpha = pixel[3];
        pixel[0] = alpha;
        pixel[1] = alpha;
        pixel[2] = alpha;
    }
    Ok(Image::from_raw(size, size, data))
}

fn covers_anything(mask: &Image) -> bool {
    mask.pixel_data_slice()
        .chunks_exact(4)
        .any(|pixel| pixel[3] != 0)
}

/// Why an SVG was not taken, in words for the user.
#[derive(Debug)]
pub(crate) enum ImportError {
    TooLarge,
    NotAnIcon,
    Io(anyhow::Error),
}

impl ImportError {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::TooLarge => crate::i18n::tr("tab-icons-import-too-large"),
            Self::NotAnIcon => crate::i18n::tr("tab-icons-import-not-svg"),
            Self::Io(_) => crate::i18n::tr("tab-icons-import-failed"),
        }
    }
}

/// Take a copy of the SVG at `path` into the icon directory, named by its
/// hash so the same file imported twice is kept once. The copy is what the
/// card uses: moving or deleting the original changes nothing.
pub(crate) fn import_svg(path: &Path) -> std::result::Result<String, ImportError> {
    let len = std::fs::metadata(path)
        .map_err(|err| ImportError::Io(err.into()))?
        .len();
    if len > MAX_SVG_BYTES {
        return Err(ImportError::TooLarge);
    }
    // The size was checked above; what can still go wrong is reading.
    let bytes = read_bounded(path).map_err(ImportError::Io)?;
    // It has to draw something: parse it and look for a single covered
    // pixel, so a file that is not SVG, or an SVG of nothing, is refused
    // here rather than showing up as an empty circle.
    let mask = rasterize_mask(&bytes, 64).map_err(|_| ImportError::NotAnIcon)?;
    if !covers_anything(&mask) {
        return Err(ImportError::NotAnIcon);
    }
    use sha2::Digest;
    let id: String = sha2::Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let dir = icon_dir();
    std::fs::create_dir_all(&dir).map_err(|err| ImportError::Io(err.into()))?;
    let target = svg_path(&id);
    if !target.exists() {
        let tmp = target.with_extension("svg.tmp");
        std::fs::write(&tmp, &bytes).map_err(|err| ImportError::Io(err.into()))?;
        std::fs::rename(&tmp, &target).map_err(|err| ImportError::Io(err.into()))?;
    }
    Ok(id)
}

/// How long an imported SVG may sit unreferenced before a cleanup takes it.
/// An import writes its file on a worker and names it in the settings only
/// when it comes back; a save in between must not mistake it for litter.
const UNREFERENCED_SVG_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

/// Delete imported SVGs no card names any more. Called after every change
/// that can drop one, so the directory never outgrows the cards.
fn remove_unreferenced_svgs(settings: &NativeTabIconSettings) {
    let Ok(entries) = std::fs::read_dir(icon_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(id) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".svg"))
        else {
            continue;
        };
        if !is_svg_id(id) {
            continue;
        }
        let referenced = settings
            .cards
            .iter()
            .any(|card| card.svg.as_deref() == Some(id));
        let fresh = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_none_or(|age| age < UNREFERENCED_SVG_GRACE);
        if !referenced && !fresh {
            if let Err(err) = std::fs::remove_file(&path) {
                log::warn!(
                    "unable to remove unused tab icon {}: {err:#}",
                    path.display()
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Drawing, shared by the pane tabs and the settings page so both show the
// same icon.

/// How a tab icon's circle is lit: the fill lightens toward white at the
/// top and darkens toward black at the bottom, and the rim -- one point on
/// a 25pt icon -- does the same more strongly, so the edge reads as a bevel
/// in the circle's own colour. The shadow is in its hue too.
const PLATE: TileStyle = TileStyle {
    fill_top_lighten: 0.18,
    fill_bottom_darken: 0.13,
    rim_top_lighten: 0.42,
    rim_bottom_darken: 0.25,
    rim_per_side: 1.0 / 25.0,
    shadow_darken: 0.55,
    shadow_alpha_dark: 0.5,
    shadow_alpha_light: 0.32,
    shadow_sigma: 3.5,
    shadow_drop: 2.5,
};

/// The circle a glyph sits on. `dark` is the chrome's appearance.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_plate(
    ctx: &DrawContext,
    layers: &mut TripleLayerQuadAllocator,
    layer: usize,
    x: f32,
    y: f32,
    diameter: f32,
    circle: Rgb,
    dark: bool,
) -> Result<()> {
    draw_tile(
        ctx,
        layers,
        layer,
        x,
        y,
        diameter,
        diameter / 2.0,
        circle.linear(),
        dark,
        &PLATE,
    )
}

/// A card's glyph, `size` pixels square: its white mask from the glyph
/// cache, tinted. An imported SVG deleted from under its card draws
/// nothing -- the circle alone still says which card it is -- rather than
/// failing the frame; a full atlas does fail it, so the painter grows the
/// atlas and paints again.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_glyph(
    ctx: &DrawContext,
    layers: &mut TripleLayerQuadAllocator,
    layer: usize,
    glyph: &GlyphKey,
    x: f32,
    y: f32,
    size: f32,
    color: LinearRgba,
) -> Result<()> {
    if size < 1.0 {
        return Ok(());
    }
    let sprite = ctx
        .render_state
        .glyph_cache
        .borrow_mut()
        .cached_tab_glyph(glyph, size.round() as usize)?;
    let Some(sprite) = sprite else {
        return Ok(());
    };
    let sprite = sprite.texture_coords();
    let left_offset = ctx.dimensions.pixel_width as f32 / 2.0;
    let top_offset = ctx.dimensions.pixel_height as f32 / 2.0;
    let mut quad = layers.allocate(layer)?;
    quad.set_position(
        x - left_offset,
        y - top_offset,
        x + size - left_offset,
        y + size - top_offset,
    );
    quad.set_texture(sprite);
    quad.set_fg_color(color);
    quad.set_alt_color_and_mix_value(color, 0.0);
    quad.set_hsv(None);
    quad.set_has_color(false);
    quad.set_grayscale();
    Ok(())
}

// ---------------------------------------------------------------------------
// Changes, as the settings page makes them. Each reads the settings, changes
// them, saves them and cleans up after itself; the painters pick the result
// up through `catalog`, which notices the settings were replaced.

fn edit(change: impl FnOnce(&mut NativeTabIconSettings)) -> Result<()> {
    let settings = native_settings::update(|settings| {
        change(&mut settings.tab_icons);
        // A built-in card whose every field went back to the default no
        // longer needs an entry, and the terminal card never has one that
        // counts.
        settings.tab_icons.cards.retain(|card| {
            is_custom_id(&card.id)
                || (card.id != TERMINAL_CARD
                    && *card
                        != NativeTabIconCard {
                            id: card.id.clone(),
                            ..Default::default()
                        })
        });
    })?;
    remove_unreferenced_svgs(&settings.tab_icons);
    Ok(())
}

/// Change card `id`'s entry. The card has to still be there: an import or
/// a file picker that comes back after its card was deleted must not bring
/// the card back empty, and the terminal card takes no changes at all.
fn edit_card(id: &str, change: impl FnOnce(&mut NativeTabIconCard)) -> Result<()> {
    let exists = if is_custom_id(id) {
        native_settings::load_shared()
            .tab_icons
            .cards
            .iter()
            .any(|card| card.id == id)
    } else {
        id != TERMINAL_CARD && BUILTIN_CARDS.iter().any(|card| card.id == id)
    };
    if !exists {
        bail!("no tab icon card {id}");
    }
    edit(|settings| change(entry(settings, id)))
}

fn entry<'a>(settings: &'a mut NativeTabIconSettings, id: &str) -> &'a mut NativeTabIconCard {
    let index = match settings.cards.iter().position(|card| card.id == id) {
        Some(index) => index,
        None => {
            settings.cards.push(NativeTabIconCard {
                id: id.to_string(),
                ..Default::default()
            });
            settings.cards.len() - 1
        }
    };
    &mut settings.cards[index]
}

pub(crate) fn set_enabled(enabled: bool) -> Result<()> {
    edit(|settings| settings.enabled = (!enabled).then_some(false))?;
    mux::foreground_program::refresh_enabled();
    Ok(())
}

/// Whether tab icons are on; the foreground program observer asks this.
pub(crate) fn enabled() -> bool {
    native_settings::load_shared()
        .tab_icons
        .enabled
        .unwrap_or(true)
}

/// Add a card of the user's own, wearing `svg` if one was just imported,
/// and return its id -- or `None` when they already have as many as they
/// may. The card and its SVG go in one save: a separate save in between
/// would find the SVG referenced by nothing and clean it away.
pub(crate) fn create_card(svg: Option<String>) -> Result<Option<String>> {
    let catalog = catalog();
    let custom = catalog.cards.iter().filter(|card| !card.builtin).count();
    if custom >= MAX_CUSTOM_CARDS {
        return Ok(None);
    }
    let next = catalog
        .cards
        .iter()
        .filter_map(|card| card.id.strip_prefix("custom-")?.parse::<u32>().ok())
        .max()
        .map_or(1, |n| n + 1);
    let id = format!("custom-{next}");
    let circle = NEW_CARD_CIRCLES[custom % NEW_CARD_CIRCLES.len()];
    edit(|settings| {
        let card = entry(settings, &id);
        card.circle = Some(circle.to_hex());
        card.glyph = Some(circle.legible_glyph().to_hex());
        card.svg = svg;
    })?;
    Ok(Some(id))
}

pub(crate) fn delete_card(id: &str) -> Result<()> {
    if !is_custom_id(id) {
        bail!("{id} is built in and cannot be deleted");
    }
    edit(|settings| settings.cards.retain(|card| card.id != id))
}

/// Put a built-in card back as it shipped.
pub(crate) fn reset_card(id: &str) -> Result<()> {
    edit(|settings| settings.cards.retain(|card| card.id != id))
}

pub(crate) fn rename_card(id: &str, name: &str) -> Result<()> {
    if !is_custom_id(id) {
        bail!("{id} is built in and keeps its name");
    }
    let name = name.trim().to_string();
    edit_card(id, |card| card.name = (!name.is_empty()).then_some(name))
}

pub(crate) fn set_card_svg(id: &str, svg: String) -> Result<()> {
    edit_card(id, |card| card.svg = Some(svg))
}

/// Give the card its built-in glyph back (a card of the user's own goes
/// back to the plain one it started with).
pub(crate) fn clear_card_svg(id: &str) -> Result<()> {
    edit_card(id, |card| card.svg = None)
}

pub(crate) fn set_card_circle(id: &str, color: Rgb) -> Result<()> {
    edit_card(id, |card| card.circle = Some(color.to_hex()))
}

pub(crate) fn set_card_glyph_color(id: &str, color: Rgb) -> Result<()> {
    edit_card(id, |card| card.glyph = Some(color.to_hex()))
}

/// Give `program` to card `id`. A name belongs to one card at a time, so it
/// is taken off whichever card had it; the returned name is that card's,
/// for the page to say where it came from. The terminal card is not
/// looked at: its shells are the fallback, fixed, and any other card wins
/// a name from it.
pub(crate) fn add_program(id: &str, program: &str) -> Result<Option<String>> {
    let Some(program) = normalize_program_name(program) else {
        bail!("{program:?} is not a program name");
    };
    let catalog = catalog();
    let previous = catalog
        .cards
        .iter()
        .find(|card| {
            card.id != id
                && card.id != TERMINAL_CARD
                && card
                    .programs
                    .iter()
                    .any(|name| normalize_program_name(name).as_deref() == Some(&program))
        })
        .map(|card| (card.id.clone(), card.name.clone(), card.programs.clone()));
    let mut programs = catalog
        .card(id)
        .map(|card| card.programs.clone())
        .ok_or_else(|| anyhow!("no tab icon card {id}"))?;
    if !programs.iter().any(|name| name == &program) {
        programs.push(program.clone());
    }
    edit(|settings| {
        if let Some((other, _, other_programs)) = &previous {
            entry(settings, other).programs = Some(
                other_programs
                    .iter()
                    .filter(|name| normalize_program_name(name).as_deref() != Some(&program))
                    .cloned()
                    .collect(),
            );
        }
        entry(settings, id).programs = Some(programs);
    })?;
    Ok(previous.map(|(_, name, _)| name))
}

pub(crate) fn remove_program(id: &str, program: &str) -> Result<()> {
    let catalog = catalog();
    let programs: Vec<String> = catalog
        .card(id)
        .map(|card| card.programs.clone())
        .ok_or_else(|| anyhow!("no tab icon card {id}"))?
        .into_iter()
        .filter(|name| name != program)
        .collect();
    edit(|settings| entry(settings, id).programs = Some(programs))
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

    fn card_id(catalog: &Catalog, program: &ForegroundProgram) -> String {
        catalog
            .card_for_program(program)
            .map(|index| catalog.cards[index].id.clone())
            .unwrap_or_else(|| TERMINAL_CARD.to_string())
    }

    #[test]
    fn programs_are_found_by_their_most_specific_name() {
        let catalog = Catalog::build(&NativeTabIconSettings::default());
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
    fn a_card_of_the_users_own_wins_over_a_built_in() {
        let settings = NativeTabIconSettings {
            enabled: None,
            cards: vec![NativeTabIconCard {
                id: "custom-1".to_string(),
                programs: Some(vec!["deploy.sh".to_string(), "python".to_string()]),
                ..Default::default()
            }],
        };
        let catalog = Catalog::build(&settings);
        assert_eq!(
            card_id(&catalog, &program("bash", Some("deploy.sh"))),
            "custom-1"
        );
        assert_eq!(card_id(&catalog, &program("python3", None)), "custom-1");
    }

    #[test]
    fn a_built_in_card_keeps_its_defaults_where_unchanged() {
        let settings = NativeTabIconSettings {
            enabled: Some(false),
            cards: vec![NativeTabIconCard {
                id: "python".to_string(),
                circle: Some("#112233".to_string()),
                svg: Some("../../outside".to_string()),
                ..Default::default()
            }],
        };
        let catalog = Catalog::build(&settings);
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
        let settings = NativeTabIconSettings {
            enabled: None,
            cards: vec![NativeTabIconCard {
                id: TERMINAL_CARD.to_string(),
                circle: Some("#112233".to_string()),
                programs: Some(Vec::new()),
                ..Default::default()
            }],
        };
        let catalog = Catalog::build(&settings);
        let terminal = catalog.card(TERMINAL_CARD).unwrap();
        assert_eq!(terminal.circle, BUILTIN_CARDS[0].circle);
        assert!(terminal.programs.iter().any(|name| name == "zsh"));
        assert!(!terminal.changed);
    }

    #[test]
    fn a_default_saved_as_a_change_is_no_change() {
        let settings = NativeTabIconSettings {
            enabled: None,
            cards: vec![NativeTabIconCard {
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
        assert!(!Catalog::build(&settings).card("monitor").unwrap().changed);
    }

    #[test]
    fn the_search_finds_cards_by_name_or_program() {
        let catalog = Catalog::build(&NativeTabIconSettings::default());
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
    fn every_built_in_glyph_draws_a_shape() {
        for card in BUILTIN_CARDS {
            let mask = rasterize_mask(card.glyph.bytes(), 32).unwrap();
            assert!(covers_anything(&mask), "{} draws nothing", card.id);
        }
        rasterize_mask(BuiltinGlyph::Package.bytes(), 32).unwrap();
    }

    #[test]
    fn a_mask_is_white_at_the_shapes_coverage() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 4 4"><rect width="2" height="4" fill="#ff0000"/></svg>"##;
        let mask = rasterize_mask(svg, 4).unwrap();
        let pixels: Vec<&[u8]> = mask.pixel_data_slice().chunks_exact(4).collect();
        for pixel in &pixels {
            assert_eq!(
                (pixel[0], pixel[1], pixel[2]),
                (pixel[3], pixel[3], pixel[3])
            );
        }
        assert!(pixels.iter().any(|pixel| pixel[3] == 0xFF));
        assert!(pixels.iter().any(|pixel| pixel[3] == 0));
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
}
