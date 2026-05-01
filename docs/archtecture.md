# アーキテクチャ

Named Pipe (`\\.\pipe\aviutl2_alias_inserter`) を介したクライアント・サーバーモデルを採用する。

## システムコンポーネント図

```mermaid
graph LR
    subgraph ext["外部デバイス (Steam Deck)"]
        BTN(["ボタン押下\n(Steam Input)"])
        CLI["alias_inserter_cli.exe\nCLI クライアント"]
        BTN -->|トリガー| CLI
    end

    PIPE[["Named Pipe\naviutl2_alias_inserter\nメッセージモード / UTF-16LE"]]

    subgraph dll["aviutl2_alias_inserter.aux2"]
        subgraph worker["ワーカースレッド"]
            LOOP["pipe_server_loop()"]
            DEC["decode_utf16le()"]
            IA["insert_alias()"]
            CALL["call_edit_section()"]
            LOOP --> DEC --> IA --> CALL
        end
        GEH[("GlobalEditHandle")]
        subgraph main["メインスレッド (AviUtl2 UI スレッド)"]
            REG["register()"]
            CB["create_object_from_alias()"]
        end
        REG -->|"create_edit_handle()"| GEH
        GEH -. "EDIT_HANDLE" .-> CALL
        CALL -->|"SDK 自動ディスパッチ\n(スレッドセーフ)"| CB
    end

    TL[["AviUtl2 / ExEdit2\nタイムライン"]]

    CLI -->|"WriteFile\n(UTF-16LE パス)"| PIPE
    PIPE -->|"ConnectNamedPipe\nReadFile"| LOOP
    CB -->|"オブジェクト挿入"| TL

    style worker fill:#dbeafe,stroke:#3b82f6
    style main fill:#dcfce7,stroke:#22c55e
    style ext fill:#fef3c7,stroke:#f59e0b
```

## IPC 接続・送信シーケンス

CLI クライアントがパスを検証し、Named Pipe へ送信するまでのフロー。

```mermaid
sequenceDiagram
    autonumber
    participant SD as Steam Deck
    participant CLI as alias_inserter_cli.exe
    participant OS as Named Pipe (OS)
    participant W as ワーカースレッド

    W->>OS: ConnectNamedPipe (待機中)

    SD->>CLI: ボタン押下 (トリガー)
    CLI->>CLI: validate_path()<br/>拡張子 / ファイル存在チェック
    CLI->>OS: CreateFileW (接続)
    OS-->>W: 接続確立
    CLI->>OS: WriteFile (UTF-16LE パス)
    CLI->>OS: CloseHandle
```

## エイリアス挿入シーケンス

ワーカースレッドがデータを受信し、メインスレッドでオブジェクトを挿入するまでのフロー。

```mermaid
sequenceDiagram
    autonumber
    participant OS as Named Pipe (OS)
    participant W as ワーカースレッド
    participant SDK as AviUtl2 SDK
    participant M as メインスレッド
    participant EE as ExEdit2

    OS-->>W: UTF-16LE バイト列 (ReadFile)
    W->>W: decode_utf16le()
    W->>W: insert_alias() 検証<br/>拡張子 / ファイル存在 / EDIT_HANDLE 状態
    W->>SDK: call_edit_section()
    SDK->>M: ディスパッチ (スレッドセーフ)
    M->>EE: create_object_from_alias()
    EE-->>M: OBJECT_HANDLE
    M-->>SDK: Ok / Err
    SDK-->>W: 結果返却
```

## シャットダウンシーケンス

```mermaid
sequenceDiagram
    participant A as AviUtl2
    participant D as drop()
    participant P as Active Pipe
    participant W as ワーカースレッド

    A->>D: UninitializePlugin()
    D->>D: shutdown_flag = true

    alt ReadFile でブロック中
        D->>P: DisconnectNamedPipe()
        P-->>W: ReadFile エラー (中断)
    else ConnectNamedPipe でブロック中
        D->>D: connect_shutdown_client()<br/>(ダミー接続)
        D-->>W: ConnectNamedPipe 解除
    end

    W->>W: shutdown_flag 確認<br/>ループ終了
    D->>W: join() 待機
    W-->>D: スレッド終了
    D-->>A: 終了完了
```
