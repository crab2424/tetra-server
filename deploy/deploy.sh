#!/usr/bin/env bash
# サーバー上で実行されるデプロイスクリプト。GitHub Actionsからssh経由で呼ばれる。
set -euo pipefail

# ssh で直接コマンドを実行すると非対話シェルになり，Ubuntu の ~/.bashrc は冒頭で抜けるため
# rustup が追記した PATH 設定が読まれない（cargo: command not found になる）。明示的に読み込む。
if [ -f "$HOME/.cargo/env" ]; then
  # shellcheck disable=SC1091
  source "$HOME/.cargo/env"
fi

cd ~/tetra-server

echo "==> git pull"
git fetch origin main
git reset --hard origin/main

echo "==> cargo build --release"
cargo build --release

echo "==> restart tetra-server"
sudo systemctl restart tetra-server
sleep 2
systemctl is-active tetra-server

echo "==> deploy done: $(git rev-parse --short HEAD)"
