use std::path::Path;
use std::process::Command;

/// Cargo reruns this script on every build while a watched path is missing,
/// which rebuilds and relinks everything that depends on this crate.
fn watch(path: &Path) {
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn watch_git_path(name: &str) {
    if let Ok(output) = Command::new("git")
        .args(["rev-parse", "--git-path", name])
        .output()
    {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout);
            watch(Path::new(path.trim()));
        }
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Release packaging writes this into a fresh checkout before building,
    // and a fresh checkout reruns this script through the tracked files.
    watch(Path::new("../.tag"));
    let mut ci_tag = std::fs::read_to_string("../.tag")
        .unwrap_or_default()
        .trim()
        .to_string();

    if let Ok(repo) = git2::Repository::discover(".") {
        // HEAD can switch branches without changing the ref that the old
        // build watched. Packed refs and worktree refs need their real paths.
        for name in ["HEAD", "index", "packed-refs"] {
            watch_git_path(name);
        }
        if let Ok(head) = repo.head() {
            if let Some(name) = head.name() {
                watch_git_path(name);
            }
        }
        // Index mtimes do not change for unstaged edits. Watch tracked inputs
        // rather than the checkout directory (which contains target and logs).
        if let (Some(workdir), Ok(index)) = (repo.workdir(), repo.index()) {
            for entry in index.iter() {
                if let Ok(path) = std::str::from_utf8(&entry.path) {
                    watch(&workdir.join(Path::new(path)));
                }
            }
        }

        if ci_tag.is_empty() {
            if let Ok(output) = Command::new("git")
                .args([
                    "-c",
                    "core.abbrev=8",
                    "show",
                    "-s",
                    "--format=%cd-%h",
                    "--date=format:%Y%m%d-%H%M%S",
                ])
                .output()
            {
                if output.status.success() {
                    ci_tag = String::from_utf8_lossy(&output.stdout).trim().to_string();
                }
            }
        }
        let mut options = git2::StatusOptions::new();
        options.include_untracked(false);
        if repo
            .statuses(Some(&mut options))
            .is_ok_and(|statuses| !statuses.is_empty())
        {
            ci_tag.push_str("-dirty");
        }
    }

    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=WEZTERM_TARGET_TRIPLE={target}");
    println!("cargo:rustc-env=WEZTERM_CI_TAG={ci_tag}");
}
