//! What the panel says, in the languages ThinkTerm speaks: the one it is
//! told, else English.

pub struct Words {
    /// There is no terminal on this machine beside the panel.
    pub no_terminal: &'static str,
    pub looking: &'static str,
    pub no_git: &'static str,
    pub not_repo: &'static str,
    /// Before why git could not be asked.
    pub failed: &'static str,
    pub clean: &'static str,
    pub unborn: &'static str,
    pub files: fn(usize) -> String,
    pub more: fn(usize) -> String,
    pub reading: &'static str,
    pub binary: &'static str,
    pub no_newline: &'static str,
    pub mode: &'static str,
    pub unchanged: &'static str,
    pub cut: &'static str,
}

/// The words for `locale`, a tag such as `zh-CN`.
pub fn words(locale: &str) -> &'static Words {
    let language = locale.split(['-', '_']).next().unwrap_or_default();
    match language {
        "zh" => &CHINESE,
        "ja" => &JAPANESE,
        "de" => &GERMAN,
        "fr" => &FRENCH,
        _ => &ENGLISH,
    }
}

static ENGLISH: Words = Words {
    no_terminal: "Open a folder in a terminal on this computer to see its changes here.",
    looking: "Looking for changes…",
    no_git: "git is not installed.",
    not_repo: "This folder is not in a git repository.",
    failed: "The changes could not be read: ",
    clean: "No changes.",
    unborn: "no commits yet",
    files: |n| match n {
        1 => "1 changed file".into(),
        n => format!("{n} changed files"),
    },
    more: |n| match n {
        1 => "and 1 more file".into(),
        n => format!("and {n} more files"),
    },
    reading: "Reading the changes…",
    binary: "Binary file: not shown.",
    no_newline: "No newline at end of file",
    mode: "Only the file's mode changed.",
    unchanged: "Renamed, without changes.",
    cut: "Too long: the rest is not shown.",
};

static CHINESE: Words = Words {
    no_terminal: "在本机的终端里打开一个目录，这里会显示它的改动。",
    looking: "正在查看改动…",
    no_git: "没有找到 git。",
    not_repo: "这个目录不在 git 仓库里。",
    failed: "读不到改动：",
    clean: "没有改动。",
    unborn: "还没有提交",
    files: |n| format!("{n} 个文件有改动"),
    more: |n| format!("还有 {n} 个文件没列出"),
    reading: "正在读取改动…",
    binary: "二进制文件，不显示内容。",
    no_newline: "文件末尾没有换行",
    mode: "只改了文件权限。",
    unchanged: "只改了名字，内容没变。",
    cut: "太长了，后面的不显示。",
};

static JAPANESE: Words = Words {
    no_terminal: "このコンピュータのターミナルでフォルダを開くと、その変更がここに表示されます。",
    looking: "変更を確認しています…",
    no_git: "git が見つかりません。",
    not_repo: "このフォルダは git リポジトリにありません。",
    failed: "変更を読み取れませんでした: ",
    clean: "変更はありません。",
    unborn: "まだコミットがありません",
    files: |n| format!("{n} 個のファイルに変更"),
    more: |n| format!("ほかに {n} 個のファイル"),
    reading: "変更を読み込んでいます…",
    binary: "バイナリファイルのため表示しません。",
    no_newline: "ファイルの末尾に改行がありません",
    mode: "ファイルのモードだけが変わりました。",
    unchanged: "名前だけが変わり、内容は同じです。",
    cut: "長すぎるため、以降は表示しません。",
};

static GERMAN: Words = Words {
    no_terminal: "Öffne einen Ordner in einem Terminal auf diesem Rechner, um hier seine Änderungen zu sehen.",
    looking: "Änderungen werden gesucht …",
    no_git: "git wurde nicht gefunden.",
    not_repo: "Dieser Ordner liegt in keinem git-Repository.",
    failed: "Die Änderungen konnten nicht gelesen werden: ",
    clean: "Keine Änderungen.",
    unborn: "noch keine Commits",
    files: |n| match n {
        1 => "1 geänderte Datei".into(),
        n => format!("{n} geänderte Dateien"),
    },
    more: |n| match n {
        1 => "und 1 weitere Datei".into(),
        n => format!("und {n} weitere Dateien"),
    },
    reading: "Änderungen werden gelesen …",
    binary: "Binärdatei, nicht angezeigt.",
    no_newline: "Kein Zeilenumbruch am Dateiende",
    mode: "Nur der Dateimodus wurde geändert.",
    unchanged: "Umbenannt, ohne Änderungen.",
    cut: "Zu lang: Der Rest wird nicht angezeigt.",
};

static FRENCH: Words = Words {
    no_terminal:
        "Ouvrez un dossier dans un terminal de cet ordinateur pour voir ici ses modifications.",
    looking: "Recherche des modifications…",
    no_git: "git est introuvable.",
    not_repo: "Ce dossier n'est dans aucun dépôt git.",
    failed: "Impossible de lire les modifications : ",
    clean: "Aucune modification.",
    unborn: "aucun commit",
    files: |n| match n {
        1 => "1 fichier modifié".into(),
        n => format!("{n} fichiers modifiés"),
    },
    more: |n| match n {
        1 => "et 1 autre fichier".into(),
        n => format!("et {n} autres fichiers"),
    },
    reading: "Lecture des modifications…",
    binary: "Fichier binaire, non affiché.",
    no_newline: "Pas de retour à la ligne en fin de fichier",
    mode: "Seul le mode du fichier a changé.",
    unchanged: "Renommé sans modification.",
    cut: "Trop long : la suite n'est pas affichée.",
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_language_is_found_by_its_tag_and_english_stands_in() {
        assert_eq!(words("zh-CN").clean, "没有改动。");
        assert_eq!(words("de_DE").clean, "Keine Änderungen.");
        assert_eq!(words("").clean, "No changes.");
        assert_eq!(words("pt-BR").clean, "No changes.");
        assert_eq!((words("en-US").files)(1), "1 changed file");
        assert_eq!((words("fr-FR").more)(3), "et 3 autres fichiers");
    }
}
