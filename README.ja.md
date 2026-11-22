# Kojora（コジョラ）

**ローカルでもサーバーでも動く、軽量・拡張可能なOSSのSoftware Factory。**

> **現在はpre-alpha（設計・立ち上げ段階）です。** Rust Workspaceと最小CLIのひな形のみを含みます。
> AIエージェントの実行、Bot UI、GUI、サーバー常駐機能はまだありません。

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
```

現時点では動作状況を表示するだけです。エージェントは起動しません。

設計への参加は[CONTRIBUTING.md](CONTRIBUTING.md)を参照してください。
ライセンスは[MIT](LICENSE)です。