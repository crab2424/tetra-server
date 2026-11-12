#!/usr/bin/env bash
# Ampere A1 (VM.Standard.A1.Flex) インスタンスを，作成できるまで繰り返し試すスクリプト．
#
# 使い方:
#   bash deploy/oci-a1-retry.sh [設定ファイル]
#   設定ファイルの既定は ~/.config/oci-a1-retry.env（例: deploy/oci-a1-retry.env.example）
#
# 動作:
#   - 毎回の試行前に，同じ compartment に A1 インスタンスがすでにあるか確認し，あれば終了する
#     （二重作成・無料枠超過の防止）．
#   - "Out of host capacity" などの一時的なエラーは INTERVAL_SEC（+ ランダムな揺らぎ）待って再試行．
#   - 429 TooManyRequests は待ち時間を倍にして再試行（上限 MAX_BACKOFF_SEC）．
#   - 認証エラー・権限エラー・上限超過（LimitExceeded）など，待っても直らないエラーは即終了．
#   - 作成に成功したら RUNNING まで待ち，公開 IP を表示して終了する．
#
# 必要なもの: oci (OCI CLI), jq, flock
set -euo pipefail

CONFIG_FILE="${1:-$HOME/.config/oci-a1-retry.env}"
if [[ ! -f "$CONFIG_FILE" ]]; then
  echo "設定ファイルがありません: $CONFIG_FILE" >&2
  echo "deploy/oci-a1-retry.env.example をコピーして編集してください．" >&2
  exit 1
fi
# shellcheck source=/dev/null
source "$CONFIG_FILE"

# ---- 設定（設定ファイルで上書き） ----
: "${COMPARTMENT_ID:?COMPARTMENT_ID を設定してください}"
: "${SUBNET_ID:?SUBNET_ID を設定してください}"
: "${SSH_PUBKEY_FILE:?SSH_PUBKEY_FILE を設定してください}"
AVAILABILITY_DOMAIN="${AVAILABILITY_DOMAIN:-}"   # 空なら自動取得（東京/大阪は 1 つだけ）
IMAGE_ID="${IMAGE_ID:-}"                         # 空なら Ubuntu 24.04 aarch64 の最新を自動取得
DISPLAY_NAME="${DISPLAY_NAME:-tetlabo-a1}"
OCPUS="${OCPUS:-1}"
MEMORY_GB="${MEMORY_GB:-6}"
BOOT_VOLUME_GB="${BOOT_VOLUME_GB:-50}"
INTERVAL_SEC="${INTERVAL_SEC:-120}"
JITTER_SEC="${JITTER_SEC:-30}"
MAX_BACKOFF_SEC="${MAX_BACKOFF_SEC:-900}"
MAX_UNKNOWN_ERRORS="${MAX_UNKNOWN_ERRORS:-5}"    # 未知のエラーが連続でこの回数続いたら終了
NTFY_TOPIC="${NTFY_TOPIC:-}"                     # 設定すると成功・異常終了時に ntfy.sh へ通知
LOCK_FILE="${LOCK_FILE:-${XDG_RUNTIME_DIR:-/tmp}/oci-a1-retry.lock}"

# 無料枠（2 OCPU / 12GB）を超える指定は拒否する
if (( OCPUS > 2 || MEMORY_GB > 12 )); then
  echo "OCPUS=$OCPUS MEMORY_GB=$MEMORY_GB は Always Free の上限（2 OCPU / 12GB）を超えています．" >&2
  exit 1
fi

for cmd in oci jq flock; do
  command -v "$cmd" >/dev/null || { echo "$cmd が見つかりません．" >&2; exit 1; }
done
[[ -r "$SSH_PUBKEY_FILE" ]] || { echo "SSH 公開鍵が読めません: $SSH_PUBKEY_FILE" >&2; exit 1; }

exec 9>"$LOCK_FILE"
flock -n 9 || { echo "すでに別のプロセスが実行中です（$LOCK_FILE）．" >&2; exit 1; }

log() { printf '%s %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$*"; }

notify() {
  [[ -n "$NTFY_TOPIC" ]] || return 0
  curl -fsS -m 10 -H "Title: oci-a1-retry" -d "$1" "https://ntfy.sh/$NTFY_TOPIC" >/dev/null || true
}

die() {
  log "終了: $*"
  notify "A1 取得スクリプトが停止しました: $*"
  exit 1
}

# 既存の A1 インスタンス（TERMINATED 以外）を数える
count_existing_a1() {
  local out
  out="$(oci compute instance list --compartment-id "$COMPARTMENT_ID" --all 2>&1)" || {
    log "インスタンス一覧の取得に失敗: $(echo "$out" | tr '\n' ' ' | cut -c1-300)"
    echo "error"
    return
  }
  [[ -z "$out" ]] && { echo 0; return; }   # 0 件のとき OCI CLI は何も出力しない
  echo "$out" | jq '[.data[] | select(.shape == "VM.Standard.A1.Flex" and ."lifecycle-state" != "TERMINATED" and ."lifecycle-state" != "TERMINATING")] | length'
}

# ---- 自動取得 ----
if [[ -z "$AVAILABILITY_DOMAIN" ]]; then
  AVAILABILITY_DOMAIN="$(oci iam availability-domain list --compartment-id "$COMPARTMENT_ID" \
    --query 'data[0].name' --raw-output)"
  log "Availability Domain: $AVAILABILITY_DOMAIN"
fi
if [[ -z "$IMAGE_ID" ]]; then
  IMAGE_ID="$(oci compute image list --compartment-id "$COMPARTMENT_ID" \
    --operating-system "Canonical Ubuntu" --operating-system-version "24.04" \
    --shape VM.Standard.A1.Flex --sort-by TIMECREATED --sort-order DESC --limit 1 \
    --query 'data[0].id' --raw-output)"
  [[ -n "$IMAGE_ID" && "$IMAGE_ID" != "null" ]] || die "Ubuntu 24.04 aarch64 のイメージが見つかりません．IMAGE_ID を指定してください．"
  log "Image: $IMAGE_ID"
fi

log "開始: $DISPLAY_NAME（${OCPUS} OCPU / ${MEMORY_GB}GB / boot ${BOOT_VOLUME_GB}GB），間隔 ${INTERVAL_SEC}s"

attempt=0
backoff="$INTERVAL_SEC"
unknown_errors=0

while true; do
  attempt=$((attempt + 1))

  existing="$(count_existing_a1)"
  if [[ "$existing" == "error" ]]; then
    unknown_errors=$((unknown_errors + 1))
    (( unknown_errors >= MAX_UNKNOWN_ERRORS )) && die "一覧取得の失敗が ${unknown_errors} 回続きました．"
  elif (( existing > 0 )); then
    log "A1 インスタンスがすでに ${existing} 台あります．終了します．"
    exit 0
  fi

  if [[ "$existing" != "error" ]]; then
    set +e
    out="$(oci compute instance launch \
      --compartment-id "$COMPARTMENT_ID" \
      --availability-domain "$AVAILABILITY_DOMAIN" \
      --shape VM.Standard.A1.Flex \
      --shape-config "{\"ocpus\": $OCPUS, \"memoryInGBs\": $MEMORY_GB}" \
      --image-id "$IMAGE_ID" \
      --subnet-id "$SUBNET_ID" \
      --assign-public-ip true \
      --boot-volume-size-in-gbs "$BOOT_VOLUME_GB" \
      --display-name "$DISPLAY_NAME" \
      --ssh-authorized-keys-file "$SSH_PUBKEY_FILE" \
      --wait-for-state RUNNING --max-wait-seconds 900 2>&1)"
    rc=$?
    set -e

    if (( rc == 0 )); then
      instance_id="$(echo "$out" | sed -n '/^{/,$p' | jq -r '.data.id' 2>/dev/null || true)"
      ip=""
      if [[ -n "$instance_id" && "$instance_id" != "null" ]]; then
        ip="$(oci compute instance list-vnics --instance-id "$instance_id" \
          --query 'data[0]."public-ip"' --raw-output 2>/dev/null || true)"
      fi
      log "成功（試行 ${attempt} 回目）: id=${instance_id:-?} public-ip=${ip:-?}"
      notify "A1 インスタンスを作成しました: ${DISPLAY_NAME} ${ip:-}"
      exit 0
    fi

    msg="$(echo "$out" | tr '\n' ' ' | cut -c1-300)"
    case "$out" in
      *"Out of host capacity"*|*"out of host capacity"*)
        log "#${attempt} 容量不足"
        backoff="$INTERVAL_SEC"; unknown_errors=0 ;;
      *TooManyRequests*|*'"status": 429'*)
        backoff=$(( backoff * 2 > MAX_BACKOFF_SEC ? MAX_BACKOFF_SEC : backoff * 2 ))
        log "#${attempt} 429 TooManyRequests → ${backoff}s 待機"
        unknown_errors=0 ;;
      *LimitExceeded*|*QuotaExceeded*)
        die "上限超過（LimitExceeded）．無料枠をすでに使い切っていないか確認してください: $msg" ;;
      *NotAuthenticated*|*NotAuthorizedOrNotFound*|*'"status": 401'*|*'"status": 404'*|*InvalidParameter*|*'"status": 400'*)
        die "設定・権限のエラー: $msg" ;;
      *InternalError*|*'"status": 500'*|*'"status": 502'*|*'"status": 503'*|*"timed out"*|*"Connection"*)
        log "#${attempt} 一時的なエラー: $msg"
        backoff="$INTERVAL_SEC"; unknown_errors=0 ;;
      *)
        unknown_errors=$((unknown_errors + 1))
        log "#${attempt} 不明なエラー（${unknown_errors}/${MAX_UNKNOWN_ERRORS}）: $msg"
        (( unknown_errors >= MAX_UNKNOWN_ERRORS )) && die "不明なエラーが ${unknown_errors} 回続きました．" ;;
    esac
  fi

  sleep $(( backoff + RANDOM % (JITTER_SEC + 1) ))
done
