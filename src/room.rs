use std::collections::HashMap;
use uuid::Uuid;

#[derive(Clone, Debug, Copy)]
#[repr(u32)]
#[allow(dead_code)]
pub enum RoomTag {
    PuyoTet = 0,
    PuyoOnly = 1,
    TetOnly = 2,
    Casual = 3,
    Competitive = 4,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
#[derive(PartialEq)]
pub enum RoomStatus {
    Waiting,
    Playing,
}

/// 部屋の種類（ZONE BATTLE ONLINE 設計 §3）。部屋を作るときに決め、後から変えない。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Battle {
    /// 通常の VERSUS（テト・ぷよ）
    Versus,
    /// ZONE BATTLE（テトのみ。フェーズ・SCORE のマージンはサーバーが通知する）
    Zone,
}

impl Battle {
    /// クライアントの値（"versus" | "zone"）から。知らない値・無しは VERSUS
    pub fn from_wire(v: Option<&str>) -> Self {
        match v {
            Some("zone") => Battle::Zone,
            _ => Battle::Versus,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Battle::Versus => "versus",
            Battle::Zone => "zone",
        }
    }
}

/// ZONE BATTLE の対戦中の進み具合（サーバーが StatsUpdate のスコアから決めて全員へ通知する。設計 §5）。
/// 対戦の頭（start_match）で作り直す。
#[derive(Clone, Debug, Default)]
pub struct ZoneProgress {
    /// PHASE TRIGGER: SCORE のときの 02・03 へ進む点（試合の始まりからの値）。フェーズなし・TIME は None
    pub phase_scores: Option<[u32; 2]>,
    /// MARGIN TRIGGER: SCORE のときの突入の点。それ以外は None
    pub margin_score: Option<u32>,
    /// 部屋の全員（脱落した人も含む）のスコアの最大値
    pub max_score: u32,
    /// いまのフェーズ（1〜3）
    pub phase: u8,
    /// いまのマージンの段階（None = 未突入、0〜5）
    pub margin_step: Option<u8>,
}

#[derive(Clone)]
pub struct Room {
    /// 部屋の状態
    pub status: RoomStatus,
    /// ルームのオーナーID
    pub owner: Uuid,
    /// プレイヤー (ID, 名前, ルール"tet"|"puyo") のタプルの Vec。
    /// ルールは対戦前に相手へ伝える（ロビーで相手のテト/ぷよを表示する）ために保持する。
    pub players: Vec<(Uuid, String, String)>,
    /// 部屋の名前
    pub room_name: String,
    /// 最大プレイヤー数
    pub max_players: u8,
    /// ルームの公開設定（true=ルーム一覧に表示、false=非表示でコード参加のみ）
    pub is_public: bool,
    /// ルーム種別を表すタグ
    pub tags: Vec<RoomTag>,
    /// 友人参加用の短いルームコード（UUIDの代わりに共有する）
    pub code: String,
    /// マッチ設定（vs_settings.js の設定を JSON シリアライズした文字列）。デフォルト "{}"。
    pub match_setting: String,
    /// 対戦中の生存プレイヤー ID リスト。start_match 時に全員で初期化され、ゲームオーバーごとに削除。
    pub alive_players: Vec<Uuid>,
    /// READY 済みプレイヤー ID リスト。マッチ開始/終了・入退室でリセットされる。
    pub ready_players: Vec<Uuid>,
    /// 各プレイヤーの RTT (ms)。クライアントからの自己報告値。
    pub pings: HashMap<Uuid, u32>,
    /// 決着した直後のリザルト表示中か。true の間は入室を断る（リザルト中に入った人がいると
    /// 「全員が RETRY」にならず再戦が始まらないため）。次の start_match か、誰かが「ルームに戻る」を
    /// 選んだとき・残りが 1 人以下になったときに false へ戻す。
    pub post_match: bool,
    /// 直前のマッチの結果（Some(Some(勝者)) / Some(None)=引き分け / None=結果なし）。
    /// 切断中に決着した人が再接続したとき、WinnerNotification を送り直すために持つ。
    pub last_result: Option<Option<Uuid>>,
    /// 部屋の種類（作った後は変えない）
    pub battle: Battle,
    /// ZONE BATTLE の対戦中の進み具合（VERSUS の部屋では使わない）
    pub zone: ZoneProgress,
}
