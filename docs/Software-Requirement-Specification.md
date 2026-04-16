# 拡張編集エイリアス自動挿入システム 要件定義書

本要件定義は、Steam Deckをインターフェースとし、**AviUtl2（ExEdit2）*- の拡張編集タイムライン上にワンボタンで特定のオブジェクトエイリアス（`.object`ファイル）を挿入するシステムの構築を目的とする。後続のフェーズにおける変更・調整のベースラインとして機能する。

> **重要**: 本システムの対象は旧AviUtl（32bit）ではなく、ＫＥＮくん氏による **AviUtl ExEdit2（64bit対応版）*- である。旧AviUtlとの互換性は考慮しない。旧形式（`.exo`）は対象外とし、AviUtl2ネイティブの `.object` 形式のみを扱う。
>
> AviUtl2公式Plugin SDKの精査により、**リバースエンジニアリングは不要**であることが確認された。SDKが提供する `EDIT_SECTION::create_object_from_alias()` APIにより、エイリアスデータからのオブジェクト生成を公式に安全な手段で実行できる。また、`EDIT_HANDLE::call_edit_section()` によりスレッド安全性もSDK側で保証される。

## 1. システムアーキテクチャ概要

Steam Deckの入力イベントをトリガーとしてコマンドラインツールを実行し、IPC（プロセス間通信）経由でAviUtl2上の常駐プラグインに命令を送信するクライアント・サーバーモデルを採用する。

- **開発言語**: AviUtl2プラグイン（レシーバー）およびCLIクライアントの双方を **Rust*- で実装する。プラグイン側は `aviutl2-rs` crate（AviUtl2 SDKのRustバインディング）を使用する。多言語連携が不要な本システムにおいて、Rustはメモリ安全性・エラーハンドリング・クロスコンパイル容易性の面で最適な選択である。
- **ビルド環境**: Rust toolchain（`x86_64-pc-windows-msvc` target）を使用し、**64bitバイナリ（x64）*- としてコンパイルする。AviUtl2は64bitアプリケーションであるため、プラグインDLLも64bitに統一する。ビルド・デプロイ・パッケージングには `aviutl2-cli`（`au2`コマンド）を使用する。
- **文字エンコーディング**:
  - AviUtl2のSDK APIはUnicode（UTF-16 / ワイド文字）ベースの `LPCWSTR` を使用する。
  - ただし、エイリアスデータ本体（`create_object_from_alias` の引数）は **UTF-8*- 文字列として扱う（SDK仕様）。
  - Named Pipe上のIPC通信では **UTF-16LE（BOMなし）*- を使用する。WindowsのネイティブUnicode表現であり、受信後の変換コストが不要なため最適である。

## 2. 機能要件 (Functional Requirements)

### 2.1. レシーバー（AviUtl2汎用プラグイン）側

- **プラグイン形式**:
  - AviUtl2の汎用プラグイン（`.aux2`）として実装する。`aviutl2-rs` crateのマクロにより以下のSDKエントリーポイントが自動的にエクスポートされる。
    - `GetCommonPluginTable()` — プラグイン情報の提供
    - `RegisterPlugin(HOST_APP_TABLE- host)` — プラグイン登録・初期化
    - `InitializePlugin(DWORD version)` — バージョン確認付き初期化（任意）
    - `UninitializePlugin()` — 終了処理（任意）
    - `InitializeLogger(LOG_HANDLE- logger)` — ログ機能初期化（任意）
- **初期化・常駐プロセス**:
  - `RegisterPlugin` 内で `HOST_APP_TABLE::create_edit_handle()` を呼び出し、`EDIT_HANDLE` を取得・保持する。
  - メインスレッドをブロックしないワーカースレッド（Rustの `std::thread::spawn`）を生成する。
  - ワーカースレッド上で **Named Pipe（名前付きパイプ）*- のリスナーをサーバーとして構築・常駐させる。Windows API呼び出しには `windows` crate を使用する。
- **終了処理**:
  - `UninitializePlugin` でシャットダウンフラグ（`AtomicBool` 等）を設定し、パイプの切断およびスレッドの安全な合流（`JoinHandle::join`）を実行する。
- **ペイロード解析**:
  - クライアントから受信した **UTF-16LEバイト列*- を、挿入対象となる `.object` ファイルの絶対パスとしてデコードする。Rustの `String::from_utf16` 等で安全に変換する。不正なサロゲートペアを含む入力は検出・拒否する。
- **スレッドモデルとSDK連携**:
  - ワーカースレッドはNamed Pipeの受信処理のみを担当する。
  - エイリアス挿入処理は、SDKが提供する `EDIT_HANDLE::call_edit_section_param()` を呼び出すことで実行する。**この関数はSDK内部でメインスレッドへのディスパッチと排他制御を自動的に行う**ため、ウィンドウプロシージャのサブクラス化やカスタムウィンドウメッセージ（`WM_APP`等）は不要である。
- **エイリアス挿入ロジック（コールバック関数内）**:
  - 以下の処理を `call_edit_section_param` のコールバック（`EDIT_SECTION`）内で実行する。
    1. ファイルパスの有効性検証（ファイル存在確認。不在の場合は処理を中断しログ出力）。
    2. `.object` ファイルの全内容をUTF-8文字列として読み込む。
    3. `EDIT_SECTION::info->frame`（現在のカーソルフレーム）および `EDIT_SECTION::info->layer`（現在の選択レイヤー）を取得。
    4. `EDIT_SECTION::create_object_from_alias(alias_utf8, layer, frame, 0)` を呼び出す。`length=0` を指定することでオブジェクト長と配置位置が自動調整される。
    5. 戻り値（`OBJECT_HANDLE`）を確認し、`nullptr` の場合は挿入失敗としてログを出力する。

### 2.2. 送信（CLIクライアント / トリガー）側

- **CLIツールの実装**:
  - Rustで実装する軽量な実行ファイル（`.exe`）。引数として `.object` ファイルの絶対パスを受け取り、AviUtl2側のNamed Pipeに接続して **UTF-16LE（BOMなし）*- でエンコードしたパス文字列を送信後、即座に終了する。
- **Steam Deck統合**:
  - Steam Inputの機能を利用し、特定ボタンの押下時に前述のCLIツールを対象の `.object` ファイルパスを引数として実行するようプロファイルを設定する。

## 3. 非機能要件 (Non-Functional Requirements)

- **実行環境**: 対象OSはWindows 10/11を前提とする。Steam Deckが単体（SteamOS/Proton）で動作する場合は通信規格の再評価を要する。
- **パフォーマンス**: Steam Deckのボタン押下からAviUtl2へのエイリアス挿入完了までのレイテンシは、知覚不可能なレベル（100ms未満）を目標とする。
- **依存関係**: AviUtl2 Plugin SDK（ＫＥＮくん公式配布）および `aviutl2-rs` crate に依存する。SDKの後方互換性は `RequiredVersion()` によるバージョンチェックで制御する。
- **ロギング**: SDK提供の `LOG_HANDLE` を使用し、`logger->log()` / `logger->warn()` でログ出力を行う（`aviutl2-rs` 経由で利用）。
- **安全性**:
  - Named Pipeの名前はハードコードされた固定値（例: `\\.\pipe\aviutl2_alias_inserter`）とする。
  - ペイロードの最大長を制限する（例: 32,768バイト）。受信バッファのオーバーフローを防止するため、読み取りサイズの上限チェックを行う。Rustの所有権システムにより、バッファオーバーフロー等のメモリ安全性問題は言語レベルで防止される。

## 4. アーキテクチャの利益とリスク

- **利益**:
  - IPCとSDK APIの組み合わせにより、AviUtl2のウィンドウフォーカスやマウスカーソル位置に依存しない確実なオブジェクト挿入が保証される。
  - 公式SDK APIの使用により、リバースエンジニアリングやメモリオフセット依存が完全に排除される。ExEdit2のバージョンアップ時のプラグイン修正リスクが大幅に低減される。
  - `call_edit_section` によりスレッド安全性がSDKレベルで保証され、サブクラス化やカスタムメッセージの自前実装が不要になる。
  - Rustの型システム・所有権モデルにより、メモリ安全性とスレッド安全性がコンパイル時に保証される。
- **リスク**:
  - AviUtl2 SDK自体がまだ発展途上であり、APIの破壊的変更が発生する可能性がある（`RequiredVersion()` で緩和可能）。`aviutl2-rs` crateも同様に不安定（README記載）であり、SDKの変更に追従するアップデートが必要。
  - 不正なエイリアスデータが `create_object_from_alias` に渡された場合、関数の戻り値は `nullptr`（失敗）となるが、データ内容によってはExEdit2側で予期しない挙動を引き起こす可能性がある。ファイル存在確認と拡張子検証を前段で実施することでリスクを低減する。
  - `call_edit_section` は出力中等の状態で `false` を返す（編集不可）。この場合の対処（リトライ or エラーログ）を定義する必要がある。

## 5. 前提条件と制約

- AviUtl2は64bitプロセス（x64）として動作する。プラグインDLL（`.aux2`）およびCLIツールも64bitとしてコンパイルする。
- AviUtl2 Plugin SDKおよび `aviutl2-rs` crate（`aviutl2-sys` + `aviutl2`）が開発環境に導入済みであること。
- 挿入対象のエイリアスファイルは AviUtl2 ネイティブの `.object` 形式（UTF-8テキスト）のみとする。旧AviUtlの `.exo` 形式は対象外。
- タイムラインが存在しない状態（プロジェクト未作成）でコマンドを受信した場合：`call_edit_section` が `false` を返す可能性があるため、処理をスキップしエラーログを出力する。

## 6. IPC通信プロトコル仕様

| 項目 | 仕様 |
| --- | --- |
| 通信方式 | Named Pipe（`\\.\pipe\aviutl2_alias_inserter`） |
| 方向 | CLIクライアント → プラグイン（単方向） |
| エンコーディング | UTF-16LE（BOMなし） |
| ペイロード | `.object` ファイルの絶対パス文字列 |
| 最大ペイロード長 | 32,768バイト |
| 終端 | null終端（`\0\0`）またはパイプ切断で受信完了 |

UTF-16LEを選定した理由:

- WindowsのネイティブUnicode表現（`WCHAR`）と一致し、受信後の変換コストが不要。
- Named Pipeのバイトモードでそのまま送受信でき、エンコーディングのあいまいさが生じない。
- Rust側では `encode_utf16()` / `String::from_utf16()` で安全に変換可能。

---

- **Named Pipe**: Windows OSが提供するプロセス間通信機能。ファイルシステムと同様のAPIを使用してデータの送受信を行い、ローカル環境においてパケットロスのない高信頼な通信を実現する。
- **Payload**: 送受信されるデータ通信において、ヘッダ等の制御情報を除いた「目的となる実データ」部分。本要件においては `.object` ファイルのパス文字列を指す。
- **EDIT_SECTION**: AviUtl2 SDK が提供する編集セクション構造体。オブジェクトの作成・検索・削除・移動等の編集操作APIを関数ポインタとして公開する。`call_edit_section` 経由で排他制御されたコールバック関数内でのみ利用可能。
- **EDIT_HANDLE**: AviUtl2 SDK が提供する編集ハンドル構造体。`call_edit_section` でメインスレッドへのディスパッチと排他制御を行い、安全に編集操作を実行するためのエントリーポイント。
- **aviutl2-rs**: sevenc-nanashi氏が開発するAviUtl2 SDKのRustバインディング。`aviutl2-sys`（FFI）と `aviutl2`（高レベルラッパー）の2層構成。
- **aviutl2-cli**: sevenc-nanashi氏が開発するAviUtl2プラグイン開発支援CLIツール。ビルド・AviUtl2開発環境セットアップ・リリースパッケージ（`.au2pkg.zip`）作成を自動化する。
- **.object**: AviUtl2のオブジェクトエイリアスファイル形式。UTF-8テキストで記述され、`[Object]` セクション配下にエフェクト定義とパラメータを含む。

---
