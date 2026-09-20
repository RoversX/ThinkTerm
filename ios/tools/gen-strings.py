#!/usr/bin/env python3
"""Build ios/Resources/strings.json, the app's string tables.

The prototype's tables (research/mobile-ui/i18n.js) are the source for the
keys it already had; EXTRA below carries the keys only the Swift app needs,
in the same five languages. research/ is not in the repository, so the
merged JSON is what ships: with the JS absent the script keeps the strings
already in strings.json and only adds what EXTRA brings.

    python3 ios/tools/gen-strings.py
"""

import json
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
PROTO = os.path.join(ROOT, "research", "mobile-ui", "i18n.js")
OUT = os.path.join(ROOT, "ios", "Resources", "strings.json")

LANGS = ["en-US", "zh-CN", "ja-JP", "fr-FR", "de-DE"]

# Keys the phone app has and the prototype never had. Same order as LANGS.
EXTRA = {
    # common
    "ok": ["OK", "好", "OK", "OK", "OK"],
    "copy": ["Copy", "复制", "コピー", "Copier", "Kopieren"],
    "p.layout": ["Layout", "布局", "レイアウト", "Disposition", "Layout"],
    "project.new": ["New project", "新建项目", "新規プロジェクト", "Nouveau projet", "Neues Projekt"],
    "sidebar": ["Sidebar", "侧栏", "サイドバー", "Barre latérale", "Seitenleiste"],
    "thread.delete.title": ["Delete “%@”?", "删除“%@”？", "「%@」を削除しますか？", "Supprimer « %@ » ?", "„%@“ löschen?"],
    "thread.delete.body": [
        "Its tabs close and the programs in them end.",
        "它的标签会关闭，其中的程序会结束。",
        "タブは閉じられ、実行中のプログラムは終了します。",
        "Ses onglets se ferment et les programmes qu’ils contiennent s’arrêtent.",
        "Seine Tabs werden geschlossen und die Programme darin beendet.",
    ],
    "more": ["More…", "更多…", "その他…", "Plus…", "Mehr…"],
    "rename": ["Rename", "重命名", "名前を変更", "Renommer", "Umbenennen"],
    "none": ["None", "无", "なし", "Aucun", "Keine"],
    "optional": ["Optional", "可选", "任意", "Facultatif", "Optional"],
    # hosts
    "hosts.nomatch.q": [
        "No hosts match “%@”.",
        "没有匹配“%@”的主机。",
        "「%@」に一致するホストはありません。",
        "Aucun hôte ne correspond à « %@ ».",
        "Keine Hosts passen zu „%@“.",
    ],
    "host.delete.title": [
        "Delete this host?",
        "删除这台主机？",
        "このホストを削除しますか？",
        "Supprimer cet hôte ?",
        "Diesen Host löschen?",
    ],
    "host.delete.confirm": ["Delete %@", "删除 %@", "%@ を削除", "Supprimer %@", "%@ löschen"],
    "host.delete.body": [
        "Its saved key or password is removed from the Keychain too.",
        "保存的密钥或密码也会一并从钥匙串里删掉。",
        "保存された鍵やパスワードもキーチェーンから削除されます。",
        "Sa clé ou son mot de passe enregistré est aussi retiré du trousseau.",
        "Der gesicherte Schlüssel oder das Passwort wird ebenfalls aus dem Schlüsselbund entfernt.",
    ],
    "host.copysuffix": [" copy", " 副本", " のコピー", " copie", " Kopie"],
    "badge.key": ["key", "密钥", "鍵", "clé", "Schlüssel"],
    "badge.password": ["password", "密码", "パスワード", "mot de passe", "Passwort"],
    # host editor
    "f.method": ["Method", "方式", "方式", "Méthode", "Methode"],
    "key.paste": [
        "Paste from clipboard",
        "从剪贴板粘贴",
        "クリップボードから貼り付け",
        "Coller depuis le presse-papiers",
        "Aus der Zwischenablage einsetzen",
    ],
    "key.import": ["Import file…", "导入文件…", "ファイルを読み込む…", "Importer un fichier…", "Datei importieren…"],
    "key.generate": [
        "Generate new key",
        "生成新密钥",
        "新しい鍵を生成",
        "Générer une nouvelle clé",
        "Neuen Schlüssel erzeugen",
    ],
    "key.pastetext": [
        "Paste key text",
        "粘贴密钥文本",
        "鍵のテキストを貼り付け",
        "Coller le texte de la clé",
        "Schlüsseltext einsetzen",
    ],
    "key.passphrase": [
        "Key passphrase (if any)",
        "密钥口令（如果有）",
        "鍵のパスフレーズ（あれば）",
        "Phrase secrète de la clé (le cas échéant)",
        "Schlüssel-Passphrase (falls vorhanden)",
    ],
    "key.loaded": ["Key loaded", "已载入密钥", "鍵を読み込みました", "Clé chargée", "Schlüssel geladen"],
    "key.loaded.ed25519": [
        "ed25519 key loaded",
        "已载入 ed25519 密钥",
        "ed25519 鍵を読み込みました",
        "Clé ed25519 chargée",
        "ed25519-Schlüssel geladen",
    ],
    "key.stored": ["Key stored", "密钥已保存", "鍵は保存済み", "Clé enregistrée", "Schlüssel gesichert"],
    "key.none": ["No key yet", "还没有密钥", "鍵がありません", "Pas encore de clé", "Noch kein Schlüssel"],
    "key.copypub": [
        "Copy public key",
        "复制公钥",
        "公開鍵をコピー",
        "Copier la clé publique",
        "Öffentlichen Schlüssel kopieren",
    ],
    "key.authorized": [
        "Add this line to ~/.ssh/authorized_keys on the host.",
        "把这行加到主机的 ~/.ssh/authorized_keys 里。",
        "この行をホストの ~/.ssh/authorized_keys に追加してください。",
        "Ajoutez cette ligne à ~/.ssh/authorized_keys sur l’hôte.",
        "Fügen Sie diese Zeile auf dem Host in ~/.ssh/authorized_keys ein.",
    ],
    "key.importfailed": [
        "Import failed",
        "导入失败",
        "読み込みに失敗しました",
        "Échec de l’import",
        "Import fehlgeschlagen",
    ],
    "key.notext": [
        "That file is not readable as text.",
        "这个文件不是可读的文本。",
        "そのファイルはテキストとして読めません。",
        "Ce fichier n’est pas lisible comme du texte.",
        "Diese Datei ist nicht als Text lesbar.",
    ],
    "f.remotecmd": ["Remote command", "远程命令", "リモートコマンド", "Commande distante", "Remote-Befehl"],
    "f.remotecmd.foot": [
        "Blank runs `thinkterm cli --prefer-mux proxy` on the host.",
        "留空就在主机上跑 `thinkterm cli --prefer-mux proxy`。",
        "空欄ならホストで `thinkterm cli --prefer-mux proxy` を実行します。",
        "Vide, la commande `thinkterm cli --prefer-mux proxy` est lancée sur l’hôte.",
        "Leer führt auf dem Host `thinkterm cli --prefer-mux proxy` aus.",
    ],
    "f.hostkey": ["Host key", "主机密钥", "ホスト鍵", "Clé de l’hôte", "Hostschlüssel"],
    # settings
    "cursor.auto": [
        "As the program sets",
        "跟随程序",
        "プログラムに従う",
        "Comme le programme",
        "Wie vom Programm gesetzt",
    ],
    "k.keybar.foot": [
        "The keys under the terminal. More of them live in the key panel, on the button at the bar's right end.",
        "终端下面那排键。更多按键在按键面板里，点按键条右端的按钮打开。",
        "ターミナル下のキーです。残りはキーバー右端のボタンで開くキーパネルにあります。",
        "Les touches sous le terminal. Les autres sont dans le panneau, ouvert par le bouton à droite de la barre.",
        "Die Tasten unter dem Terminal. Weitere liegen im Tastenfeld, über die Taste am rechten Ende der Leiste.",
    ],
    "theme.search": [
        "Search schemes",
        "搜索配色",
        "配色を検索",
        "Rechercher un thème",
        "Farbschemas durchsuchen",
    ],
    # tree and overview
    "otherwindows": ["Other windows", "其他窗口", "ほかのウインドウ", "Autres fenêtres", "Andere Fenster"],
    "window.n": ["Window %d", "窗口 %d", "ウインドウ %d", "Fenêtre %d", "Fenster %d"],
    "tab.one": ["%d tab", "%d 个标签", "タブ %d", "%d onglet", "%d Tab"],
    "tab.n": ["%d tabs", "%d 个标签", "タブ %d", "%d onglets", "%d Tabs"],
    "panes.n": ["%d panes", "%d 个窗格", "ペイン %d", "%d volets", "%d Bereiche"],
    "thread.off": ["off", "已关", "停止中", "arrêté", "aus"],
    "tab.num": ["Tab %d", "标签 %d", "タブ %d", "Onglet %d", "Tab %d"],
    "overview.none": [
        "No threads on this server.",
        "这台服务器上没有线程。",
        "このサーバーにスレッドはありません。",
        "Aucun fil sur ce serveur.",
        "Keine Threads auf diesem Server.",
    ],
    # terminal screen
    "m.threadstabs": ["Threads and tabs…", "线程与标签…", "スレッドとタブ…", "Fils et onglets…", "Threads & Tabs…"],
    "m.overview": ["Overview…", "总览…", "概要…", "Vue d’ensemble…", "Übersicht…"],
    "m.settings": ["Settings…", "设置…", "設定…", "Réglages…", "Einstellungen…"],
    "pane.title": ["Pane", "窗格", "ペイン", "Volet", "Bereich"],
    "tab.title": ["Tab", "标签", "タブ", "Onglet", "Tab"],
    "card.connecting": [
        "Connecting to %@",
        "正在连接 %@",
        "%@ に接続中",
        "Connexion à %@",
        "Verbinde mit %@",
    ],
    "card.attaching": [
        "Starting ThinkTerm on %@",
        "正在 %@ 上启动 ThinkTerm",
        "%@ で ThinkTerm を起動中",
        "Démarrage de ThinkTerm sur %@",
        "ThinkTerm wird auf %@ gestartet",
    ],
    "card.reconnecting": [
        "Reconnecting to %@…",
        "正在重连 %@…",
        "%@ に再接続中…",
        "Reconnexion à %@…",
        "Neu verbinden mit %@…",
    ],
    "card.failed": [
        "Couldn’t connect to %@",
        "无法连接 %@",
        "%@ に接続できません",
        "Connexion impossible à %@",
        "Keine Verbindung zu %@",
    ],
    "forgetretry": [
        "Forget key & retry",
        "忘记密钥并重试",
        "鍵を忘れて再試行",
        "Oublier la clé et réessayer",
        "Schlüssel vergessen & erneut",
    ],
    "hint.auth": [
        "The host did not accept the user name with this key or password.",
        "主机不接受这个用户名配这把密钥或这个密码。",
        "ホストがこのユーザ名と鍵（またはパスワード）を受け付けませんでした。",
        "L’hôte n’a pas accepté ce nom d’utilisateur avec cette clé ou ce mot de passe.",
        "Der Host hat den Benutzernamen mit diesem Schlüssel oder Passwort nicht akzeptiert.",
    ],
    "hint.keychanged": [
        "The host's key is not the one seen before. If the host was reinstalled, forget the old key.",
        "主机密钥和上次见到的不一样。如果主机重装过，就忘掉旧密钥。",
        "ホスト鍵が以前と違います。ホストを入れ直したなら、古い鍵を忘れてください。",
        "La clé de l’hôte n’est pas celle vue précédemment. Si l’hôte a été réinstallé, oubliez l’ancienne clé.",
        "Der Hostschlüssel ist nicht der bekannte. Wurde der Host neu installiert, vergessen Sie den alten Schlüssel.",
    ],
    "hint.unreachable": [
        "The host did not answer on this address and port. Check the network, or a VPN such as Tailscale.",
        "主机在这个地址和端口上没有应答。检查网络，或者 Tailscale 这类 VPN。",
        "このアドレスとポートでホストが応答しません。ネットワークや Tailscale などの VPN を確認してください。",
        "L’hôte n’a pas répondu à cette adresse et ce port. Vérifiez le réseau, ou un VPN tel que Tailscale.",
        "Der Host antwortet nicht auf dieser Adresse und diesem Port. Prüfen Sie das Netzwerk oder ein VPN wie Tailscale.",
    ],
    "hint.notfound": [
        "ThinkTerm is not installed on the host, or not on its PATH.",
        "主机上没装 ThinkTerm，或者它不在 PATH 里。",
        "ホストに ThinkTerm が入っていないか、PATH にありません。",
        "ThinkTerm n’est pas installé sur l’hôte, ou n’est pas dans son PATH.",
        "ThinkTerm ist auf dem Host nicht installiert oder nicht im PATH.",
    ],
    "hint.version": [
        "The host runs another version of ThinkTerm. Update one of the two.",
        "主机上的 ThinkTerm 是另一个版本。升级其中一边。",
        "ホストの ThinkTerm はバージョンが違います。どちらかを更新してください。",
        "L’hôte utilise une autre version de ThinkTerm. Mettez l’un des deux à jour.",
        "Auf dem Host läuft eine andere ThinkTerm-Version. Aktualisieren Sie eine von beiden.",
    ],
    # key bar and panel
    "k.function": ["Function", "功能键", "ファンクション", "Fonction", "Funktion"],
    "k.navigation": ["Navigation", "导航", "ナビゲーション", "Navigation", "Navigation"],
    "k.symbols": ["Symbols", "符号", "記号", "Symboles", "Zeichen"],
    "k.control": ["Control", "控制键", "コントロール", "Contrôle", "Steuerung"],
    "p.panel": ["Panel", "面板", "パネル", "Panneau", "Panel"],
    "snip.new": ["New snippet", "新建片段", "新しいスニペット", "Nouvel extrait", "Neuer Schnipsel"],
    "snip.title": ["Snippet", "片段", "スニペット", "Extrait", "Schnipsel"],
    "snip.command": ["Command", "命令", "コマンド", "Commande", "Befehl"],
    "snip.enter": [
        "Press Enter after sending",
        "发送后按回车",
        "送信後に Enter を押す",
        "Appuyer sur Entrée après l’envoi",
        "Nach dem Senden Enter drücken",
    ],
    "hist.empty": [
        "Nothing sent from here yet",
        "还没有从这里发送过内容",
        "ここから送ったものはまだありません",
        "Rien n’a encore été envoyé d’ici",
        "Von hier wurde noch nichts gesendet",
    ],
}


def prototype_tables():
    """The prototype's window.I18N object, or None when research/ is absent."""
    if not os.path.exists(PROTO):
        return None
    text = open(PROTO, encoding="utf-8").read()
    match = re.search(r"window\.I18N\s*=\s*(\{.*?\});\s*\n", text, re.S)
    if not match:
        sys.exit("i18n.js: no window.I18N object")
    return json.loads(match.group(1))


def main():
    tables = prototype_tables()
    if tables is None:
        if not os.path.exists(OUT):
            sys.exit("no research/mobile-ui/i18n.js and no strings.json to extend")
        tables = json.load(open(OUT, encoding="utf-8"))
        print("research/ absent: extending the shipped strings.json")
    for lang in LANGS:
        tables.setdefault(lang, {})
    for key, values in EXTRA.items():
        for lang, value in zip(LANGS, values):
            tables[lang][key] = value
    out = {lang: dict(sorted(tables[lang].items())) for lang in LANGS}
    with open(OUT, "w", encoding="utf-8") as f:
        json.dump(out, f, ensure_ascii=False, indent=1, sort_keys=True)
        f.write("\n")
    missing = [
        (lang, key)
        for key in out["en-US"]
        for lang in LANGS
        if key not in out[lang]
    ]
    print("%d keys x %d languages -> %s" % (len(out["en-US"]), len(LANGS), OUT))
    if missing:
        print("missing: %s" % missing[:10])


main()
