#!/usr/bin/env bash
# EC2上で実行されるデプロイスクリプト。GitHub Actionsからssh経由で呼ばれる。
set -euo pipefail

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
