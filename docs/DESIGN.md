# loopreview 設計ドキュメント

*2026-07-22 設立時点の要件・設計の集約。以後の変更はこのファイルを正として更新する。*

## 1. プロダクト定義

**loopreview** — review-first な汎用 diff TUI(Rust)。エージェントループの成果物(worktree / PR)を人間が検分・コメント・決裁する場所。

- 一行で: **「GitHub PR が TUI で見れる」**。素の diff にはページャ的な軽さ、コメントのある文脈にはレビュー UI
- CLI: binary `loopreview` + alias bin `lr`(同一 main)。loopkeep(`lk`)との製品ファミリー
- repo: `loopkeep/loopreview`(private 開始。公開判断は品質が整ってから。ライセンスは公開時に決定)

### 前身と教訓

前身は herdr plugin「hunk-review」(hunk 統合で PR コメント双方向同期まで完成)。hunk の構造的制約 — reload の root 固定、表示状態を外部から制御不可、コメントが flat でスレッド/返信は body マーカーのハック、ノート UI 固定 — を理由に自前化を決断。ローカルドナー(`~/workspaces/github.com/nanopx/herdr-plugin-hunk-review`)から gh/GraphQL 層・TUI 背景ジョブ基盤・CI の型を流用する。

## 2. スコープ方針

1. **まず単体 CLI として完成させる**。loopkeep のことは考えない
2. ただし**ロジックの分離を最初から綺麗に**保ち、将来の crate 公開・loopkeep 連携を安価にする
3. 判断基準: **実装で楽をしない・後回しにしない。迷ったら利用者体験が向上する方**

### 必須要件(ユーザー指定・降格不可)

- **watch モード**: diff 入力の変化に自動追従(エージェントの作業をリアルタイムで眺める用途)。live ソース(worktree / ref)は**既定 ON**、`--no-watch` で無効化。再読込時はビュー位置を保持
- **agent 連携(セッション制御面)**: 外部エージェントが読む・注釈する・操作するための CLI/API(M3)。**loopkeep から使うときの前提要件**でもある
- **レイアウトのレスポンシブ両対応**: unified / side-by-side + ターミナル幅による auto(§4)
- **TUI のマウス対応**(§4)

### 希望要件(将来)

- **プラグイン機構**: 第一歩は git/cargo 型の**外部サブコマンド発見**(`lr <verb>` → PATH 上の `lr-<verb>` を exec。`lk review` シムと同じ思想で、逆方向にも同じ仕組みを持つ)。その先はソース/シンクのサブプロセスプラグイン(manifest + プロトコル。herdr plugin 型)を検討。core のソース/シンク抽象がそのまま接続点になる

## 3. アーキテクチャ

Cargo workspace(edition 2024)。M1 は 2 crate、育ったら分割。

```
crates/
  loopreview-core   純粋層(将来 crates.io 公開に耐える)
  loopreview-cli    TUI・ハイライト・git 実行・CLI
```

### レイヤリング規律

- **core**: diff モデル + ソース抽象のみ。ratatui / crossterm / syntect / clap 非依存。プロセス起動も trait 越し
- **DiffSource trait**: `WorktreeSource` / `RefSource` / `StdinPatchSource`(将来 `PrSource` / 外部 run ソースが同じ trait に刺さる)。engine は trait の裏に隠す
- **モデル型は自前所有**: similar 等の型を public API に漏らさない。File / Hunk / Line は自前型
- **行アンカー概念をモデルに内蔵**: 任意の行から `(file, side, line)` が一意に取れる。前後 N 行の文脈を安価に取り出せる API 形状
- **provenance**: diff モデルは比較両側の由来(base/head の commit sha。stdin は None)を記録する — outdated コメントの履歴再構成(§6)の土台
- **ハイライトは独立レイヤ**: core のモデルを入力に色付けを返す純関数群。core はハイライトを知らない
- core の pub アイテムには doc コメント(公開時の semver 責務に備える)

### diff 構築方式(裁定済み)

- **git の unified diff 出力を自前パーサで解析するのが主軸**(stdin patch と同一パーサ)。理由: レビュアーの基準系は `git diff` そのものであり hunk 境界の一致が信頼性に直結。rename / binary / mode は git 任せが堅牢。`-U<n>` 等のオプション透過も安価
- **similar は「変更行ペアの語単位(intra-line)ハイライト」レイヤとして使用**。リッチな diff view の中核体験

## 3.5 CLI サーフェス

```
lr                          # シュガー: stdin がパイプ → patch 表示 / TTY & repo 内 → worktree diff
lr diff [target] [--staged] [-- <pathspec>]   # VCS diff 専用(stdin は読まない)
lr patch [file]             # unified diff をファイル or stdin から(明示形)
lr show [ref]               # コミットレビュー(M2 以降)
lr pr <number|url>          # PR レビュー(M2b)
lr session <verb>           # 制御面(M3)
lr daemon serve             # (M3)
lr skill path               # agent スキル文書(M3)
```

- 裸の `lr` は dispatch シュガー(pager 慣習)。`lr diff` は VCS 専用と割り切り、stdin との曖昧さを構造から排除(パイプ + `lr diff` は丁寧なエラーで `lr` / `lr patch` へ誘導)
- 将来 verb の名前空間は M1 の clap 構造で予約。`--help` / `-V` は TTY ガードより先に処理

## 4. UI 設計

### ビューの出し分け

| 文脈 | UI |
|---|---|
| `git diff \| lr`、`lr diff [target]`(素の diff) | **Diff ビュー単体**(タブなし、ページャ的軽さ) |
| コメントが存在する文脈(PR / ローカルレビュー) | **Conversation \| Files changed のタブ構造**(GitHub PR 型) |

Commits タブは当面スコープ外。

### Diff ビュー(Files changed 相当)

- **レイアウトは unified / side-by-side(split)の両対応 + `auto` がデフォルト**: ターミナル幅でレスポンシブに自動選択(狭い → unified)。CLI/config で固定指定可、実行中もキーでトグル可(hunk の auto split/stack を継承する必須要件)
- syntect + two-face のシンタックスハイライト + intra-line 強調(両レイアウトで共通のハイライトパイプライン)
- **マウス対応は必須**: ホイールスクロール、クリックで行カーソル移動(M1)。ドラッグ選択 → 範囲コメントは M2 で lumen 方式(行番号ドラッグ = 行単位選択)を参考に
- **ナビゲーションの基本単位は行カーソル**(j/k で行移動・ビューポート追従、Ctrl-D/U で半ページ、n/p でファイル間)。カーソルは (file, side, line) の行アンカーを常に指す
- 配置可能なコメントスレッドのみインライン表示。**unanchored の明示セクションは作らない**
- **任意の diff 行に新規コメントを作成できる**(`c` キー想定・M2a)— カーソル行にドラフトスレッドを生成。複数行 range 指定は将来
- watch 再読込時のビュー位置保持はカーソル(file + 行)基準
- 長いパスは末尾(ファイル名)保持の省略。エラー・ロード状態は必ず表示(握りつぶし禁止)

### Conversation ビュー

- **トップレベルコメント(スレッドのルート)単位の時系列**に並べ、返信は各スレッドブロック内にネスト表示(全コメントのインターリーブではない)
- 対象: changeset 全体の会話 / outdated(履歴再構成付き)/ file-level / resolved
- 返信・resolve の主戦場

### キーバインド方針

- fuzzy 入力を伴うリスト画面では素キーを入力に譲りアクションは Ctrl 系、モーダル/ビュー内は素キー可(前身の実戦知見)

## 5. コメントモデル(M2 の心臓部)

```
Thread  { id, anchor, state: open | resolved, comments: [Comment] }  // データは flat、UI は tree
Comment { id, author, body, created_at, remote_id? }                 // 先頭 = ルート、以降 = 返信(時系列)

Anchor =
  | Line   { file, side: old|new, range: start..=end, commit?, context_snippet }
  | File   { file }
  | Review                       // changeset 全体 = diff に紐づかない会話の正規の住所
```

- GitHub の review thread と同型 → 往復変換が無損失。`in_reply_to` は行情報不要(outdated / resolved スレッドにも返信可能)
- **draft / published をモデルに持つ**: ローカル下書き → まとめて review submit(1 リクエスト = review 1件 + comments N件 + event)。remote_id で対応管理。**body 埋め込みマーカーは廃止**
- **Outdated**: GitHub に揃えてバッジ表示(消さない・返信可能)。該当行の表示は ①保存済み文脈スニペット → ②`git show <commit>:<path>` による実履歴からの再構成、の2段構え。GitHub の original_commit_id + diff_hunk が同じ表示パスに写像される
- 再配置は表示時に計算。行番号ドリフトには文脈スニペットの fuzzy マッチで追随
- **ストア**: `~/.config/loopreview/` 配下、repo 単位キー。アンカーが commit を持つため worktree 間共有で曖昧にならない

## 6. マイルストーン

| M | 内容 |
|---|---|
| **M1** | 読む: workspace scaffold、3入力(worktree / ref / stdin patch)、unified + シンタックス + intra-line、ナビ、**watch(live ソース既定 ON)**、CLI(clap、TTY ガード)、CI |
| **M2a** | レビュー(ローカル完結): コメントモデル + `~/.config/loopreview/` ストア + 2ビュー UI。PR なしの worktree レビューでコメント可能 |
| **M2b** | GitHub シンク: PR ソース、コメント双方向(pull = スレッド注入 / push = review submit・返信)、resolve |
| **M3** | 制御面: セッションデーモン + CLI(エージェントが読む・注釈する・操作するための API)。hunk の制約(root 固定・表示状態外部制御不可)を設計で回避 |

### M3 制御面の設計方針 — hunk の骨格を継承し、モデルを対応させる

**継承**(hunk で実証済み): ローカルデーモン + セッションレジストリ(外部から live セッションを発見)/ CLI 動詞体系 list・get・context・review・**navigate(人間の視線誘導)**・reload・comment / 「共有成果物への相互書き込み」という非同期対話モデル / エージェント向けスキル文書をツール自身が同梱(`lr skill path` 相当)/ watch が対話の背景で live 反映。

**置き換え・超える**: flat ノート + マーカーハック → Thread が第一級(返信・resolve・draft)/ 人間入力のポーリング → **イベント購読・wait 動詞**(例: `lr session wait --for reply`)/ reload の root 固定 → 任意ソース読込可 / 表示状態も制御面の操作対象。

### Review は第一級概念

コメントは「レビュー」に属する。**PR はレビューの一種、ローカルレビュー(PR なし・worktree 上)も同格**。素の diff 表示(`git diff | lr`)にコメント UI は出ず、レビュー文脈が始まる瞬間 — 人間が最初の `c` を押す / エージェントが制御面で注釈する / PR を開く — にタブ構造が現れる。ローカルレビューは repo 単位ストアに閉じ、後から同 branch の PR へ昇格(submit)できる余地を残す。エージェント対話(M3)と loopkeep 連携(M4)は PR なしレビューが前提。
| **M4** | 統合アダプタ: herdr ラッパー(picker 移植)、loopkeep ソース/シンク |

## 7. 外部統合方針

- **Cargo 依存は双方向とも禁止**。境界はプロセス + 文書化契約(hello 型のバージョン/capability 交渉)
- loopkeep → loopreview: `lk review` サブコマンドが実行時に解決して exec するだけのシム(git/cargo の外部サブコマンド方式)。不在時は劣化 + インストール案内
- loopreview → loopkeep: loopkeep アダプタが `lk` CLI / loopkeepd ソケットを外部クライアントとして叩く(GitHub と同格の1ソース/シンク)
- 配布同梱はパッケージングレベル(brew depends_on / Tauri sidecar / Releases 取得)。**lk バイナリへの静的埋め込みは不可**
- **core crate の crates.io 公開は将来オプション**(管制室 GUI への埋め込み描画用)。API 安定後(M3 以降)に判断。公開はライセンス決定を強制する点に注意。それ以前の link 実験は cargo git 依存で可

## 8. 開発運用ルール

- 依存は `cargo add` で最新安定版を固定し Cargo.lock をコミット。**tokio 禁止**(std::thread + mpsc)
- `cargo test` / `cargo clippy --all-targets -- -D warnings` / `cargo fmt --check` 常時グリーン。純ロジックに unit test
- Conventional Commits(英語)、main 直コミット
- **private repo でも、他社・他 repo の私有識別子(owner/repo#N・内部 repo 名・内部ブランチ名・内部コード断片)をコミット・コード・docs に一切書かない**(GitHub 自動リンクは相手タイムラインに公開 reference を刻み、削除不可。前身で実事故あり)
- リリース: `v*` タグ → CI がバージョン一致検証(タグ = Cargo.toml)→ マルチターゲットビルド → Releases(前身の release.yml の型を踏襲)

## 9. 実戦で確定済みの技術ファクト(前身より)

- PR の base 比較は **`git fetch origin <baseRefName>` 後の `origin/<base>...`** が正。ローカル ref は「古い base との merge-base に飛び、無関係な変更が混入する」バグの元
- GitHub API: resolve 状態は GraphQL `reviewThreads.isResolved` のみ(REST は返さない)/ REST review comment の `diff_hunk` は文脈表示にそのまま使える / review POST は 1リクエストで body + event + comments[] / 返信 `in_reply_to` は行情報不要 / 会話は issue comments
- 重い処理(checkout・API 呼び出し)は背景スレッド + 段階ラベル付き進捗表示。TUI を同期ブロックしない
- 非 TTY では TUI 起動を丁寧に拒否(help / version は引数処理でガードより先に返す)
