//! # AviUtl2 エイリアス自動挿入プラグイン
//!
//! Steam Deck などの外部デバイスからワンボタンで AviUtl2（ExEdit2）の
//! タイムラインに `.object` エイリアスファイルを挿入するための汎用プラグイン（`.aux2`）。
//!
//! ## アーキテクチャ
//!
//! ```text
//! [CLIクライアント] --UTF-16LE(path)--> [Named Pipe] --> [本プラグイン]
//!                                                            |
//!                                               call_edit_section_param()
//!                                                            |
//!                                               create_object_from_alias()
//!                                                            |
//!                                                   [AviUtl2 タイムライン]
//! ```
//!
//! 1. プラグインロード時にワーカースレッドを起動し、Named Pipe サーバーを常駐させる。
//! 2. CLI クライアントが `.object` ファイルの絶対パスを UTF-16LE で送信する。
//! 3. ワーカースレッドがパスを受信し、`EDIT_HANDLE::call_edit_section_param()` 経由で
//!    メインスレッドに処理を委譲する。
//! 4. メインスレッドで `EDIT_SECTION::create_object_from_alias()` を呼び出し、
//!    現在のカーソル位置にオブジェクトを挿入する。
//!
//! ## スレッドモデル
//!
//! - メインスレッド：AviUtl2 の UI スレッド。SDK API の呼び出しはここで行われる。
//! - ワーカースレッド：Named Pipe の待ち受けと受信処理を担当。
//!   SDK 呼び出しは `call_edit_section_param()` 経由でメインスレッドへディスパッチされる。
//!
//! ## IPC プロトコル
//!
//! - 通信方式：Named Pipe（`\\.\pipe\aviutl2_alias_inserter`）
//! - エンコーディング：UTF-16LE（BOM なし）
//! - ペイロード：`.object` ファイルの絶対パス文字列
//! - 最大ペイロード長：32,768 バイト

use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;

use aviutl2::AnyResult;
use aviutl2::generic::{GenericPlugin, GenericPluginTable, GlobalEditHandle, HostAppHandle};
use windows::Win32::Foundation::{CloseHandle, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_NORMAL, FILE_SHARE_NONE, OPEN_EXISTING, ReadFile,
};
use windows::Win32::Storage::FileSystem::PIPE_ACCESS_INBOUND;
use windows::Win32::System::Pipes::{
    PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, WaitNamedPipeW,
};
use windows::core::PCWSTR;

// ─────────────────────────────────────────────────────────────
// 定数
// ─────────────────────────────────────────────────────────────

/// Named Pipe の名前。  
/// Windows のローカルパイプ名前空間に固定値として登録する。
const PIPE_NAME: &str = r"\\.\pipe\aviutl2_alias_inserter";

/// 受信バッファの最大バイト数（32,768 バイト）。  
/// パスの最大長（約 16,384 文字 × 2 バイト/文字）を考慮した上限。
const MAX_PAYLOAD_BYTES: usize = 32_768;

/// パイプ接続待機のタイムアウト（ミリ秒）。  
/// シャットダウン時にダミークライアントが接続するまでの最大待機時間。
const PIPE_CONNECT_TIMEOUT_MS: u32 = 5_000;

/// ダミー接続に使用する書き込みアクセス権（`GENERIC_WRITE = 0x40000000`）。  
/// `windows::Win32::Security::GENERIC_WRITE` に相当する生 u32 値。
const GENERIC_WRITE_ACCESS: u32 = 0x4000_0000u32;

// ─────────────────────────────────────────────────────────────
// グローバル編集ハンドル
// ─────────────────────────────────────────────────────────────

/// AviUtl2 の編集ハンドルをグローバルに保持するためのコンテナ。
///
/// `register()` 内で初期化され、ワーカースレッドから
/// `call_edit_section()` を呼び出すために使用する。
static EDIT_HANDLE: GlobalEditHandle = GlobalEditHandle::new();

// ─────────────────────────────────────────────────────────────
// プラグイン構造体
// ─────────────────────────────────────────────────────────────

/// AviUtl2 エイリアス挿入プラグインのメイン構造体。
///
/// `register()` が呼ばれた時点でワーカースレッドを起動し、
/// プラグインがアンロードされる（`Drop` が呼ばれる）時点でスレッドを安全に終了する。
///
/// ## フィールド
///
/// - `shutdown_flag`：ワーカースレッドへのシャットダウン通知フラグ。
/// - `worker_thread`：Named Pipe サーバーを実行するバックグラウンドスレッド。
#[aviutl2::plugin(GenericPlugin)]
pub struct AliasInserterPlugin {
    /// シャットダウン要求を伝えるアトミックフラグ。  
    /// `true` に設定するとワーカースレッドはパイプ受信ループを終了する。
    shutdown_flag: Arc<AtomicBool>,

    /// Named Pipe サーバーを実行するワーカースレッドのハンドル。  
    /// `Drop` 時に `join()` して安全に終了を待機する。
    worker_thread: Option<JoinHandle<()>>,
}

// JoinHandle<()> は Sync を実装しないため、unsafe impl Sync が必要。
// プラグインシングルトンは SDK 内の RwLock で保護されているため安全。
// Arc<AtomicBool> と JoinHandle<()> はどちらも Send を実装するため、
// Send は自動導出される。
unsafe impl Sync for AliasInserterPlugin {}

// ─────────────────────────────────────────────────────────────
// GenericPlugin トレイト実装
// ─────────────────────────────────────────────────────────────

impl GenericPlugin for AliasInserterPlugin {
    /// プラグインインスタンスを生成する。
    ///
    /// ロギングの初期化のみを行い、スレッド起動は [`Self::register`] で実施する。
    ///
    /// # 引数
    ///
    /// * `_info` - AviUtl2 のバージョン情報（使用しない）
    ///
    /// # 戻り値
    ///
    /// プラグインインスタンス。初期化に失敗した場合はエラーを返す。
    fn new(_info: aviutl2::AviUtl2Info) -> AnyResult<Self> {
        init_logging();
        tracing::info!("AviUtl2 エイリアス挿入プラグインを初期化中...");
        Ok(Self {
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            worker_thread: None,
        })
    }

    /// プラグインのメタ情報を返す。
    ///
    /// AviUtl2 の「プラグイン情報」ダイアログに表示される名前と説明文を設定する。
    fn plugin_info(&self) -> GenericPluginTable {
        GenericPluginTable {
            name: "AviUtl2 Alias Inserter".to_string(),
            information: format!(
                "AviUtl2 エイリアス自動挿入プラグイン v{} \
                / Named Pipe 経由でエイリアスをタイムラインに挿入します",
                env!("CARGO_PKG_VERSION")
            ),
        }
    }

    /// プラグインをホストに登録し、ワーカースレッドを起動する。
    ///
    /// `create_edit_handle()` で編集ハンドルを取得してグローバル変数に保存した後、
    /// Named Pipe サーバーを実行するワーカースレッドを生成する。
    ///
    /// # 引数
    ///
    /// * `registry` - ホストアプリケーションへのハンドル
    fn register(&mut self, registry: &mut HostAppHandle) {
        tracing::info!("プラグインをホストに登録中...");

        // 編集ハンドルをグローバル変数に保存
        EDIT_HANDLE.init(registry.create_edit_handle());

        // ワーカースレッドを起動
        let flag = Arc::clone(&self.shutdown_flag);
        let thread = std::thread::Builder::new()
            .name("alias_inserter_pipe_server".to_string())
            .spawn(move || {
                tracing::info!("Named Pipe サーバースレッドを開始しました");
                pipe_server_loop(flag);
                tracing::info!("Named Pipe サーバースレッドを終了しました");
            })
            .expect("ワーカースレッドの起動に失敗しました");

        self.worker_thread = Some(thread);
        tracing::info!("Named Pipe サーバーを起動しました: {}", PIPE_NAME);
    }
}

// ─────────────────────────────────────────────────────────────
// Drop 実装（終了処理）
// ─────────────────────────────────────────────────────────────

impl Drop for AliasInserterPlugin {
    /// プラグインのアンロード時にワーカースレッドを安全に終了する。
    ///
    /// 1. シャットダウンフラグを `true` に設定する。
    /// 2. ダミークライアントをパイプに接続して `ConnectNamedPipe` のブロックを解除する。
    /// 3. ワーカースレッドの終了を `join()` で待機する。
    fn drop(&mut self) {
        tracing::info!("プラグインをシャットダウン中...");

        // シャットダウンフラグを設定
        self.shutdown_flag.store(true, Ordering::Relaxed);

        // ブロック中の ConnectNamedPipe を解除するためにダミー接続を行う
        connect_shutdown_client();

        // ワーカースレッドの終了を待機
        if let Some(thread) = self.worker_thread.take() {
            if thread.join().is_err() {
                tracing::error!("ワーカースレッドがパニックしました。強制終了します");
            }
        }

        tracing::info!("プラグインのシャットダウンが完了しました");
    }
}

// ─────────────────────────────────────────────────────────────
// ロギング初期化
// ─────────────────────────────────────────────────────────────

/// AviUtl2 向けのロギングを初期化する。
///
/// デバッグビルドでは `DEBUG` レベル、リリースビルドでは `INFO` レベルで出力する。
/// ログは AviUtl2 の「ログ」ウィンドウに表示される。
fn init_logging() {
    aviutl2::tracing_subscriber::fmt()
        .with_max_level(if cfg!(debug_assertions) {
            aviutl2::tracing::Level::DEBUG
        } else {
            aviutl2::tracing::Level::INFO
        })
        .event_format(aviutl2::logger::AviUtl2Formatter)
        .with_writer(aviutl2::logger::AviUtl2LogWriter)
        .init();
}

// ─────────────────────────────────────────────────────────────
// Named Pipe サーバーループ
// ─────────────────────────────────────────────────────────────

/// Named Pipe サーバーのメインループ。
///
/// シャットダウンフラグが `true` になるまで、クライアントの接続→受信→処理を繰り返す。
/// ループの各イテレーションで新しいパイプインスタンスを作成し、接続を待機する。
///
/// # 引数
///
/// * `shutdown` - シャットダウン要求を示すアトミックフラグ
fn pipe_server_loop(shutdown: Arc<AtomicBool>) {
    loop {
        // ─── パイプインスタンスを作成 ───
        let pipe = create_server_pipe();
        if pipe == INVALID_HANDLE_VALUE {
            tracing::error!("Named Pipe インスタンスの作成に失敗しました。サーバーを終了します");
            break;
        }

        // ─── クライアントの接続を待機（ブロッキング）───
        if !wait_for_client(pipe) {
            // 接続エラー（シャットダウンダミー接続の場合も含む）
            let _ = unsafe { DisconnectNamedPipe(pipe) };
            let _ = unsafe { CloseHandle(pipe) };
            break;
        }

        // ─── シャットダウンフラグを確認 ───
        if shutdown.load(Ordering::Relaxed) {
            tracing::info!("シャットダウンフラグを検出しました。ループを終了します");
            let _ = unsafe { DisconnectNamedPipe(pipe) };
            let _ = unsafe { CloseHandle(pipe) };
            break;
        }

        // ─── データを受信 ───
        let received = read_pipe_data(pipe);

        // ─── パイプを切断してクローズ ───
        let _ = unsafe { DisconnectNamedPipe(pipe) };
        let _ = unsafe { CloseHandle(pipe) };

        // ─── 受信データを処理 ───
        if let Some(data) = received {
            match decode_utf16le(&data) {
                Ok(path) => {
                    tracing::info!("エイリアスパスを受信しました: {}", path);
                    insert_alias(path);
                }
                Err(e) => {
                    tracing::error!("UTF-16LE デコードに失敗しました: {}", e);
                }
            }
        }
    }
}

/// Named Pipe のサーバーインスタンスを作成する。
///
/// 読み取り専用（`PIPE_ACCESS_INBOUND`）のバイトモードパイプを作成する。
///
/// # 戻り値
///
/// 成功時はパイプのハンドル。失敗時は `INVALID_HANDLE_VALUE`。
fn create_server_pipe() -> HANDLE {
    let pipe_name_wide: Vec<u16> = PIPE_NAME
        .encode_utf16()
        .chain(std::iter::once(0u16))
        .collect();

    let pipe = unsafe {
        CreateNamedPipeW(
            PCWSTR(pipe_name_wide.as_ptr()),
            PIPE_ACCESS_INBOUND,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            PIPE_UNLIMITED_INSTANCES,
            MAX_PAYLOAD_BYTES as u32,
            MAX_PAYLOAD_BYTES as u32,
            0,
            None,
        )
    };

    if pipe == INVALID_HANDLE_VALUE {
        tracing::error!("CreateNamedPipeW が失敗しました");
    }

    pipe
}

/// クライアントの接続を待機する（ブロッキング）。
///
/// 接続が確立されるか、エラーが発生するまでブロックする。
/// `ERROR_PIPE_CONNECTED`（すでに接続済み）も成功として扱う。
///
/// # 引数
///
/// * `pipe` - Named Pipe のハンドル
///
/// # 戻り値
///
/// 接続成功時は `true`、エラー時は `false`。
fn wait_for_client(pipe: HANDLE) -> bool {
    match unsafe { ConnectNamedPipe(pipe, None) } {
        Ok(()) => true,
        Err(e) => {
            // ERROR_PIPE_CONNECTED: クライアントがすでに接続している場合は成功扱い
            if e.code() == ERROR_PIPE_CONNECTED.to_hresult() {
                true
            } else {
                tracing::error!("ConnectNamedPipe が失敗しました: {}", e);
                false
            }
        }
    }
}

/// パイプからデータを読み取る。
///
/// 最大 `MAX_PAYLOAD_BYTES` バイトを一度に読み取る。
///
/// # 引数
///
/// * `pipe` - 接続済みの Named Pipe ハンドル
///
/// # 戻り値
///
/// 受信データのバイト列。読み取り失敗または 0 バイトの場合は `None`。
fn read_pipe_data(pipe: HANDLE) -> Option<Vec<u8>> {
    let mut buffer = vec![0u8; MAX_PAYLOAD_BYTES];
    let mut bytes_read: u32 = 0;

    match unsafe { ReadFile(pipe, Some(&mut buffer), Some(&mut bytes_read), None) } {
        Ok(()) if bytes_read > 0 => {
            buffer.truncate(bytes_read as usize);
            Some(buffer)
        }
        Ok(()) => {
            tracing::warn!("パイプから 0 バイトを受信しました");
            None
        }
        Err(e) => {
            tracing::error!("ReadFile が失敗しました: {}", e);
            None
        }
    }
}

/// シャットダウン時に `ConnectNamedPipe` のブロックを解除するためのダミー接続。
///
/// ワーカースレッドが `ConnectNamedPipe` でブロック中の場合、このダミー接続が
/// ブロックを解除し、スレッドがシャットダウンフラグを確認できるようにする。
fn connect_shutdown_client() {
    let pipe_name_wide: Vec<u16> = PIPE_NAME
        .encode_utf16()
        .chain(std::iter::once(0u16))
        .collect();

    // パイプが利用可能になるまで待機（最大 PIPE_CONNECT_TIMEOUT_MS ミリ秒）
    let _ = unsafe { WaitNamedPipeW(PCWSTR(pipe_name_wide.as_ptr()), PIPE_CONNECT_TIMEOUT_MS) };

    // ダミー接続（書き込みはしない、接続するだけで ConnectNamedPipe を解除）
    let handle = unsafe {
        windows::Win32::Storage::FileSystem::CreateFileW(
            PCWSTR(pipe_name_wide.as_ptr()),
            GENERIC_WRITE_ACCESS,
            FILE_SHARE_NONE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    };

    if let Ok(h) = handle {
        let _ = unsafe { CloseHandle(h) };
    }
    // 接続失敗は無視（ワーカースレッドがすでに終了している可能性がある）
}

// ─────────────────────────────────────────────────────────────
// ユーティリティ関数
// ─────────────────────────────────────────────────────────────

/// UTF-16LE エンコードされたバイト列を Rust の UTF-8 文字列にデコードする。
///
/// IPC プロトコル仕様に従い、BOM なしの UTF-16LE バイト列を受け付ける。
/// null 終端文字（`\0\0`）が含まれる場合は、その手前までを有効データとして扱う。
///
/// # 引数
///
/// * `bytes` - UTF-16LE エンコードされたバイト列
///
/// # 戻り値
///
/// デコード成功時は UTF-8 文字列。バイト数が奇数の場合や不正なサロゲートペアが
/// 含まれる場合はエラーメッセージを返す。
///
/// # エラー
///
/// - バイト数が奇数の場合：`"バイト数が奇数です: N"`
/// - 不正な UTF-16 シーケンス（孤立サロゲートなど）：`"UTF-16 デコードエラー: ..."`
fn decode_utf16le(bytes: &[u8]) -> Result<String, String> {
    if bytes.len() % 2 != 0 {
        return Err(format!("バイト数が奇数です: {}", bytes.len()));
    }

    // バイト列を u16 スライスに変換（リトルエンディアン）
    let u16_units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();

    // null 終端文字を削除
    let u16_units = match u16_units.iter().position(|&c| c == 0) {
        Some(pos) => &u16_units[..pos],
        None => &u16_units,
    };

    // 孤立サロゲートを含む不正なシーケンスを検出して拒否
    String::from_utf16(u16_units).map_err(|e| format!("UTF-16 デコードエラー: {}", e))
}

/// エイリアスファイルをバリデーションし、タイムラインに挿入する。
///
/// 以下の順序で処理を行う：
/// 1. 拡張子が `.object` であることを確認する。
/// 2. ファイルが存在することを確認する。
/// 3. 編集ハンドルが準備済みであることを確認する。
/// 4. `call_edit_section()` でメインスレッドに処理を委譲する。
/// 5. コールバック内でファイルを読み込み、現在のカーソル位置に挿入する。
///
/// # 引数
///
/// * `path` - `.object` ファイルの絶対パス（UTF-8 文字列）
fn insert_alias(path: String) {
    // ─── 拡張子の検証 ───
    if !path.to_ascii_lowercase().ends_with(".object") {
        tracing::warn!(
            "'.object' 拡張子ではないファイルを受信しました（無視します）: {}",
            path
        );
        return;
    }

    // ─── ファイルの存在確認 ───
    if !Path::new(&path).exists() {
        tracing::error!("エイリアスファイルが存在しません: {}", path);
        return;
    }

    // ─── 編集ハンドルの準備状態を確認 ───
    if !EDIT_HANDLE.is_ready() {
        tracing::error!("編集ハンドルがまだ準備できていません（タイムラインが開かれていない可能性があります）");
        return;
    }

    // ─── メインスレッドで挿入処理を実行 ───
    let result = EDIT_HANDLE.call_edit_section(move |edit_section| -> Result<(), String> {
        // ファイルの内容を UTF-8 文字列として読み込む
        let alias_data = std::fs::read_to_string(&path)
            .map_err(|e| format!("ファイルの読み込みに失敗しました: {}", e))?;

        // 現在のカーソルフレームとレイヤー番号を取得
        let frame = edit_section.info.frame;
        let layer = edit_section.info.layer;

        tracing::info!(
            "エイリアスを挿入中... ファイル={}, フレーム={}, レイヤー={}",
            path,
            frame,
            layer
        );

        // create_object_from_alias でオブジェクトを生成
        // length=0 を指定することで長さと配置位置が自動調整される
        edit_section
            .create_object_from_alias(&alias_data, layer, frame, 0)
            .map(|_handle| {
                tracing::info!("エイリアスの挿入に成功しました: {}", path);
            })
            .map_err(|e| format!("create_object_from_alias が失敗しました: {}", e))
    });

    match result {
        Ok(Ok(())) => {}
        Ok(Err(msg)) => {
            tracing::error!("エイリアス挿入処理でエラーが発生しました: {}", msg);
        }
        Err(e) => {
            // call_edit_section が false を返した場合（出力中など編集不可状態）
            tracing::error!(
                "call_edit_section が失敗しました（編集不可状態の可能性があります）: {:?}",
                e
            );
        }
    }
}

// ─────────────────────────────────────────────────────────────
// プラグイン登録マクロ
// ─────────────────────────────────────────────────────────────

// AviUtl2 汎用プラグインとして `AliasInserterPlugin` を登録する。
//
// このマクロにより、以下の C エクスポート関数が自動生成される：
// - `RequiredVersion()` — 対応最小バージョンを返す
// - `InitializeLogger()` — ログハンドルを初期化する
// - `InitializePlugin()` — プラグインを初期化する（`new()` を呼び出す）
// - `GetCommonPluginTable()` — プラグイン情報テーブルを返す
// - `UninitializePlugin()` — プラグインをアンロードする（`drop()` を呼び出す）
// - `RegisterPlugin()` — プラグインをホストに登録する（`register()` を呼び出す）
aviutl2::register_generic_plugin!(AliasInserterPlugin);

// ─────────────────────────────────────────────────────────────
// テスト
// ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// UTF-16LE デコードの基本動作を確認する。
    #[test]
    fn test_decode_utf16le_ascii() {
        // "hello" を UTF-16LE にエンコードしてデコードを確認
        let input: Vec<u8> = "hello"
            .encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .collect();
        assert_eq!(decode_utf16le(&input).unwrap(), "hello");
    }

    /// null 終端文字が正しく除去されることを確認する。
    #[test]
    fn test_decode_utf16le_null_terminated() {
        let input: Vec<u8> = "path"
            .encode_utf16()
            .chain(std::iter::once(0u16))
            .flat_map(|c| c.to_le_bytes())
            .collect();
        assert_eq!(decode_utf16le(&input).unwrap(), "path");
    }

    /// 奇数バイト数の入力がエラーになることを確認する。
    #[test]
    fn test_decode_utf16le_odd_bytes() {
        let result = decode_utf16le(&[0x68, 0x00, 0x65]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("奇数"));
    }

    /// 日本語パスが正しくデコードされることを確認する。
    #[test]
    fn test_decode_utf16le_japanese() {
        let path = "C:\\ユーザー\\テスト.object";
        let encoded: Vec<u8> = path
            .encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .collect();
        assert_eq!(decode_utf16le(&encoded).unwrap(), path);
    }

    /// 空バイト列のデコードが空文字列になることを確認する。
    #[test]
    fn test_decode_utf16le_empty() {
        assert_eq!(decode_utf16le(&[]).unwrap(), "");
    }
}

