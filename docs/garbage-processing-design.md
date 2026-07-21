# おじゃま送信処理の実装設計

作成日: 2026-07-21

## 目的

TETLABO の TET／PUYO 対戦におけるおじゃま処理を、次セッションから実装できる粒度で整理する。

対象は次の3ケース。

- TET vs TET
- PUYO vs PUYO
- TET vs PUYO

## 基本方針

- ゲームルールに依存する処理はクライアントが担当する。
- サーバーはおじゃまを計算・相殺・降下させず、安全なゲームイベントとして相手へ中継する。
- CPU戦とオンライン戦で攻撃計算・相殺・降下ロジックを分けない。
- CPU戦とオンライン戦の差は、おじゃまをローカルキューへ直接投入するか、ネットワークへ送信するかだけにする。
- おじゃまが相手へ届いたことと、降下可能になったことは別状態として扱う。

## クライアントの責務

クライアントは以下を担当する。

- 消去、T-spin、REN、B2B、PC、連鎖から攻撃量を計算する。
- TET／PUYO 間の攻撃量変換を行う。
- 自分の受信おじゃまを相殺する。
- 相殺後の余りだけを相手へ送る。
- `internal`、`ready:false`、`ready:true` のおじゃまキューを管理する。
- 予告表示、猶予時間、点滅状態を管理する。
- ready 状態のおじゃまを、ゲームエンジンの正しいタイミングで降下させる。
- TET の穴生成、PUYO の列生成、トップアウト判定を行う。

### 送信側

#### TET

1. ミノ固定時に攻撃量を計算する。
2. 自分の受信キューを相殺する。
3. 相殺後の余りを相手ルール向けに変換する。
4. TET 受けならライン数と穴配列を作る。
5. PUYO 受けならおじゃま個数と列配列を作る。
6. Garbage イベントを reliable チャネルへ送信する。

#### PUYO

1. 消去後に連鎖攻撃量を計算する。
2. 自分の受信おじゃまを相殺する。
3. 相殺後の余りを相手ルール向けに変換する。
4. TET 受けならライン数と穴配列を作る。
5. PUYO 受けならおじゃま個数と列配列を作る。
6. Garbage イベントを reliable チャネルへ送信する。

### 受信側

受信時点では直ちに降下させない。

```text
Garbage受信
  -> ready:false でキューへ追加
  -> 猶予タイマー終了
  -> ready:true へ遷移
  -> 次のミノ固定／連鎖終了で降下
```

- TET は次のミノ固定時に `applyGarbage()` する。
- PUYO は連鎖終了後の `checkErase` から `_generateOjama()` を呼ぶ。
- 相殺は通常、`ready:true` を古い順に先に消費し、その後 `ready:false` を消費する。
- PUYO の1回の降下上限など、既存ゲームエンジンの制限は維持する。

## 猶予タイマーの管理場所

猶予タイマーはサーバーではなく、クライアントのゲーム状態機械に属する `GarbageQueue`／`GarbageScheduler` で管理する。

理由:

- ポーズ、一時停止、タブ非表示、ゲームオーバーなど、ゲーム状態と同期させる必要がある。
- 実際に降下できるタイミングは TET／PUYO のゲームエンジンごとに異なる。
- CPU戦とオンライン戦で同じ状態遷移を利用できる。
- サーバータイマーにすると、ネットワーク遅延・再接続・クライアントの描画時刻とのずれが増える。
- サーバーはゲーム盤面を持たないため、タイマー終了後に正しく降下できるか判断できない。

### 推奨構造

```text
GarbageQueue
  - enqueue(amount, holes, source, receivedAt)
  - advance(now, paused)
  - cancel(amount)
  - takeReady(maxAmount)

CPU送信経路      -> 相手の GarbageQueue.enqueue()
オンライン受信  -> 自分の GarbageQueue.enqueue()
ゲームエンジン  -> advance() / takeReady() / cancel()
```

タイマーは `setTimeout` をゲーム状態の唯一の根拠にせず、キュー項目に `readyAt` または残り時間を持たせ、ゲームループまたは再開処理で `advance(now)` を呼ぶ。これによりポーズ中は状態を進めず、復帰時にも正しく再計算できる。

既存の CPU 戦にあるローカル配送タイマーは、最終的にこのキューのスケジューラへ統合する。オンライン受信時も、ネットワーク層が直接 `ready:true` に変更せず、同じ `enqueue()` を呼ぶ。

## サーバーの責務

サーバーは次を行う。

- 送信元が対戦中のルームに所属しているか確認する。
- 対戦相手にだけ中継する。送信者自身には反射しない。
- Garbage は reliable チャネルからのみ受け付ける。
- opcode、プロトコルバージョン、payload の最小／最大サイズを検証する。
- `amount` と `holes` の個数が一致することを検証する。
- TET の穴が `0..9`、PUYO の列が `0..5` であることを検証する。
- 対戦終了後、またはルーム外からの Garbage を破棄する。
- 送信レートと1フレームあたりの最大量を制限する。
- 不正フレームを理由付きでログに記録する。

サーバーは以下を行わない。

- 得点・連鎖からの攻撃量再計算
- TET／PUYO の攻撃量変換
- 相殺
- 猶予タイマー
- おじゃま降下
- トップアウト判定

## wire format

現在の Rust 実装を正規形式として、ゲーム opcode は `0x20..0x28` に統一する。

| 用途 | opcode | チャネル |
|---|---:|---|
| PieceState | `0x20` | unreliable |
| Spawn | `0x21` | reliable |
| Lock | `0x22` | reliable |
| Clear | `0x23` | reliable |
| Garbage | `0x24` | reliable |
| Hold | `0x25` | reliable |
| GameOver | `0x26` | reliable |
| ChainReplay | `0x27` | reliable |
| SE | `0x28` | reliable |

現在のドキュメントと `testclient` は `0x06`／`0x07` の旧形式を使用しているため、実装前に更新する。

Garbage payload は少なくとも以下を満たす。

```text
version:u8
amount:u16
holes:u8[amount]
```

`holes` の意味は受け手のルールで決める。

- TET 受け: 各ラインの穴位置 `0..9`
- PUYO 受け: 各おじゃまの列 `0..5`

## 実装順序

1. opcode とドキュメント／testclient／クライアント本体を統一する。
2. サーバーの reliable／unreliable 別 opcode 判定を実装する。
3. Garbage payload の decode とサイズ・範囲検証を追加する。
4. サーバーのルーム状態・ルール・レート制限検証を追加する。
5. クライアントに共通 `GarbageQueue`／`GarbageScheduler` を作る。
6. CPU 戦のローカル配送を共通キューへ接続する。
7. オンライン受信を共通キューへ接続する。
8. TET／PUYO の降下トリガーへ接続する。
9. TET vs TET、PUYO vs PUYO、TET vs PUYO の順に統合テストする。

## テスト項目

- TET -> TET のライン数・穴配列
- PUYO -> PUYO のおじゃま個数・列配列
- TET -> PUYO の変換
- PUYO -> TET の変換
- `ready:true` と `ready:false` の相殺順序
- ポーズ中の猶予タイマー停止
- 再開後の `readyAt` 再計算
- 連鎖中の PUYO おじゃま降下タイミング
- ミノ固定時の TET おじゃま降下タイミング
- Garbage の unreliable 送信拒否
- `amount` と穴配列数の不一致拒否
- 範囲外の穴／列拒否
- 対戦終了後・ルーム外からの送信拒否
- レート制限と最大 payload
