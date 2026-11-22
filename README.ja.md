# Kojora（コジョラ）

**ローカルでもサーバーでも動く、軽量・拡張可能なOSSのSoftware Factory。**

> **現在はpre-alpha（ローカルStudio MVP）です。** Rust製の実行ランナーとCodex CLI Adapterに加え、
> Bot/Task/Runを永続化する制御層とネイティブGUIを試験実装しました。
> 常駐Daemon、遠隔Worker、無人コード編集、強制終了と完全復旧は未実装です。

[English README](README.md) · [アーキテクチャ](docs/ARCHITECTURE.md) · [ロードマップ](docs/ROADMAP.md)

## 目指すもの

- 人間は長期的な**Bot**と対話し、その内部のTask / Run / Session / Worker / Artifactを追跡できる
- ローカルMacとリモートLinuxで共通のHeadless Runtimeを利用できる
- Rustでメモリ・CPU・I/O・MCPなどのリソースを監視し、適切にスケジューリングする
- Planner / Scheduler / Agent / Validator / Storage / UIなどを交換可能にする
- 権限を最小化し、認証情報は信頼されたWorkerの境界に閉じ込める
- Factory自身の開発をFactoryで行い、性能・信頼性を再現可能な方法で改善する

## 初期段階の利用

安定版Rustをインストールした環境で以下を実行してください。

```sh
cargo test --workspace
cargo run -p kojora-cli -- --help
cargo run -p kojora-cli -- doctor
cargo run -p kojora-cli -- run -- git --version
```

`run` はユーザーが指定した信頼済みコマンドを直接実行し、終了状態と標準出力・標準エラーを `.kojora/runs/` 以下に保存します。**サンドボックス・常駐Daemon・AIエージェント管理機能ではありません。** 子プロセスは呼び出し元の権限と環境変数を継承します。ログに秘密情報が混入する可能性があるため、信頼できるコマンドのみを実行してください。詳細は[Runnerの説明](docs/RUNNER.md)を参照してください。

## 試験的なCodex CLI Adapter

専用の認証プロファイルを手動で初期化した環境で、次のように実行できます。

```sh
mkdir -p "$HOME/.config/kojora/profiles"
mkdir -m 700 "$HOME/.config/kojora/profiles/codex-primary"
CODEX_HOME="$HOME/.config/kojora/profiles/codex-primary" codex login

cargo run -p kojora-cli -- agent codex \
  --profile "$HOME/.config/kojora/profiles/codex-primary" \
  -- "このRustプロジェクトの構成を説明して"
```

このAdapterは非対話・読み取り専用モードのみです。親プロセスの環境変数をクリアし、認証プロファイルを同時利用しないための排他ロックを設定します。**OS全体のSandboxや安全な自律編集機能を提供するものではありません。** 実際のログイン済みCodexによる統合テストはまだ実施していません。詳しくは[Agent Adapterの説明](docs/AGENT_ADAPTER.md)を参照してください。

## ネイティブGUI（Studio MVP）

macOS / Linuxのデスクトップで、Botの作成、タスク送信、実行履歴、Codexからの返信、
Run/Sessionの識別子、標準出力・エラーを確認できます。

```sh
cargo run --release -p kojora-desktop
```

GUIのフォームでBot名、作業ディレクトリ、認証済みCodexプロファイルの絶対パスを登録してください。
画面表示は英語ですが、日本語のタスク文はOSにある日本語フォントを探して表示します。
文字化けする場合は `KOJORA_GUI_FONT` に利用可能なCJKフォントの絶対パスを指定してください。現段階ではGUIプロセス内のバックグラウンドスレッドで実行する方式で、
遠隔Daemonではありません。同時実行は1タスク、Codexは読み取り専用モードです。
詳細は[Studio MVP](docs/STUDIO_MVP.md)をご覧ください。

設計への参加は[CONTRIBUTING.md](CONTRIBUTING.md)を参照してください。
ライセンスは[MIT](LICENSE)です。