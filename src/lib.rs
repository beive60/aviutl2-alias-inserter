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

use std::io::Read;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;

use aviutl2::AnyResult;
use aviutl2::generic::{GenericPlugin, GenericPluginTable, GlobalEditHandle, HostAppHandle};
use windows::Win32::Foundation::{
    BOOL, CloseHandle, ERROR_MORE_DATA, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL, INVALID_HANDLE_VALUE,
    LocalFree,
};
use windows::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_NORMAL, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_SHARE_NONE,
    FILE_TYPE_DISK, GetFileType, OPEN_EXISTING, PIPE_ACCESS_INBOUND, ReadFile,
};
use windows::Win32::System::Pipes::{
    PIPE_READMODE_MESSAGE, PIPE_TYPE_MESSAGE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, WaitNamedPipeW,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::{PCWSTR, PWSTR};

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

/// 受信したエイリアスファイルの最大許容サイズ（バイト）。  
/// 意図的に巨大なファイルを指定することで AviUtl2 プロセスを OOM クラッシュさせる
/// DoS 攻撃を防ぐ上限。典型的な `.object` ファイルは数 KiB 以下であるため、
/// 1 MiB（1,048,576 バイト）は正規の使用を十分にカバーする。
const MAX_OBJECT_FILE_SIZE: u64 = 1_048_576; // 1 MiB

// ─────────────────────────────────────────────────────────────
// スレッド間共有ハンドルラッパー
// ─────────────────────────────────────────────────────────────

/// `HANDLE` をスレッド間で安全に共有するためのラッパー型。
///
/// `HANDLE` は Windows カーネルオブジェクトへの不透明なポインタであり、
/// Rust では `Send`/`Sync` を実装しない。本型では `Mutex` による排他制御を前提に
/// `unsafe impl Send` を宣言し、安全にスレッド間受け渡しを可能にする。
struct SendableHandle(HANDLE);

// Mutex で保護するため、スレッド間送受信は安全。
unsafe impl Send for SendableHandle {}

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
/// ## シャットダウンフロー
///
/// 1. `shutdown_flag` を `true` に設定する。
/// 2. `active_pipe` が `Some` ならワーカーは `ReadFile` でブロック中であるため、
///    `DisconnectNamedPipe` でパイプを強制切断して `ReadFile` を中断させる。
/// 3. ダミークライアントを接続して `ConnectNamedPipe` のブロックを解除する。
/// 4. ワーカースレッドの終了を `join()` で待機する。
#[aviutl2::plugin(GenericPlugin)]
pub struct AliasInserterPlugin {
    /// シャットダウン要求を伝えるアトミックフラグ。  
    /// `true` に設定するとワーカースレッドはパイプ受信ループを終了する。
    shutdown_flag: Arc<AtomicBool>,

    /// Named Pipe サーバーを実行するワーカースレッドのハンドル。  
    /// `Mutex` でラップして `Sync` を安全に満たす。  
    /// `Drop` 時に `join()` して安全に終了を待機する。
    worker_thread: Mutex<Option<JoinHandle<()>>>,

    /// ワーカースレッドが現在接続中のパイプハンドル。  
    /// `ReadFile` でブロック中の場合は `Some` が設定されており、  
    /// `Drop` から `DisconnectNamedPipe` を呼び出して I/O を中断できる。
    active_pipe: Arc<Mutex<Option<SendableHandle>>>,
}

// Mutex<Option<JoinHandle<()>>> と Arc<Mutex<Option<SendableHandle>>> は
// どちらも Send + Sync を満たすため、unsafe impl は不要になった。

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
            worker_thread: Mutex::new(None),
            active_pipe: Arc::new(Mutex::new(None)),
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
        let active_pipe = Arc::clone(&self.active_pipe);
        let thread = std::thread::Builder::new()
            .name("alias_inserter_pipe_server".to_string())
            .spawn(move || {
                tracing::info!("Named Pipe サーバースレッドを開始しました");
                pipe_server_loop(flag, active_pipe);
                tracing::info!("Named Pipe サーバースレッドを終了しました");
            })
            .expect("ワーカースレッドの起動に失敗しました");

        *self.worker_thread.lock().unwrap() = Some(thread);
        tracing::info!("Named Pipe サーバーを起動しました: {}", PIPE_NAME);
    }
}

// ─────────────────────────────────────────────────────────────
// Drop 実装（終了処理）
// ─────────────────────────────────────────────────────────────

impl Drop for AliasInserterPlugin {
    /// プラグインのアンロード時にワーカースレッドを安全に終了する。
    ///
    /// ワーカースレッドには 2 つのブロッキングポイントがあるため、
    /// それぞれを個別に解除する：
    ///
    /// 1. シャットダウンフラグを `true` に設定する。
    /// 2. `active_pipe` が `Some` の場合（ワーカーが `ReadFile` でブロック中）、
    ///    `DisconnectNamedPipe` でパイプを強制切断して `ReadFile` を中断させる。
    /// 3. ダミークライアントをパイプに接続して `ConnectNamedPipe` のブロックを解除する。
    /// 4. ワーカースレッドの終了を `join()` で待機する。
    fn drop(&mut self) {
        tracing::info!("プラグインをシャットダウン中...");

        // シャットダウンフラグを設定
        self.shutdown_flag.store(true, Ordering::Relaxed);

        // ReadFile でブロック中のワーカーを解除（active_pipe が Some ならブロック中）
        if let Some(h) = self.active_pipe.lock().unwrap().take() {
            let _ = unsafe { DisconnectNamedPipe(h.0) };
        }

        // ブロック中の ConnectNamedPipe を解除するためにダミー接続を行う
        connect_shutdown_client();

        // ワーカースレッドの終了を待機
        if let Some(thread) = self.worker_thread.lock().unwrap().take() {
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
    // try_init() を使用して、テスト時やプラグイン再ロード時の二重初期化パニックを回避する。
    let _ = aviutl2::tracing_subscriber::fmt()
        .with_max_level(if cfg!(debug_assertions) {
            aviutl2::tracing::Level::DEBUG
        } else {
            aviutl2::tracing::Level::INFO
        })
        .event_format(aviutl2::logger::AviUtl2Formatter)
        .with_writer(aviutl2::logger::AviUtl2LogWriter)
        .try_init();
}

// ─────────────────────────────────────────────────────────────
// Named Pipe サーバーループ
// ─────────────────────────────────────────────────────────────

/// Named Pipe サーバーのメインループ。
///
/// シャットダウンフラグが `true` になるまで、クライアントの接続→受信→処理を繰り返す。
/// ループの各イテレーションで新しいパイプインスタンスを作成し、接続を待機する。
///
/// `ReadFile` 中に `Drop` が呼ばれた場合でも安全に終了できるように、
/// 接続後のパイプハンドルを `active_pipe` に格納する。
/// `Drop` は `active_pipe` を介して `DisconnectNamedPipe` を呼び出して
/// `ReadFile` を中断させる。
///
/// # 引数
///
/// * `shutdown` - シャットダウン要求を示すアトミックフラグ
/// * `active_pipe` - 現在接続中のパイプハンドルを共有するコンテナ
fn pipe_server_loop(shutdown: Arc<AtomicBool>, active_pipe: Arc<Mutex<Option<SendableHandle>>>) {
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

        // ─── アクティブパイプハンドルを登録 ───
        // Drop 側が DisconnectNamedPipe で ReadFile を中断できるようにする
        *active_pipe.lock().unwrap() = Some(SendableHandle(pipe));

        // ─── データを受信（ブロッキング; Drop から中断可能）───
        let received = read_pipe_data(pipe);

        // ─── アクティブパイプハンドルをクリア ───
        active_pipe.lock().unwrap().take();

        // ─── パイプを切断してクローズ ───
        let _ = unsafe { DisconnectNamedPipe(pipe) };
        let _ = unsafe { CloseHandle(pipe) };

        // ─── ReadFile 後のシャットダウンフラグを確認 ───
        if shutdown.load(Ordering::Relaxed) {
            tracing::info!("シャットダウンフラグを検出しました（ReadFile 後）。ループを終了します");
            break;
        }

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
/// メッセージモード（`PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE`）で作成することで、
/// クライアントの `WriteFile` 1 回分が 1 メッセージとして届くことが保証される。
/// バイトモードと異なり、`ReadFile` で部分的なデータを受け取ることがない。
///
/// `FILE_FLAG_FIRST_PIPE_INSTANCE` を指定することで、同名のパイプが既に存在する場合は
/// 作成を拒否し、パイプスカッティング（ハイジャック）を防止する。
///
/// `build_owner_security_descriptor` が返したセキュリティ記述子を使用して、
/// 現在のユーザーのみがパイプに接続できる DACL を設定する。
///
/// # 戻り値
///
/// 成功時はパイプのハンドル。失敗時は `INVALID_HANDLE_VALUE`。
fn create_server_pipe() -> HANDLE {
    let pipe_name_wide: Vec<u16> = PIPE_NAME
        .encode_utf16()
        .chain(std::iter::once(0u16))
        .collect();

    // 現在のユーザーのみアクセス可能なセキュリティ記述子を構築する
    let sd_guard = build_owner_security_descriptor();
    if sd_guard.is_none() {
        tracing::warn!(
            "セキュリティ記述子の構築に失敗しました。\
             OS デフォルトのセキュリティ属性でパイプを作成します（同一システム上の別ユーザーからアクセス可能になる場合があります）"
        );
    }
    let sa_storage = sd_guard.as_ref().map(|g| SECURITY_ATTRIBUTES {
        nLength: core::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: g.0 .0,
        bInheritHandle: BOOL(0),
    });
    let sa_opt: Option<*const SECURITY_ATTRIBUTES> =
        sa_storage.as_ref().map(|sa| sa as *const _);

    let pipe = unsafe {
        CreateNamedPipeW(
            PCWSTR(pipe_name_wide.as_ptr()),
            // FILE_FLAG_FIRST_PIPE_INSTANCE: 同名パイプが既存の場合は失敗してハイジャックを防止する
            PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
            PIPE_UNLIMITED_INSTANCES,
            MAX_PAYLOAD_BYTES as u32,
            MAX_PAYLOAD_BYTES as u32,
            0,
            sa_opt,
        )
    };

    if pipe == INVALID_HANDLE_VALUE {
        let err = windows::core::Error::from_win32();
        tracing::error!("CreateNamedPipeW が失敗しました: {}", err);
    }

    pipe
}

// ─────────────────────────────────────────────────────────────
// セキュリティ記述子ユーティリティ
// ─────────────────────────────────────────────────────────────

/// `LocalFree` で解放が必要な Windows ヒープメモリの RAII ラッパー。
///
/// スコープを抜けると `LocalFree` を呼び出してメモリを解放する。
struct LocalFreeGuard(PSECURITY_DESCRIPTOR);

impl Drop for LocalFreeGuard {
    fn drop(&mut self) {
        if !self.0 .0.is_null() {
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0 .0)));
            }
        }
    }
}

// 安全性の根拠：`LocalFreeGuard` は生成後、セキュリティ記述子の内部ポインタを
// 読み取り専用で `SECURITY_ATTRIBUTES` に設定する目的にのみ使用する。
// `CreateNamedPipeW` 呼び出し後は参照を保持しないため、内容への再アクセスは発生しない。
// `drop` は任意のスレッドから呼ばれる可能性があるが、`LocalFree` 自体はスレッドセーフであり、
// ポインタを他のスレッドと共有して読み書きするわけではないため、データ競合は生じない。
unsafe impl Send for LocalFreeGuard {}

/// `build_owner_security_descriptor` 内でプロセストークンハンドルを RAII 管理する型。
///
/// スコープを抜けると自動的に `CloseHandle` を呼び出す。
struct TokenHandleGuard(HANDLE);

impl Drop for TokenHandleGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// `ConvertSidToStringSidW` が確保する SID 文字列バッファの RAII ラッパー。
///
/// スコープを抜けると `LocalFree` で SID 文字列バッファを解放する。
/// `PWSTR::to_string()` が失敗した場合もリークなく解放される。
struct SidStringGuard(PWSTR);

impl Drop for SidStringGuard {
    fn drop(&mut self) {
        if !self.0.as_ptr().is_null() {
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0.as_ptr().cast())));
            }
        }
    }
}

/// 現在のプロセスオーナーのみがアクセス可能な Named Pipe 用セキュリティ記述子を構築する。
///
/// 処理フロー：
/// 1. 現在のプロセストークンを開く。
/// 2. `GetTokenInformation(TokenUser)` で現在ユーザーの SID を取得する。
/// 3. `ConvertSidToStringSidW` で SID を文字列に変換する。
/// 4. SDDL `D:P(A;;GRGW;;;<SID>)` で現在ユーザーにのみ汎用読み書きを許可する DACL を定義する。
/// 5. `ConvertStringSecurityDescriptorToSecurityDescriptorW` でセキュリティ記述子を生成する。
///
/// いずれかのステップに失敗した場合は `None` を返す。呼び出し元はその場合
/// デフォルトのセキュリティ属性（`None`）にフォールバックすること。
///
/// # 戻り値
///
/// 成功時は `LocalFreeGuard` でラップされたセキュリティ記述子ポインタ。
/// 失敗時は `None`。
fn build_owner_security_descriptor() -> Option<LocalFreeGuard> {
    unsafe {
        // ─── トークンを開く ───
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).ok()?;
        // RAII ガードでトークンを自動クローズ（以降のすべての早期リターンでも確実に閉じる）
        let _token_guard = TokenHandleGuard(token);

        // ─── TOKEN_USER 情報のバッファサイズを取得 ───
        let mut required_size = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut required_size);
        if required_size == 0 {
            return None;
        }

        // ─── TOKEN_USER を取得 ───
        let mut buffer = vec![0u8; required_size as usize];
        GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            required_size,
            &mut required_size,
        )
        .ok()?;

        // ─── ユーザー SID を取得 ───
        // Vec<u8> はアラインメント 1 しか保証しないため read_unaligned で TOKEN_USER を読み出す
        let token_user = core::ptr::read_unaligned(buffer.as_ptr().cast::<TOKEN_USER>());
        let user_sid: PSID = token_user.User.Sid;

        // ─── SID を文字列に変換 ───
        let mut sid_pwstr = PWSTR::null();
        ConvertSidToStringSidW(user_sid, &mut sid_pwstr).ok()?;
        // RAII ガードで確保済みバッファを保護（to_string 失敗時もリークなく解放）
        let _sid_str_guard = SidStringGuard(sid_pwstr);
        let sid_str = sid_pwstr.to_string().ok()?;

        // ─── SDDL で現在ユーザーのみに GENERIC_READ/WRITE を許可する DACL を生成 ───
        // D:P = 保護された DACL（継承なし）
        // A;;GRGW;;;<SID> = <SID> に GENERIC_READ と GENERIC_WRITE を許可
        let sddl = format!("D:P(A;;GRGW;;;{})", sid_str);
        let sddl_wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0u16)).collect();

        let mut sd = PSECURITY_DESCRIPTOR(core::ptr::null_mut());
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl_wide.as_ptr()),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )
        .ok()?;

        Some(LocalFreeGuard(sd))
    }
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

/// パイプから 1 メッセージ分のデータをすべて読み取る。
///
/// メッセージモードパイプでは、バッファが足りない場合に `ReadFile` が
/// `ERROR_MORE_DATA` を返す。そのためループで読み取りを繰り返し、
/// メッセージ全体を受信するまで続ける。
///
/// シャットダウン時（`DisconnectNamedPipe` 呼び出し後）は `ReadFile` が
/// エラーを返して中断され、`None` を返す。
///
/// # 引数
///
/// * `pipe` - 接続済みの Named Pipe ハンドル
///
/// # 戻り値
///
/// 受信データのバイト列（メッセージ全体）。読み取り中断時は `None`。
fn read_pipe_data(pipe: HANDLE) -> Option<Vec<u8>> {
    let mut message: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; MAX_PAYLOAD_BYTES];

    loop {
        let mut bytes_read: u32 = 0;

        match unsafe { ReadFile(pipe, Some(&mut chunk), Some(&mut bytes_read), None) } {
            Ok(()) => {
                // メッセージ全体の読み取り完了
                message.extend_from_slice(&chunk[..bytes_read as usize]);
                break;
            }
            Err(e) if e.code() == ERROR_MORE_DATA.to_hresult() => {
                // メッセージが大きくバッファに収まらなかった: 続きを読む
                message.extend_from_slice(&chunk[..bytes_read as usize]);
            }
            Err(_) => {
                // 接続断（Drop からの DisconnectNamedPipe 等）またはその他のエラー。
                // シャットダウン時は正常なパスのためログは出さない。
                return None;
            }
        }
    }

    if message.is_empty() {
        None
    } else {
        Some(message)
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
/// 2. 編集ハンドルが準備済みであることを確認する。
/// 3. `call_edit_section()` でメインスレッドに処理を委譲する。
/// 4. コールバック内でファイルを直接開き、サイズ確認後に読み込んで挿入する。
///    `exists()` による事前確認は行わず `File::open` のエラーで判断することで
///    TOCTOU 競合状態を排除する。
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

    // ─── 編集ハンドルの準備状態を確認 ───
    if !EDIT_HANDLE.is_ready() {
        tracing::error!("編集ハンドルがまだ準備できていません（タイムラインが開かれていない可能性があります）");
        return;
    }

    // ─── メインスレッドで挿入処理を実行 ───
    let result = EDIT_HANDLE.call_edit_section(move |edit_section| -> Result<(), String> {
        // TOCTOU 対策: exists() による事前確認は行わず File::open を直接試みる。
        // DoS 対策: 開いたハンドルのメタデータでサイズを検査してから読み込む。
        let file = std::fs::File::open(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                format!("エイリアスファイルが存在しません: {}", path)
            }
            _ => format!("ファイルを開けませんでした: {} ({})", path, e),
        })?;

        // DoS 対策: Named Pipe やデバイスファイルが指定された場合に read_to_string が
        // ブロックして UI スレッドを占有し続ける事態を防ぐため、通常のディスクファイルのみ許可する。
        let file_type = unsafe {
            use std::os::windows::io::AsRawHandle;
            GetFileType(HANDLE(file.as_raw_handle()))
        };
        if file_type != FILE_TYPE_DISK {
            return Err(format!(
                "通常のディスクファイルではありません（パイプやデバイスファイルは拒否します）: {}",
                path
            ));
        }

        let file_size = file
            .metadata()
            .map_err(|e| format!("ファイルのメタデータ取得に失敗しました: {}", e))?
            .len();

        if file_size > MAX_OBJECT_FILE_SIZE {
            return Err(format!(
                "ファイルサイズが上限を超えています（{} バイト、上限 {} バイト）: {}",
                file_size, MAX_OBJECT_FILE_SIZE, path
            ));
        }

        // 同じファイルハンドル経由で読み込むことで、サイズ検査後の差し替えを防ぐ
        let mut alias_data = String::new();
        std::io::BufReader::new(file)
            .read_to_string(&mut alias_data)
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

