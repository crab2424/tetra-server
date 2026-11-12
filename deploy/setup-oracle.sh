#!/usr/bin/env bash
# Oracle Cloud (Ubuntu 22.04/24.04) の新規インスタンス初期セットアップ。
# インスタンス上で一度だけ実行する:  bash setup-oracle.sh <ドメイン> <パブリックIP>
# 例: bash setup-oracle.sh tetlabo-canary.duckdns.org 203.0.113.10
set -euo pipefail

DOMAIN="${1:?usage: setup-oracle.sh <domain> <public-ip>}"
PUBLIC_IP="${2:?usage: setup-oracle.sh <domain> <public-ip>}"
USER_NAME="$(id -un)"
REPO_DIR="$HOME/tetra-server"

echo "==> packages"
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libssl-dev git curl nginx certbot python3-certbot-nginx iptables-persistent

echo "==> swap (1GB RAM の AMD micro でもビルドできるように 2GB 確保)"
if ! swapon --show | grep -q swapfile; then
  sudo fallocate -l 2G /swapfile
  sudo chmod 600 /swapfile
  sudo mkswap /swapfile
  sudo swapon /swapfile
  echo '/swapfile none swap sw 0 0' | sudo tee -a /etc/fstab
fi

echo "==> OS側ファイアウォール (Oracle の Ubuntu は既定で iptables が閉じている)"
# REJECT ルールより前 (位置 5) に挿入する
sudo iptables -I INPUT 5 -p tcp --dport 80 -j ACCEPT
sudo iptables -I INPUT 5 -p tcp --dport 443 -j ACCEPT
sudo iptables -I INPUT 5 -p udp --dport 50000:50999 -j ACCEPT
sudo netfilter-persistent save

echo "==> rust"
if ! command -v cargo >/dev/null; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
fi
# shellcheck disable=SC1091
source "$HOME/.cargo/env"

echo "==> repo"
if [ ! -d "$REPO_DIR" ]; then
  echo "先に ~/tetra-server へ git clone してから再実行してください" >&2
  exit 1
fi
cd "$REPO_DIR"
cargo build --release

echo "==> systemd unit (User を現在のユーザーに置換)"
sed "s#/home/ec2-user#$HOME#g; s#^User=.*#User=$USER_NAME#" deploy/tetra-server.service \
  | sudo tee /etc/systemd/system/tetra-server.service >/dev/null
sudo mkdir -p /etc/systemd/system/tetra-server.service.d
sudo tee /etc/systemd/system/tetra-server.service.d/override.conf >/dev/null <<EOF
[Service]
Environment=NAT_1TO1_IP=$PUBLIC_IP
Environment=ICE_UDP_PORT_RANGE=50000-50999
Environment=TICKET_AUDIENCE=$DOMAIN
# TICKET_PUBLIC_KEY / CF_TURN_*（使う場合）は旧サーバーの override.conf からコピーしてここに追記する
EOF
sudo systemctl daemon-reload
sudo systemctl enable tetra-server

echo "==> nginx (certbot 発行前なので 80 番だけの暫定設定)"
sudo tee /etc/nginx/sites-available/tetra >/dev/null <<EOF
server {
    listen 80;
    server_name $DOMAIN;
    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        proxy_set_header Upgrade \$http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_set_header Host \$host;
        proxy_read_timeout 3600s;
        proxy_send_timeout 3600s;
    }
}
EOF
sudo ln -sf /etc/nginx/sites-available/tetra /etc/nginx/sites-enabled/tetra
sudo rm -f /etc/nginx/sites-enabled/default
sudo nginx -t && sudo systemctl reload nginx

cat <<EOF

セットアップ完了。残りは手動:
  1. 旧サーバーの override.conf から TICKET_PUBLIC_KEY / CF_TURN_*（使う場合）を
     /etc/systemd/system/tetra-server.service.d/override.conf に追記し daemon-reload
  2. DuckDNS の IP を $PUBLIC_IP に切り替え
  3. sudo certbot --nginx -d $DOMAIN   (443 の設定は certbot が追加する)
  4. sudo systemctl start tetra-server && journalctl -u tetra-server -f
EOF
