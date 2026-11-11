# サーバー運用先の比較検討（2026-11-12 時点）

tetra-server を今後どこで動かすかの検討結果．料金・受付状況は調査日時点のもので，契約前に各公式ページで再確認すること．
円換算は 1 USD ≒ 150 円の仮定．

## 結論

1. **EC2 のまま続ける理由はない．** 無料プラン終了後は月 $13〜15（年 2 万円強）かかり，同等性能の VPS の 3〜4 倍になる．
2. **第一候補は「東京・メモリ 1GB・月額固定」の VPS（月 450〜1,050 円）．** 現行のデプロイ手順（サーバー上で `cargo build`）をそのまま使え，レイテンシも現状と同等．
3. **完全無料にこだわるなら Oracle Cloud Always Free（東京/大阪）** が唯一の現実的な選択肢．ただし回収・仕様変更のリスクを許容できる場合のみ．
4. **期限に注意．** 現在の AWS アカウントがクレジット制の無料プランなら，作成 6 か月後（またはクレジット枯渇時）にアカウントが停止される．アカウント作成が 2026 年 7 月頃なので，**2027 年 1 月頃までに移行か有料プランへの切替が必要**．正確な期限は AWS の Billing 画面（Credits）で確認する．

## 前提：このサーバーが運用先に求めるもの

| 要件 | 理由 | 影響 |
| --- | --- | --- |
| UDP の受信（ポートレンジ） | WebRTC の ICE（`ICE_UDP_PORT_RANGE=50000-50999`）．全通信がサーバー経由の C/S 型 | HTTP 専用の PaaS は使えない |
| 常時起動・単一プロセス | ルーム状態をメモリに保持 | スケールtoゼロ型・サーバーレスは不可 |
| 固定のグローバル IPv4 | `NAT_1TO1_IP`，DNS の A レコード | IPv6 専用プランは不可 |
| メモリ 1GB（ビルドする場合） | `webrtc` クレートの release ビルド | 512MB プランは CI でビルドして配布する構成に変更が必要 |
| 日本国内リージョン | 対戦のレイテンシ | 米国リージョンのみの無料枠は不向き |

負荷は問題にならない．同時 10 人程度なら CPU もメモリも転送量もどのプランでも余る．

## 現状のコスト内訳（推定）

4 か月で約 $50 ＝ 月 約 $12.5．東京リージョンの概算と整合する．

| 項目 | 月額の目安 |
| --- | --- |
| EC2 t3.micro（約 $0.0136/時） | 約 $10 |
| パブリック IPv4（$0.005/時） | 約 $3.65 |
| EBS（gp3） | 約 $1〜3 |

2025-07-15 以降に作成した AWS アカウントの「無料枠」は，旧来の「t2/t3.micro 12 か月無料」ではなく **クレジット（$100＋最大 $100）を消費する方式**．$50 はこのクレジットの消費分と考えられる（Billing 画面で要確認）．利用者数に関係なく起動しているだけで発生する固定費であり，アクティブユーザーがいなくても減らない．

## 候補の比較

| 候補 | 月額 | 課金形態 | メモリ | リージョン | UDP | 評価 |
| --- | --- | --- | --- | --- | --- | --- |
| AWS EC2 t3.micro（現状） | 約 $13〜15（約 2,000〜2,250 円） | 従量 | 1GB | 東京 | ○ | 割高．継続の理由なし |
| AWS Lightsail | $7（約 1,050 円）／0.5GB は $5 | 月額固定（転送量込み） | 1GB | 東京 | ○ | 移行が最も楽．AWS に残るならこれ |
| WebARENA Indigo | 449 円 | 時間課金・月額上限あり | 1GB | 東京 | ○ | 最安．**新規申込一時停止中**（2026-10-21 告知） |
| KAGOYA CLOUD VPS | 550 円（日額 20 円） | 日額課金・月額上限あり | 1GB | 国内（所在地は要確認） | ○ | 現時点で申し込める最安クラス |
| さくらの VPS | 512MB：643〜698 円／1GB：880〜990 円 | 月額固定 | 512MB / 1GB | 石狩・大阪・東京 | ○ | 2 週間無料お試しあり．老舗で安定 |
| ConoHa VPS | 512MB：460 円／1GB：763 円（1 か月契約） | 月額固定 or 時間課金 | 512MB / 1GB | 東京 | ○ | **新規申込停止中**（公式ページに告知） |
| Vultr | $5（約 750 円） | 時間課金・月額上限あり | 1GB | 東京・大阪 | ○ | 海外事業者．ドル建て |
| Oracle Cloud Always Free | 0 円 | 完全無料（期限なし） | 1GB（AMD）／最大 12GB（Arm） | 東京・大阪（ホームリージョンのみ） | ○ | 無料だがリスクあり（後述） |
| Google Cloud e2-micro | 0 円 | 無料枠（期限なし） | 1GB | **米国 3 リージョンのみ** | ○ | レイテンシで不適 |
| Fly.io | 約 $4.2〜（マシン $2.19＋専用 IPv4 $2） | 従量 | 256MB〜 | 東京 | △ | コード変更が必要．利点なし |
| Render / Railway / Cloud Run など | — | — | — | — | × | UDP 受信不可のため対象外 |

### レイテンシ

- 国内リージョン（東京・大阪・石狩）同士の差は数 ms〜十数 ms 程度で，対戦の体感差はほぼ出ない．事業者による差より，プレイヤー側の回線の影響の方が大きい．
- Google Cloud の無料枠は米国（オレゴン等）限定で，日本からの往復遅延は 100ms 以上になる．対戦用途では採用しない．
- 上記は一般的な目安であり，実測はしていない．移行時は試用期間中にクライアントから実測して確認する．

### 各候補の補足

**AWS Lightsail**
- 同じ AWS アカウント内で完結し，静的 IP はインスタンスに付けている限り無料．転送量 1〜2TB 込みで請求が読みやすい．
- 無料プランのクレジットが Lightsail に適用できるかは未確認．
- $5 プラン（0.5GB）はサーバー上でのビルドが厳しい．

**国内格安 VPS（Indigo / KAGOYA / さくら / ConoHa）**
- グローバル IPv4 が直接付くため，`NAT_1TO1_IP` が不要になる可能性が高い（インスタンスの NIC にグローバル IP が付く場合）．
- 2026 年 10 月以降，Indigo と ConoHa が新規申込を停止している．申込時点の受付状況を必ず確認する．
- 円建て・月額上限ありで，為替と従量課金の心配がない．

**Oracle Cloud Always Free**
- AMD マイクロ（1/8 OCPU・1GB・グローバル IP 付き）2 台，または Arm（計 2 OCPU・12GB）が期限なしで無料．転送量も月 10TB まで無料．
- リスク：
  - **アイドル回収**：7 日間の CPU・ネットワーク使用率（95 パーセンタイル）が 20% 未満だと回収対象になりうる．利用者がほぼいないこのサーバーは該当する．
  - **仕様変更**：2026 年に Arm 枠が 4 OCPU/24GB から 2 OCPU/12GB に半減された（第三者報告多数，公式ドキュメントも現在は 2 OCPU/12GB 表記）．無料枠は一方的に変わりうる．
  - 登録時のカード審査落ち，無料枠インスタンスの空き不足（out of host capacity），ホームリージョンは後から変更不可．
- 従量課金（PAYG）アカウントに上げるとアイドル回収を避けられるという報告があるが，公式の確約は確認できていない．

**Fly.io**
- UDP は専用 IPv4 が必要で，`fly-global-services` へのバインドなどアプリ側の対応が必要．ICE のポートレンジ運用が可能かは公式ドキュメントで確認できなかった．検証コストに見合う利点がない．

## 推奨

| 方針 | 選択 | 年額の目安 |
| --- | --- | --- |
| 手間最小・確実（推奨） | 1GB の国内 VPS：KAGOYA（550 円）／受付再開していれば Indigo（449 円） | 約 5,400〜6,600 円 |
| AWS から出たくない | Lightsail $7 | 約 12,600 円 |
| 完全無料を優先 | Oracle Cloud Always Free（AMD マイクロ，東京 or 大阪） | 0 円 |
| 参考：EC2 継続 | — | 約 23,000〜27,000 円 |

推奨は 1GB の国内 VPS．理由は次のとおり．

- 現在の EC2（1GB）と同じ条件なので，`deploy.sh`（サーバー上で `cargo build --release`）をそのまま使える．
- 月 500 円前後の固定額で，放置していても請求が増えない．
- 無料枠の回収や仕様変更で，気づかないうちにサーバーが止まる事態を避けられる．

Oracle を選ぶ場合は「止まったら作り直す」前提で，まず無料で試し，問題があれば VPS に切り替える進め方が安全．

## 移行時の作業（どの候補でも共通）

現行構成（systemd＋Nginx＋certbot）はそのまま持っていける．ドメイン（`tetlabo-canary.duckdns.org`）を変えなければ，クライアント側の変更と `TICKET_AUDIENCE` の変更は不要．

1. 新サーバーに Rust・Nginx・certbot を導入し，リポジトリを clone してビルド
2. `deploy/tetra-server.service` の `User` / パス（現在は `ec2-user` 前提）を新環境に合わせる
3. 環境変数を移す（`override.conf`）．`NAT_1TO1_IP` は新 IP に変更，または不要なら削除．編集後は `systemctl daemon-reload`
4. ファイアウォールで TCP 80/443 と UDP 50000-50999 を開放
5. DuckDNS の A レコードを新 IP に向け，certbot で証明書を再取得
6. GitHub Secrets（`EC2_HOST`，`EC2_SSH_PRIVATE_KEY`）と `deploy.sh` の接続ユーザーを更新
7. クライアントから接続・対戦を確認後，EC2 インスタンスの終了と Elastic IP の解放（解放し忘れると課金が続く）

## 出典

- AWS 無料プラン：<https://aws.amazon.com/free/faqs/> ／ <https://aws.amazon.com/blogs/aws/aws-free-tier-update-new-customers-can-get-started-and-explore-aws-with-up-to-200-in-credits>
- Lightsail 料金：<https://aws.amazon.com/lightsail/pricing/>
- WebARENA Indigo：<https://web.arena.ne.jp/indigo/>
- KAGOYA CLOUD VPS：<https://www.kagoya.jp/cloud/vps/>
- さくらの VPS：<https://vps.sakura.ad.jp/specification/>
- ConoHa VPS：<https://vps.conoha.jp/pricing/>
- Oracle Always Free：<https://docs.oracle.com/en-us/iaas/Content/FreeTier/freetier_topic-Always_Free_Resources.htm> ／ 変更の報告 <https://news.ycombinator.com/item?id=49183750>
- Google Cloud 無料枠：<https://cloud.google.com/free/docs/compute-getting-started>
- Fly.io：<https://fly.io/pricing> ／ <https://docs.fly.io/networking/udp-and-tcp>
- Vultr（第三者集計）：<https://www.costbench.com/software/cloud-infrastructure/vultr/>
