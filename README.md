# aviutl2-alias-inserter

[![standard-readme compliant](https://img.shields.io/badge/readme%20style-standard-brightgreen.svg?style=flat-square)](https://github.com/RichardLitt/standard-readme)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

Steam Deck などの外部デバイスからワンボタンで AviUtl2（ExEdit2）タイムラインに `.object` エイリアスファイルを挿入する Rust 製プラグインと CLI クライアント。

AviUtl2 の汎用プラグイン（`.aux2`）が Named Pipe サーバーとして常駐し、CLI クライアントからのパス文字列を受け取って `EDIT_SECTION::create_object_from_alias()` SDK API 経由でオブジェクトを挿入します。Steam Input のプロファイルと組み合わせることで、コントローラーの1ボタン操作でエイリアスを挿入できます。

## 背景

AviUtl2（ExEdit2、KENくん氏による 64bit 対応版）で映像編集を行う際、同じエイリアスオブジェクトを繰り返しタイムラインに配置する作業が発生します。本プロジェクトはこの手順を自動化し、Steam Deck 等のコントローラーのボタン1つで挿入できるようにすることを目的としています。

公式 Plugin SDK が提供する `EDIT_SECTION::create_object_from_alias()` および `EDIT_HANDLE::call_edit_section_param()` API を使用しているため、リバースエンジニアリングは不要であり、SDK 更新への追従も容易です。

> **対象**: 旧 AviUtl（32bit）ではなく **AviUtl2（ExEdit2、64bit）** のみを対象とします。旧形式（`.exo`）は非対応です。

## インストール

### 前提条件

- Windows 10 または Windows 11（64bit）
- AviUtl2（ExEdit2）がインストール済みであること

### リリースからインストール（推奨）

1. [GitHub Releases](https://github.com/beive60/aviutl2-alias-inserter/releases/latest) から最新版の zip をダウンロードする。
2. zip を展開する。
3. `aviutl2_alias_inserter.aux2` を `C:\ProgramData\AviUtl2\plugins\` にコピーする。
4. `alias_inserter_cli.exe` を任意の場所（例: `C:\ProgramData\AviUtl2\`）にコピーする。
5. AviUtl2 を起動するとプラグインが自動的に読み込まれ、Named Pipe サーバーが起動します。

### ソースからビルド

ビルドには追加で以下が必要です。

- [Rust toolchain](https://rustup.rs/) — `x86_64-pc-windows-msvc` ターゲット

```powershell
git clone https://github.com/beive60/aviutl2-alias-inserter.git
cd aviutl2-alias-inserter
cargo build --release
```

ビルド成功後、以下のファイルが `target/release/` に生成されます。

| ファイル | 説明 |
| --- | --- |
| `aviutl2_alias_inserter.dll` | AviUtl2 プラグイン本体（`.aux2` にリネームして使用） |
| `alias_inserter_cli.exe` | CLI クライアント |

`aviutl2_alias_inserter.dll` を `aviutl2_alias_inserter.aux2` にリネームし、`C:\ProgramData\AviUtl2\plugins\` に配置してください。

## 使い方

### CLI

AviUtl2 が起動した状態で、挿入したい `.object` ファイルの絶対パスを引数に指定して実行します。

```powershell
alias_inserter_cli.exe "C:\path\to\your\alias.object"
```

成功すると現在のタイムラインカーソル位置にオブジェクトが挿入されます。エラー時は標準エラー出力にメッセージが表示され、非ゼロの終了コードで終了します。

### Steam Deck 統合

Steam Input のプロファイルで特定ボタンの「コマンドの実行」アクションに以下のように設定します。

```text
alias_inserter_cli.exe "C:\エイリアス\テキスト.object"
```

## アーキテクチャ

```text
[Steam Deck ボタン]
       |
       v
[alias_inserter_cli.exe] --UTF-16LE(path)--> [Named Pipe]
                                                  |
                                    [aviutl2_alias_inserter.aux2]
                                         (ワーカースレッド)
                                                  |
                                   call_edit_section_param()
                                                  |
                                         (メインスレッド)
                                                  |
                                   create_object_from_alias()
                                                  |
                                         [AviUtl2 タイムライン]
```

### IPC プロトコル

| 項目 | 仕様 |
| --- | --- |
| 通信方式 | Named Pipe（`\\.\pipe\aviutl2_alias_inserter`） |
| 方向 | CLI クライアント → プラグイン（単方向） |
| エンコーディング | UTF-16LE（BOM なし） |
| ペイロード | `.object` ファイルの絶対パス |
| 最大ペイロード長 | 32,768 バイト |

## コントリビューション

Issue や Pull Request を歓迎します。[GitHub Issues](https://github.com/beive60/aviutl2-alias-inserter/issues) で質問・提案を受け付けています。

詳細は [CONTRIBUTING.md](CONTRIBUTING.md) を参照してください。

## ライセンス

[MIT](LICENSE) © 2026 べいぶ
