fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    generate_material_icons().expect("generate material file icon lookup tables");

    #[cfg(windows)]
    {
        use anyhow::Context as _;
        use std::io::Write;
        use std::path::Path;
        let profile = std::env::var("PROFILE").unwrap();
        let repo_dir = std::env::current_dir()
            .ok()
            .and_then(|cwd| cwd.parent().map(|p| p.to_path_buf()))
            .unwrap();
        let exe_output_dir = repo_dir.join("target").join(profile);
        let windows_dir = repo_dir.join("assets").join("windows");

        let conhost_dir = windows_dir.join("conhost");
        for name in &["conpty.dll", "OpenConsole.exe"] {
            let dest_name = exe_output_dir.join(name);
            let src_name = conhost_dir.join(name);

            if !dest_name.exists() {
                std::fs::copy(&src_name, &dest_name)
                    .context(format!(
                        "copy {} -> {}",
                        src_name.display(),
                        dest_name.display()
                    ))
                    .unwrap();
            }
        }

        let angle_dir = windows_dir.join("angle");
        for name in &["libEGL.dll", "libGLESv2.dll"] {
            let dest_name = exe_output_dir.join(name);
            let src_name = angle_dir.join(name);

            if !dest_name.exists() {
                std::fs::copy(&src_name, &dest_name)
                    .context(format!(
                        "copy {} -> {}",
                        src_name.display(),
                        dest_name.display()
                    ))
                    .unwrap();
            }
        }

        {
            let dest_mesa = exe_output_dir.join("mesa");
            let _ = std::fs::create_dir(&dest_mesa);
            let dest_name = dest_mesa.join("opengl32.dll");
            let src_name = windows_dir.join("mesa").join("opengl32.dll");
            if !dest_name.exists() {
                std::fs::copy(&src_name, &dest_name)
                    .context(format!(
                        "copy {} -> {}",
                        src_name.display(),
                        dest_name.display()
                    ))
                    .unwrap();
            }
        }

        // If a file named `.tag` is present, we'll take its contents for the
        // version number that we report in wezterm -h.
        let mut ci_tag = String::new();
        if let Ok(tag) = std::fs::read("../.tag") {
            if let Ok(s) = String::from_utf8(tag) {
                ci_tag = s.trim().to_string();
                println!("cargo:rerun-if-changed=../.tag");
            }
        }
        let version = if ci_tag.is_empty() {
            let mut cmd = std::process::Command::new("git");
            cmd.args(&[
                "-c",
                "core.abbrev=8",
                "show",
                "-s",
                "--format=%cd-%h",
                "--date=format:%Y%m%d-%H%M%S",
            ]);
            if let Ok(output) = cmd.output() {
                if output.status.success() {
                    String::from_utf8_lossy(&output.stdout).trim().to_owned()
                } else {
                    "UNKNOWN".to_owned()
                }
            } else {
                "UNKNOWN".to_owned()
            }
        } else {
            ci_tag
        };

        let rcfile_name = Path::new(&std::env::var_os("OUT_DIR").unwrap()).join("resource.rc");
        let mut rcfile = std::fs::File::create(&rcfile_name).unwrap();
        println!("cargo:rerun-if-changed=../assets/windows/terminal.ico");
        write!(
            rcfile,
            r#"
#include <winres.h>
// This ID is coupled with code in window/src/os/windows/window.rs
#define IDI_ICON 0x101
1 RT_MANIFEST "{win}\\manifest.manifest"
IDI_ICON ICON "{win}\\terminal.ico"
VS_VERSION_INFO VERSIONINFO
FILEVERSION     1,0,0,0
PRODUCTVERSION  1,0,0,0
FILEFLAGSMASK   VS_FFI_FILEFLAGSMASK
FILEFLAGS       0
FILEOS          VOS__WINDOWS32
FILETYPE        VFT_APP
FILESUBTYPE     VFT2_UNKNOWN
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904E4"
        BEGIN
            VALUE "CompanyName",      "Wez Furlong\0"
            VALUE "FileDescription",  "WezTerm - Wez's Terminal Emulator\0"
            VALUE "FileVersion",      "{version}\0"
            VALUE "LegalCopyright",   "Wez Furlong, MIT licensed\0"
            VALUE "InternalName",     "\0"
            VALUE "OriginalFilename", "\0"
            VALUE "ProductName",      "WezTerm\0"
            VALUE "ProductVersion",   "{version}\0"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1252
    END
END
"#,
            win = windows_dir.display().to_string().replace("\\", "\\\\"),
            version = version,
        )
        .unwrap();
        drop(rcfile);

        // Obtain MSVC environment so that the rc compiler can find the right headers.
        // https://github.com/nabijaczleweli/rust-embed-resource/issues/11#issuecomment-603655972
        let target = std::env::var("TARGET").unwrap();
        if let Some(tool) = cc::windows_registry::find_tool(target.as_str(), "cl.exe") {
            for (key, value) in tool.env() {
                std::env::set_var(key, value);
            }
        }
        embed_resource::compile(rcfile_name);
    }

    #[cfg(target_os = "macos")]
    {
        use anyhow::Context as _;
        let profile = std::env::var("PROFILE").unwrap();
        let repo_dir = std::env::current_dir()
            .ok()
            .and_then(|cwd| cwd.parent().map(|p| p.to_path_buf()))
            .unwrap();

        // We need to copy the plist to avoid the UNUserNotificationCenter asserting
        // due to not finding the application bundle
        let src_plist = repo_dir
            .join("assets")
            .join("macos")
            .join("ThinkTerm.app")
            .join("Contents")
            .join("Info.plist");
        let build_target_dir = std::env::var("CARGO_TARGET_DIR")
            .and_then(|s| Ok(std::path::PathBuf::from(s)))
            .unwrap_or(repo_dir.join("target").join(profile));
        let dest_plist = build_target_dir.join("Info.plist");
        let src_icon = repo_dir.join("assets").join("icon").join("ThinkTerm.icns");
        let dest_icon = build_target_dir.join("ThinkTerm.icns");
        let src_simple_icon = repo_dir
            .join("assets")
            .join("icon")
            .join("ThinkTerm_simple.icns");
        let dest_simple_icon = build_target_dir.join("ThinkTerm_simple.icns");
        println!("cargo:rerun-if-changed=assets/macos/ThinkTerm.app/Contents/Info.plist");
        println!("cargo:rerun-if-changed=assets/icon/ThinkTerm.icns");
        println!("cargo:rerun-if-changed=assets/icon/ThinkTerm_simple.icns");

        std::fs::copy(&src_plist, &dest_plist)
            .context(format!(
                "copy {} -> {}",
                src_plist.display(),
                dest_plist.display()
            ))
            .unwrap();

        std::fs::copy(&src_icon, &dest_icon)
            .context(format!(
                "copy {} -> {}",
                src_icon.display(),
                dest_icon.display()
            ))
            .unwrap();

        std::fs::copy(&src_simple_icon, &dest_simple_icon)
            .context(format!(
                "copy {} -> {}",
                src_simple_icon.display(),
                dest_simple_icon.display()
            ))
            .unwrap();
    }
}

fn generate_material_icons() -> anyhow::Result<()> {
    use anyhow::Context as _;
    use serde_json::Value;
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::path::{Path, PathBuf};

    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let material_dir = manifest_dir.join("../third_party/material-icon-theme");
    let theme_path = material_dir.join("dist/material-icons.json");
    let icons_dir = material_dir.join("icons");

    println!("cargo:rerun-if-changed={}", theme_path.display());
    println!("cargo:rerun-if-changed={}", icons_dir.display());

    let theme: Value = serde_json::from_slice(
        &std::fs::read(&theme_path).with_context(|| format!("reading {}", theme_path.display()))?,
    )
    .with_context(|| format!("parsing {}", theme_path.display()))?;

    let definitions = theme
        .get("iconDefinitions")
        .and_then(Value::as_object)
        .context("material-icons.json is missing iconDefinitions")?;

    let mut definition_ids = BTreeMap::new();
    let mut icon_paths = Vec::new();
    for (name, definition) in definitions {
        let Some(icon_path) = definition.get("iconPath").and_then(Value::as_str) else {
            continue;
        };
        let Some(relative_path) = normalize_material_icon_path(icon_path) else {
            continue;
        };
        if Path::new(&relative_path)
            .extension()
            .and_then(|ext| ext.to_str())
            != Some("svg")
        {
            continue;
        }
        if !material_dir.join(&relative_path).is_file() {
            continue;
        }

        let id = icon_paths.len() as u16;
        definition_ids.insert(name.clone(), id);
        icon_paths.push(relative_path);
    }

    let file = material_default_icon(&theme, &definition_ids, "file");
    let folder = material_default_icon(&theme, &definition_ids, "folder");
    let folder_expanded = material_default_icon(&theme, &definition_ids, "folderExpanded");
    let root_folder = material_default_icon(&theme, &definition_ids, "rootFolder");
    let root_folder_expanded = material_default_icon(&theme, &definition_ids, "rootFolderExpanded");

    let file_names = material_icon_map(&theme, &definition_ids, "fileNames", false);
    let file_extensions = material_icon_map(&theme, &definition_ids, "fileExtensions", true);
    let folder_names = material_icon_map(&theme, &definition_ids, "folderNames", false);
    let folder_names_expanded =
        material_icon_map(&theme, &definition_ids, "folderNamesExpanded", false);
    let root_folder_names = material_icon_map(&theme, &definition_ids, "rootFolderNames", false);
    let root_folder_names_expanded =
        material_icon_map(&theme, &definition_ids, "rootFolderNamesExpanded", false);

    let mut extension_keys = file_extensions.keys().cloned().collect::<Vec<_>>();
    extension_keys
        .sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));

    let out_path =
        PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("material_icons_generated.rs");
    let mut out = std::fs::File::create(&out_path)
        .with_context(|| format!("creating {}", out_path.display()))?;

    writeln!(out, "// @generated by wezterm-gui/build.rs")?;
    writeln!(out, "use super::MaterialIcon;")?;
    writeln!(out)?;
    emit_optional_icon_const(&mut out, "DEFAULT_FILE", file)?;
    emit_optional_icon_const(&mut out, "DEFAULT_FOLDER", folder)?;
    emit_optional_icon_const(&mut out, "DEFAULT_FOLDER_EXPANDED", folder_expanded)?;
    emit_optional_icon_const(&mut out, "DEFAULT_ROOT_FOLDER", root_folder)?;
    emit_optional_icon_const(
        &mut out,
        "DEFAULT_ROOT_FOLDER_EXPANDED",
        root_folder_expanded,
    )?;
    emit_phf_map(&mut out, "FILE_NAMES", &file_names)?;
    emit_phf_map(&mut out, "FILE_EXTENSIONS", &file_extensions)?;
    emit_phf_map(&mut out, "FOLDER_NAMES", &folder_names)?;
    emit_phf_map(&mut out, "FOLDER_NAMES_EXPANDED", &folder_names_expanded)?;
    emit_phf_map(&mut out, "ROOT_FOLDER_NAMES", &root_folder_names)?;
    emit_phf_map(
        &mut out,
        "ROOT_FOLDER_NAMES_EXPANDED",
        &root_folder_names_expanded,
    )?;

    writeln!(out, "pub static FILE_EXTENSION_SUFFIXES: &[&str] = &[")?;
    for extension in &extension_keys {
        writeln!(out, "    {:?},", extension)?;
    }
    writeln!(out, "];")?;
    writeln!(out)?;

    writeln!(
        out,
        "pub fn material_icon_bytes(icon: MaterialIcon) -> &'static [u8] {{"
    )?;
    writeln!(out, "    match icon.0 {{")?;
    for (id, relative_path) in icon_paths.iter().enumerate() {
        let include_path = format!("/../third_party/material-icon-theme/{relative_path}");
        writeln!(
            out,
            "        {id} => include_bytes!(concat!(env!(\"CARGO_MANIFEST_DIR\"), {:?})) as &'static [u8],",
            include_path
        )?;
    }
    writeln!(out, "        _ => &[],")?;
    writeln!(out, "    }}")?;
    writeln!(out, "}}")?;

    Ok(())
}

fn normalize_material_icon_path(path: &str) -> Option<String> {
    let mut components = Vec::new();
    for component in std::path::Path::new(path).components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                components.pop();
            }
            std::path::Component::Normal(part) => {
                components.push(part.to_string_lossy().to_string())
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => return None,
        }
    }
    if components.first().map(String::as_str) != Some("icons") {
        return None;
    }
    Some(components.join("/"))
}

fn material_default_icon(
    theme: &serde_json::Value,
    definition_ids: &std::collections::BTreeMap<String, u16>,
    field: &str,
) -> Option<u16> {
    theme
        .get(field)
        .and_then(serde_json::Value::as_str)
        .and_then(|name| definition_ids.get(name).copied())
}

fn material_icon_map(
    theme: &serde_json::Value,
    definition_ids: &std::collections::BTreeMap<String, u16>,
    field: &str,
    normalize_extension: bool,
) -> std::collections::BTreeMap<String, u16> {
    let mut map = std::collections::BTreeMap::new();
    let Some(object) = theme.get(field).and_then(serde_json::Value::as_object) else {
        return map;
    };
    for (key, value) in object {
        let Some(icon_name) = value.as_str() else {
            continue;
        };
        let Some(icon_id) = definition_ids.get(icon_name).copied() else {
            continue;
        };
        let key = if normalize_extension {
            key.trim_start_matches('.').to_ascii_lowercase()
        } else {
            key.to_ascii_lowercase()
        };
        if key.is_empty() {
            continue;
        }
        map.insert(key, icon_id);
    }
    map
}

fn emit_optional_icon_const<W: std::io::Write>(
    out: &mut W,
    name: &str,
    icon: Option<u16>,
) -> anyhow::Result<()> {
    match icon {
        Some(icon) => writeln!(
            out,
            "pub const {name}: Option<MaterialIcon> = Some(MaterialIcon({icon}));"
        )?,
        None => writeln!(out, "pub const {name}: Option<MaterialIcon> = None;")?,
    }
    Ok(())
}

fn emit_phf_map<W: std::io::Write>(
    out: &mut W,
    name: &str,
    values: &std::collections::BTreeMap<String, u16>,
) -> anyhow::Result<()> {
    let mut map = phf_codegen::Map::new();
    for (key, icon) in values {
        map.entry(key, &format!("MaterialIcon({icon})"));
    }
    writeln!(
        out,
        "pub static {name}: phf::Map<&'static str, MaterialIcon> = {};",
        map.build()
    )?;
    writeln!(out)?;
    Ok(())
}
