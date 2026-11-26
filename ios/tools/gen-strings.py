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
    # The chat's file and picture viewer
    "chat.file.loading": ["Opening…", "正在打开…", "開いています…", "Ouverture…", "Wird geöffnet…"],
    "chat.file.failed": ["Couldn't open this file", "打不开这个文件", "このファイルを開けません", "Impossible d’ouvrir ce fichier", "Datei kann nicht geöffnet werden"],
    "chat.file.copy_path": ["Copy path", "复制路径", "パスをコピー", "Copier le chemin", "Pfad kopieren"],
    # common
    "ok": ["OK", "好", "OK", "OK", "OK"],
    "copy": ["Copy", "复制", "コピー", "Copier", "Kopieren"],
    "p.layout": ["Layout", "布局", "レイアウト", "Disposition", "Layout"],
    "project.new": ["New project", "新建项目", "新規プロジェクト", "Nouveau projet", "Neues Projekt"],
    "sidebar": ["Sidebar", "侧栏", "サイドバー", "Barre latérale", "Seitenleiste"],
    # The desktop's tooltip-sidebar-thread-search, with this app's word for a thread.
    "thread.find": ["Find Thread", "查找会话", "セッションを検索", "Rechercher une session", "Thread finden"],
    "thread.find.none": ["No thread matches", "没有匹配的会话", "一致するセッションはありません", "Aucune session ne correspond", "Kein Thread passt"],
    "keyboard.incognito": ["Incognito keyboard", "无痕键盘", "シークレット キーボード", "Clavier incognito", "Inkognito-Tastatur"],
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
    "host.autoconnect": ["Connect automatically", "自动连接", "自動接続", "Connexion automatique", "Automatisch verbinden"],
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
    "hostkey.approve.title": [
        "Trust this SSH host key?",
        "信任这个 SSH 主机密钥？",
        "この SSH ホスト鍵を信頼しますか？",
        "Faire confiance à cette clé d’hôte SSH ?",
        "Diesem SSH-Hostschlüssel vertrauen?",
    ],
    "hostkey.approve.body": [
        "Before sending a password or authenticating with a key, verify this SHA-256 fingerprint with the server administrator over a trusted channel. Cancel if it differs.",
        "发送密码或进行密钥认证前，请通过可信渠道与服务器管理员核对以上 SHA-256 指纹。不一致请取消。",
        "パスワードを送信したり鍵で認証したりする前に、信頼できる通信手段でサーバー管理者にこの SHA-256 フィンガープリントを確認してください。一致しない場合はキャンセルしてください。",
        "Avant d’envoyer un mot de passe ou de vous authentifier avec une clé, vérifiez cette empreinte SHA-256 auprès de l’administrateur du serveur via un canal de confiance. Annulez si elle ne correspond pas.",
        "Bevor Sie ein Passwort senden oder sich mit einem Schlüssel authentifizieren, gleichen Sie diesen SHA-256-Fingerabdruck über einen vertrauenswürdigen Kanal mit dem Serveradministrator ab. Brechen Sie bei einer Abweichung ab.",
    ],
    "hostkey.approve.accept": [
        "Trust and connect",
        "信任并连接",
        "信頼して接続",
        "Faire confiance et se connecter",
        "Vertrauen und verbinden",
    ],
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
        "这台服务器上没有会话。",
        "このサーバーにスレッドはありません。",
        "Aucun fil sur ce serveur.",
        "Keine Threads auf diesem Server.",
    ],
    # terminal screen
    "m.threadstabs": ["Threads and tabs…", "会话与标签…", "スレッドとタブ…", "Fils et onglets…", "Threads & Tabs…"],
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
    # files left on the host from the phone
    "attach": ["Attach", "添加附件", "添付", "Joindre", "Anhängen"],
    "attach.photo": ["Photo Library", "照片图库", "写真ライブラリ", "Photothèque", "Fotomediathek"],
    "attach.file": ["Choose File", "选取文件", "ファイルを選択", "Choisir un fichier", "Datei auswählen"],
    "attach.paste": ["Paste Image", "粘贴图片", "画像をペースト", "Coller l’image", "Bild einsetzen"],
    "upload.unread": ["Could not read %@", "无法读取 %@", "%@ を読み込めません", "Impossible de lire %@", "%@ kann nicht gelesen werden"],
    # the app's own device in a tailnet
    "tn.join": ["Join a Tailnet", "加入 Tailnet", "Tailnet に参加", "Rejoindre un tailnet", "Einem Tailnet beitreten"],
    "tn.join.foot": ["ThinkTerm joins as a device of its own: the Tailscale app is not needed. Experimental.", "ThinkTerm 以独立设备加入，不需要另装 Tailscale。实验功能。", "ThinkTerm は独立したデバイスとして参加します。Tailscale アプリは不要です。試験的な機能です。", "ThinkTerm rejoint le tailnet comme un appareil à part : l’app Tailscale n’est pas nécessaire. Expérimental.", "ThinkTerm tritt als eigenes Gerät bei: Die Tailscale-App wird nicht benötigt. Experimentell."],
    "tn.off": ["Off", "未开启", "オフ", "Désactivé", "Aus"],
    "tn.starting": ["Connecting…", "正在连接…", "接続中…", "Connexion…", "Verbinden …"],
    "tn.needslogin": ["Needs login", "需要登录", "ログインが必要", "Connexion requise", "Anmeldung erforderlich"],
    "tn.connected": ["Connected as %@", "已连接：%@", "%@ として接続済み", "Connecté en tant que %@", "Verbunden als %@"],
    "tn.failed": ["Not connected", "未连上", "未接続", "Non connecté", "Nicht verbunden"],
    "tn.login": ["Log In…", "登录…", "ログイン…", "Se connecter…", "Anmelden …"],
    "tn.copylink": ["Copy Login Link", "复制登录链接", "ログインリンクをコピー", "Copier le lien de connexion", "Anmeldelink kopieren"],
    # The first run's pages (ThinkTerm's own tagline, from the READMEs).
    "ob.tagline": ["Your machines fuse into one.", "让你的多台机器融为一体。", "複数のマシンを、ひとつの作業環境に。", "Vos machines réunies dans un même espace de travail.", "Deine Rechner werden zu einer gemeinsamen Arbeitsumgebung."],
    "ob.continue": ["Continue", "继续", "続ける", "Continuer", "Weiter"],
    "ob.connect": ["Connect a Computer", "连接电脑", "コンピュータに接続", "Connecter un ordinateur", "Computer verbinden"],
    "ob.connect.body": ["The computer needs ThinkTerm installed.", "电脑上需要装有 ThinkTerm。", "コンピュータに ThinkTerm がインストールされている必要があります。", "ThinkTerm doit être installé sur l’ordinateur.", "Auf dem Computer muss ThinkTerm installiert sein."],
    "ob.tailscale": ["Sign In with Tailscale", "通过 Tailscale 登录", "Tailscale でログイン", "Se connecter avec Tailscale", "Mit Tailscale anmelden"],
    "ob.manual": ["Add Manually", "手动添加", "手動で追加", "Ajouter manuellement", "Manuell hinzufügen"],
    "ob.later": ["Later", "稍后", "あとで", "Plus tard", "Später"],
    "ob.welcome": ["Welcome", "欢迎", "ようこそ", "Bienvenue", "Willkommen"],
    "ob.start": ["Get Started", "开始使用", "はじめる", "Commencer", "Los geht’s"],
    "ob.show": ["Show Welcome", "显示欢迎页", "ようこそ画面を表示", "Afficher l’introduction", "Begrüßung anzeigen"],
    "f.icon": ["Icon", "图标", "アイコン", "Icône", "Symbol"],
    "host.open": ["Open", "打开", "開く", "Ouvrir", "Öffnen"],
    "host.connected": ["Connected", "已连接", "接続済み", "Connecté", "Verbunden"],
    "host.notconnected": ["Not Connected", "未连接", "未接続", "Non connecté", "Nicht verbunden"],
    "host.duplicate": ["Duplicate Host", "复制这台主机", "ホストを複製", "Dupliquer l’hôte", "Host duplizieren"],
    "host.delete": ["Delete Host", "删除主机", "ホストを削除", "Supprimer l’hôte", "Host löschen"],
    "about.versionline": ["Version %@", "版本 %@", "バージョン %@", "Version %@", "Version %@"],
    "about.copy": ["Copy Info", "复制信息", "情報をコピー", "Copier les infos", "Infos kopieren"],
    "about.build": ["Build", "构建", "ビルド", "Compilation", "Build"],
    "about.platform": ["Platform", "平台", "プラットフォーム", "Plateforme", "Plattform"],
    "about.license": ["License", "许可证", "ライセンス", "Licence", "Lizenz"],
    "about.source": ["Source Code", "源代码", "ソースコード", "Code source", "Quellcode"],
    "about.releasenotes": ["Release Notes", "发布说明", "リリースノート", "Notes de version", "Release-Notes"],
    "about.notices": ["Notices", "第三方声明", "表記", "Mentions", "Hinweise"],
    "about.privacy": ["Privacy", "隐私", "プライバシー", "Confidentialité", "Datenschutz"],
    "tn.added": ["Added", "已添加", "追加済み", "Ajouté", "Hinzugefügt"],
    "tn.signin": ["Sign In to Tailscale", "登录 Tailscale", "Tailscale にログイン", "Se connecter à Tailscale", "Bei Tailscale anmelden"],
    "tn.login.body": ["Sign in to Tailscale in the browser; it connects by itself once you have.", "在浏览器里登录 Tailscale，登录后自动连上。", "ブラウザで Tailscale にログインすると、自動で接続します。", "Connectez-vous à Tailscale dans le navigateur ; la connexion se fait d’elle-même ensuite.", "Melde dich im Browser bei Tailscale an; danach verbindet es sich von selbst."],
    "tn.connected.body": ["This phone is in your tailnet; its other machines are on the Hosts page.", "这台手机在你的 tailnet 里，主机页能看到其他机器。", "この端末は tailnet に参加しています。ほかのマシンはホストのページにあります。", "Ce téléphone est dans votre tailnet ; ses autres machines sont sur la page Hôtes.", "Dieses Telefon ist in deinem Tailnet; die anderen Geräte findest du unter Hosts."],
    "tn.qr.title": ["Sign In on Another Device", "用另一台设备登录", "別の端末でログイン", "Se connecter sur un autre appareil", "Auf einem anderen Gerät anmelden"],
    "tn.thisdevice": ["This Device", "这台设备", "この端末", "Cet appareil", "Dieses Gerät"],
    "tn.address": ["Address", "地址", "アドレス", "Adresse", "Adresse"],
    "tn.machines": ["Other Machines", "其他机器", "ほかのマシン", "Autres machines", "Andere Geräte"],
    "tn.online.count": ["%@ online", "%@ 台在线", "%@ 台オンライン", "%@ en ligne", "%@ online"],
    "tn.turnoff": ["Turn Off for Now", "暂时关闭", "一時的にオフ", "Désactiver pour l’instant", "Vorerst ausschalten"],
    "tn.turnoff.foot": ["Turning off keeps the sign-in: on again, it connects at once. Logging out needs a new sign-in.", "暂时关闭会保留登录，下次打开直接连上；退出登录后要重新登录。", "オフにしてもログインは残り、次にオンにするとすぐ接続します。ログアウトすると再ログインが必要です。", "Désactiver garde la connexion au compte : réactivé, il se connecte aussitôt. Se déconnecter demande de se reconnecter.", "Ausschalten behält die Anmeldung: wieder eingeschaltet, verbindet es sich sofort. Nach dem Abmelden musst du dich neu anmelden."],
    # Settings, About: what the app ships that is not its own (Licenses.swift).
    "licenses": ["Open Source Licenses", "开源许可", "オープンソースライセンス", "Licences open source", "Open-Source-Lizenzen"],
    "licenses.assets": ["Fonts and Icons", "字体和图标", "フォントとアイコン", "Polices et icônes", "Schriften und Symbole"],
    "licenses.rust": ["Rust Libraries", "Rust 库", "Rust ライブラリ", "Bibliothèques Rust", "Rust-Bibliotheken"],
    "licenses.android": ["Android Libraries", "Android 库", "Android ライブラリ", "Bibliothèques Android", "Android-Bibliotheken"],
    "licenses.includes": ["Includes %@", "包含 %@", "含まれるもの: %@", "Comprend %@", "Enthält %@"],
    "licenses.search": ["Search Licenses", "搜索许可", "ライセンスを検索", "Rechercher une licence", "Lizenzen durchsuchen"],
    "tn.qr": ["Or scan this with another device's camera and log in there.", "也可以用另一台设备的相机扫这个码，在那台设备上登录。", "または別のデバイスのカメラでこのコードを読み取り、そちらでログインします。", "Ou scannez ce code avec l’appareil photo d’un autre appareil et connectez-vous sur celui-ci.", "Oder scannen Sie diesen Code mit der Kamera eines anderen Geräts und melden Sie sich dort an."],
    "tn.authkey": ["Auth Key", "认证密钥", "認証キー", "Clé d’authentification", "Auth-Schlüssel"],
    "tn.control": ["Control Server", "控制服务器", "コントロールサーバー", "Serveur de contrôle", "Kontrollserver"],
    "tn.name": ["Device Name", "设备名称", "デバイス名", "Nom de l’appareil", "Gerätename"],
    "tn.auto": ["Automatic", "自动", "自動", "Automatique", "Automatisch"],
    "tn.logout": ["Log Out", "退出登录", "ログアウト", "Se déconnecter", "Abmelden"],
    "tn.online": ["Online", "在线", "オンライン", "En ligne", "Online"],
    "tn.offline": ["Offline", "离线", "オフライン", "Hors ligne", "Offline"],
    "tn.none": ["No other machines in this tailnet.", "这个 tailnet 里没有其他机器。", "この tailnet に他のマシンはありません。", "Aucune autre machine dans ce tailnet.", "Keine weiteren Geräte in diesem Tailnet."],
    "tn.needsapproval": ["Waiting for approval", "等待批准", "承認待ち", "En attente d’approbation", "Wartet auf Genehmigung"],
    "tn.approval.hint": ["This device is signed in and waits for an admin to approve it in the tailnet’s admin console. It connects by itself within a minute or two of being approved. If it joined with an auth key, check that the key is still valid.", "这台设备已登录，正在等管理员在 tailnet 管理后台批准。批准后一两分钟内会自动连上。如果是用认证密钥加入的，请确认密钥仍然有效。", "このデバイスはサインイン済みで、tailnet の管理コンソールで管理者が承認するのを待っています。承認されると 1～2 分以内に自動で接続します。認証キーで参加した場合は、キーが有効か確認してください。", "Cet appareil est connecté et attend qu’un administrateur l’approuve dans la console d’administration du tailnet. Il se connecte de lui-même une à deux minutes après l’approbation. S’il a rejoint le tailnet avec une clé d’authentification, vérifiez qu’elle est toujours valide.", "Dieses Gerät ist angemeldet und wartet darauf, dass ein Admin es in der Admin-Konsole des Tailnets genehmigt. Nach der Genehmigung verbindet es sich innerhalb von ein bis zwei Minuten von selbst. Wurde es mit einem Auth-Schlüssel hinzugefügt, prüfen Sie, ob der Schlüssel noch gültig ist."],
    # the right sidebar
    "panel.right": ["Right Sidebar", "右侧栏", "右サイドバー", "Barre latérale droite", "Rechte Seitenleiste"],
    "panel.agents": ["Agents", "Agents", "エージェント", "Agents", "Agenten"],
    # Under an agent's open sub-tasks: how many more there are than it
    # lists, as the desktop's right-agents-more-subtasks says it.
    "agents.more": ["%d more", "还有 %d 个", "ほか %d 件", "%d de plus", "%d weitere"],
    # how the sidebar shows the Spaces
    "i.spaces": ["Spaces in the Sidebar", "侧栏里的 Space", "サイドバーの Space", "Spaces dans la barre latérale", "Spaces in der Seitenleiste"],
    "i.spaces.list": ["List", "列表", "リスト", "Liste", "Liste"],
    "i.spaces.menu": ["Menu", "下拉菜单", "メニュー", "Menu", "Menü"],
    # the panes of a tab, one at a time
    "panes.title": ["Panes", "窗格", "ペイン", "Volets", "Bereiche"],
    "pane.split.right": ["Split Right", "右边新开", "右に分割", "Diviser à droite", "Rechts teilen"],
    "pane.split.below": ["Split Below", "下面新开", "下に分割", "Diviser en bas", "Unten teilen"],
    "pane.close": ["Close Pane", "关闭窗格", "ペインを閉じる", "Fermer le volet", "Bereich schließen"],
    "keyboard.show": ["Show Keyboard", "显示键盘", "キーボードを表示", "Afficher le clavier", "Tastatur einblenden"],
    "keyboard.hide": ["Hide Keyboard", "收起键盘", "キーボードを隠す", "Masquer le clavier", "Tastatur ausblenden"],
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
    # jump hosts: a host reached through another saved one
    "f.jump": ["Jump host", "跳板机", "踏み台ホスト", "Hôte de rebond", "Sprunghost"],
    "f.jump.none": ["None", "无", "なし", "Aucun", "Keiner"],
    "f.jump.foot": [
        "Reach this host through another saved host, as ssh -J does. Each signs in with its own login, and its key is checked on its own.",
        "经另一台已保存的主机连到这台，和 ssh -J 一样。两台各用自己的登录方式，指纹也各自确认。",
        "ssh -J と同じように、保存済みの別のホストを経由してこのホストに接続します。それぞれ自分のログイン情報でサインインし、鍵もそれぞれ確認します。",
        "Atteindre cet hôte via un autre hôte enregistré, comme ssh -J. Chacun se connecte avec ses propres identifiants et sa clé est vérifiée à part.",
        "Diesen Host über einen anderen gespeicherten Host erreichen, wie mit ssh -J. Jeder meldet sich mit seinen eigenen Zugangsdaten an, und sein Schlüssel wird für sich geprüft.",
    ],
    "f.jump.isjump": [
        "Other hosts are reached through this one, so it can't have a jump host of its own.",
        "有别的主机经它连接，所以它自己不能再设跳板机。",
        "他のホストがこのホストを経由しているため、このホスト自体には踏み台を設定できません。",
        "D'autres hôtes passent par celui-ci : il ne peut pas avoir son propre hôte de rebond.",
        "Andere Hosts werden über diesen erreicht, daher kann er selbst keinen Sprunghost haben.",
    ],
    "jump.via": ["via %@", "经由 %@", "%@ 経由", "via %@", "über %@"],
    "jump.inuse.title": [
        "This host is a jump host",
        "这台主机正被当作跳板机",
        "このホストは踏み台として使われています",
        "Cet hôte sert d'hôte de rebond",
        "Dieser Host ist ein Sprunghost",
    ],
    "jump.inuse.body": [
        "%@ is reached through it. Change that host's jump host first, then delete this one.",
        "%@ 经它连接。先把那台的跳板机改掉，再删这台。",
        "%@ がこのホストを経由しています。先にそのホストの踏み台を変更してから削除してください。",
        "%@ passe par lui. Changez d'abord l'hôte de rebond de cet hôte, puis supprimez celui-ci.",
        "%@ wird über ihn erreicht. Ändern Sie zuerst den Sprunghost dieses Hosts und löschen Sie dann diesen.",
    ],
    "jump.missing": [
        "the jump host this host is reached through is no longer saved",
        "这台主机设的跳板机已经不在了",
        "このホストが経由する踏み台ホストはもう保存されていません",
        "l'hôte de rebond par lequel passe cet hôte n'est plus enregistré",
        "der Sprunghost, über den dieser Host erreicht wird, ist nicht mehr gespeichert",
    ],
    "hostkey.approve.jumptitle": [
        "Trust the jump host's key?",
        "信任跳板机的主机密钥？",
        "踏み台ホストの鍵を信頼しますか？",
        "Faire confiance à la clé de l'hôte de rebond ?",
        "Dem Schlüssel des Sprunghosts vertrauen?",
    ],
    "hostkey.approve.jumpwho": [
        "This is the key of %@, the jump host on the way.",
        "这是路上经过的跳板机 %@ 的密钥。",
        "これは経由する踏み台ホスト %@ の鍵です。",
        "C'est la clé de %@, l'hôte de rebond sur le chemin.",
        "Dies ist der Schlüssel von %@, dem Sprunghost auf dem Weg.",
    ],
    # Android's notification while it keeps connections up in the background
    "conn.notify.channel": ["Connections", "连接", "接続", "Connexions", "Verbindungen"],
    "conn.notify.title": [
        "Keeping connections open",
        "正在保持连接",
        "接続を維持しています",
        "Maintien des connexions",
        "Verbindungen bleiben offen",
    ],
    # The app lock (ios/AppLockKit, android/applock) and its Settings page
    "sec.security": ["Security", "安全", "セキュリティ", "Sécurité", "Sicherheit"],
    "lock.app": ["Lock ThinkTerm", "锁定 ThinkTerm", "ThinkTerm をロック", "Verrouiller ThinkTerm", "ThinkTerm sperren"],
    "lock.after": ["Lock After", "离开多久后锁定", "ロックまでの時間", "Verrouiller après", "Sperren nach"],
    "lock.after.60": ["1 minute", "1 分钟", "1 分", "1 minute", "1 Minute"],
    "lock.after.300": ["5 minutes", "5 分钟", "5 分", "5 minutes", "5 Minuten"],
    "lock.after.600": ["10 minutes", "10 分钟", "10 分", "10 minutes", "10 Minuten"],
    "lock.after.1200": ["20 minutes", "20 分钟", "20 分", "20 minutes", "20 Minuten"],
    "lock.after.3600": ["1 hour", "1 小时", "1 時間", "1 heure", "1 Stunde"],
    "lock.note.ios": [
        "Opening ThinkTerm asks for Face ID or your passcode. Back within the time set, it does not ask again.",
        "打开 ThinkTerm 时需要面容 ID 或密码。离开不超过设定的时间，回来不用再验证。",
        "ThinkTerm を開くと Face ID またはパスコードを求めます。設定した時間内に戻れば、もう一度は求めません。",
        "À l’ouverture, ThinkTerm demande Face ID ou votre code. De retour dans le délai choisi, il ne redemande pas.",
        "Beim Öffnen fragt ThinkTerm nach Face ID oder Ihrem Code. Zurück innerhalb der eingestellten Zeit, fragt es nicht erneut.",
    ],
    "lock.note.android": [
        "Opening ThinkTerm asks for your fingerprint, face or screen lock. Back within the time set, it does not ask again.",
        "打开 ThinkTerm 时需要指纹、人脸或屏幕锁。离开不超过设定的时间，回来不用再验证。",
        "ThinkTerm を開くと指紋、顔、または画面ロックを求めます。設定した時間内に戻れば、もう一度は求めません。",
        "À l’ouverture, ThinkTerm demande votre empreinte, votre visage ou le verrouillage de l’écran. De retour dans le délai choisi, il ne redemande pas.",
        "Beim Öffnen fragt ThinkTerm nach Fingerabdruck, Gesicht oder Displaysperre. Zurück innerhalb der eingestellten Zeit, fragt es nicht erneut.",
    ],
    "lock.nopasscode.ios": [
        "Set a passcode in the iPhone’s Settings first.",
        "请先在 iPhone 的设置里设置密码。",
        "先に iPhone の設定でパスコードを設定してください。",
        "Définissez d’abord un code dans les Réglages de l’iPhone.",
        "Legen Sie zuerst in den Einstellungen des iPhone einen Code fest.",
    ],
    "lock.nopasscode.android": [
        "Set a screen lock in the phone’s Settings first.",
        "请先在手机的设置里设置屏幕锁。",
        "先にスマートフォンの設定で画面ロックを設定してください。",
        "Définissez d’abord un verrouillage de l’écran dans les paramètres du téléphone.",
        "Richten Sie zuerst in den Einstellungen des Telefons eine Displaysperre ein.",
    ],
    "lock.locked": ["ThinkTerm is locked", "ThinkTerm 已锁定", "ThinkTerm はロックされています", "ThinkTerm est verrouillé", "ThinkTerm ist gesperrt"],
    "lock.unlock": ["Unlock", "解锁", "ロック解除", "Déverrouiller", "Entsperren"],
    "lock.reason": ["Unlock ThinkTerm", "解锁 ThinkTerm", "ThinkTerm のロックを解除", "Déverrouiller ThinkTerm", "ThinkTerm entsperren"],
    "lock.reason.on": [
        "Confirm it’s you to turn on the lock",
        "验证身份以开启锁定",
        "本人確認してロックをオンにします",
        "Confirmez votre identité pour activer le verrouillage",
        "Bestätigen Sie Ihre Identität, um die Sperre einzuschalten",
    ],
    "lock.reason.off": [
        "Confirm it’s you to turn off the lock",
        "验证身份以关闭锁定",
        "本人確認してロックをオフにします",
        "Confirmez votre identité pour désactiver le verrouillage",
        "Bestätigen Sie Ihre Identität, um die Sperre auszuschalten",
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
