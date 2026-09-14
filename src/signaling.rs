use serde::{Deserialize, Serialize};

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
    },
    AuthResult {
        success: bool,
        message: Option<String>,
        rtc_peer_ice_config: Option<String>,
        /// サーバー側のプロトコル番号。拒否時にクライアントが「サーバーの方が新しい/古い」を
        /// 判断するために使う。新クライアントだけが解釈し、旧クライアントは無視する。
        #[serde(default)]
        protocol: Option<u32>,
    },
}
