# lazyscript

`package.json` の `scripts` と独自の `scripts.json` を、lazygit ライクな TUI で一覧・実行するためのターミナルツールです。複数ディレクトリを横並びに扱え、ヤンク用の copy-mode やフィルタ、その場で打つ ad-hoc コマンド実行も備えています。

## 特徴

- `package.json` / `scripts.json` を自動 discover してツリー表示
- 複数の作業ディレクトリを引数で同時に開ける
- スクリプトの追加 (`a`) / 編集 (`e`) をその場で行い、ファイルへ書き戻し
- 任意のシェルコマンドを `!` で ad-hoc 実行（履歴はセッション内で保持）
- ファイル監視で `package.json` / `scripts.json` の変更を即時反映
- 各プロセスごとに PTY を割り当て、`vt100` でカラー・カーソルを忠実に描画
- tmux 風 copy-mode (`v`)：hjkl/単語移動・ビジュアル選択・`/?` 検索・`y` でヤンク
- ヤンク先はシステムクリップボードと OSC 52 を選択可能（既定は両方）
- ユーザーのシェル（`$SHELL -i -c`）経由で起動するため、`.zshrc` / `.bashrc` のエイリアスがそのまま使える

## インストール

Rust ツールチェイン（edition 2024 対応版）が必要です。

```sh
cargo install --path .
```

## 使い方

カレントディレクトリを対象にする場合:

```sh
lazyscript
```

複数ディレクトリを並べる場合:

```sh
lazyscript ./apps/web ./apps/api
```

引数で渡されたディレクトリそれぞれについて、`package.json` と `scripts.json` を探して 1 つのツリーに統合します。

## キーバインド

### Normal モード

| キー | 動作 |
| --- | --- |
| `j` / `k` (`↓` / `↑`) | カーソル移動 |
| `h` / `l` (`←` / `→`) | 親ヘッダ / 最初の子へ移動 |
| `Enter` / `Space` | ヘッダ=開閉 / スクリプト=実行（実行中なら停止） |
| `a` | 新しいスクリプトを追加 |
| `e` | 選択中スクリプトを編集 |
| `!` | ad-hoc コマンドを入力して実行 |
| `/` | スクリプト名でフィルタ |
| `v` | copy-mode に入る（出力ペインを凍結） |
| `za` / `zo` / `zc` | 現在のヘッダを toggle / open / close |
| `q` / `Ctrl-C` | 終了 |

### Copy モード（`v`）

| キー | 動作 |
| --- | --- |
| `h` / `j` / `k` / `l` | カーソル移動 |
| `w` / `b` / `e` | 単語単位の移動 |
| `Ctrl-U` / `Ctrl-D` / `Ctrl-F` / `Ctrl-B` | ページ移動 |
| `v` / `V` / `Ctrl-V` | 文字 / 行 / 矩形選択 |
| `/` / `?` | 前方 / 後方検索 |
| `n` / `N` | 次 / 前のマッチへ |
| `y` / `Enter` | ヤンクして Normal へ戻る |
| `Esc` | 選択/検索をクリア |
| `q` | copy-mode を抜ける |

### 追加・編集モーダル（`a` / `e`）

| キー | 動作 |
| --- | --- |
| `Tab` | Name / Command フィールドを切り替え |
| `Enter` | 次のフィールドへ / Command 上で保存 |
| `Esc` | キャンセル |

## scripts.json

`package.json` を持たないプロジェクトでも独自にスクリプトを管理できます。形式は `package.json` の `scripts` と同じです。

```json
{
  "scripts": {
    "build": "cargo build",
    "run": "cargo run -- examples/demo",
    "test": "cargo test"
  }
}
```

トップレベルが直接 `{ "name": "command" }` の形でも読みます。`scripts.json` のスクリプトは `npm run` を介さず、ユーザーシェルで直接実行されます。

## 設定ファイル

`~/.config/lazyscript/setting.json`（または `$XDG_CONFIG_HOME/lazyscript/setting.json`）に JSON で書きます。ファイルが無ければすべて既定値で動作します。

```json
{
  "theme": {
    "cursor": "yellow",
    "search": "cyan",
    "running": "green",
    "failed": "red",
    "focus": "cyan",
    "selection": "#303030"
  },
  "clipboard": "both",
  "scrollback": 10000
}
```

| キー | 説明 | 既定値 |
| --- | --- | --- |
| `theme.cursor` | カーソルセルの色 | `yellow` |
| `theme.search` | 検索ヒットの色 | `cyan` |
| `theme.running` | 実行中/正常終了アイコンの色 | `green` |
| `theme.failed` | 失敗アイコンの色 | `red` |
| `theme.focus` | フォーカス中ペインの枠色 | `cyan` |
| `theme.selection` | 選択行/選択範囲の背景色（未指定なら反転表示） | なし |
| `clipboard` | `system` / `osc52` / `both` | `both` |
| `scrollback` | プロセスごとに保持する行数 | `10000` |

色は `red` などの名前、`#rrggbb`、`0`〜`255` のインデックスが使えます。

## 開発

`scripts.json` にこのリポジトリ向けのコマンドがまとめてあります。lazyscript 自身を使って実行できます。

```sh
cargo run -- .
```

主なターゲット:

- `cargo build` / `cargo build --release`
- `cargo test`
- `cargo clippy --all-targets`
- `cargo fmt`

## ライセンス

未指定。
