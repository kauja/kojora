# Kojora（コジョラ）

**ローカルでもサーバーでも動く、軽量・拡張可能なOSSのSoftware Factory。**

> **現在はpre-alpha（設計・立ち上げ段階）です。** Rust Workspaceと単一コマンド実行ランナーを含みます。
> AIエージェントの管理実行、Bot UI、GUI、サーバー常駐機能はまだありません。

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

設計への参加は[CONTRIBUTING.md](CONTRIBUTING.md)を参照してください。
ライセンスは[MIT](LICENSE)です。