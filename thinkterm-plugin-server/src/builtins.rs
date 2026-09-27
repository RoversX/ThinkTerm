//! The plugins built into the host.

use crate::manifest::Manifest;
use crate::registry::Builtin;
use std::path::Path;
use thinkterm_plugin_sdk::Plugin;

/// Every built-in plugin, keeping its files in `data_dir`, in the order a
/// list shows them.
pub fn all(data_dir: &Path) -> Vec<Builtin> {
    vec![Builtin {
        manifest: Manifest::parse(include_str!("snippets.toml"), true)
            .expect("the snippets manifest reads"),
        plugin: Box::new(crate::snippets::Snippets::new(
            data_dir.join("snippets.json"),
        )),
    }]
}

/// The built-in plugin `id` alone, to serve out of process.
pub fn one(id: &str, data_dir: &Path) -> Option<Box<dyn Plugin>> {
    all(data_dir)
        .into_iter()
        .find(|builtin| builtin.manifest.id == id)
        .map(|builtin| builtin.plugin)
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_manifest_reads() {
        let builtins = super::all(std::path::Path::new("/nonexistent"));
        assert_eq!(builtins[0].manifest.id, "snippets");
        assert_eq!(builtins[0].manifest.localized("zh-CN").name, "片段");
    }
}
