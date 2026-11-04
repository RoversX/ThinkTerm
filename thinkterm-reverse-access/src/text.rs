//! What native authentication says. It is chosen here, in the actual host,
//! from the frozen intent: a caller picks a language, never the words, so no
//! language can make it describe less than is granted. On macOS the system's
//! dialog is the only window, so `reason` alone has to state all of that.

use serde::Serialize;

struct Language {
    code: &'static str,
    title: &'static str,
    heading: &'static str,
    connection: &'static str,
    all_terminals: &'static str,
    terminals: &'static str,
    access: &'static str,
    read_only: &'static str,
    read_and_input: &'static str,
    duration: &'static str,
    warning: &'static str,
    approve: &'static str,
    cancel: &'static str,
    /// Finishes the system's "… is trying to", and ends with the source so
    /// that a long one cannot push the rest out of the dialog.
    reason: &'static str,
    all_local: &'static str,
    selected: &'static str,
    password: &'static str,
}

const ENGLISH: Language = Language {
    code: "en-US",
    title: "ThinkTerm Connect — Allow terminal access",
    heading: "Allow this connection to access your terminals",
    connection: "Connection: {}",
    all_terminals: "All local terminals you own, including terminals opened while this authorization is active.",
    terminals: "Terminals:",
    access: "Access: {}",
    read_only: "Read only",
    read_and_input: "Read and send input",
    duration: "Duration: {} minutes",
    warning: "Sending input can execute commands with your account's permissions.",
    approve: "Authenticate and allow",
    cancel: "Cancel",
    reason: "allow access to {scope} ({access}, {minutes} minutes): {source}",
    all_local: "all local terminals",
    selected: "the selected terminals",
    password: "ThinkTerm Connect — System authentication",
};

const LANGUAGES: [Language; 6] = [
    ENGLISH,
    Language {
        code: "zh-CN",
        title: "ThinkTerm Connect — 允许访问终端",
        heading: "允许此连接访问你的终端",
        connection: "连接：{}",
        all_terminals: "你拥有的全部本地终端，包括授权期间新开的终端。",
        terminals: "终端：",
        access: "权限：{}",
        read_only: "只读",
        read_and_input: "读取和输入",
        duration: "时长：{} 分钟",
        warning: "输入可使用你的账户权限运行命令。",
        approve: "认证并允许",
        cancel: "取消",
        reason: "允许访问{scope}（{access}，{minutes} 分钟）：{source}",
        all_local: "全部本地终端",
        selected: "选定的终端",
        password: "ThinkTerm Connect — 系统认证",
    },
    Language {
        code: "de-DE",
        title: "ThinkTerm Connect — Terminalzugriff erlauben",
        heading: "Dieser Verbindung den Zugriff auf Ihre Terminals erlauben",
        connection: "Verbindung: {}",
        all_terminals: "Alle Ihre lokalen Terminals, auch die, die während dieser Autorisierung geöffnet werden.",
        terminals: "Terminals:",
        access: "Zugriff: {}",
        read_only: "Nur lesen",
        read_and_input: "Lesen und Eingaben senden",
        duration: "Dauer: {} Minuten",
        warning: "Eingaben können Befehle mit den Rechten Ihres Kontos ausführen.",
        approve: "Authentifizieren und erlauben",
        cancel: "Abbrechen",
        reason: "den Zugriff auf {scope} erlauben ({access}, {minutes} Minuten): {source}",
        all_local: "alle lokalen Terminals",
        selected: "die ausgewählten Terminals",
        password: "ThinkTerm Connect — Systemauthentifizierung",
    },
    Language {
        code: "fr-FR",
        title: "ThinkTerm Connect — Autoriser l’accès aux terminaux",
        heading: "Autoriser cette connexion à accéder à vos terminaux",
        connection: "Connexion : {}",
        all_terminals: "Tous vos terminaux locaux, y compris ceux ouverts pendant cette autorisation.",
        terminals: "Terminaux :",
        access: "Accès : {}",
        read_only: "Lecture seule",
        read_and_input: "Lecture et saisie",
        duration: "Durée : {} minutes",
        warning: "La saisie peut exécuter des commandes avec les droits de votre compte.",
        approve: "S’authentifier et autoriser",
        cancel: "Annuler",
        reason: "autoriser l’accès {scope} ({access}, {minutes} minutes) : {source}",
        all_local: "à tous les terminaux locaux",
        selected: "aux terminaux sélectionnés",
        password: "ThinkTerm Connect — Authentification système",
    },
    Language {
        code: "ja-JP",
        title: "ThinkTerm Connect — ターミナルへのアクセスを許可",
        heading: "この接続にターミナルへのアクセスを許可します",
        connection: "接続: {}",
        all_terminals: "所有するすべてのローカルターミナル（この認可の間に開くものを含む）。",
        terminals: "ターミナル:",
        access: "アクセス: {}",
        read_only: "読み取りのみ",
        read_and_input: "読み取りと入力",
        duration: "期間: {} 分",
        warning: "入力を送信すると、あなたのアカウントの権限でコマンドを実行できます。",
        approve: "認証して許可",
        cancel: "キャンセル",
        reason: "{scope}へのアクセスを許可する（{access}、{minutes} 分）: {source}",
        all_local: "すべてのローカルターミナル",
        selected: "選択したターミナル",
        password: "ThinkTerm Connect — システム認証",
    },
    Language {
        code: "ru-RU",
        title: "ThinkTerm Connect — разрешить доступ к терминалам",
        heading: "Разрешить этому подключению доступ к вашим терминалам",
        connection: "Подключение: {}",
        all_terminals: "Все ваши локальные терминалы, включая открытые во время действия этого разрешения.",
        terminals: "Терминалы:",
        access: "Доступ: {}",
        read_only: "Только чтение",
        read_and_input: "Чтение и ввод",
        duration: "Длительность: {} мин.",
        warning: "Ввод может выполнять команды с правами вашей учётной записи.",
        approve: "Аутентифицироваться и разрешить",
        cancel: "Отмена",
        reason: "разрешить доступ к {scope} ({access}, {minutes} мин.): {source}",
        all_local: "всем локальным терминалам",
        selected: "выбранным терминалам",
        password: "ThinkTerm Connect — системная аутентификация",
    },
];

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct Text {
    title: String,
    heading: String,
    connection: String,
    terminals: String,
    detail: String,
    approve: String,
    cancel: String,
    reason: String,
    password: String,
}

impl Text {
    pub(crate) fn new(
        language: &str,
        source: &str,
        all_terminals: bool,
        input: bool,
        seconds: u64,
    ) -> (Self, &'static str) {
        let text = LANGUAGES
            .iter()
            .find(|text| text.code == language)
            .unwrap_or(&ENGLISH);
        let access = if input {
            text.read_and_input
        } else {
            text.read_only
        };
        let minutes = seconds.div_ceil(60).to_string();
        let detail = format!(
            "{}\n{}\n\n{}",
            text.access.replace("{}", access),
            text.duration.replace("{}", &minutes),
            text.warning,
        );
        // The source last: what it contains is not a placeholder.
        let reason = text
            .reason
            .replace(
                "{scope}",
                if all_terminals {
                    text.all_local
                } else {
                    text.selected
                },
            )
            .replace("{access}", access)
            .replace("{minutes}", &minutes)
            .replace("{source}", source);
        let scope = if all_terminals {
            text.all_terminals
        } else {
            ""
        };
        (
            Self {
                title: text.title.into(),
                heading: text.heading.into(),
                connection: text.connection.replace("{}", source),
                terminals: text.terminals.into(),
                detail,
                approve: text.approve.into(),
                cancel: text.cancel.into(),
                reason,
                password: text.password.into(),
            },
            scope,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_states_the_frozen_access() {
        for language in &LANGUAGES {
            let (read, scope) = Text::new(language.code, "server-a", true, false, 1800);
            let (input, _) = Text::new(language.code, "server-a", true, true, 1800);
            assert!(read.connection.contains("server-a"));
            assert!(read.detail.contains(language.read_only));
            assert!(read.detail.contains("30"));
            assert!(input.detail.contains(language.read_and_input));
            assert_eq!(scope, language.all_terminals);
            assert!(!read.detail.contains("{}") && !read.connection.contains("{}"));

            // macOS shows nothing else.
            assert!(read.reason.contains("server-a"));
            assert!(read.reason.contains(language.all_local));
            assert!(read.reason.contains(language.read_only));
            assert!(read.reason.contains("30"));
            assert!(input.reason.contains(language.read_and_input));
            assert!(!read.reason.contains('{'));
            let (selected, _) = Text::new(language.code, "server-a", false, false, 1800);
            assert!(selected.reason.contains(language.selected));
        }
    }

    #[test]
    fn the_longest_source_fits_what_the_helpers_accept_and_stays_a_source() {
        let source = "{minutes}".repeat(28);
        assert!(source.len() <= 256);
        for language in &LANGUAGES {
            let (text, _) = Text::new(language.code, &source, true, true, 7200);
            assert!(text.reason.len() <= 512);
            assert!(text.reason.ends_with(&source));
        }
    }

    #[test]
    fn an_unknown_language_reads_as_english() {
        let (text, _) = Text::new("xx-XX", "server-a", false, false, 60);
        assert_eq!(text.title, ENGLISH.title);
    }
}
