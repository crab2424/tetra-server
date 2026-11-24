use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tracing::{debug, info, warn};
use uuid::Uuid;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;

use crate::account::Account;
use crate::room::{Battle, Room, RoomStatus, ZoneProgress};

/// 宛先なしの旧形式（PROTOCOL 1/2 のクライアント）。生きている相手全員へ中継する。
pub const GARBAGE_PROTOCOL_VERSION_LEGACY: u8 = 1;
/// 宛先ルール付きの形式（PROTOCOL 3 のクライアント）:
/// version(=2) + target:u8(1=tet / 2=puyo) + amount:u16(LE) + holes:u8[amount]。
/// 混在 3 人以上の対戦では 1 回の攻撃で tet 向け（ライン数）・puyo 向け（おじゃま個数）の 2 本が
/// 飛ぶので、target のルールの相手だけへ中継する（宛先が無いと受け手が違う単位で読んでしまう）。
pub const GARBAGE_PROTOCOL_VERSION: u8 = 2;
/// 版 3（PROTOCOL 4 のクライアント。ZONE BATTLE ONLINE 設計 §6.2）:
/// version(=3) + target:u8 + flags:u8 + amount:u16(LE) + holes:u8[amount]。
/// flags bit0 = zoneChunk（かたまりごとに穴を引き直す）、bit1 = zoneHold（1 手に 1 かたまり）。
/// ZONE 以外の火力も版 3（flags = 0）で届く。印はテト向けだけ（ぷよ向けは 0 でなければ断る）。
pub const GARBAGE_PROTOCOL_VERSION_ZONE: u8 = 3;
pub const GARBAGE_FLAGS_MASK: u8 = 0b0000_0011;
pub const MAX_GARBAGE_AMOUNT: usize = 1_000;
pub const MAX_GARBAGE_FRAME_BYTES: usize = 1 + 1 + 1 + 1 + 2 + MAX_GARBAGE_AMOUNT;
pub const GARBAGE_RATE_LIMIT: usize = 60;
pub const GARBAGE_RATE_WINDOW_SECS: u64 = 1;

/// Garbage の宛先。None は旧形式（宛先なし）。
pub fn garbage_target_rule(body: &[u8]) -> Result<Option<&'static str>, String> {
    match body.first() {
        Some(&GARBAGE_PROTOCOL_VERSION_LEGACY) => Ok(None),
        Some(&GARBAGE_PROTOCOL_VERSION) | Some(&GARBAGE_PROTOCOL_VERSION_ZONE) => match body.get(1) {
            Some(1) => Ok(Some("tet")),
            Some(2) => Ok(Some("puyo")),
            Some(t) => Err(format!("Invalid Garbage target: {t}")),
            None => Err("Garbage payload is too short".to_string()),
        },
        Some(v) => Err(format!("Unsupported Garbage version: {v}")),
        None => Err("Garbage payload is too short".to_string()),
    }
}

/// Garbage 本体を検証し、受信側ルールに応じた穴/列の値を返す。
pub fn validate_garbage_payload(body: &[u8], receiver_rule: &str) -> Result<(), String> {
    // 版ごとの「amount の位置」。旧形式は version の直後、版 2 は version + target の後、
    // 版 3 は version + target + flags の後
    let target = garbage_target_rule(body)?;
    let header = match (target, body.first()) {
        (None, _) => 1,
        (Some(_), Some(&GARBAGE_PROTOCOL_VERSION_ZONE)) => 3,
        (Some(_), _) => 2,
    };
    if header == 3 {
        let flags = *body.get(2).ok_or_else(|| "Garbage payload is too short".to_string())?;
        if flags & !GARBAGE_FLAGS_MASK != 0 {
            return Err(format!("Invalid Garbage flags: {flags:#04x}"));
        }
        if flags != 0 && target != Some("tet") {
            return Err("Garbage ZONE flags are only for TET receivers".to_string());
        }
    }
    if body.len() < header + 2 {
        return Err("Garbage payload is too short".to_string());
    }

    let amount = u16::from_le_bytes([body[header], body[header + 1]]) as usize;
    if amount == 0 {
        return Err("Garbage amount must be greater than zero".to_string());
    }
    if amount > MAX_GARBAGE_AMOUNT {
        return Err(format!(
            "Garbage amount exceeds maximum: {MAX_GARBAGE_AMOUNT}"
        ));
    }
    let expected_len = header + 2 + amount;
    if body.len() != expected_len {
        return Err(format!(
            "Garbage amount does not match holes length: amount={amount}, holes={}",
            body.len().saturating_sub(header + 2)
        ));
    }

    let max_value = match receiver_rule {
        "tet" => 9,
        "puyo" => 5,
        _ => return Err("Invalid receiver rule".to_string()),
    };
    if body[header + 2..].iter().any(|hole| *hole > max_value) {
        return Err(format!(
            "Garbage hole/column must be between 0 and {max_value}"
        ));
    }
    Ok(())
}

/// オンライン対戦で CPU 対戦設定から独立して使う標準ルール。
/// ランダムマッチと通常のルームマッチの初期値をサーバー側で統一する。
pub const ONLINE_DEFAULT_MATCH_SETTING: &str = r#"{"setTarget":1,"holdEnabled":true,"b2bBonus":true,"garbageMultiplier":1.0,"marginTime":96,"garbageHoleRate":70,"garbageDamageOnClear":true,"ojamaRate":70}"#;

/// ZONE BATTLE の部屋の既定（設計 §4。クライアントのローカル ZONE BATTLE の既定
/// public/config/vs_settings.js DEFAULT_ZONE_BATTLE_SETTINGS と手で合わせる）。
pub const ONLINE_DEFAULT_ZONE_MATCH_SETTING: &str = r#"{"setTarget":1,"garbageMultiplier":1.0,"holdEnabled":true,"b2bBonus":true,"garbageDamageOnClear":false,"zoneSplit":true,"marginEnabled":true,"marginTrigger":"time","marginTime":96,"marginScore":20000,"phase":true,"phaseTrigger":"score","phaseTable":[{"holeRate":90,"holeChunk":90,"zoneTime":5000,"zoneCharge":6,"scoreAdd":0,"timeAdd":0},{"holeRate":90,"holeChunk":10,"zoneTime":4000,"zoneCharge":8,"scoreAdd":20000,"timeAdd":64000},{"holeRate":10,"holeChunk":10,"zoneTime":2500,"zoneCharge":10,"scoreAdd":40000,"timeAdd":128000}],"garbageHoleRate":90,"garbageHoleChunk":10,"zoneQuarterMs":5000,"zoneQuarterLines":6}"#;

/// 部屋の種類ごとの既定の部屋設定
pub fn default_match_setting(battle: Battle) -> &'static str {
    match battle {
        Battle::Versus => ONLINE_DEFAULT_MATCH_SETTING,
        Battle::Zone => ONLINE_DEFAULT_ZONE_MATCH_SETTING,
    }
}

// ── ZONE BATTLE の部屋設定の選択肢（クライアントと手で合わせる。設計 §4・§12）──
// MARGIN TIME（秒。public/config/vs_settings.js MARGIN_TIME_STEPS）
const ZONE_MARGIN_TIME_STEPS: &[i64] = &[-160, -128, -96, -64, -32, 0, 32, 64, 96, 128, 160, 192];
// MARGIN SCORE（突入の点。MARGIN_SCORE_STEPS）
const ZONE_MARGIN_SCORE_STEPS: &[i64] = &[0, 20000, 40000, 80000, 100000, 160000, 200000];
// MARGIN TRIGGER: SCORE の 1 段階の点・段階の上限（public/game/tet/zone.js TET_MARGIN_SCORE_STEP / TET_MARGIN_MAX_STEP_SCORE）
pub const ZONE_MARGIN_SCORE_STEP: u32 = 20000;
pub const ZONE_MARGIN_MAX_STEP: u8 = 5;
// PHASE 02・03 へ進むしきい値（前のフェーズに足す値。PHASE_SCORE_STEPS / PHASE_TIME_STEPS）
const ZONE_PHASE_SCORE_STEPS: &[i64] = &[10000, 20000, 30000, 40000, 60000, 80000, 100000, 120000, 160000, 200000];
const ZONE_PHASE_TIME_STEPS: &[i64] = &[16000, 32000, 48000, 64000, 96000, 128000, 160000, 192000, 256000, 320000];
// ZONE TIME（1/4 の長さ ms）・ZONE CHARGE（1/4 のライン数）（public/game/tet/zone.js ZONE_QUARTER_MS_OPTIONS / ZONE_QUARTER_LINES_OPTIONS）
const ZONE_TIME_OPTIONS: &[i64] = &[2500, 4000, 5000, 8000, 10000, 15000];
const ZONE_CHARGE_OPTIONS: &[i64] = &[3, 4, 6, 8, 10, 12];

/// 整数で、選択肢のどれかであること
fn int_in(key: &str, value: &serde_json::Value, options: &[i64]) -> Result<(), String> {
    match value.as_i64() {
        Some(v) if options.contains(&v) => Ok(()),
        _ => Err(format!("{key} must be one of {options:?}")),
    }
}

/// HOLE RATE: 0〜100 の 10 刻みの整数
fn hole_rate(key: &str, value: &serde_json::Value) -> Result<(), String> {
    match value.as_i64() {
        Some(v) if (0..=100).contains(&v) && v % 10 == 0 => Ok(()),
        _ => Err(format!("{key} must be an integer between 0 and 100 in steps of 10")),
    }
}

/// PHASE 01〜03 の表（3 行。行の中の知らないキーは捨てる）
fn sanitize_phase_table(value: &serde_json::Value) -> Result<serde_json::Value, String> {
    let rows = value
        .as_array()
        .filter(|rows| rows.len() == 3)
        .ok_or_else(|| "phaseTable must be an array of 3 rows".to_string())?;
    let mut out = Vec::with_capacity(3);
    for (i, row) in rows.iter().enumerate() {
        let row = row
            .as_object()
            .ok_or_else(|| "phaseTable rows must be objects".to_string())?;
        let mut clean = serde_json::Map::new();
        for (key, v) in row {
            match key.as_str() {
                "holeRate" | "holeChunk" => hole_rate(key, v)?,
                "zoneTime" => int_in(key, v, ZONE_TIME_OPTIONS)?,
                "zoneCharge" => int_in(key, v, ZONE_CHARGE_OPTIONS)?,
                // 01 は 0 固定（開始時）。02・03 は前のフェーズに足す値
                "scoreAdd" if i == 0 => int_in(key, v, &[0])?,
                "timeAdd" if i == 0 => int_in(key, v, &[0])?,
                "scoreAdd" => int_in(key, v, ZONE_PHASE_SCORE_STEPS)?,
                "timeAdd" => int_in(key, v, ZONE_PHASE_TIME_STEPS)?,
                unknown => {
                    warn!("Ignoring unknown phaseTable key: {unknown}");
                    continue;
                }
            }
            clean.insert(key.clone(), v.clone());
        }
        out.push(serde_json::Value::Object(clean));
    }
    Ok(serde_json::Value::Array(out))
}

/// ZONE BATTLE の部屋の設定の検査（知らないキー・VERSUS だけのキーは捨てる。知っているキーの値異常はエラー）
fn sanitize_zone_match_setting(object: &serde_json::Map<String, serde_json::Value>) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let mut sanitized = serde_json::Map::new();
    for (key, value) in object {
        match key.as_str() {
            "holdEnabled" | "b2bBonus" | "garbageDamageOnClear" | "zoneSplit" | "marginEnabled" | "phase" => {
                if !value.is_boolean() {
                    return Err(format!("{key} must be a boolean"));
                }
            }
            "setTarget" => {
                let number = value
                    .as_f64()
                    .ok_or_else(|| format!("{key} must be a number"))?;
                if number.fract() != 0.0 || number < 1.0 || number > 9.0 {
                    return Err(format!("{key} must be an integer between 1 and 9"));
                }
            }
            "garbageMultiplier" => {
                let number = value
                    .as_f64()
                    .ok_or_else(|| format!("{key} must be a number"))?;
                if !number.is_finite() || !(0.0..=3.0).contains(&number) {
                    return Err(format!("{key} must be between 0 and 3"));
                }
            }
            "marginTrigger" | "phaseTrigger" => {
                let ok = match key.as_str() {
                    "marginTrigger" => matches!(value.as_str(), Some("time") | Some("score")),
                    _ => matches!(value.as_str(), Some("score") | Some("time")),
                };
                if !ok {
                    return Err(format!("{key} must be \"time\" or \"score\""));
                }
            }
            "marginTime" => int_in(key, value, ZONE_MARGIN_TIME_STEPS)?,
            "marginScore" => int_in(key, value, ZONE_MARGIN_SCORE_STEPS)?,
            "garbageHoleRate" | "garbageHoleChunk" => hole_rate(key, value)?,
            "zoneQuarterMs" => int_in(key, value, ZONE_TIME_OPTIONS)?,
            "zoneQuarterLines" => int_in(key, value, ZONE_CHARGE_OPTIONS)?,
            "phaseTable" => {
                sanitized.insert(key.clone(), sanitize_phase_table(value)?);
                continue;
            }
            unknown => {
                warn!("Ignoring unknown ZONE BATTLE match setting key: {unknown}");
                continue;
            }
        }
        sanitized.insert(key.clone(), value.clone());
    }
    Ok(sanitized)
}

/// ZONE BATTLE の部屋設定から、サーバーが見るしきい値（SCORE のフェーズ・SCORE のマージン）を読む。
/// 対戦の頭（start_match）で 1 回だけ読む（StatsUpdate のたびに JSON を読まない）。
pub fn zone_progress_from_setting(setting: &str) -> ZoneProgress {
    let v: serde_json::Value = serde_json::from_str(setting).unwrap_or(serde_json::Value::Null);
    let default: serde_json::Value =
        serde_json::from_str(ONLINE_DEFAULT_ZONE_MATCH_SETTING).unwrap_or(serde_json::Value::Null);
    // 無いキーは既定（クライアントの parseZoneMatchSetting も同じ）
    let get = |key: &str| -> serde_json::Value {
        v.get(key).cloned().unwrap_or_else(|| default.get(key).cloned().unwrap_or(serde_json::Value::Null))
    };
    let phase_on = get("phase").as_bool().unwrap_or(true);
    let phase_by_score = get("phaseTrigger").as_str() != Some("time");
    let phase_scores = if phase_on && phase_by_score {
        let table = get("phaseTable");
        let add = |i: usize| -> u32 {
            table
                .get(i)
                .and_then(|r| r.get("scoreAdd"))
                .and_then(|x| x.as_u64())
                .or_else(|| default["phaseTable"][i]["scoreAdd"].as_u64())
                .unwrap_or(0) as u32
        };
        let p2 = add(1);
        Some([p2, p2.saturating_add(add(2))])
    } else {
        None
    };
    let margin_by_score = get("marginEnabled").as_bool().unwrap_or(true)
        && get("marginTrigger").as_str() == Some("score");
    let margin_score = if margin_by_score {
        get("marginScore").as_u64().map(|x| x as u32)
    } else {
        None
    };
    ZoneProgress {
        phase_scores,
        margin_score,
        max_score: 0,
        phase: 1,
        // 突入 0 点は即時（段階 0）。クライアントの _zoneInit も同じなので通知しない
        margin_step: if margin_score == Some(0) { Some(0) } else { None },
    }
}

/// 最大スコアからフェーズ（1〜3）と SCORE のマージンの段階を求める（zone.js zonePhaseForScore・_zoneBattleCheck と同じ）
pub fn zone_targets(progress: &ZoneProgress, max_score: u32) -> (u8, Option<u8>) {
    let phase = match progress.phase_scores {
        Some([p2, p3]) => {
            if max_score >= p3 {
                3
            } else if max_score >= p2 {
                2
            } else {
                1
            }
        }
        None => 1,
    };
    let step = progress.margin_score.and_then(|m| {
        if max_score >= m {
            Some(((max_score - m) / ZONE_MARGIN_SCORE_STEP).min(ZONE_MARGIN_MAX_STEP as u32) as u8)
        } else {
            None
        }
    });
    (phase, step)
}

/// 既知キーのみを検証・再シリアライズして返す。未知キーはクライアントの先行
/// リリース（新設定の追加）で既存設定が丸ごと拒否されないよう、エラーにせず
/// 警告ログを出して黙って捨てる（[[project-tetra-server]] 「フレーム拡張は
/// サーバー無改造で可能」と同じ前方互換方針）。既知キーの値異常は従来通りエラー。
fn sanitize_match_setting(setting: &str, battle: Battle) -> Result<String, String> {
    let value: serde_json::Value = serde_json::from_str(setting)
        .map_err(|_| "Match setting must be valid JSON".to_string())?;
    let object = value
        .as_object()
        .ok_or_else(|| "Match setting must be a JSON object".to_string())?;

    if battle == Battle::Zone {
        let sanitized = sanitize_zone_match_setting(object)?;
        return serde_json::to_string(&sanitized)
            .map_err(|e| format!("Failed to serialize match setting: {e}"));
    }

    let mut sanitized = serde_json::Map::new();

    for (key, value) in object {
        match key.as_str() {
            "holdEnabled" | "b2bBonus" | "garbageDamageOnClear" => {
                if !value.is_boolean() {
                    return Err(format!("{key} must be a boolean"));
                }
                sanitized.insert(key.clone(), value.clone());
            }
            "setTarget" => {
                let number = value
                    .as_f64()
                    .ok_or_else(|| format!("{key} must be a number"))?;
                if number.fract() != 0.0 || number < 1.0 || number > 9.0 {
                    return Err(format!("{key} must be an integer between 1 and 9"));
                }
                sanitized.insert(key.clone(), value.clone());
            }
            "garbageMultiplier" | "marginTime" | "garbageHoleRate" | "ojamaRate" => {
                let (min, max) = match key.as_str() {
                    "garbageMultiplier" => (0.0, 3.0),
                    "marginTime" => (0.0, 300.0),
                    "garbageHoleRate" => (0.0, 100.0),
                    "ojamaRate" => (0.0, 200.0),
                    _ => unreachable!(),
                };
                let number = value
                    .as_f64()
                    .ok_or_else(|| format!("{key} must be a number"))?;
                if !number.is_finite() || number < min || number > max {
                    return Err(format!("{key} must be between {min} and {max}"));
                }
                sanitized.insert(key.clone(), value.clone());
            }
            unknown => {
                warn!("Ignoring unknown match setting key: {unknown}");
            }
        }
    }

    serde_json::to_string(&sanitized)
        .map_err(|e| format!("Failed to serialize match setting: {e}"))
}

/// ランダムマッチの結果。待機列に入ったか、相手とマッチしてルームができたか。
pub enum MatchResult {
    Waiting,
    #[allow(unused)]
    Matched {
        room_id: Uuid,
        opponent: Uuid,
    },
}

/// ゲームオーバー通知後の生存判定結果。
pub enum WinnerStatus {
    MatchContinues,
    Winner(Uuid),
    Draw,
}

#[derive(Clone)]
pub enum ConnectionState {
    Establishing,
    Connected,
    Disconnected,
}

#[derive(Clone)]
pub struct ChannelPair {
    pub reliable: Option<Arc<RTCDataChannel>>,
    pub unreliable: Option<Arc<RTCDataChannel>>,
    pub state: Arc<Mutex<ConnectionState>>,
}

#[derive(Clone)]
pub struct Game {
    pub rooms: HashMap<Uuid, Room>,
    connections: HashMap<Uuid, ChannelPair>,
    /// 経路表: player_id -> room_id の逆引き。中継時に
    /// player -> room -> opponent -> channel を高頻度(unreliable 30〜60Hz)で引くため。
    player_rooms: HashMap<Uuid, Uuid>,
    /// 友人参加用: ルームコード -> room_id の逆引き。
    room_codes: HashMap<String, Uuid>,
    /// ランダムマッチの待機列（FIFO）。(player_id, username, rule)。
    matchmaking_queue: VecDeque<(Uuid, String, String)>,
    /// Garbage の送信時刻。古い時刻を捨てながらプレイヤー単位でレート制限する。
    garbage_sent_at: HashMap<Uuid, VecDeque<Instant>>,
    /// online アカウント化(設計 v2.2.2 §7.2): チケット検証済みの Discord プロフィール。
    /// キーは Offer 処理で確定した player_id。ゲストは登録されない。
    accounts: HashMap<Uuid, Account>,
    /// 再接続の乗っ取り対策(設計 v2.2.2 §7.3): player_id ごとの現在有効なシークレット。
    /// Offer の再接続要求はこれと一致した場合のみ再バインドを許可する。
    reconnect_secrets: HashMap<Uuid, String>,
    /// player_id ごとの「今の接続」（PeerConnection 1 本 = WebSocket 1 本ごとの識別子）。
    /// 再接続で付け替えた後に古い接続の切断が遅れて届いても、今の接続でなければ無視する。
    sessions: HashMap<Uuid, Uuid>,
}

impl Default for Game {
    fn default() -> Self {
        Self {
            rooms: HashMap::new(),
            connections: HashMap::new(),
            player_rooms: HashMap::new(),
            room_codes: HashMap::new(),
            matchmaking_queue: VecDeque::new(),
            garbage_sent_at: HashMap::new(),
            accounts: HashMap::new(),
            reconnect_secrets: HashMap::new(),
            sessions: HashMap::new(),
        }
    }
}

impl Game {
    pub fn add_reliable_connection(
        &mut self,
        player_id: Uuid,
        dc: Arc<webrtc::data_channel::RTCDataChannel>,
    ) {
        let pair = self.connections.entry(player_id).or_insert(ChannelPair {
            reliable: None,
            unreliable: None,
            state: Arc::new(Mutex::new(ConnectionState::Establishing)),
        });
        pair.reliable = Some(dc);

        // 2本揃ったかチェック
        self.check_if_ready(player_id);
    }

    pub fn add_unreliable_connection(&mut self, player_id: Uuid, dc: Arc<RTCDataChannel>) {
        let pair = self.connections.entry(player_id).or_insert(ChannelPair {
            reliable: None,
            unreliable: None,
            state: Arc::new(Mutex::new(ConnectionState::Establishing)),
        });
        pair.unreliable = Some(dc);

        // 2本揃ったかチェック
        self.check_if_ready(player_id);
    }

    pub fn check_if_ready(&self, id: Uuid) {
        if let Some(pair) = self.connections.get(&id) {
            if pair.reliable.is_some() && pair.unreliable.is_some() {
                let mut state = match pair.state.lock() {
                    Ok(s) => s,
                    Err(e) => {
                        info!("Failed to lock connection state for player {}: {:?}", id, e);
                        return;
                    }
                };
                *state = ConnectionState::Connected;
                info!(
                    "🎉 プレイヤー [{}] の Reliable と Unreliable が両方揃いました！通信準備完了！",
                    id
                );
            }
        }
    }

    /// プレイヤーを Game の内部状態（接続表・待機列・経路表）から取り除き、
    /// close すべきチャンネルを返す。
    /// この場で `close().await` してしまうと Game のロックを保持したまま
    /// 待つことになり、全 Game 操作がブロックされる。よってクローズは
    /// 呼び出し側でロックを解放した後に `tokio::spawn` で行う責務とする。
    #[must_use]
    pub fn remove_connection(
        &mut self,
        player_id: &Uuid,
    ) -> (Option<Arc<RTCDataChannel>>, Option<Arc<RTCDataChannel>>) {
        let channels: (Option<Arc<RTCDataChannel>>, Option<Arc<RTCDataChannel>>) = self
            .connections
            .remove(player_id)
            .map(|pair| (pair.reliable, pair.unreliable))
            .unwrap_or((None, None));

        // 待機列からも除外
        self.matchmaking_queue
            .retain(|(pid, _, _)| pid != player_id);
        self.garbage_sent_at.remove(player_id);
        // アカウント紐付け・再接続シークレットも掃除する（設計 v2.2.2 §7.2/§7.3）
        self.accounts.remove(player_id);
        self.reconnect_secrets.remove(player_id);
        self.sessions.remove(player_id);
        // 経路表・所属ルームからも退去させる
        self.leave_room(player_id);

        channels
    }

    /// プレイヤーが reliable チャンネルで実際に応答可能かを判定する。
    /// 接続エントリが無い、または reliable channel が Open でなければ「無応答」とみなす
    /// （ConnectionState は on_peer_connection_state_change の取りこぼしで更新され損ねる
    /// ことがあるため、実体である RTCDataChannel の状態を直接見る）。
    fn is_player_responsive(&self, player_id: &Uuid) -> bool {
        self.connections
            .get(player_id)
            .and_then(|pair| pair.reliable.as_ref())
            .map(|dc| dc.ready_state() == RTCDataChannelState::Open)
            .unwrap_or(false)
    }

    /// ロビーの人数バッジ表示用の集計値。
    /// (現在接続中の人数, 対戦中の人数, ランダムマッチ待機中の人数) の集計値のみを返す。
    /// ユーザー名・ルームの組み合わせ等の個人が追える情報は一切含まない。
    pub fn online_stats(&self) -> (u32, u32, u32) {
        // connections には DataChannel が1本入った時点でエントリが出来るため、
        // len() をそのまま使うと接続確立途中のものを過大カウントする。
        // is_player_responsive と同じ基準（reliable が Open）で数える。
        let online_count = self
            .connections
            .keys()
            .filter(|id| self.is_player_responsive(id))
            .count() as u32;
        let in_match_count = self
            .rooms
            .values()
            .filter(|room| room.status == RoomStatus::Playing)
            .map(|room| room.players.len() as u32)
            .sum();
        let matching_count = self.matchmaking_queue.len() as u32;
        (online_count, in_match_count, matching_count)
    }

    /// 定期スイープ用: 全員のreliableチャンネルが無応答（Open でない）になっているルームを
    /// まとめて破棄する。PeerConnection の Failed/Closed 遷移を取りこぼして
    /// disconnect_player が起動しないケースの保険（メモリに残り続けるゾンビ部屋対策）。
    /// 除去したプレイヤーの (reliable, unreliable) チャンネルを返すので、呼び出し側で
    /// ロック外で close する。
    #[must_use]
    pub fn sweep_unresponsive_rooms(
        &mut self,
    ) -> Vec<(Uuid, Option<Arc<RTCDataChannel>>, Option<Arc<RTCDataChannel>>)> {
        let stale_room_ids: Vec<Uuid> = self
            .rooms
            .iter()
            .filter(|(_, room)| {
                !room.players.is_empty()
                    && room
                        .players
                        .iter()
                        .all(|(pid, _, _)| !self.is_player_responsive(pid))
            })
            .map(|(room_id, _)| *room_id)
            .collect();

        let mut closed = Vec::new();
        for room_id in stale_room_ids {
            let player_ids: Vec<Uuid> = self
                .rooms
                .get(&room_id)
                .map(|room| room.players.iter().map(|(pid, _, _)| *pid).collect())
                .unwrap_or_default();
            for player_id in player_ids {
                let (reliable, unreliable) = self.remove_connection(&player_id);
                closed.push((player_id, reliable, unreliable));
            }
        }
        closed
    }

    /// プレイヤーをルームに参加させる（players への追加＋経路表更新）。
    /// 部屋の存在・満員・開始済みをチェックする。パスワード検証は呼び出し側の責務。
    pub fn add_player_to_room(
        &mut self,
        room_id: Uuid,
        player_id: Uuid,
        username: String,
        rule: String,
    ) -> Result<(), String> {
        // ★A(設計 v2.2.2 §7.2): アカウント持ちは表示名を Discord 表示名に固定し、
        // クライアントの自由入力(username引数)は無視する。
        let username = self
            .accounts
            .get(&player_id)
            .map(|account| account.name.clone())
            .unwrap_or(username);
        let room = self
            .rooms
            .get_mut(&room_id)
            .ok_or_else(|| "Room not found".to_string())?;
        if room.status != RoomStatus::Waiting {
            return Err("Game has already started".to_string());
        }
        if room.players.iter().any(|(pid, _, _)| *pid == player_id) {
            // すでに参加済み。冪等に成功扱い。
            return Ok(());
        }
        if room.post_match {
            return Err("The previous match result is being shown. Please try again later".to_string());
        }
        if room.players.len() as u8 >= room.max_players {
            return Err("Room is full".to_string());
        }
        // ZONE BATTLE の部屋は全員テト（設計 §3）。コードで入る人は部屋の種類を知らずにふだんの
        // ルール（ぷよのこともある）で来るので、断らずにテトで入れる（ルールの変更は update_player_rule が断る）
        let rule = if room.battle == Battle::Zone { "tet".to_string() } else { rule };
        room.players.push((player_id, username, rule));
        // 入室直後は未READY状態（再入室時の stale READY も除去）
        room.ready_players.retain(|pid| *pid != player_id);
        self.player_rooms.insert(player_id, room_id);
        Ok(())
    }

    /// プレイヤー自身のルール ("tet" | "puyo") を更新する。マッチ開始前のみ有効。
    pub fn update_player_rule(&mut self, player_id: &Uuid, rule: String) -> Result<(), String> {
        let room_id = self
            .player_rooms
            .get(player_id)
            .copied()
            .ok_or_else(|| "Not in a room".to_string())?;
        let room = self
            .rooms
            .get_mut(&room_id)
            .ok_or_else(|| "Room not found".to_string())?;
        if room.status != RoomStatus::Waiting {
            return Err("Cannot change rule after match has started".to_string());
        }
        match rule.as_str() {
            "tet" | "puyo" => {}
            _ => return Err("Invalid rule".to_string()),
        }
        if room.battle == Battle::Zone && rule != "tet" {
            return Err("ZONE BATTLE rooms are TET only".to_string());
        }
        if let Some(entry) = room.players.iter_mut().find(|(pid, _, _)| pid == player_id) {
            entry.2 = rule;
        }
        Ok(())
    }

    /// 部屋主が対戦前にルール設定を更新する。設定変更は READY を無効化する。
    /// 戻り値は保存後（サニタイズ済み）の設定文字列。呼び出し側の通知配信は
    /// クライアントが送った生の文字列ではなくこの値を使うこと。
    pub fn update_match_setting(
        &mut self,
        player_id: &Uuid,
        room_id: Uuid,
        setting: String,
    ) -> Result<String, String> {
        let room = self
            .rooms
            .get_mut(&room_id)
            .ok_or_else(|| "Room not found".to_string())?;
        let sanitized = sanitize_match_setting(&setting, room.battle)?;
        if room.owner != *player_id {
            return Err("Only the room owner can change match settings".to_string());
        }
        if room.status != RoomStatus::Waiting {
            return Err("Cannot change match settings after the match has started".to_string());
        }
        room.match_setting = sanitized.clone();
        room.ready_players.clear();
        Ok(sanitized)
    }

    /// プレイヤー自身の表示名を更新する。
    /// 名前は表示専用なのでルーム状態を問わず許可する（対戦中の変更は次ラウンドから反映）。
    /// ルーム・待機列のどちらにも居ない場合は、次の参加リクエストで新しい名前が送られる
    /// ため何もせず成功扱いにする。
    pub fn update_player_username(
        &mut self,
        player_id: &Uuid,
        username: String,
    ) -> Result<(), String> {
        // ★A(設計 v2.2.2 §7.2): アカウント持ちは表示名を Discord 表示名に固定する。
        // クライアントの自由入力は検証すらせず無視する(ゲストのみ従来の1-16文字チェックが働く)。
        let username = if let Some(account) = self.accounts.get(player_id) {
            account.name.clone()
        } else {
            let trimmed = username.trim().to_string();
            if trimmed.is_empty() || trimmed.chars().count() > 16 {
                return Err("Name must be 1-16 characters".to_string());
            }
            trimmed
        };
        if let Some(room_id) = self.player_rooms.get(player_id).copied() {
            if let Some(room) = self.rooms.get_mut(&room_id) {
                if let Some(entry) = room.players.iter_mut().find(|(pid, _, _)| pid == player_id) {
                    entry.1 = username.clone();
                }
            }
        }
        if let Some(entry) = self
            .matchmaking_queue
            .iter_mut()
            .find(|(pid, _, _)| pid == player_id)
        {
            entry.1 = username;
        }
        Ok(())
    }

    /// プレイヤーを所属ルームの players から除き、経路表からも削除する。
    /// ルームが無人になったら、ルーム本体とコード索引も破棄する。
    pub fn leave_room(&mut self, player_id: &Uuid) {
        let Some(room_id) = self.player_rooms.remove(player_id) else {
            return;
        };
        let mut empty_code: Option<String> = None;
        if let Some(room) = self.rooms.get_mut(&room_id) {
            room.players.retain(|(pid, _, _)| pid != player_id);
            room.alive_players.retain(|pid| pid != player_id);
            room.ready_players.retain(|pid| pid != player_id);
            room.pings.remove(player_id);
            // オーナーが退出したら残った先頭プレイヤーへオーナー権を移譲する
            if room.owner == *player_id {
                if let Some((next_owner, _, _)) = room.players.first() {
                    room.owner = *next_owner;
                }
            }
            if room.players.len() <= 1 {
                // 再戦の相手がいなくなったので、リザルト中の入室制限を解く
                room.post_match = false;
            }
            if room.players.is_empty() {
                empty_code = Some(room.code.clone());
            }
        }
        if let Some(code) = empty_code {
            self.rooms.remove(&room_id);
            self.room_codes.remove(&code);
        }
    }

    /// ルームコードから room_id を引く。
    pub fn find_room_by_code(&self, code: &str) -> Option<Uuid> {
        self.room_codes.get(code).copied()
    }

    /// 既存コードと衝突しない6桁の数字ルームコードを生成する。
    /// 英字（特に大文字 O/0・I/1 など）は数字と紛らわしいため、数字のみ・ゼロ埋め6桁にする。
    fn generate_room_code(&self) -> String {
        loop {
            let n = (Uuid::new_v4().as_u128() % 1_000_000) as u32;
            let code = format!("{n:06}");
            if !self.room_codes.contains_key(&code) {
                return code;
            }
        }
    }

    /// ランダムマッチ。待機列に相手がいればルームを作って両者を入れる。
    /// いなければ自身を待機列へ追加する。
    pub fn random_match(&mut self, player_id: Uuid, username: String, rule: String) -> MatchResult {
        if let Some(pos) = self
            .matchmaking_queue
            .iter()
            .position(|(pid, _, _)| *pid != player_id)
        {
            let (opp_id, opp_name, opp_rule) = match self.matchmaking_queue.remove(pos) {
                Some(entry) => entry,
                None => {
                    // まれに remove(pos) が None を返すことがある（pos は存在していたのに）。念のため安全策。
                    return MatchResult::Waiting;
                }
            };
            // 待機していた相手をオーナーにしてルームを作成（相手は player[0] として入る）
            // ランダムマッチは VERSUS だけ（設計 §3・回答 5）
            let (room_id, _code) = self.new_room(
                opp_id,
                "Random Match".to_string(),
                2,
                false,
                vec![],
                opp_name,
                opp_rule,
                Battle::Versus,
            );
            let _ = self.add_player_to_room(room_id, player_id, username, rule);
            MatchResult::Matched {
                room_id,
                opponent: opp_id,
            }
        } else {
            if !self
                .matchmaking_queue
                .iter()
                .any(|(pid, _, _)| *pid == player_id)
            {
                self.matchmaking_queue
                    .push_back((player_id, username, rule));
            }
            MatchResult::Waiting
        }
    }

    /// ランダムマッチの待機列から離脱する。離脱できたら true。
    pub fn cancel_random_match(&mut self, player_id: &Uuid) -> bool {
        let before = self.matchmaking_queue.len();
        self.matchmaking_queue
            .retain(|(pid, _, _)| pid != player_id);
        before != self.matchmaking_queue.len()
    }

    /// プレイヤーの所属ルームを引く。切断削除の前にルームを控え、削除後に
    /// 残ったメンバーへ通知するために使う（削除後は経路表から消えて引けなくなる）。
    pub fn room_of(&self, player_id: &Uuid) -> Option<Uuid> {
        self.player_rooms.get(player_id).copied()
    }

    /// 要求に書かれた room_id が、送ってきた人の所属ルームと一致するか。
    /// 一致しない通知（他の部屋へのポーズ・投票など）は流さない。
    pub fn is_in_room(&self, player_id: &Uuid, room_id: &Uuid) -> bool {
        self.player_rooms.get(player_id) == Some(room_id)
    }

    /// この接続（session）をプレイヤーの「今の接続」として記録する。
    pub fn set_session(&mut self, player_id: Uuid, session: Uuid) {
        self.sessions.insert(player_id, session);
    }

    /// session がプレイヤーの今の接続か。記録が無ければ（古い経路・テスト）今の接続とみなす。
    pub fn is_current_session(&self, player_id: &Uuid, session: &Uuid) -> bool {
        self.sessions.get(player_id).is_none_or(|s| s == session)
    }

    /// 対戦中なら負け（ゲームオーバー）として記録してから部屋を抜ける。
    /// 対戦中の LeaveRoom / Close で勝敗が判定されず部屋が Playing のまま残るのを防ぐ。
    /// 戻り値: 対戦中だった場合の (room_id, 判定結果)。
    pub fn leave_room_recording_result(&mut self, player_id: &Uuid) -> Option<(Uuid, WinnerStatus)> {
        let result = self.room_of(player_id).and_then(|rid| {
            let playing = self
                .rooms
                .get(&rid)
                .map(|r| r.status == RoomStatus::Playing && r.alive_players.contains(player_id))
                .unwrap_or(false);
            if playing {
                Some((rid, self.record_game_over(player_id)))
            } else {
                None
            }
        });
        self.leave_room(player_id);
        result
    }

    /// 「ルームに戻る」が選ばれたら、リザルト中の入室制限を解く。
    pub fn end_post_match(&mut self, room_id: &Uuid) {
        if let Some(room) = self.rooms.get_mut(room_id) {
            room.post_match = false;
        }
    }

    /// 再接続した人へ送り直す、切断中に起きたかもしれない対戦の状態。
    /// (room_id, 対戦中に脱落済みの相手, 決着済みならその結果)
    pub fn reattach_state(&self, player_id: &Uuid) -> Option<(Uuid, Vec<Uuid>, Option<Option<Uuid>>)> {
        let room_id = self.room_of(player_id)?;
        let room = self.rooms.get(&room_id)?;
        match room.status {
            RoomStatus::Playing => {
                let dead = room
                    .players
                    .iter()
                    .map(|(pid, _, _)| *pid)
                    .filter(|pid| pid != player_id && !room.alive_players.contains(pid))
                    .collect();
                Some((room_id, dead, None))
            }
            RoomStatus::Waiting => {
                if room.post_match {
                    Some((room_id, vec![], room.last_result))
                } else {
                    None
                }
            }
        }
    }

    /// player -> room -> opponent を引く（1対1前提: 同ルームの自分以外のプレイヤー）。
    #[cfg(test)]
    pub fn get_opponent(&self, player_id: &Uuid) -> Option<Uuid> {
        let room_id = self.player_rooms.get(player_id)?;
        let room = self.rooms.get(room_id)?;
        room.players
            .iter()
            .map(|(pid, _, _)| *pid)
            .find(|pid| pid != player_id)
    }

    pub fn get_reliable_connection(
        &self,
        player_id: &Uuid,
    ) -> Option<Arc<webrtc::data_channel::RTCDataChannel>> {
        self.connections
            .get(player_id)
            .and_then(|pair| pair.reliable.clone())
    }

    pub fn get_unreliable_connection(
        &self,
        player_id: &Uuid,
    ) -> Option<Arc<webrtc::data_channel::RTCDataChannel>> {
        self.connections
            .get(player_id)
            .and_then(|pair| pair.unreliable.clone())
    }

    pub fn get_connection_state(&self, player_id: &Uuid) -> Option<Arc<Mutex<ConnectionState>>> {
        self.connections
            .get(player_id)
            .map(|pair| Arc::clone(&pair.state))
    }

    /// Offer 処理で player_id が確定した時点で呼ぶ（設計 v2.2.2 §7.2）。
    pub fn set_account(&mut self, player_id: Uuid, account: Account) {
        self.accounts.insert(player_id, account);
    }

    /// 再接続の乗っ取り対策(設計 v2.2.2 §7.3)用シークレットを設定/更新する。
    pub fn set_reconnect_secret(&mut self, player_id: Uuid, secret: String) {
        self.reconnect_secrets.insert(player_id, secret);
    }

    /// 現在有効な reconnect_secret を読む（消費しない）。
    pub fn reconnect_secret_of(&self, player_id: &Uuid) -> Option<String> {
        self.reconnect_secrets.get(player_id).cloned()
    }

    /// reconnect_secret を取り出して削除する。再接続の成立時、使い捨ての pid 側の
    /// エントリを正式な pid 側へ移す(ローテーション)のに使う。
    pub fn take_reconnect_secret(&mut self, player_id: &Uuid) -> Option<String> {
        self.reconnect_secrets.remove(player_id)
    }

    /// ルーム内のアカウント持ちプレイヤーだけを (player_id, 表示名, avatarUrl) で返す。
    /// RoomInfoNotification.accounts の元データ(設計 v2.2.2 §7.2)。
    pub fn room_accounts(&self, room_id: &Uuid) -> Vec<(Uuid, String, String)> {
        let Some(room) = self.rooms.get(room_id) else {
            return vec![];
        };
        room.players
            .iter()
            .filter_map(|(pid, _, _)| {
                self.accounts
                    .get(pid)
                    .map(|account| (*pid, account.name.clone(), account.avatar_url.clone()))
            })
            .collect()
    }

    /// ルームの全ピア（自分以外）の ID を返す。0x2N 中継で使う。
    pub fn get_room_peers(&self, player_id: &Uuid) -> Vec<Uuid> {
        let room_id = match self.player_rooms.get(player_id) {
            Some(id) => *id,
            None => return vec![],
        };
        let room = match self.rooms.get(&room_id) {
            Some(r) => r,
            None => return vec![],
        };
        room.players
            .iter()
            .map(|(pid, _, _)| *pid)
            .filter(|pid| *pid != *player_id)
            .collect()
    }

    /// ルームの全ピアの reliable/unreliable チャンネルを収集する。0x2N 中継ホットパス。
    pub fn get_room_peer_channels(
        &self,
        player_id: &Uuid,
        reliable: bool,
    ) -> Vec<Arc<RTCDataChannel>> {
        self.get_room_peers(player_id)
            .into_iter()
            .filter_map(|pid| {
                if reliable {
                    self.get_reliable_connection(&pid)
                } else {
                    self.get_unreliable_connection(&pid)
                }
            })
            .collect()
    }

    /// 対戦中かつ生存している相手の reliable チャンネルだけを返す。
    pub fn get_live_opponent_reliable_channels(
        &self,
        player_id: &Uuid,
        target_rule: Option<&str>,
    ) -> Vec<Arc<webrtc::data_channel::RTCDataChannel>> {
        let Some(room_id) = self.player_rooms.get(player_id) else {
            return vec![];
        };
        let Some(room) = self.rooms.get(room_id) else {
            return vec![];
        };
        if room.status != RoomStatus::Playing || !room.alive_players.contains(player_id) {
            return vec![];
        }
        room.players
            .iter()
            .filter(|(pid, _, _)| *pid != *player_id && room.alive_players.contains(pid))
            .filter(|(_, _, rule)| target_rule.is_none_or(|t| t == rule.as_str()))
            .filter_map(|(pid, _, _)| self.get_reliable_connection(pid))
            .collect()
    }

    /// Garbage を中継してよい状態か検証し、送信レートも記録する。
    pub fn validate_garbage(&mut self, player_id: &Uuid, body: &[u8]) -> Result<(), String> {
        if body.len() + 1 > MAX_GARBAGE_FRAME_BYTES {
            return Err(format!(
                "Garbage frame exceeds {MAX_GARBAGE_FRAME_BYTES} bytes"
            ));
        }
        let room_id = self
            .player_rooms
            .get(player_id)
            .copied()
            .ok_or_else(|| "Garbage sender is not in a room".to_string())?;
        let room = self
            .rooms
            .get(&room_id)
            .ok_or_else(|| "Garbage room not found".to_string())?;
        if room.status != RoomStatus::Playing {
            return Err("Garbage is only accepted during a match".to_string());
        }
        if !room.alive_players.contains(player_id) {
            return Err("Garbage sender is no longer alive".to_string());
        }
        let target = garbage_target_rule(body)?;
        let receiver_rules: Vec<&str> = room
            .players
            .iter()
            .filter(|(pid, _, _)| *pid != *player_id && room.alive_players.contains(pid))
            .map(|(_, _, rule)| rule.as_str())
            .filter(|rule| target.is_none_or(|t| t == *rule))
            .collect();
        if receiver_rules.is_empty() {
            return Err("Garbage has no live opponent".to_string());
        }
        for rule in receiver_rules {
            validate_garbage_payload(body, rule)?;
        }

        let now = Instant::now();
        let sent = self.garbage_sent_at.entry(*player_id).or_default();
        sent.retain(|at| now.duration_since(*at).as_secs() < GARBAGE_RATE_WINDOW_SECS);
        if sent.len() >= GARBAGE_RATE_LIMIT {
            return Err("Garbage rate limit exceeded".to_string());
        }
        sent.push_back(now);
        Ok(())
    }

    /// ZONE BATTLE の部屋で対戦中か（StatsUpdate のたびに書き込みロックを取らないための先読み）
    pub fn in_zone_match(&self, player_id: &Uuid) -> bool {
        self.player_rooms
            .get(player_id)
            .and_then(|room_id| self.rooms.get(room_id))
            .is_some_and(|room| room.battle == Battle::Zone && room.status == RoomStatus::Playing)
    }

    /// ZONE BATTLE: StatsUpdate（0x2B）の中身からスコアを読み、部屋の全員のスコアの最大値で
    /// フェーズ・SCORE のマージンの段階が上がったら Some((部屋, フェーズ, 段階)) を返す（設計 §5）。
    /// 呼び出し側が部屋の全員へ JSONZonePhaseNotification を送る。一度上がったら下げない。
    /// 脱落した人のスコアも数える（最大値は下がらないので単調。§13 Q1）。
    /// body は opcode を除いた中身: rule:u8(0=tet) + score:u32(LE) + …
    pub fn record_zone_score(&mut self, player_id: &Uuid, body: &[u8]) -> Option<(Uuid, u8, Option<u8>)> {
        if body.len() < 5 || body[0] != 0 {
            return None;
        }
        let score = u32::from_le_bytes([body[1], body[2], body[3], body[4]]);
        let room_id = self.player_rooms.get(player_id).copied()?;
        let room = self.rooms.get_mut(&room_id)?;
        if room.battle != Battle::Zone || room.status != RoomStatus::Playing {
            return None;
        }
        if !room.players.iter().any(|(pid, _, _)| pid == player_id) {
            return None;
        }
        let z = &mut room.zone;
        if score <= z.max_score {
            return None;
        }
        z.max_score = score;
        let (phase, step) = zone_targets(z, score);
        let phase_up = phase > z.phase;
        let step_up = match (step, z.margin_step) {
            (Some(s), Some(cur)) => s > cur,
            (Some(_), None) => true,
            _ => false,
        };
        if !phase_up && !step_up {
            return None;
        }
        if phase_up {
            z.phase = phase;
        }
        if step_up {
            z.margin_step = step;
        }
        Some((room_id, z.phase, z.margin_step))
    }

    /// ルーム全メンバーの reliable チャンネルを収集する（通知配布用）。
    pub fn get_room_reliable_channels(&self, room_id: &Uuid) -> Vec<Arc<RTCDataChannel>> {
        let Some(room) = self.rooms.get(room_id) else {
            return vec![];
        };
        room.players
            .iter()
            .filter_map(|(pid, _, _)| self.get_reliable_connection(pid))
            .collect()
    }

    /// マッチを開始する。バリデーション後ステータスを Playing に変更し、
    /// 16 バイト共有シードと現在の match_setting を返す。
    pub fn start_match(&mut self, room_id: Uuid) -> Result<([u8; 16], String), String> {
        let room = self.rooms.get_mut(&room_id).ok_or("Room not found")?;
        if room.status != RoomStatus::Waiting {
            return Err("Game already started".to_string());
        }
        if room.players.len() < 2 {
            return Err("Need at least 2 players".to_string());
        }
        // オーナー以外の全員が READY であること（オーナーの開始操作 = オーナーのREADY扱い）
        let not_ready: Vec<&String> = room
            .players
            .iter()
            .filter(|(pid, _, _)| *pid != room.owner && !room.ready_players.contains(pid))
            .map(|(_, name, _)| name)
            .collect();
        if !not_ready.is_empty() {
            return Err(format!(
                "Not all players are ready: {}",
                not_ready
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        room.status = RoomStatus::Playing;
        room.post_match = false;
        room.last_result = None;
        room.alive_players = room.players.iter().map(|(pid, _, _)| *pid).collect();
        // 次のマッチに向けて READY をリセット
        room.ready_players.clear();
        // ZONE BATTLE: フェーズ・マージンの進み具合を 0 に戻す（セットの次の試合・RETRY も。設計 §5.2）
        room.zone = if room.battle == Battle::Zone {
            zone_progress_from_setting(&room.match_setting)
        } else {
            ZoneProgress::default()
        };
        let seed: [u8; 16] = *Uuid::new_v4().as_bytes();
        let match_setting = room.match_setting.clone();
        Ok((seed, match_setting))
    }

    /// プレイヤーの READY 状態を設定する。Waiting 中のみ有効。
    pub fn set_ready(&mut self, player_id: &Uuid, ready: bool) -> Result<(), String> {
        let room_id = self
            .player_rooms
            .get(player_id)
            .copied()
            .ok_or_else(|| "Not in a room".to_string())?;
        let room = self
            .rooms
            .get_mut(&room_id)
            .ok_or_else(|| "Room not found".to_string())?;
        if room.status != RoomStatus::Waiting {
            return Err("Cannot change ready state during a match".to_string());
        }
        room.ready_players.retain(|pid| pid != player_id);
        if ready {
            room.ready_players.push(*player_id);
        }
        Ok(())
    }

    /// プレイヤーの自己報告 RTT (ms) を記録する。ルーム未所属なら何もしない。
    pub fn set_ping(&mut self, player_id: &Uuid, ping_ms: u32) {
        let Some(room_id) = self.player_rooms.get(player_id).copied() else {
            return;
        };
        if let Some(room) = self.rooms.get_mut(&room_id) {
            room.pings.insert(*player_id, ping_ms);
        }
    }

    /// プレイヤーのゲームオーバーを記録する。生存者数を確認して WinnerStatus を返す。
    /// 勝敗が確定したらルームを Waiting に戻す（再戦・ルール変更・新規参加を可能にするため）。
    pub fn record_game_over(&mut self, player_id: &Uuid) -> WinnerStatus {
        let room_id = match self.player_rooms.get(player_id).copied() {
            Some(id) => id,
            None => return WinnerStatus::MatchContinues,
        };
        let room = match self.rooms.get_mut(&room_id) {
            Some(r) => r,
            None => return WinnerStatus::MatchContinues,
        };
        // 既に決着済み（status != Playing）なら何もしない。
        // このゲームに引き分けは無く「先に死んだ方が負け・最後まで生きた方が勝ち」。
        // 2人がほぼ同時に死んでも、先に処理された死亡で勝者(=後に生き残っていた方)が確定する。
        // その後に届いた勝者自身の gameOver でDRAWに上書きしてしまうのを防ぐ。
        if room.status != RoomStatus::Playing {
            return WinnerStatus::MatchContinues;
        }
        room.alive_players.retain(|pid| pid != player_id);
        debug!(
            "Recorded game over for player [{}] in room [{}]. Alive players remaining: {}",
            player_id,
            room_id,
            room.alive_players.len()
        );
        match room.alive_players.len() {
            0 => {
                room.status = RoomStatus::Waiting;
                room.alive_players.clear();
                room.ready_players.clear();
                room.post_match = true;
                room.last_result = Some(None);
                WinnerStatus::Draw
            }
            1 => {
                let winner = room.alive_players[0];
                room.status = RoomStatus::Waiting;
                room.alive_players.clear();
                room.ready_players.clear();
                room.post_match = true;
                room.last_result = Some(Some(winner));
                WinnerStatus::Winner(winner)
            }
            _ => WinnerStatus::MatchContinues,
        }
    }

    /// ルームを新規作成する。作成者(owner)は自動的に最初のプレイヤーとして参加する。
    /// 戻り値は (room_id, ルームコード)。
    pub fn new_room(
        &mut self,
        owner: Uuid,
        room_name: String,
        max_players: u8,
        is_public: bool,
        tags: Vec<crate::room::RoomTag>,
        owner_username: String,
        owner_rule: String,
        battle: Battle,
    ) -> (Uuid, String) {
        let room_id = Uuid::new_v4();
        let code = self.generate_room_code();
        // ZONE BATTLE の部屋は全員テト（作成者も）
        let owner_rule = if battle == Battle::Zone { "tet".to_string() } else { owner_rule };
        let room = Room {
            status: RoomStatus::Waiting,
            owner,
            players: vec![(owner, owner_username, owner_rule)],
            room_name,
            max_players,
            is_public,
            tags,
            code: code.clone(),
            match_setting: default_match_setting(battle).to_string(),
            alive_players: vec![],
            ready_players: vec![],
            pings: std::collections::HashMap::new(),
            post_match: false,
            last_result: None,
            battle,
            zone: ZoneProgress::default(),
        };
        self.rooms.insert(room_id, room);
        self.room_codes.insert(code.clone(), room_id);
        self.player_rooms.insert(owner, room_id);
        (room_id, code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::RwLock;

    fn garbage_payload(holes: &[u8]) -> Vec<u8> {
        let mut body = vec![GARBAGE_PROTOCOL_VERSION_LEGACY];
        body.extend_from_slice(&(holes.len() as u16).to_le_bytes());
        body.extend_from_slice(holes);
        body
    }

    #[test]
    fn garbage_payload_is_validated_for_receiver_rule() {
        assert!(validate_garbage_payload(&garbage_payload(&[0, 9]), "tet").is_ok());
        assert!(validate_garbage_payload(&garbage_payload(&[0, 5]), "puyo").is_ok());
        assert!(validate_garbage_payload(&garbage_payload(&[10]), "tet").is_err());
        assert!(validate_garbage_payload(&garbage_payload(&[6]), "puyo").is_err());
    }

    #[test]
    fn garbage_payload_rejects_version_length_and_amount_errors() {
        assert!(validate_garbage_payload(&garbage_payload(&[]), "tet").is_err());

        let mut wrong_version = garbage_payload(&[0]);
        wrong_version[0] = 3;
        assert!(validate_garbage_payload(&wrong_version, "tet").is_err());

        let mut wrong_length = garbage_payload(&[0, 1]);
        wrong_length[1..3].copy_from_slice(&3u16.to_le_bytes());
        assert!(validate_garbage_payload(&wrong_length, "tet").is_err());

        let mut too_large = vec![GARBAGE_PROTOCOL_VERSION_LEGACY];
        too_large.extend_from_slice(&((MAX_GARBAGE_AMOUNT + 1) as u16).to_le_bytes());
        too_large.resize(3 + MAX_GARBAGE_AMOUNT + 1, 0);
        assert!(validate_garbage_payload(&too_large, "tet").is_err());
    }

    fn targeted_garbage_payload(target: u8, holes: &[u8]) -> Vec<u8> {
        let mut body = vec![GARBAGE_PROTOCOL_VERSION, target];
        body.extend_from_slice(&(holes.len() as u16).to_le_bytes());
        body.extend_from_slice(holes);
        body
    }

    #[test]
    fn targeted_garbage_payload_is_validated() {
        assert!(validate_garbage_payload(&targeted_garbage_payload(1, &[0, 9]), "tet").is_ok());
        assert!(validate_garbage_payload(&targeted_garbage_payload(2, &[5]), "puyo").is_ok());
        assert!(validate_garbage_payload(&targeted_garbage_payload(3, &[0]), "tet").is_err());
        assert_eq!(garbage_target_rule(&targeted_garbage_payload(1, &[0])), Ok(Some("tet")));
        assert_eq!(garbage_target_rule(&targeted_garbage_payload(2, &[0])), Ok(Some("puyo")));
        assert_eq!(garbage_target_rule(&garbage_payload(&[0])), Ok(None));
        let mut short = targeted_garbage_payload(1, &[0, 1]);
        short.pop();
        assert!(validate_garbage_payload(&short, "tet").is_err());
    }

    /// 混在戦: 宛先ルールの生きている相手がいなければ断る（いれば受ける）
    #[test]
    fn targeted_garbage_requires_live_opponent_of_target_rule() {
        let mut game = Game::default();
        let owner = Uuid::new_v4();
        let tet = Uuid::new_v4();
        let puyo = Uuid::new_v4();
        let (room_id, _) = game.new_room(
            owner, "test".to_string(), 3, false, vec![], "owner".to_string(), "tet".to_string(), Battle::Versus,
        );
        game.add_player_to_room(room_id, tet, "t".to_string(), "tet".to_string()).unwrap();
        game.add_player_to_room(room_id, puyo, "p".to_string(), "puyo".to_string()).unwrap();
        game.set_ready(&tet, true).unwrap();
        game.set_ready(&puyo, true).unwrap();
        game.start_match(room_id).unwrap();
        assert!(game.validate_garbage(&owner, &targeted_garbage_payload(2, &[0])).is_ok());
        assert!(game.validate_garbage(&owner, &targeted_garbage_payload(1, &[0])).is_ok());
        game.record_game_over(&puyo);
        assert!(game.validate_garbage(&owner, &targeted_garbage_payload(2, &[0])).is_err());
        assert!(game.validate_garbage(&owner, &targeted_garbage_payload(1, &[0])).is_ok());
    }

    /// 対戦中の退出は負けとして記録され、残り 1 人なら勝者が決まる。決着後はリザルト中で入室できない
    #[test]
    fn leaving_during_match_decides_winner_and_blocks_join_until_room() {
        let mut game = Game::default();
        let owner = Uuid::new_v4();
        let guest = Uuid::new_v4();
        let late = Uuid::new_v4();
        let (room_id, _) = game.new_room(
            owner, "test".to_string(), 3, true, vec![], "owner".to_string(), "tet".to_string(), Battle::Versus,
        );
        game.add_player_to_room(room_id, guest, "g".to_string(), "tet".to_string()).unwrap();
        game.set_ready(&guest, true).unwrap();
        game.start_match(room_id).unwrap();

        let result = game.leave_room_recording_result(&guest);
        assert!(matches!(result, Some((rid, WinnerStatus::Winner(w))) if rid == room_id && w == owner));
        let room = game.rooms.get(&room_id).unwrap();
        assert_eq!(room.status, RoomStatus::Waiting);
        // 残り 1 人になったのでリザルト中の制限は解けている
        assert!(!room.post_match);
        assert!(game.add_player_to_room(room_id, late, "l".to_string(), "tet".to_string()).is_ok());
    }

    #[test]
    fn join_is_refused_while_result_is_shown() {
        let mut game = Game::default();
        let owner = Uuid::new_v4();
        let guest = Uuid::new_v4();
        let late = Uuid::new_v4();
        let (room_id, _) = game.new_room(
            owner, "test".to_string(), 3, true, vec![], "owner".to_string(), "tet".to_string(), Battle::Versus,
        );
        game.add_player_to_room(room_id, guest, "g".to_string(), "tet".to_string()).unwrap();
        game.set_ready(&guest, true).unwrap();
        game.start_match(room_id).unwrap();
        game.record_game_over(&guest);
        assert!(game.add_player_to_room(room_id, late, "l".to_string(), "tet".to_string()).is_err());
        assert_eq!(game.reattach_state(&guest).map(|(_, _, r)| r), Some(Some(Some(owner))));
        game.end_post_match(&room_id);
        assert!(game.add_player_to_room(room_id, late, "l".to_string(), "tet".to_string()).is_ok());
        assert!(game.reattach_state(&guest).is_none());
    }

    #[test]
    fn stale_session_is_not_current() {
        let mut game = Game::default();
        let pid = Uuid::new_v4();
        let old = Uuid::new_v4();
        let new = Uuid::new_v4();
        assert!(game.is_current_session(&pid, &old));
        game.set_session(pid, old);
        game.set_session(pid, new);
        assert!(!game.is_current_session(&pid, &old));
        assert!(game.is_current_session(&pid, &new));
    }

    #[test]
    fn garbage_is_only_accepted_for_live_players_in_playing_room() {
        let mut game = Game::default();
        let owner = Uuid::new_v4();
        let guest = Uuid::new_v4();
        let (room_id, _) = game.new_room(
            owner,
            "test".to_string(),
            2,
            false,
            vec![],
            "owner".to_string(),
            "tet".to_string(), Battle::Versus,
        );
        game.add_player_to_room(room_id, guest, "guest".to_string(), "puyo".to_string())
            .unwrap();
        let body = garbage_payload(&[0]);

        assert!(game.validate_garbage(&owner, &body).is_err());
        game.set_ready(&guest, true).unwrap();
        game.start_match(room_id).unwrap();
        assert!(game.validate_garbage(&owner, &body).is_ok());
        game.record_game_over(&owner);
        assert!(game.validate_garbage(&owner, &body).is_err());
    }

    /// 中継ホットパス(get_opponent_channel)が依存する経路引き get_opponent が
    /// 正しく相手を返すこと、退室で経路が消えることを確認する。
    #[test]
    fn get_opponent_routes_through_room() {
        let mut game = Game::default();
        let owner = Uuid::new_v4();
        let guest = Uuid::new_v4();

        let (room_id, _code) = game.new_room(
            owner,
            "test".to_string(),
            2,
            false,
            vec![],
            "owner".to_string(),
            "tet".to_string(), Battle::Versus,
        );
        game.add_player_to_room(room_id, guest, "guest".to_string(), "puyo".to_string())
            .expect("guest should join");

        // 双方向に相手を引ける
        assert_eq!(game.get_opponent(&owner), Some(guest));
        assert_eq!(game.get_opponent(&guest), Some(owner));
        // 切断通知用に所属ルームも引ける
        assert_eq!(game.room_of(&guest), Some(room_id));

        // 退室すると経路が消える（中継先なし／所属ルームも消える）
        game.leave_room(&guest);
        assert_eq!(game.get_opponent(&owner), None);
        assert_eq!(game.room_of(&guest), None);
    }

    /// remove_connection は close すべきチャンネルを返すだけで、その場では await しない
    /// （ロックを保持したまま close.await すると全 Game 操作がブロックされるため）。
    /// 接続表・待機列・経路表からはちゃんと消えること、戻り値で close 対象を受け取れることを確認する。
    #[test]
    fn remove_connection_clears_state_and_returns_channels() {
        let mut game = Game::default();
        let owner = Uuid::new_v4();
        let guest = Uuid::new_v4();

        let (room_id, _code) = game.new_room(
            owner,
            "test".to_string(),
            2,
            false,
            vec![],
            "owner".to_string(),
            "tet".to_string(), Battle::Versus,
        );
        game.add_player_to_room(room_id, guest, "guest".to_string(), "puyo".to_string())
            .expect("guest should join");

        // 退室させると接続表からも消え、close 対象のチャンネル（今回は None／None）を返す
        let (rel, unrel) = game.remove_connection(&guest);
        assert!(rel.is_none() && unrel.is_none());

        // 経路表・ルームの players から消えている
        assert_eq!(game.room_of(&guest), None);
        assert_eq!(game.get_opponent(&owner), None);

        // owner を抜くと無人になりルームごと消える
        let _ = game.remove_connection(&owner);
        assert!(game.rooms.get(&room_id).is_none());
    }

    /// 表示名の更新: ルーム内・待機列の両方に反映され、
    /// 空文字や17文字以上は拒否されること、どこにも居なければ no-op 成功になることを確認する。
    #[test]
    fn update_player_username_updates_room_and_queue() {
        let mut game = Game::default();
        let owner = Uuid::new_v4();
        let guest = Uuid::new_v4();
        let queued = Uuid::new_v4();
        let stranger = Uuid::new_v4();

        let (room_id, _code) = game.new_room(
            owner,
            "test".to_string(),
            2,
            false,
            vec![],
            "owner".to_string(),
            "tet".to_string(), Battle::Versus,
        );
        game.add_player_to_room(room_id, guest, "guest".to_string(), "puyo".to_string())
            .expect("guest should join");
        // 待機列に1人（相手がいないのでキュー入りする）
        game.random_match(queued, "queued".to_string(), "tet".to_string());

        // 在室プレイヤーの名前が更新される（前後の空白はtrim）
        game.update_player_username(&guest, "  newname  ".to_string())
            .expect("rename should succeed");
        let room = game.rooms.get(&room_id).unwrap();
        let entry = room.players.iter().find(|(pid, _, _)| *pid == guest);
        assert_eq!(entry.map(|(_, name, _)| name.as_str()), Some("newname"));

        // 待機列エントリの名前も更新される
        game.update_player_username(&queued, "qname".to_string())
            .expect("queued rename should succeed");
        assert!(
            game.matchmaking_queue
                .iter()
                .any(|(pid, name, _)| *pid == queued && name == "qname")
        );

        // 空文字・16文字超は拒否
        assert!(
            game.update_player_username(&guest, "   ".to_string())
                .is_err()
        );
        assert!(game.update_player_username(&guest, "a".repeat(17)).is_err());

        // どこにも居ないプレイヤーは no-op 成功
        game.update_player_username(&stranger, "ghost".to_string())
            .expect("no-op rename should succeed");
    }

    /// Issue #8 の核心: Game を RwLock で包むと「読み取りは並行・書き込みは排他」に
    /// なることを実体で確認する。中継(読み取り)が全ルーム並行に走れる根拠。
    #[test]
    fn rwlock_allows_concurrent_reads_but_exclusive_writes() {
        let lock = Arc::new(RwLock::new(Game::default()));

        // 複数の読み取りロックを同時に保持できる（= 複数ルームの中継が並行）
        let r1 = lock.try_read().expect("first read lock");
        let r2 = lock.try_read().expect("concurrent read lock");

        // 読み取り保持中は書き込みを取れない（排他）
        assert!(
            lock.try_write().is_err(),
            "writer must not acquire while readers hold the lock"
        );

        drop(r1);
        drop(r2);

        // 読み取りが全て解放されれば書き込みを取れる
        assert!(
            lock.try_write().is_ok(),
            "writer should acquire once all readers released"
        );
    }

    /// このゲームに引き分けは無い：先に死んだ方が負け、最後まで生きた方が勝ち。
    /// 2人がほぼ同時に死んでも、先に処理された死亡で勝者が確定し、その後に届いた
    /// 勝者自身の gameOver は無視される（DRAWにならない）。
    #[test]
    fn last_survivor_wins_and_never_draws() {
        let mut game = Game::default();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let (room_id, _code) = game.new_room(
            a,
            "t".to_string(),
            2,
            false,
            vec![],
            "A".to_string(),
            "tet".to_string(), Battle::Versus,
        );
        game.add_player_to_room(room_id, b, "B".to_string(), "tet".to_string())
            .expect("B joins");
        {
            let room = match game.rooms.get_mut(&room_id) {
                Some(r) => r,
                None => panic!("room should exist"),
            };
            room.status = RoomStatus::Playing;
            room.alive_players = vec![a, b];
        }
        // A が先に死亡 → 最後の生存者 B が勝利
        assert!(
            matches!(game.record_game_over(&a), WinnerStatus::Winner(w) if w == b),
            "first death loses; last survivor wins"
        );
        // 直後に勝者 B の gameOver が届いても DRAW にならず無視される
        assert!(
            matches!(game.record_game_over(&b), WinnerStatus::MatchContinues),
            "post-resolution gameOver must be ignored (no draw)"
        );
    }

    #[test]
    fn match_setting_is_owner_only_and_clears_ready_players() {
        let mut game = Game::default();
        let owner = Uuid::new_v4();
        let guest = Uuid::new_v4();
        let (room_id, _code) = game.new_room(
            owner,
            "settings".to_string(),
            2,
            false,
            vec![],
            "owner".to_string(),
            "tet".to_string(), Battle::Versus,
        );
        game.add_player_to_room(room_id, guest, "guest".to_string(), "tet".to_string())
            .expect("guest joins");
        game.rooms.get_mut(&room_id).unwrap().ready_players = vec![owner, guest];

        // クライアントが実際に送るJSON（ms-gridの全キーを {...ms, ...patch} でスプレッド）を模す。
        let setting = r#"{"setTarget":3,"holdEnabled":false,"b2bBonus":true,"garbageMultiplier":1.5,"marginTime":96,"garbageHoleRate":70,"garbageDamageOnClear":true,"ojamaRate":70}"#;
        let saved = game
            .update_match_setting(&owner, room_id, setting.to_string())
            .expect("full client-sent setting must be accepted");
        let room = game.rooms.get(&room_id).unwrap();
        assert_eq!(room.match_setting, saved);
        let saved_value: serde_json::Value = serde_json::from_str(&saved).unwrap();
        let expected_value: serde_json::Value = serde_json::from_str(setting).unwrap();
        assert_eq!(saved_value, expected_value);
        assert!(room.ready_players.is_empty());

        assert!(
            game.update_match_setting(&guest, room_id, setting.to_string())
                .is_err()
        );
        assert!(
            game.update_match_setting(&owner, room_id, r#"{"garbageMultiplier":99}"#.to_string(),)
                .is_err()
        );
        assert!(
            game.update_match_setting(&owner, room_id, r#"{"setTarget":0}"#.to_string())
                .is_err(),
            "setTarget out of 1..=9 range must be rejected"
        );
    }

    #[test]
    fn match_setting_unknown_keys_are_dropped_not_rejected() {
        let mut game = Game::default();
        let owner = Uuid::new_v4();
        let (room_id, _code) = game.new_room(
            owner,
            "settings".to_string(),
            2,
            false,
            vec![],
            "owner".to_string(),
            "tet".to_string(), Battle::Versus,
        );

        // 未知キーが混ざっていても既知キーだけ保存され、エラーにはならない
        // （クライアント先行リリースで既存設定が丸ごと拒否されるのを防ぐ方針）。
        let saved = game
            .update_match_setting(
                &owner,
                room_id,
                r#"{"setTarget":2,"someFutureKey":true}"#.to_string(),
            )
            .expect("unknown keys must be sanitized, not rejected");
        let saved_value: serde_json::Value = serde_json::from_str(&saved).unwrap();
        assert_eq!(saved_value.get("setTarget").unwrap(), 2);
        assert!(saved_value.get("someFutureKey").is_none());
    }

    // ── ZONE BATTLE ONLINE（設計 tetlabo-zone-online-design.md §3〜§7）──────────────

    fn zone_room(game: &mut Game, owner: Uuid) -> Uuid {
        let (room_id, _) = game.new_room(
            owner, "zone".to_string(), 4, true, vec![], "owner".to_string(), "puyo".to_string(), Battle::Zone,
        );
        room_id
    }

    fn stats(score: u32) -> Vec<u8> {
        // rule(0=tet) + score(u32 LE) + lines + chainMax + attackSent + activeMs（StatsUpdate の中身）
        let mut v = vec![0u8];
        v.extend_from_slice(&score.to_le_bytes());
        v.extend_from_slice(&[0; 10]);
        v
    }

    #[test]
    fn zone_room_has_its_own_default_and_kind() {
        let mut game = Game::default();
        let owner = Uuid::new_v4();
        let zone = zone_room(&mut game, owner);
        let room = &game.rooms[&zone];
        assert_eq!(room.battle, Battle::Zone);
        assert_eq!(room.battle.as_str(), "zone");
        assert_eq!(room.match_setting, ONLINE_DEFAULT_ZONE_MATCH_SETTING);
        // 作成者もテト
        assert_eq!(room.players[0].2, "tet");
        // 既定の設定は自分の検査を通る（クライアントとの手合わせの確認）
        assert_eq!(
            sanitize_match_setting(ONLINE_DEFAULT_ZONE_MATCH_SETTING, Battle::Zone).map(|s| s.len() > 0),
            Ok(true)
        );
        let other = Uuid::new_v4();
        let (versus, _) = game.new_room(
            other, "vs".to_string(), 2, true, vec![], "o".to_string(), "puyo".to_string(), Battle::Versus,
        );
        assert_eq!(game.rooms[&versus].battle, Battle::Versus);
        assert_eq!(game.rooms[&versus].players[0].2, "puyo");
        assert_eq!(Battle::from_wire(None), Battle::Versus);
        assert_eq!(Battle::from_wire(Some("zone")), Battle::Zone);
        assert_eq!(Battle::from_wire(Some("??")), Battle::Versus);
    }

    #[test]
    fn zone_room_is_tet_only() {
        let mut game = Game::default();
        let owner = Uuid::new_v4();
        let room_id = zone_room(&mut game, owner);
        // コードで入る人はふだんのルール（ぷよ）で来ても、テトで入る
        let p = Uuid::new_v4();
        game.add_player_to_room(room_id, p, "p".to_string(), "puyo".to_string()).unwrap();
        assert_eq!(game.rooms[&room_id].players[1].2, "tet");
        // ぷよへの変更は断る・テトは通す
        assert!(game.update_player_rule(&p, "puyo".to_string()).is_err());
        assert!(game.update_player_rule(&p, "tet".to_string()).is_ok());
        assert_eq!(game.rooms[&room_id].players[1].2, "tet");
    }

    #[test]
    fn random_match_is_versus_only() {
        let mut game = Game::default();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        assert!(matches!(game.random_match(a, "a".to_string(), "tet".to_string()), MatchResult::Waiting));
        match game.random_match(b, "b".to_string(), "tet".to_string()) {
            MatchResult::Matched { room_id, .. } => {
                assert_eq!(game.rooms[&room_id].battle, Battle::Versus);
                assert_eq!(game.rooms[&room_id].match_setting, ONLINE_DEFAULT_MATCH_SETTING);
            }
            _ => panic!("expected a match"),
        }
    }

    #[test]
    fn zone_match_setting_is_validated() {
        let ok = |s: &str| sanitize_match_setting(s, Battle::Zone);
        // 知らないキー・VERSUS だけのキーは捨てる
        let out = ok(r#"{"ojamaRate":70,"foo":1,"zoneSplit":false}"#).unwrap();
        assert_eq!(out, r#"{"zoneSplit":false}"#);
        // 選択肢の外は断る
        assert!(ok(r#"{"marginTime":100}"#).is_err());
        assert!(ok(r#"{"marginTime":-96}"#).is_ok());
        assert!(ok(r#"{"marginScore":30000}"#).is_err());
        assert!(ok(r#"{"marginScore":200000}"#).is_ok());
        assert!(ok(r#"{"marginTrigger":"later"}"#).is_err());
        assert!(ok(r#"{"phaseTrigger":"time"}"#).is_ok());
        assert!(ok(r#"{"garbageHoleRate":75}"#).is_err());
        assert!(ok(r#"{"garbageHoleChunk":0}"#).is_ok());
        assert!(ok(r#"{"zoneQuarterMs":6000}"#).is_err());
        assert!(ok(r#"{"zoneQuarterLines":12}"#).is_ok());
        assert!(ok(r#"{"garbageMultiplier":3.5}"#).is_err());
        assert!(ok(r#"{"marginEnabled":1}"#).is_err());
        // フェーズの表: 3 行・01 のしきい値は 0・行の中の知らないキーは捨てる
        let row = |s: u32, t: u32| format!(r#"{{"holeRate":90,"holeChunk":10,"zoneTime":4000,"zoneCharge":8,"scoreAdd":{s},"timeAdd":{t},"x":1}}"#);
        let table = |a: String, b: String, c: String| format!(r#"{{"phaseTable":[{a},{b},{c}]}}"#);
        let good = ok(&table(row(0, 0), row(20000, 64000), row(40000, 128000))).unwrap();
        assert!(!good.contains("\"x\""));
        assert!(ok(&table(row(10000, 0), row(20000, 64000), row(40000, 128000))).is_err());
        assert!(ok(&table(row(0, 0), row(25000, 64000), row(40000, 128000))).is_err());
        assert!(ok(&table(row(0, 0), row(20000, 64000), row(40000, 100000))).is_err());
        assert!(ok(r#"{"phaseTable":[]}"#).is_err());
        // VERSUS の部屋は従来どおり（ZONE のキーは捨てる）
        let vs = sanitize_match_setting(r#"{"zoneSplit":true,"marginTime":100}"#, Battle::Versus).unwrap();
        assert_eq!(vs, r#"{"marginTime":100}"#);
    }

    #[test]
    fn update_match_setting_uses_room_kind() {
        let mut game = Game::default();
        let owner = Uuid::new_v4();
        let room_id = zone_room(&mut game, owner);
        // ZONE の部屋では MARGIN TIME は 32 秒刻みだけ
        assert!(game.update_match_setting(&owner, room_id, r#"{"marginTime":100}"#.to_string()).is_err());
        let saved = game
            .update_match_setting(&owner, room_id, r#"{"marginTime":128,"phaseTrigger":"time"}"#.to_string())
            .unwrap();
        assert_eq!(saved, r#"{"marginTime":128,"phaseTrigger":"time"}"#);
    }

    fn zone_garbage(target: u8, flags: u8, holes: &[u8]) -> Vec<u8> {
        let mut v = vec![GARBAGE_PROTOCOL_VERSION_ZONE, target, flags];
        v.extend_from_slice(&(holes.len() as u16).to_le_bytes());
        v.extend_from_slice(holes);
        v
    }

    #[test]
    fn garbage_v3_carries_zone_flags() {
        assert!(validate_garbage_payload(&zone_garbage(1, 0, &[0, 0, 0]), "tet").is_ok());
        assert!(validate_garbage_payload(&zone_garbage(1, 0b11, &[0; 4]), "tet").is_ok());
        assert!(validate_garbage_payload(&zone_garbage(1, 0b01, &[0]), "tet").is_ok());
        // 知らない印・ぷよ向けの印は断る
        assert!(validate_garbage_payload(&zone_garbage(1, 0b100, &[0]), "tet").is_err());
        assert!(validate_garbage_payload(&zone_garbage(2, 0b01, &[0]), "puyo").is_err());
        assert!(validate_garbage_payload(&zone_garbage(2, 0, &[5]), "puyo").is_ok());
        // 長さぴったり（1 バイト多い・少ないは断る）
        let mut long = zone_garbage(1, 0, &[0, 0]);
        long.push(0);
        assert!(validate_garbage_payload(&long, "tet").is_err());
        let mut short = zone_garbage(1, 0, &[0, 0]);
        short.pop();
        assert!(validate_garbage_payload(&short, "tet").is_err());
        assert_eq!(garbage_target_rule(&zone_garbage(1, 3, &[0])), Ok(Some("tet")));
        assert_eq!(garbage_target_rule(&zone_garbage(2, 0, &[0])), Ok(Some("puyo")));
        // 印の位置を読む前に切れているもの
        assert!(validate_garbage_payload(&[GARBAGE_PROTOCOL_VERSION_ZONE, 1], "tet").is_err());
    }

    #[test]
    fn zone_state_opcode_is_known() {
        assert!(crate::payload::Opcode::try_from(0x2Cu8).is_ok());
    }

    fn start_zone_match(game: &mut Game, owner: Uuid, others: &[Uuid], setting: Option<&str>) -> Uuid {
        let room_id = zone_room(game, owner);
        for (i, p) in others.iter().enumerate() {
            game.add_player_to_room(room_id, *p, format!("p{i}"), "tet".to_string()).unwrap();
        }
        if let Some(s) = setting {
            game.update_match_setting(&owner, room_id, s.to_string()).unwrap();
        }
        for p in others {
            game.set_ready(p, true).unwrap();
        }
        game.start_match(room_id).unwrap();
        room_id
    }

    #[test]
    fn zone_phase_follows_the_max_score_of_everyone() {
        let mut game = Game::default();
        let owner = Uuid::new_v4();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let room_id = start_zone_match(&mut game, owner, &[a, b], None);
        assert!(game.in_zone_match(&a));
        // 既定: 02 = 20,000、03 = 60,000（PHASE TRIGGER: SCORE）。マージンは TIME なので段階は通知しない
        assert_eq!(game.record_zone_score(&a, &stats(19_999)), None);
        assert_eq!(game.record_zone_score(&b, &stats(20_000)), Some((room_id, 2, None)));
        // 同じフェーズのままなら通知しない・下がらない
        assert_eq!(game.record_zone_score(&owner, &stats(30_000)), None);
        assert_eq!(game.record_zone_score(&a, &stats(25_000)), None);
        // 脱落した人のスコアも数える
        game.record_game_over(&b);
        assert_eq!(game.record_zone_score(&b, &stats(60_000)), Some((room_id, 3, None)));
        assert_eq!(game.record_zone_score(&a, &stats(90_000)), None);
        assert_eq!(game.rooms[&room_id].zone.max_score, 90_000);
    }

    #[test]
    fn zone_phase_can_jump_two_steps_and_resets_each_match() {
        let mut game = Game::default();
        let owner = Uuid::new_v4();
        let a = Uuid::new_v4();
        let room_id = start_zone_match(&mut game, owner, &[a], None);
        assert_eq!(game.record_zone_score(&a, &stats(70_000)), Some((room_id, 3, None)));
        // 決着 → 次の試合の頭で 0 に戻る
        game.record_game_over(&owner);
        assert!(game.record_zone_score(&a, &stats(80_000)).is_none()); // 対戦中でない
        assert!(!game.in_zone_match(&a));
        game.end_post_match(&room_id);
        game.set_ready(&a, true).unwrap();
        game.start_match(room_id).unwrap();
        assert_eq!(game.rooms[&room_id].zone.phase, 1);
        assert_eq!(game.rooms[&room_id].zone.max_score, 0);
        assert_eq!(game.record_zone_score(&a, &stats(20_000)), Some((room_id, 2, None)));
    }

    #[test]
    fn zone_margin_score_steps_and_custom_thresholds() {
        let mut game = Game::default();
        let owner = Uuid::new_v4();
        let a = Uuid::new_v4();
        // しきい値は「前のフェーズに足す」: 02 = 30,000、03 = 30,000 + 60,000。マージンは 40,000 で突入・20,000 ごと
        let setting = r#"{"marginEnabled":true,"marginTrigger":"score","marginScore":40000,"phaseTable":[{"scoreAdd":0},{"scoreAdd":30000},{"scoreAdd":60000}]}"#;
        let room_id = start_zone_match(&mut game, owner, &[a], Some(setting));
        assert_eq!(game.record_zone_score(&a, &stats(29_999)), None);
        assert_eq!(game.record_zone_score(&a, &stats(30_000)), Some((room_id, 2, None)));
        assert_eq!(game.record_zone_score(&a, &stats(40_000)), Some((room_id, 2, Some(0))));
        assert_eq!(game.record_zone_score(&a, &stats(59_999)), None);
        assert_eq!(game.record_zone_score(&owner, &stats(90_000)), Some((room_id, 3, Some(2))));
        // 段階の上限は 5
        assert_eq!(game.record_zone_score(&a, &stats(500_000)), Some((room_id, 3, Some(5))));
        assert_eq!(game.record_zone_score(&a, &stats(600_000)), None);
    }

    #[test]
    fn zone_progress_is_quiet_for_time_triggers_and_versus_rooms() {
        let mut game = Game::default();
        let owner = Uuid::new_v4();
        let a = Uuid::new_v4();
        // PHASE TRIGGER: TIME・MARGIN TRIGGER: TIME → サーバーは通知しない（各クライアントの時計）
        start_zone_match(&mut game, owner, &[a], Some(r#"{"phaseTrigger":"time"}"#));
        assert_eq!(game.record_zone_score(&a, &stats(500_000)), None);
        // PHASE OFF も同じ
        let mut game = Game::default();
        start_zone_match(&mut game, owner, &[a], Some(r#"{"phase":false}"#));
        assert_eq!(game.record_zone_score(&a, &stats(500_000)), None);
        // MARGIN SCORE 0（即時）は始まりから段階 0（クライアントも同じなので通知しない）
        let mut game = Game::default();
        let room_id = start_zone_match(&mut game, owner, &[a], Some(r#"{"phase":false,"marginTrigger":"score","marginScore":0}"#));
        assert_eq!(game.rooms[&room_id].zone.margin_step, Some(0));
        assert_eq!(game.record_zone_score(&a, &stats(19_999)), None);
        assert_eq!(game.record_zone_score(&a, &stats(20_000)), Some((room_id, 1, Some(1))));
        // MARGIN OFF なら SCORE でも通知しない
        let mut game = Game::default();
        start_zone_match(&mut game, owner, &[a], Some(r#"{"phase":false,"marginEnabled":false,"marginTrigger":"score"}"#));
        assert_eq!(game.record_zone_score(&a, &stats(500_000)), None);
        // VERSUS の部屋は読まない
        let mut game = Game::default();
        let (vs, _) = game.new_room(
            owner, "vs".to_string(), 2, true, vec![], "o".to_string(), "tet".to_string(), Battle::Versus,
        );
        game.add_player_to_room(vs, a, "a".to_string(), "tet".to_string()).unwrap();
        game.set_ready(&a, true).unwrap();
        game.start_match(vs).unwrap();
        assert_eq!(game.record_zone_score(&a, &stats(500_000)), None);
        // ぷよの StatsUpdate（rule=1）・短い中身は読まない
        let mut game = Game::default();
        start_zone_match(&mut game, owner, &[a], None);
        let mut puyo = stats(500_000);
        puyo[0] = 1;
        assert_eq!(game.record_zone_score(&a, &puyo), None);
        assert_eq!(game.record_zone_score(&a, &[0, 1, 2]), None);
    }
}
