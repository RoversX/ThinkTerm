#![allow(dead_code)]

pub(crate) mod anim;
pub(crate) mod draw;
pub(crate) mod events;
pub(crate) mod geometry;
pub(crate) mod icons;
pub(crate) mod keys;
pub(crate) mod primitives;
pub(crate) mod shadow;
pub(crate) mod state;
pub(crate) mod tile;
pub(crate) mod tokens;
pub(crate) mod widgets;

pub(crate) use draw::*;
pub(crate) use events::*;
pub(crate) use geometry::*;
pub(crate) use icons::*;
pub(crate) use keys::*;
pub(crate) use primitives::*;
pub(crate) use state::*;
pub(crate) use tokens::*;
pub(crate) use widgets::*;

use std::path::Path;

/// `path` with the home directory written `~`, as people say it.
pub(crate) fn home_relative(path: &Path) -> String {
    home_relative_to(path, &config::HOME_DIR)
}

/// Compared a component at a time, so `/home/user` never shortens
/// `/home/username`.
fn home_relative_to(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        Err(_) => path.display().to_string(),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::home_relative_to;
    use std::path::Path;

    #[test]
    fn home_is_shortened_a_whole_component_at_a_time() {
        let home = Path::new("/home/user");
        assert_eq!(home_relative_to(Path::new("/home/user"), home), "~");
        assert_eq!(
            home_relative_to(Path::new("/home/user/projects"), home),
            "~/projects"
        );
        assert_eq!(
            home_relative_to(Path::new("/home/username/projects"), home),
            "/home/username/projects"
        );
        assert_eq!(home_relative_to(Path::new("/srv/data"), home), "/srv/data");
    }
}
