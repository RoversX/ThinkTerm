# ThinkTerm

[English](README.md) · [简体中文](README.zh-CN.md) · **日本語** · [Français](README.fr-FR.md) · [Deutsch](README.de-DE.md)

## 📥 ダウンロード

**[こちらからダウンロード](https://github.com/RoversX/ThinkTerm/releases)** - 最新リリースを入手

🍺 **Homebrew**（macOS）： 
```
brew install --cask roversx/tap/thinkterm
```

🌐 **公式サイト**: [closex.org/thinkterm](https://closex.org/thinkterm/)

📚 **ドキュメント**: [docs.closex.org/thinkterm](https://docs.closex.org/thinkterm/)

⭐ よろしければ [ThinkTerm](https://github.com/RoversX/ThinkTerm) にスターをお願いします！

**複数のマシンを、ひとつの作業環境に。**

ThinkTerm は [WezTerm](https://github.com/wezterm/wezterm) を基盤とした、**Rust で書かれた、マルチプレクサー内蔵のオープンソースターミナル**です。mux サーバーがシェル、ツール、コーディングエージェントを実行し、デスクトップ、TUI、ブラウザーの各クライアントがそのセッションに接続します。ローカルとリモートの作業を同じワークスペースにまとめ、別のデバイスからも接続できます。

**対応プラットフォーム：** macOS · Linux · Windows · Web · TUI — iOS と Android は開発中。

<table>
  <tr>
    <td width="50%" align="center">
      <img src="assets/screenshots/workspace.jpeg" alt="分割ターミナル、ソースのプレビュー、プロジェクトのファイルツリー" width="100%">
      <br><strong>プロジェクトのワークスペース</strong>
    </td>
    <td width="50%" align="center">
      <img src="assets/screenshots/agents.jpeg" alt="リモートターミナル横のエージェント状態とワークスペースのサイドバー" width="100%">
      <br><strong>複数マシンのエージェント</strong>
    </td>
  </tr>
  <tr>
    <td width="50%" align="center">
      <img src="assets/screenshots/overview.jpeg" alt="Space ごとにまとめたターミナルのライブプレビュー" width="100%">
      <br><strong>セッションの全体表示</strong>
    </td>
    <td width="50%" align="center">
      <img src="assets/screenshots/spaces.jpeg" alt="プロジェクトの Thread と Space の選択メニュー" width="100%">
      <br><strong>スペースの切り替え</strong>
    </td>
  </tr>
</table>

## 中心にあるのはマルチプレクサー

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/architecture-dark.svg">
    <img src="assets/architecture-light.svg" alt="ThinkTerm の簡略アーキテクチャ：デスクトップアプリ、TUI と CLI、ブラウザー、スマートフォンアプリが各マシンの mux サーバーに接続し、マシン同士も接続できます。デスクトップアプリは通常の SSH や Mosh のホストにも接続できます" width="100%">
  </picture>
</p>

ターミナルマルチプレクサーは複数のターミナルセッションを管理し、クライアントからの接続を受け付けます。ThinkTerm では、**mux サーバーがセッション、タブ、分割ペインを保持**し、クライアントが表示と入力の送信を担当します。

- **切断しても作業を継続。** クライアントを切断しても、サーバー上のセッションは動き続けます。再接続すれば、同じシェル、ビルド、エージェントのタスクに戻れます。ペインを明示的に閉じる操作は別です。
- **異なるクライアントから同じセッションへ。** デスクトップ、`thinkterm tui`、ブラウザーからサーバーに接続できます。複数のクライアントを同時に接続し、それぞれのインターフェースで共有セッションを操作できます。
- **複数のマシンで作業。** 各ホストが独自の mux サーバーを実行します。ThinkTerm Connect はリモートのワークスペースをローカルの作業と同じデスクトップに表示し、サイドバーからマシンを切り替えられるようにします。プロセスは元のホスト上で動作し続けます。
- **更新してもセッションは途切れません。** デスクトップからサーバーの ThinkTerm を更新すると、実行中の mux サーバーがセッションを新しいバージョンに引き継ぎ、シェル、ビルド、エージェントは動き続けます。

セッションが動き続けるには、mux サーバーとそのホストが利用可能である必要があります。サーバーやホストの再起動後に、実行中だったプロセスを復元する機能ではありません。

## ThinkTerm を作った理由

複数のプロジェクトでエージェントを同時に動かしていると、タブだけでは状況がわからなくなります。どのセッションが処理中で、どれが入力待ちなのか。さっきのビルドはどこで終わったのか。ThinkTerm はプロジェクト構造と作業状態をターミナルの横に表示し、ローカルとリモートの作業をまとめて把握できるようにします。

ThinkTerm のターミナルコア、mux サーバー、デスクトップクライアント、TUI は Rust で実装されています。デスクトップ版は GPU で描画し、Electron や埋め込み WebView を使用しません。Zed の UI フレームワークである GPUI は、ThinkTerm のターミナル描画に求められる性能要件を満たしていません。WezTerm のネイティブな基盤に、ワークスペース管理、永続的なリモートセッション、エージェントと作業するためのツールを追加しています。

## パフォーマンス

WezTerm とのスループット比較です。同じ Mac で kitty の `kitten __benchmark__` を使って計測しました（MB/s、大きいほど高速）。

ThinkTerm はローカル mux を有効にした状態で計測しています。シェルは mux サーバーで動き、出力は mux を経由してからウィンドウに届きます。

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/performance-ja-JP-dark.svg">
    <img src="assets/performance-ja-JP-light.svg" alt="kitten ベンチマーク 6 項目での ThinkTerm と WezTerm のスループット比較：ThinkTerm は 5 項目で高速（1.1×〜8.7×）、画像は低速（0.80×）" width="100%">
  </picture>
</p>

<details>
<summary>表形式のデータ</summary>

| 項目 | ThinkTerm 0.4.2<br>2026-11-05 | WezTerm 20260117-154428-05343b38<br>2026-09-23 | 倍率 |
| --- | ---: | ---: | ---: |
| ASCII 文字のみ | **206.1** | 23.6 | 8.7× |
| Unicode 文字 | **192.4** | 46.7 | 4.1× |
| 複数コードポイントの Unicode セル | **200.9** | 49.3 | 4.1× |
| 文字の少ない CSI シーケンス | **90.9** | 15.6 | 5.8× |
| 長いエスケープシーケンス | **219.3** | 194.9 | 1.1× |
| 画像 | 216.4 | **270.5** | 0.80× |

</details>

Linux では kitty と比較しました。Ubuntu 20.04（i9-12900K）、180×50、`TERM=xterm-256color`、Xvfb 上で CPU ソフトウェアレンダリングです。ThinkTerm は同じくローカル mux を経由しています。

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/performance-linux-ja-JP-dark.svg">
    <img src="assets/performance-linux-ja-JP-light.svg" alt="Linux での kitten ベンチマーク 5 項目における ThinkTerm と kitty のスループット比較：ThinkTerm は全項目で高速（1.6×〜3.4×）" width="100%">
  </picture>
</p>

<details>
<summary>表形式のデータ</summary>

| 項目 | ThinkTerm 0.4.2<br>2026-11-05 | kitty 0.47.4<br>2026-11-05 | 倍率 |
| --- | ---: | ---: | ---: |
| ASCII 文字のみ | **224.1** | 80.0 | 2.8× |
| Unicode 文字 | **186.8** | 79.0 | 2.4× |
| 文字の少ない CSI シーケンス | **71.1** | 43.2 | 1.6× |
| 長いエスケープシーケンス | **624.6** | 241.2 | 2.6× |
| 画像 | **663.3** | 196.3 | 3.4× |

</details>

同じ Mac でのバージョンごとの推移です。倍率は 0.4.2 と初期ビルドの比較です。

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/performance-progress-ja-JP-dark.svg">
    <img src="assets/performance-progress-ja-JP-light.svg" alt="kitten ベンチマーク 6 項目における ThinkTerm のバージョンごとのスループット：初期ビルドから 0.3.0、0.4.1、0.4.2 まで。0.4.2 は 4 項目で初期ビルドを上回り（3.0×〜4.5×）、長いエスケープシーケンスと画像はほぼ同等" width="100%">
  </picture>
</p>

<details>
<summary>表形式のデータ</summary>

| 項目 | ThinkTerm 初期<br>2026-09 | ThinkTerm 0.3.0<br>2026-10-21 | ThinkTerm 0.4.1<br>2026-11-04 | ThinkTerm 0.4.2<br>2026-11-05 | 倍率 |
| --- | ---: | ---: | ---: | ---: | ---: |
| ASCII 文字のみ | 45.3 | 133.0 | 134.0 | **206.1** | 4.5× |
| Unicode 文字 | 52.2 | 104.5 | 109.2 | **192.4** | 3.7× |
| 複数コードポイントの Unicode セル | 67.8 | 197.5 | 234.4 | **200.9** | 3.0× |
| 文字の少ない CSI シーケンス | 25.7 | 49.5 | 53.5 | **90.9** | 3.5× |
| 長いエスケープシーケンス | 219.4 | 213.8 | 257.3 | **219.3** | 1.00× |
| 画像 | 218.0 | 257.2 | 271.0 | **216.4** | 0.99× |

</details>

同じ Linux 環境でのバージョンごとの推移です。倍率は 0.4.2 と 143b60a2 の比較です。

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/performance-linux-progress-ja-JP-dark.svg">
    <img src="assets/performance-linux-progress-ja-JP-light.svg" alt="Linux での kitten ベンチマーク 5 項目における ThinkTerm のバージョンごとのスループット：143b60a2 から 0.4.1、0.4.2 まで。0.4.2 は全項目で 143b60a2 を上回る（1.2×〜2.2×）" width="100%">
  </picture>
</p>

<details>
<summary>表形式のデータ</summary>

| 項目 | ThinkTerm 143b60a2<br>2026-10-02 | ThinkTerm 0.4.1<br>2026-11-05 | ThinkTerm 0.4.2<br>2026-11-05 | 倍率 |
| --- | ---: | ---: | ---: | ---: |
| ASCII 文字のみ | 147.4 | 156.2 | **224.1** | 1.5× |
| Unicode 文字 | 85.6 | 96.8 | **186.8** | 2.2× |
| 文字の少ない CSI シーケンス | 50.0 | 52.2 | **71.1** | 1.4× |
| 長いエスケープシーケンス | 533.5 | 649.9 | **624.6** | 1.2× |
| 画像 | 391.8 | 346.0 | **663.3** | 1.7× |

</details>

## 作業に合わせたワークスペース

| 階層 | 役割 |
| --- | --- |
| **Space（スペース）** | ローカル作業、リモート mux 接続、ノートの Vault などの作業環境。 |
| **Project（プロジェクト）** | その環境に属するプロジェクトディレクトリ。 |
| **Thread（スレッド）** | タブと分割ペインを持つセッション。ピン留めや未読設定が可能。 |

ローカルとリモートのワークスペースを同じサイドバーに表示します。Thread の状態で、処理中、対応が必要、完了、待機中を区別できます。全体表示では Space ごとにターミナルのライブプレビューが並ぶため、タブをひとつずつ開かずにセッションを探せます。

## 日々のターミナル作業に

- **Rust、性能、メモリ効率。** ThinkTerm は負荷の高いターミナル作業を想定しています。多数のセッションやエージェントを並行して動かしても快適に操作できることを目指し、ターミナルの処理性能、描画効率、メモリ使用量を継続的に最適化しています。
- **エージェントの状態。** Agents パネルは、認識したコーディングエージェントの作業状態と所属プロジェクトを一覧表示します。エージェント検出とパネルは設定で無効にできます。
- **リモートセッション。** SSH、Mosh、または ThinkTerm Connect による永続的な mux セッションを選べます。リモート mux セッションではタブ、ペイン分割、サイズ変更、再接続が可能です。予測ローカルエコーは、遅延の大きい接続で入力の体感遅延を軽減できます。サーバーに ThinkTerm がまだない場合は、接続時にユーザー権限でのインストール（root 不要）を提案します。バージョンが異なるサーバーには、対応するバージョンを提案します。
- **Tailscale を内蔵。** Tailscale アプリなしで ThinkTerm から tailnet に参加し、そのマシンに接続できます。マシンはリモートホスト画面に、保存済みのサーバーと並んで表示されます。macOS と Linux で利用できます。
- **ターミナルの横にファイルを表示。** プロジェクトのファイルを参照し、構文強調表示付きでソースをプレビューしたり、外部エディターで開いたりできます。リモートファイルには SFTP でアクセスし、アップロード、ダウンロード、ドラッグ＆ドロップによる転送に対応します。
- **ノート。** Obsidian と互換性のある Vault で Markdown を編集できます。表、コードブロック、自動保存に対応し、ファイルは選択したディレクトリに通常の Markdown として保存されます。
- **スニペットとプラグイン。** Snippets を内蔵しています。プラグインはデスクトップ版とブラウザー版のサイドバーにパネルを追加できます。このリポジトリには、隣のターミナルが使う Git リポジトリの変更を確認する [Diff プラグイン](plugins/diff)も含まれています。[ThinkTerm SDK](docs/thinkterm/plugins.md#rust-sdk) を使って、Rust で独自のプラグインを開発できます。ほかの人が公開しているプラグインは、GitHub のトピック [`thinkterm-plugin`](https://github.com/topics/thinkterm-plugin) で探せます。導入と開発については[プラグインガイド](docs/thinkterm/plugins.md)をご覧ください。
- **充実したターミナルコア。** リガチャ、カラー絵文字、トゥルーカラー、ハイパーリンク、インライン画像、コピーモード、シェル統合を WezTerm から継承しています。 その他のターミナル機能については [WezTerm の機能ガイド](https://wezterm.org/features.html)をご覧ください。
- **ネイティブのデスクトップ設定。** テーマ、UI の文字サイズ、ターミナルのオプション、描画方式を調整できます。メインウィンドウと設定ウィンドウの両方が WebGPU と OpenGL に対応し、WebGPU の初期化に失敗すると OpenGL にフォールバックします。
- **5 つの表示言語。** English、简体中文、日本語、Français、Deutsch。

CLI からペインを自動操作することもできます。`thinkterm cli send-text` は入力を送り、`send-key` はキーを押し、`get-text` はターミナル出力を読み取ります。`wait` と `events` は、コマンドの終了やペインでの出来事を知らせます。`set-pane-label` でペインにラベルを付けると、名前で指定できます。エージェントはこれらのコマンドを使ってターミナル経由で連携できます。[連携に関する文書](docs/thinkterm/agent-collaboration.md)では、実演済みのワークフローと、ターミナル入力をメッセージ伝達に使う場合の限界を説明しています。

## 接続方法を選ぶ

| クライアント | 現在の範囲 |
| --- | --- |
| **デスクトップ** | macOS、Linux、Windows 向けのネイティブアプリ。 |
| **TUI** | `thinkterm tui` を使い、既存のターミナル内でワークスペースを移動し、セッションを操作。 |
| **ブラウザー** | 自分の mux サーバーが配信するクライアント。WebGPU とブラウザーのセキュアコンテキストが必要。アクセスを明示的に有効化する必要あり。 |
| **iOS と Android** | 開発中のネイティブクライアント。Rust コアを共有し、GPU 描画と SSH 通信を使用。リリースに向けた整備と実機検証は継続中。 |

各クライアントはサーバー上のセッションに接続しますが、インターフェースや対応機能は異なります。

### ターミナルインターフェース

```sh
thinkterm tui
thinkterm tui --help
```

TUI はワークスペースの移動、タブ、ペイン分割、サイズ変更、コピーモード、マウス操作に対応しています。終了時には接続を解除するだけで、サーバー上のセッションは閉じません。別のターミナルから実行してください。ThinkTerm 自身のセッション内で起動すると、入れ子のセッションを防ぐ保護機能が働く場合があります。

### ブラウザーからのアクセス

**Settings → Web（設定 → Web）** でリスナーを有効にし、ブラウザー用のアクセストークンを作成します。mux サーバーがクライアントとそのリソースを配信します。ループバック以外の接続では、既定で HTTPS が必要です。WebGPU を利用するためにも、ブラウザーのセキュアコンテキストが必要です。

リスナー、トークン、SSH 転送、証明書については[ブラウザーからのアクセス](docs/thinkterm/web-access.md)をご覧ください。

### モバイル版の開発

[iOS](ios) と [Android](android) のアプリは、ネイティブ UI と共有の[モバイルコア](thinkterm-mobile)を使用します。SSH で別のマシン上のセッションに接続します。現在も開発中であり、リリース済みのモバイル製品としては案内していません。

## 使い始める

macOS または Linux でデスクトップ版をビルドするには、Rust とプラットフォームのビルドツールをインストールし、次を実行します。

```sh
git clone --recursive https://github.com/RoversX/ThinkTerm.git
cd ThinkTerm
./get-deps
cargo build --release -p wezterm -p wezterm-gui -p wezterm-mux-server -p thinkterm-plugin-server -p thinkterm-tailnet-node
```

実行ファイルは `target/release` に生成されます。ソース構成と開発手順は[貢献ガイド](CONTRIBUTING.md)をご覧ください。[ブラウザー用ビルドスクリプト](ci/build-web.sh)で Web リソースを別途ビルドします。モバイル版のビルドには [ios/build.sh](ios/build.sh) と [android/build.sh](android/build.sh)を使用します。

実行ファイルを `PATH` に追加したら、次のコマンドを使えます。

```sh
thinkterm start              # デスクトップアプリを起動
thinkterm tui                # ターミナルインターフェースを起動
thinkterm connect <name>     # 設定済みの mux ドメインに接続
thinkterm cli --help         # mux セッションの確認と操作
thinkterm plugin list        # 利用可能なプラグインを一覧表示
thinkterm --help             # すべてのコマンドを表示
```

### コーディングエージェントに ThinkTerm を使わせる

リポジトリには、Claude Code、Codex、Cursor などのコーディングエージェントがほかのペインを確認・操作できるようにするスキルが含まれています。次のコマンドでインストールします。

```sh
npx skills add RoversX/ThinkTerm --skill thinkterm -g
```

## ドキュメントと開発

- [ブラウザーからのアクセス](docs/thinkterm/web-access.md)
- [プラグイン](docs/thinkterm/plugins.md)
- [開発への参加](CONTRIBUTING.md)

## 設定とプライバシー

ThinkTerm は Lua で設定し、既定では ThinkTerm 独自のパスを使用します。多くの WezTerm オプションに対応しています。**Settings → Compatibility（設定 → 互換性）** では、既存の WezTerm 設定から選択した項目を取り込めます。`THINKTERM_CONFIG_FILE` や互換用の `WEZTERM_CONFIG_FILE` など、明示的なファイル指定で別のファイルを選ぶこともできます。

### ThinkTerm はテレメトリーやトラッカーを一切使用しません

`check_for_updates = false` で更新確認を無効にできます。リモート接続やノート内の画像読み込みなどは、利用時にネットワーク通信を行います。ノートのリモート画像は `note_remote_images_enabled = false` で無効にできます。

デスクトップ版の SSH ホスト一覧は、ローカルに保存した鍵でパスワードを暗号化します。バックアップを含め、鍵と暗号化されたホストデータの両方を持つ人はパスワードを復号できます。ブラウザーのアクセストークンはサーバー上のターミナルセッションへのアクセスを許可するため、認証情報として管理してください。

プライバシーポリシーとブラウザーデータの扱いについては [PRIVACY.md](PRIVACY.md)をご覧ください。

## 謝辞

- **[@wez](https://github.com/wez/) と [WezTerm](https://github.com/wezterm/wezterm) の貢献者の皆さん**に感謝します。ターミナルエミュレーション、フォントと GPU の描画、SSH 対応を含むターミナルコアとマルチプレクサーの基盤が、ThinkTerm を支えています。
- ThinkTerm が使用するエージェント検出マニフェストを提供してくださった **[herdr](https://github.com/herdrdev/herdr) の貢献者の皆さん**に感謝します。
- アイコン素材を提供する **Lucide、Simple Icons、Lobe Icons、material-icon-theme** に感謝します。
- **ThinkTerm** のコード、翻訳、テスト、不具合報告、フィードバックに協力してくださるすべての皆さんに感謝します。

貢献を歓迎します。プルリクエストを作成する前に [CONTRIBUTING.md](CONTRIBUTING.md) をお読みください。

## ライセンス

ThinkTerm のライセンスは **GPL-3.0-only** です。[LICENSE.md](LICENSE.md)をご覧ください。WezTerm 由来のコードは元の MIT ライセンスを維持し、[LICENSE-MIT](LICENSE-MIT)に記載しています。herdr 由来のエージェント検出マニフェストは **Apache-2.0** ライセンスです。

第三者のコンポーネントと同梱リソースの完全なクレジットおよびライセンスは、[NOTICE](NOTICE) と [licenses/README.md](licenses/README.md)をご覧ください。



![GitHub downloads](https://img.shields.io/github/downloads/RoversX/ThinkTerm/total)
