//! # AviUtl2 エイリアス挿入 CLI クライアント
//!
//! AviUtl2 エイリアス挿入プラグインに対して `.object` ファイルのパスを送信する
//! 軽量なコマンドラインツール。
//!
//! ## 使用方法
//!
//! ```text
//! alias_inserter_cli.exe <.objectファイルの絶対パス>
//! ```
//!
//! ## 動作概要
//!
//! 1. コマンドライン引数から `.object` ファイルの絶対パスを取得する。
//! 2. パスを UTF-16LE（BOM なし）にエンコードする。
//! 3. Named Pipe（`\\.\pipe\aviutl2_alias_inserter`）に接続する。
//! 4. エンコードされたパスを送信して接続を閉じる。
//!
//! ## エラー処理
//!
//! 接続失敗やファイル検証エラーが発生した場合は標準エラー出力にメッセージを表示し、
//! 非ゼロの終了コードで終了する。
//!
//! ## Steam Deck 統合
//!
//! Steam Input のプロファイルで特定ボタンの押下時に本ツールを呼び出すよう設定する：
//!
//! ```text
//! alias_inserter_cli.exe "C:\エイリアス\テキスト.object"
//! ```

use std::env;
use std::fs::File;
use std::process;

use windows::Win32::Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_NORMAL, FILE_SHARE_NONE, OPEN_EXISTING, WriteFile,
};
use windows::Win32::System::Pipes::WaitNamedPipeW;
use windows::core::PCWSTR;

// ─────────────────────────────────────────────────────────────
// 定数
// ─────────────────────────────────────────────────────────────

/// 接続先の Named Pipe 名。  
/// プラグイン側と同一の値でなければならない。
const PIPE_NAME: &str = r"\\.\pipe\aviutl2_alias_inserter";

/// パイプが利用可能になるまでの最大待機時間（ミリ秒）。  
/// プラグインがまだ起動していない場合の待機上限。
const MAX_WAIT_MS: u32 = 5_000;

/// 接続リトライ回数。  
/// `WaitNamedPipeW` が失敗した場合のリトライ上限。
const MAX_RETRIES: u32 = 3;

/// Named Pipe への書き込みアクセス権（`GENERIC_WRITE = 0x40000000`）。  
/// `windows::Win32::Security::GENERIC_WRITE` に相当する生 u32 値。
const GENERIC_WRITE_ACCESS: u32 = 0x4000_0000u32;

// ─────────────────────────────────────────────────────────────
// RAII ハンドルガード
// ─────────────────────────────────────────────────────────────

/// Named Pipe のハンドルを RAII で管理するガード型。
///
/// スコープを抜けると自動的に `CloseHandle` を呼び出す。
/// `WriteFile` 失敗やバイト数不一致でエラーリターンする場合でも
/// ハンドルが必ずクローズされることを保証する。
struct PipeHandleGuard(windows::Win32::Foundation::HANDLE);

impl Drop for PipeHandleGuard {
    /// スコープを抜けると自動的に `CloseHandle` を呼び出す。
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

// ─────────────────────────────────────────────────────────────
// エントリーポイント
// ─────────────────────────────────────────────────────────────
///
/// コマンドライン引数を解析し、Named Pipe 経由でプラグインにパスを送信する。
///
/// # 終了コード
///
/// - `0`：正常終了
/// - `1`：引数エラーまたはファイル検証エラー
/// - `2`：Named Pipe への接続または送信に失敗
fn main() {
    let args: Vec<String> = env::args().collect();

    // ─── 引数のバリデーション ───
    if args.len() != 2 {
        eprintln!("使用方法: {} <.objectファイルの絶対パス>", args[0]);
        eprintln!("例: {} \"C:\\エイリアス\\テキスト.object\"", args[0]);
        process::exit(1);
    }

    let path = &args[1];

    // ─── ファイルパスのバリデーション ───
    if let Err(msg) = validate_path(path) {
        eprintln!("エラー: {}", msg);
        process::exit(1);
    }

    // ─── Named Pipe への送信 ───
    if let Err(msg) = send_path_to_plugin(path) {
        eprintln!("エラー: {}", msg);
        process::exit(2);
    }
}

// ─────────────────────────────────────────────────────────────
// バリデーション
// ─────────────────────────────────────────────────────────────

/// ファイルパスのバリデーションを行う。
///
/// 以下の条件をチェックする：
/// 1. 拡張子が `.object` であること。
/// 2. ファイルが実際に開けること（`File::open` で確認）。
///
/// `Path::exists()` による事前確認を廃止し `File::open` のエラーで判断することで
/// TOCTOU 競合状態を排除している。
///
/// # 引数
///
/// * `path` - 検証対象のファイルパス
///
/// # 戻り値
///
/// バリデーション成功時は `Ok(())`。失敗時はエラーメッセージを返す。
fn validate_path(path: &str) -> Result<(), String> {
    if !path.to_ascii_lowercase().ends_with(".object") {
        return Err(format!(
            "'.object' 拡張子のファイルを指定してください: {}",
            path
        ));
    }

    File::open(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!("ファイルが見つかりません: {}", path),
        std::io::ErrorKind::PermissionDenied => {
            format!("ファイルへのアクセスが拒否されました: {}", path)
        }
        _ => format!("ファイルを開けませんでした: {} ({})", path, e),
    })?;

    Ok(())
}

// ─────────────────────────────────────────────────────────────
// Named Pipe 送信
// ─────────────────────────────────────────────────────────────

/// ファイルパスを Named Pipe 経由でプラグインに送信する。
///
/// パスを UTF-16LE（BOM なし）にエンコードしてパイプに書き込む。
/// プラグインが起動していない場合は `MAX_WAIT_MS` ミリ秒まで待機し、
/// `MAX_RETRIES` 回リトライする。
///
/// # 引数
///
/// * `path` - 送信する `.object` ファイルのパス
///
/// # 戻り値
///
/// 送信成功時は `Ok(())`。失敗時はエラーメッセージを返す。
fn send_path_to_plugin(path: &str) -> Result<(), String> {
    let pipe_name_wide: Vec<u16> = PIPE_NAME
        .encode_utf16()
        .chain(std::iter::once(0u16))
        .collect();

    let pipe_pcwstr = PCWSTR(pipe_name_wide.as_ptr());

    // ─── パイプが利用可能になるまで待機（リトライ付き）───
    let handle = connect_with_retry(pipe_pcwstr)?;

    // RAII ガード: WriteFile 失敗・バイト数不一致のエラーパスでも確実にクローズする
    let _guard = PipeHandleGuard(handle);

    // ─── パスを UTF-16LE にエンコード（null 終端付き）───
    let payload = encode_utf16le(path);

    // ─── パイプにデータを書き込む ───
    let mut bytes_written: u32 = 0;
    unsafe { WriteFile(handle, Some(&payload), Some(&mut bytes_written), None) }
        .map_err(|e| format!("WriteFile が失敗しました: {}", e))?;

    if bytes_written != payload.len() as u32 {
        return Err(format!(
            "送信バイト数が一致しません: 期待={}, 実際={}",
            payload.len(),
            bytes_written
        ));
    }

    // _guard のスコープ終了時に CloseHandle が呼ばれる
    Ok(())
}

/// Named Pipe にリトライ付きで接続する。
///
/// 接続に失敗した場合は状況に応じて待機してリトライする。
/// `MAX_RETRIES` 回試みてもすべて失敗した場合はエラーを返す。
///
/// ## リトライ条件
///
/// | エラー | 対応 |
/// |---|---|
/// | `ERROR_PIPE_BUSY` | `WaitNamedPipeW` でインスタンス空きを待つ |
/// | `ERROR_FILE_NOT_FOUND` | プラグインがまだ起動していない可能性。短いスリープ後にリトライ |
/// | その他 | 即座にエラーを返す（リトライしない） |
///
/// # 引数
///
/// * `pipe_name` - 接続先パイプのワイド文字列ポインタ
///
/// # 戻り値
///
/// 接続成功時はパイプのハンドル。失敗時はエラーメッセージ。
fn connect_with_retry(pipe_name: PCWSTR) -> Result<windows::Win32::Foundation::HANDLE, String> {
    let mut last_error = String::new();

    for attempt in 0..MAX_RETRIES {
        // パイプへの接続を試みる
        let result = unsafe {
            windows::Win32::Storage::FileSystem::CreateFileW(
                pipe_name,
                GENERIC_WRITE_ACCESS,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        };

        match result {
            Ok(handle) => return Ok(handle),
            Err(e) => {
                last_error = format!("{}", e);

                if attempt < MAX_RETRIES - 1 {
                    if e.code() == ERROR_PIPE_BUSY.to_hresult() {
                        // ERROR_PIPE_BUSY: パイプの全インスタンスが使用中 → 空き待ち
                        eprintln!(
                            "パイプが使用中です。待機してリトライします... ({}/{})",
                            attempt + 1,
                            MAX_RETRIES
                        );
                        let _ = unsafe { WaitNamedPipeW(pipe_name, MAX_WAIT_MS) };
                    } else if e.code() == ERROR_FILE_NOT_FOUND.to_hresult() {
                        // ERROR_FILE_NOT_FOUND: プラグインがまだ起動していない可能性
                        eprintln!(
                            "パイプが見つかりません。プラグインの起動を待機します... ({}/{})",
                            attempt + 1,
                            MAX_RETRIES
                        );
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    } else {
                        // その他のエラーはリトライしない
                        return Err(format!("Named Pipe への接続に失敗しました: {}", e));
                    }
                }
            }
        }
    }

    Err(format!(
        "Named Pipe への接続に {} 回試みましたが失敗しました (最後のエラー: {})",
        MAX_RETRIES, last_error
    ))
}

// ─────────────────────────────────────────────────────────────
// ユーティリティ関数
// ─────────────────────────────────────────────────────────────

/// 文字列を UTF-16LE バイト列（null 終端付き）にエンコードする。
///
/// IPC プロトコル仕様に従い、BOM なしの UTF-16LE バイト列を生成する。
/// `\0\0`（null 終端）を末尾に付加する。
///
/// # 引数
///
/// * `s` - エンコード対象の UTF-8 文字列
///
/// # 戻り値
///
/// UTF-16LE エンコードされたバイト列（null 終端付き）
fn encode_utf16le(s: &str) -> Vec<u8> {
    s.encode_utf16()
        .chain(std::iter::once(0u16)) // null 終端
        .flat_map(|c| c.to_le_bytes())
        .collect()
}

// ─────────────────────────────────────────────────────────────
// テスト
// ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// ASCII 文字列の UTF-16LE エンコードを確認する。
    #[test]
    fn test_encode_utf16le_ascii() {
        let encoded = encode_utf16le("AB");
        // 'A' = 0x0041 (LE: 0x41, 0x00), 'B' = 0x0042 (LE: 0x42, 0x00), null終端
        assert_eq!(encoded, vec![0x41, 0x00, 0x42, 0x00, 0x00, 0x00]);
    }

    /// 日本語文字列の UTF-16LE エンコードを確認する（null 終端含む）。
    #[test]
    fn test_encode_utf16le_japanese() {
        let encoded = encode_utf16le("あ");
        // 'あ' = U+3042 (LE: 0x42, 0x30), null終端
        assert_eq!(encoded, vec![0x42, 0x30, 0x00, 0x00]);
    }

    /// 空文字列のエンコードが null 終端のみになることを確認する。
    #[test]
    fn test_encode_utf16le_empty() {
        let encoded = encode_utf16le("");
        assert_eq!(encoded, vec![0x00, 0x00]);
    }

    /// '.object' 拡張子の検証が正しく機能することを確認する。
    #[test]
    fn test_validate_path_extension() {
        // 拡張子が違う場合はファイルの存在に関わらずエラー
        assert!(validate_path("/tmp/test.txt").is_err());
        assert!(validate_path("/tmp/test.exe").is_err());
        // .object であってもファイルが存在しなければエラー
        let mut nonexistent = std::env::temp_dir();
        nonexistent.push("_alias_inserter_test_nonexistent.object");
        // 確実に存在しないパスを使用（万が一存在する場合は削除）
        let _ = std::fs::remove_file(&nonexistent);
        let result = validate_path(nonexistent.to_str().unwrap());
        assert!(result.is_err());
    }

    /// 大文字小文字を区別しない拡張子チェックを確認する。
    #[test]
    fn test_validate_path_extension_case_insensitive() {
        // .OBJECT でも同様にファイル存在チェックに進む
        let mut nonexistent = std::env::temp_dir();
        nonexistent.push("_alias_inserter_test_nonexistent_upper.OBJECT");
        let _ = std::fs::remove_file(&nonexistent);
        let result = validate_path(nonexistent.to_str().unwrap());
        // 拡張子はOKだがファイルが存在しないのでエラー
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("ファイルが見つかりません"));
    }

    /// 実際に存在する `.object` ファイルがバリデーションを通過することを確認する。
    #[test]
    fn test_validate_path_existing_object_file() {
        // プラットフォームに依存しない一時ファイルを作成してテスト
        let mut path = std::env::temp_dir();
        path.push("_alias_inserter_test_valid.object");
        std::fs::write(&path, "").expect("一時ファイルの作成に失敗しました");
        let path_str = path.to_str().unwrap().to_string();
        let result = validate_path(&path_str);
        let _ = std::fs::remove_file(&path);
        assert!(result.is_ok(), "実在する .object ファイルはバリデーションを通過するはず");
    }
}
