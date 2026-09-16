use serde::{Deserialize, Serialize};

/// online アカウント化(設計 v2.2.2 §4.4)で AuthResult に載せる Discord プロフィール。
/// account.rs の `Account`(内部表現)をクライアント向けのワイヤ形式に変換したもの。
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AccountInfo {
    pub id: String,
    pub name: String,
    pub avatar_url: String,
}

impl From<&crate::account::Account> for AccountInfo {
    fn from(account: &crate::account::Account) -> Self {
        Self {
            id: account.discord_id.clone(),
            name: account.name.clone(),
            avatar_url: account.avatar_url.clone(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SignalMessage {
    // WebRTC用シグナリングメッセージ
    Offer {
        sdp: String,
        /// 再接続時にクライアントが既存の player_id を載せる。存在すれば再バインド、
        /// 無い/未知なら新規プレイヤーとして扱う（初回接続では省略=None）。
        #[serde(default)]
        player_id: Option<uuid::Uuid>,
        /// 再接続の乗っ取り対策（設計 v2.2.2 §7.3）。player_id を再利用する再接続では、
        /// 直前の AuthResult で受け取った reconnect_secret を一緒に載せる必要がある。
        /// 不一致・未指定は「不明なプレイヤーへの再接続」と同じ扱い＝新規プレイヤーとして処理する。
        #[serde(default)]
        reconnect_secret: Option<String>,
    },
    Answer {
        sdp: String,
    },
    Candidate {
        candidate: String,
        sdp_mid: Option<String>,
        sdp_m_line_index: Option<u16>,
        user_id: uuid::Uuid,
    },

    // クライアント認証
    Auth {
        version: String,
        /// 通信プロトコル番号（設計: source_assets/memory/v2.2.1/tetlabo-protocol-version-design.md）。
        /// 旧クライアント（2.2.0以前）はこのフィールドを送らない。Noneのときは移行期間として
        /// 従来どおり ALLOW_VERSIONS(version文字列)で判定する（main.rs::check_auth 参照）。
        #[serde(default)]
        protocol: Option<u32>,
        /// Cloudflare Worker が発行した短寿命チケット（online アカウント化。設計 v2.2.2 §7.1）。
        /// 無い/検証失敗でも接続は拒否せずゲスト扱いにする（main.rs 参照）。
        #[serde(default)]
        ticket: Option<String>,
    },
    AuthResult {
        success: bool,
        message: Option<String>,
        rtc_peer_ice_config: Option<String>,
        /// サーバー側のプロトコル番号。拒否時にクライアントが「サーバーの方が新しい/古い」を
        /// 判断するために使う。新クライアントだけが解釈し、旧クライアントは無視する。
        #[serde(default)]
        protocol: Option<u32>,
        /// チケット検証に成功した場合の Discord プロフィール。ゲストや未検証時は None。
        #[serde(default)]
        account: Option<AccountInfo>,
        /// 再接続の乗っ取り対策用シークレット（設計 v2.2.2 §7.3）。次回このプレイヤーとして
        /// 再接続する Offer に載せて返す必要がある。
        #[serde(default)]
        reconnect_secret: Option<String>,
        /// チケットはあったが検証に失敗した理由（クライアントは「ゲストとして接続しました」と表示）。
        /// チケット自体が無い場合（そもそも未ログイン）は None のまま。
        #[serde(default)]
        ticket_error: Option<String>,
    },
}
