# Oracle Cloud Always Free への移行手順

比較検討は [hosting-comparison.md](hosting-comparison.md) を参照．

## 方針

- **まず並行稼働で試す．** EC2 は止めずに新サーバーを立て，動作確認できてから DuckDNS を切り替える．
- 無料枠で作る．**Pay As You Go に上げる場合も無料枠内なら課金されない**が，アイドル回収を避けられるかは公式に確約されていない．
- シェイプは次のどちらか（どちらも無料）．
  - **Ampere A1（Arm）1 OCPU / 6GB 程度**（推奨）：メモリに余裕があり `cargo build` が速い．Rust は Arm でそのままビルドできる．「out of host capacity」で作れないことがある．
  - **AMD VM.Standard.E2.1.Micro（1GB）**：作りやすいがビルドが遅い（スクリプトが swap 2GB を作る）．帯域は最大 50Mbps．

## 1. あなたが行う作業（アカウント・コンソール）

1. Oracle Cloud にサインアップ（カード登録あり）．**ホームリージョンは Japan East (Tokyo) か Japan Central (Osaka)．後から変更できない．**
2. Compute → Instances → Create instance
   - イメージ：Ubuntu 24.04（Arm を選ぶなら aarch64 版）
   - シェイプ：上記のどちらか（"Always Free-eligible" 表示を確認）
   - SSH 公開鍵を登録（新しく作るか既存の鍵を使う）
   - Public IPv4 を割り当てる．IP が変わらないよう **Reserved public IP** にしておくのが望ましい（無料で付けられるはずだが，作成時に料金表示がないか確認）
3. VCN の **Security List（または NSG）の Ingress ルール**に追加（OS 側 iptables とは別に必要）
   - TCP 22（既定で開いている）, TCP 80, TCP 443
   - UDP 50000-50999（Source 0.0.0.0/0）
4. インスタンスに SSH できることを確認（Ubuntu のユーザー名は `ubuntu`）

## 2. サーバー上の作業

```bash
ssh ubuntu@<公開IP>
git clone https://github.com/<owner>/tetra-server.git ~/tetra-server   # リモートURLは既存のものを使う
bash ~/tetra-server/deploy/setup-oracle.sh tetlabo-canary.duckdns.org <公開IP>
```

スクリプトが行うこと：パッケージ導入，swap 作成，iptables 開放（TCP 80/443，UDP 50000-50999），Rust 導入とビルド，systemd 登録，Nginx 暫定設定．

Oracle 上のメモ：

- パブリック IP は 1:1 NAT でインスタンスの NIC には付かないため，EC2 と同様 **`NAT_1TO1_IP` が必要**（スクリプトが設定する）．
- `TICKET_PUBLIC_KEY` / `CF_TURN_*`（使う場合）は旧サーバーの `/etc/systemd/system/tetra-server.service.d/override.conf` からコピーして追記し，`sudo systemctl daemon-reload` を実行する（`restart` だけでは反映されない）．

## 3. 切り替え

1. `sudo systemctl start tetra-server` → `journalctl -u tetra-server` に `NAT 1:1 IP set to <新IP>` が出ること
2. DuckDNS の IP を新 IP に変更 → `sudo certbot --nginx -d tetlabo-canary.duckdns.org`（DNS 反映前は発行に失敗する）
3. クライアントから接続・マッチング・対戦を確認（可能なら複数端末で）
4. GitHub の自動デプロイを新サーバーに向ける（[deploy.yml](../.github/workflows/deploy.yml) は `ubuntu@` 接続に変更済み）
   - 既存のデプロイ用鍵（`key/tetra_deploy`）をそのまま使う：Mac から公開鍵を新サーバーの `ubuntu` に登録する
     ```bash
     cat key/tetra_deploy.pub | ssh ubuntu@<新IP> 'cat >> ~/.ssh/authorized_keys'
     ssh -i key/tetra_deploy ubuntu@<新IP> 'echo ok'   # 鍵で入れることを確認
     ```
   - Secrets：`EC2_HOST` を新 IP に変更（`EC2_SSH_PRIVATE_KEY` は同じ鍵なので変更不要）
   - Actions タブの「Deploy to server」を手動実行（Run workflow）して成功することを確認
   - `deploy.sh` は `sudo systemctl restart` を使うが，`ubuntu` ユーザーの sudo はパスワード不要（Ubuntu の既定）
5. 数日間は EC2 を残し，問題なければ EC2 を**終了**し，Elastic IP を**解放**する（解放し忘れると課金が続く）

## 4. 運用上の注意

- **アイドル回収**：7 日間，CPU・ネットワーク・（Arm は）メモリの使用率が 95 パーセンタイルで 20% 未満だと回収されうる．メールの通知を見逃さないこと．止まっても `git clone` とこの手順で再作成できる．
- **死活監視**：無料の外形監視（UptimeRobot 等）で `https://<ドメイン>/` を見ておくと，止まったことにすぐ気づける．
- 1 週間ほど運用して回収や不安定さが出るなら，国内 VPS（KAGOYA / さくら）へ同じ手順で移行できる．

## 付録：Ampere A1 を自動で取得する

「out of host capacity」で A1 が作れないときは，[deploy/oci-a1-retry.sh](../deploy/oci-a1-retry.sh) を **tetlabo の Micro 上**で動かし，作成できるまで繰り返し試す．Micro の無料枠（2 台）と A1 の無料枠（2 OCPU / 12GB）は別なので，Micro 2 台を残したまま A1 を作れる．

1. OCI CLI と jq を入れる（`cargo build` 中は避ける．メモリ不足になりやすい）

   ```bash
   sudo apt-get install -y pipx jq
   pipx install oci-cli && pipx ensurepath && exec $SHELL -l
   oci setup config        # 質問に答えると ~/.oci/config と API 鍵ができる
   ```

   表示された公開鍵（`~/.oci/oci_api_key_public.pem`）を，コンソールの「プロファイル → My profile → API keys → Add API key → Paste a public key」に登録する．`oci iam region list` が通れば OK．

2. 新インスタンス用の SSH 鍵と設定ファイルを用意する

   ```bash
   ssh-keygen -t ed25519 -f ~/.ssh/tetlabo_a1 -N ''   # 秘密鍵は Mac にもコピーしておく
   mkdir -p ~/.config
   cp ~/tetra-server/deploy/oci-a1-retry.env.example ~/.config/oci-a1-retry.env
   nano ~/.config/oci-a1-retry.env                     # COMPARTMENT_ID / SUBNET_ID を記入
   ```

   既存の公開鍵（Mac の `~/.ssh/*.pub`）を Micro に置いて `SSH_PUBKEY_FILE` に指定してもよい．

3. tmux で起動して放置する（SSH を切っても続く）

   ```bash
   tmux new -s a1
   bash ~/tetra-server/deploy/oci-a1-retry.sh 2>&1 | tee -a ~/oci-a1-retry.log
   # Ctrl-b d で抜ける．戻るときは tmux attach -t a1
   ```

スクリプトの挙動：

- 試行のたびに A1 インスタンスの有無を確認し，すでにあれば何もせず終了する（二重作成しない）．
- 容量不足なら約 2 分ごとに再試行．429（リクエスト過多）なら間隔を倍に延ばす．
- 認証・権限・上限超過などのエラーは待っても直らないので即終了する．ログを見て設定を直す．
- `NTFY_TOPIC` を設定すると，成功時・停止時にスマホの ntfy アプリへ通知が届く．

作成後：

- 公開 IP はエフェメラルで作られる．固定したい場合は，コンソールでエフェメラル IP を外して Reserved public IP を付け直す．
- 以降は「2. サーバー上の作業」と同じ手順で A1 をセットアップする（Ubuntu 24.04 aarch64）．
- 何日経っても取れない場合は Pay As You Go への切り替えを検討する（無料枠内なら課金なし．予算アラートを設定しておく）．
