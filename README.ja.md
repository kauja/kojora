# Kojora（コジョラ）

**ローカルでもサーバーでも動く、軽量・拡張可能なOSSのSoftware Factory。**

> **現在はpre-alpha（ローカルStudio MVP）です。** Rust製の実行ランナーとCodex・Claude Codeアダプターに加え、
> Bot/Task/Runを永続化する制御層とネイティブGUIを試験実装しました。
> 常駐WorkerはGUIから独立してローカルでタスクを管理します。

[English README](README.md) · [アーキテクチャ](docs/ARCHITECTURE.md) · [ロードマップ](docs/ROADMAP.md)

## 目指すもの

- 人間は長期的な**Bot**と対話し、その内部のTask / Run / Session / Worker / Artifactを追跡できる
- ローカルMacとリモートLinuxで共通のHeadless Runtimeを利用できる
- Rustでメモリ・CPU・I/O・MCPなどのリソースを監視し、適切にスケジューリングする
- Planner / Scheduler / Agent / Validator / Storage / UIなどを交換可能にする
- 権限を最小化し、認証情報は信頼されたWorkerの境界に閉じ込める
- Factory自身の開発をFactoryで行い、性能・信頼性を再現可能な方法で改善する

## 初期段階の利用

Rustupは `rust-toolchain.toml` に固定したRustを選びます。

```sh
cargo test --workspace
cargo run -p kojora-cli -- --help
cargo run -p kojora-cli -- doctor
cargo run -p kojora-cli -- run -- git --version
```

`run` はユーザーが指定した信頼済みコマンドを直接実行し、終了状態と標準出力・標準エラーを `.kojora/runs/` 以下に保存します。**サンドボックス・常駐Daemon・AIエージェント管理機能ではありません。** 子プロセスは呼び出し元の権限と環境変数を継承します。ログに秘密情報が混入する可能性があるため、信頼できるコマンドのみを実行してください。詳細は[Runnerの説明](docs/RUNNER.md)を参照してください。

## Codex・Claude Codeのアカウント設定

専用の認証プロファイルを手動で初期化した環境で、次のように実行できます。

```sh
mkdir -p "$HOME/.config/kojora/profiles"
mkdir -m 700 "$HOME/.config/kojora/profiles/codex-primary"
CODEX_HOME="$HOME/.config/kojora/profiles/codex-primary" codex login

cargo run -p kojora-cli -- agent codex \
  --profile "$HOME/.config/kojora/profiles/codex-primary" \
  -- "このRustプロジェクトの構成を説明して"
```

Claude Code 2.1.248以降で個人用と仕事用のアカウントを使う例です。

```bash
profiles="$HOME/.config/kojora/profiles"
mkdir -m 700 "$profiles/claude-personal" "$profiles/claude-work"
CLAUDE_CONFIG_DIR="$profiles/claude-personal" claude auth login
CLAUDE_CONFIG_DIR="$profiles/claude-work" claude auth login
cargo run --locked -p kojora-cli -- agent claude-code \
  --profile "$profiles/claude-work" -- "このプロジェクトの構成を説明して"
```

Kojoraはプロファイルを排他利用し、親プロセスのトークンをエージェントに渡しません。Claude Codeでは制限モードでファイルを参照できます。アカウントごとの認証状態はプロバイダーのCLIで確認してください。[アダプターの詳細](docs/AGENT_ADAPTER.md)

## ネイティブGUI（Studio MVP）

macOS / Linuxのデスクトップで、Botの作成、タスク送信、実行履歴、エージェントからの返信、
Run/Sessionの識別子、標準出力・エラーを確認できます。

```sh
cargo run --release -p kojora-desktop
```

GUIのフォームでBot名、作業ディレクトリ、プロバイダー、認証済みアカウントのプロファイルの絶対パスを登録します。Claude Codeの複数アカウントは、アカウントごとにBotを作成して切り替えます。
画面表示は英語ですが、日本語のタスク文はOSにある日本語フォントを探して表示します。
文字化けする場合は `KOJORA_GUI_FONT` に利用可能なCJKフォントの絶対パスを指定してください。現段階ではGUIプロセス内のバックグラウンドスレッドで実行する方式で、
遠隔Daemonではありません。同時実行は1タスクです。Codexは原則読み取り専用ですが、
明示的に確認した場合は分離したGit Worktree内で編集案を作成し、差分をGUIで確認できます。
このモードでも自動コミット・Pushはしません。信頼できるGitリポジトリだけで利用してください。
実行中には **Stop task** ボタンから明示的に停止を要求でき、macOS/Linuxでは
エージェントのプロセスグループを停止し、終了状態を `Cancelled` として履歴に保存します。
GUI自体が強制終了した際のプロセス自動回収は引き続き未実装です。
詳細は[Studio MVP](docs/STUDIO_MVP.md)をご覧ください。

## ローカルWorker Daemon（試験実装）

GUIとは別に `kojora-worker` を起動すると、GUIを閉じてもタスク管理が続きます。
通信はユーザー専用のUnixソケットに限定し、資格情報はGUIへ渡しません。

```sh
mkdir -p "$HOME/.local/state/kojora" && chmod 700 "$HOME/.local/state/kojora"
# ターミナル1：Workerを起動
cargo run --release -p kojora-worker -- --data-dir "$HOME/.local/state/kojora" --cpu-seconds 900 --address-space-mib 8192
# ターミナル2：GUIから接続
cargo run --release -p kojora-desktop -- --worker-socket "$HOME/.local/state/kojora/worker.sock"
```

**注意：** `RLIMIT_AS` は実メモリRSSではなく仮想アドレス空間の上限、
`RLIMIT_CPU` は壁時計時間ではなく各プロセスのCPU消費秒数です。
どちらもプロセス単位で適用されます。Workerの同時実行は既定で1件です。
別々の認証プロファイルを用意し、同時実行数とメモリ予約枠を設定すると並列実行できます。
Worker自体のクラッシュ復旧・孤児プロセス回収は未実装です。
詳しくは[Worker Daemonガイド](docs/WORKER.md)をご覧ください。

## 共通Lint／テストキューとHook（試験実装）

複数エージェントが同時にLintやテストを乱立させないよう、Workerが検査
ジョブを直列キューで管理します。成功した編集タスクの`post_edit` Hookで
Rustfmt・Clippyなどを自動登録でき、同じタスクの重複検査を防ぎます。
GUIからの手動検査や結果ログの閲覧にも対応しています。

```sh
cargo run --release -p kojora-worker -- \
  --data-dir "$HOME/.local/state/kojora" \
  --address-space-mib 8192 --cpu-seconds 900 \
  --enable-checks --post-edit-check rustfmt --post-edit-check clippy
```

**信頼できるリポジトリ限定です。** Cargoのビルドスクリプトやテストは
任意のコードを実行し得ます。仮想メモリ・CPU時間制限だけでは完全に隔離
できません。詳しくは[共通検査キュー](docs/LOCAL_CHECKS.md)をご覧ください。


設計への参加は[CONTRIBUTING.md](CONTRIBUTING.md)を参照してください。
ライセンスは[MIT](LICENSE)です。
