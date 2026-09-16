use anyhow::Result;
use axum::{
    Router,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
    routing::get,
};
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use reqwest::{
    Url,
    header::{self, HeaderMap, HeaderValue},
};
use std::env;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tracing::Instrument;
use tracing::{debug, error, info, info_span, warn};
use tracing_appender::rolling;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::prelude::*;
use webrtc::api::APIBuilder;
use webrtc::api::media_engine::MediaEngine;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

mod account;
mod connection;
mod game;
mod payload;
mod room;
mod signaling;

use connection::{handle_reliable_connection, handle_unreliable_connection, spawn_close_channels};

macro_rules! nest {
    ($($n:ident),+ $(,)?) => {
        $(let $n = std::sync::Arc::clone(&$n);)+
    };
}

/// ルームの定期無応答スイープの実行間隔(秒)。PeerConnection の Failed/Closed 遷移を
/// 取りこぼしてゾンビ部屋がメモリに残り続ける問題への保険（game::Game::sweep_unresponsive_rooms）。
const ROOM_SWEEP_INTERVAL_SECS: u64 = 60;

/// 通信プロトコル番号（設計: source_assets/memory/v2.2.1/tetlabo-protocol-version-design.md）。
/// アプリのバージョン(ALLOW_VERSIONS)とは別に、接続を許可するかどうかを決める値。
/// 次のいずれかを変えたときだけ +1 し、クライアント側の同名定数
/// (src/online/protocol_version.ts の PROTOCOL_VERSION)も同時に上げる:
///   1. シグナリング(WebSocketのJSON)の形式
///   2. ゲームフレーム(DataChannelのバイナリ)のopcode追加・形式変更・受理条件
///   3. 相手に影響する対戦ルール(おじゃまの計算・相殺など)
/// それ以外の変更(見た目・PRACTICE・シングルの修正など)では上げない＝サーバー再デプロイ不要。
///
/// 直近の変更(1→2): online アカウント化(設計 v2.2.2 §7.4)。Auth/AuthResult/Offer と
/// RoomInfoNotification のフィールドを追加したため。
const PROTOCOL_VERSION: u32 = 2;

/// 認証判定を接続処理から切り出したもの（単体テスト用）。
/// `protocol` が Some ならプロトコル番号の完全一致で判定する（新クライアント）。
/// `protocol` が None の旧クライアント（2.2.0以前）は、移行期間として従来どおり
/// ALLOW_VERSIONS(アプリのバージョン文字列)で判定する。
/// 戻り値: Ok(())=許可 / Err(拒否メッセージ)。
fn check_auth(version: &str, protocol: Option<u32>, allow_versions: &[String]) -> Result<(), String> {
    match protocol {
        Some(p) => {
            if p == PROTOCOL_VERSION {
                Ok(())
            } else {
                Err(format!(
                    "Unsupported protocol version: {p}. Server protocol version: {PROTOCOL_VERSION}.\n\
                     Please reload or reopen the client to update to a supported version."
                ))
            }
        }
        None => {
            if allow_versions.iter().any(|v| v == version) {
                Ok(())
            } else {
                Err(format!(
                    "Unsupported client version: {version}. Allowed versions: {allow_versions:?}\n\
                     Please reload or reopen the client to update to a supported version."
                ))
            }
        }
    }
}

/// Cloudflare TURNが未設定/取得失敗のときに最低限クライアントへ渡すSTUNサーバー一覧。
/// これが無いと NAT 越しのクライアントは host候補(私有IP)しか集められず、ICEが
/// 到達可能な候補ペアを1つも作れず無言のまま固まる(2026-07-23に発生した障害の原因)。
const DEFAULT_STUN_URLS: [&str; 6] = [
    "stun:stun.cloudflare.com:3478",
    "stun:stun.l.google.com:19302",
    "stun:stun1.l.google.com:19302",
    "stun:stun2.l.google.com:19302",
    "stun:stun3.l.google.com:19302",
    "stun:stun4.l.google.com:19302",
];

/// クライアントへ送る `rtc_peer_ice_config` のフォールバック値(STUNのみ)をJSON文字列で返す。
fn default_stun_ice_config_json() -> String {
    serde_json::json!({ "iceServers": [ { "urls": DEFAULT_STUN_URLS } ] }).to_string()
}

#[derive(Clone)]
struct AppState {
    game: Arc<RwLock<game::Game>>,
    api: Arc<webrtc::api::API>,
    config: RTCConfiguration,
    allow_versions: Vec<String>,
    /// online アカウント化(設計 v2.2.2 §7)。TICKET_PUBLIC_KEY が未設定/不正なら None
    /// になり、その場合は誰が送っても常にチケット検証をスキップして全員ゲスト扱いにする
    /// （移行時・鍵未発行時にオンライン対戦自体を止めないため）。
    ticket_verifier: Option<Arc<Mutex<account::TicketVerifier>>>,
}

fn now_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// 再接続の乗っ取り対策(設計 v2.2.2 §7.3)用のランダムシークレットを生成する。
/// 新規の乱数生成器を増やさず、既存依存の uuid(v4, OS RNG由来)を2個繋いで32byteにする。
fn generate_reconnect_secret() -> String {
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// ファイルディスクリプタのソフト上限を引き上げる。
/// webrtc-rs は接続ごとに ICE 用 UDP ソケットを多数開き、close 後も一部が即時解放されない
/// （ライブラリ側の挙動）。既定の soft=1024 だと多数接続でFDが枯渇するため、ハード上限の
/// 範囲で十分大きな値へ引き上げて、現実的なセッション長で枯渇しないようにする。
fn raise_fd_limit() {
    unsafe {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return;
        }
        let target = 65536u64.min(lim.rlim_max as u64);
        if (lim.rlim_cur as u64) < target {
            lim.rlim_cur = target as libc::rlim_t;
            if libc::setrlimit(libc::RLIMIT_NOFILE, &lim) == 0 {
                info!("Raised RLIMIT_NOFILE soft limit to {target}");
            } else {
                error!("Failed to raise RLIMIT_NOFILE (continuing with current limit)");
            }
        }
    }
}

/// 超サンプルエンドポイント
async fn hello() -> &'static str {
    "Hello, World!"
}

/// WebSocketをアップグレードするエンドポイント
async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

/// WebSocketのハンドラの実態
async fn handle_socket(socket: WebSocket, state: AppState) {
    let (ws_sender, mut ws_receiver) = socket.split();
    let ws_sender = Arc::new(Mutex::new(ws_sender));

    let peer_connection = Arc::new(
        state
            .api
            .new_peer_connection(state.config)
            .await
            .expect("Failed to create peer connection"),
    );

    // この接続（=1本の PeerConnection）に属する player_id。
    // 初期は新規発番だが、再接続offer(player_id付き)を受けると既存プレイヤーのIDへ
    // 差し替える（再バインド）。各ハンドラは発火時に共有値を読むため、Mutex で包む。
    let player_id_shared = Arc::new(Mutex::new(uuid::Uuid::new_v4()));

    // Auth のチケット検証に成功した場合の Discord プロフィール（設計 v2.2.2 §7.2）。
    // Offer 処理で player_id が確定した時点で Game.accounts へ書き込む。
    // 他のハンドラ(on_ice_candidate等)はこれを読まないので player_id_shared と違い clone は不要。
    let account_shared: Arc<Mutex<Option<account::Account>>> = Arc::new(Mutex::new(None));

    let ws_sender_for_ice = Arc::clone(&ws_sender);
    let player_id_for_ice = Arc::clone(&player_id_shared);
    peer_connection.on_ice_candidate(Box::new(move |candidate| {
        let ws_sender = Arc::clone(&ws_sender_for_ice);
        let player_id_for_ice = Arc::clone(&player_id_for_ice);
        Box::pin(async move {
            if let Some(c) = candidate {
                let init = match c.to_json() {
                    Ok(v) => v,
                    Err(e) => {
                        debug!("Failed to serialize ICE candidate: {e}");
                        return;
                    }
                };
                let pid = *player_id_for_ice.lock().await;
                let msg = signaling::SignalMessage::Candidate {
                    candidate: init.candidate,
                    sdp_mid: init.sdp_mid,
                    sdp_m_line_index: init.sdp_mline_index,
                    user_id: pid,
                };
                let json = serde_json::to_string(&msg);
                match json {
                    Ok(j) => {
                        let _ = ws_sender.lock().await.send(Message::Text(j.into())).await;
                    }
                    Err(e) => {
                        debug!("Failed to serialize signaling message: {e}");
                        return;
                    }
                };
            }
        })
    }));

    {
        let game = Arc::clone(&state.game);
        // PC への弱参照（cycle を作らず、実切断時に disconnect_player が close() してFDを解放する）
        let pc_weak = Arc::downgrade(&peer_connection);
        let player_id_for_state = Arc::clone(&player_id_shared);
        peer_connection.on_peer_connection_state_change(Box::new(
            move |s: RTCPeerConnectionState| {
                debug!("Peer connection state changed: {s:?}");
                // 終端状態 (Failed/Closed) のみで切断処理を起動する。
                // Disconnected は一過性(ICE瞬断で復帰しうる)なので起動しない
                //   → ICE が failed_timeout(8s) を過ぎて初めて Failed になり、実切断と確定する。
                // シグナリングWSの切断はトリガにしない（WSは確立後に閉じる設計）。
                let trigger = matches!(
                    s,
                    RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
                );
                let game = Arc::clone(&game);
                let pc_weak = pc_weak.clone();
                let player_id_for_state = Arc::clone(&player_id_for_state);
                Box::pin(async move {
                    if trigger {
                        let pid = *player_id_for_state.lock().await;
                        connection::disconnect_player(game, pid, Some(pc_weak)).await;
                    }
                })
            },
        ));
    }

    // Offer ハンドラで再接続(player_id再利用)判定に Game を読むため、ループ用クローンを
    // on_data_channel クロージャが game をムーブするより前に確保しておく。
    let game_for_loop = Arc::clone(&state.game);
    let player_id_for_loop = Arc::clone(&player_id_shared);

    let game_for_dc = Arc::clone(&state.game);
    let player_id_for_dc = Arc::clone(&player_id_shared);
    // on_close ハンドラへ渡して確実に pc.close() を呼べるようにする（Weak なのでサイクルにならない）。
    let pc_weak_for_dc = Arc::downgrade(&peer_connection);
    peer_connection.on_data_channel(Box::new(
        move |dc: Arc<webrtc::data_channel::RTCDataChannel>| {
            let game = Arc::clone(&game_for_dc);
            let player_id_for_dc = Arc::clone(&player_id_for_dc);
            let pc_weak = pc_weak_for_dc.clone();
            let dc_label = dc.label().to_string();

            info!("Data channel created: {dc_label}");

            match dc_label.as_str() {
                connection::RELIABLE_CHANNEL_LABEL => Box::pin(async move {
                    nest!(game);
                    // チャネルが開くのは offer 処理後なので、ここでは確定済みの player_id を読む。
                    let pid = *player_id_for_dc.lock().await;
                    let _ = tokio::spawn(handle_reliable_connection(dc, game, pid, pc_weak))
                        .instrument(info_span!(
                            "handle_connection",
                            dc_label = %dc_label,
                            player_id = %pid
                        ));
                }),
                connection::UNRELIABLE_CHANNEL_LABEL => Box::pin(async move {
                    nest!(game);
                    let pid = *player_id_for_dc.lock().await;
                    let _ = tokio::spawn(handle_unreliable_connection(dc, game, pid, pc_weak))
                        .instrument(info_span!(
                            "handle_connection",
                            dc_label = %dc_label,
                            player_id = %pid
                        ));
                }),
                _ => {
                    debug!("Unknown data channel label: {dc_label}");
                    return Box::pin(async {});
                }
            }
        },
    ));

    let mut version_authenticated = false;

    while let Some(Ok(msg)) = ws_receiver.next().await {
        if let Message::Text(text) = msg {
            // 不正なシグナリングメッセージでタスクを panic させない
            let signal: signaling::SignalMessage = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    debug!("Failed to parse signaling message: {e}");
                    continue;
                }
            };

            match signal {
                signaling::SignalMessage::Offer {
                    sdp,
                    player_id: reconnect_id,
                    reconnect_secret: offered_secret,
                } => {
                    if !version_authenticated {
                        warn!("Received offer before client authentication; ignoring.");
                        continue;
                    }
                    // ★ 再接続: クライアントが既存の player_id を載せてきて、その人がまだ
                    //    サーバーに居る（猶予中など）かつ、直前の AuthResult で受け取った
                    //    reconnect_secret が一致する場合だけこの新PCをそのIDへ再バインドする
                    //    （設計 v2.2.2 §7.3: player_id を知っているだけで乗っ取れないように）。
                    //    state を Establishing に戻して disconnect_player の猶予removalを止める
                    //    （新しい両チャネルが揃えば check_if_ready が Connected に進める）。
                    if let Some(rid) = reconnect_id {
                        let secret_matches = {
                            let g = game_for_loop.read().await;
                            g.get_connection_state(&rid).is_some()
                                && offered_secret.is_some()
                                && g.reconnect_secret_of(&rid) == offered_secret
                        };
                        if secret_matches {
                            // 今のこの接続自身のAuthで発行された使い捨てのsecretを rid 側へ
                            // 引き継ぎ、以後の再接続はこの新しいsecretで検証されるようにする
                            // （ローテーション。使い捨て側のエントリは残さず掃除する）。
                            let own_pid = *player_id_for_loop.lock().await;
                            *player_id_for_loop.lock().await = rid;
                            {
                                let mut g = game_for_loop.write().await;
                                if let Some(fresh_secret) = g.take_reconnect_secret(&own_pid) {
                                    g.set_reconnect_secret(rid, fresh_secret);
                                }
                            }
                            let mut was_disconnected = false;
                            let room_id = {
                                let g = game_for_loop.read().await;
                                if let Some(conn_state) = g.get_connection_state(&rid) {
                                    let s = conn_state.lock();
                                    if let Ok(mut s) = s {
                                        if matches!(*s, crate::game::ConnectionState::Disconnected)
                                        {
                                            *s = crate::game::ConnectionState::Establishing;
                                            was_disconnected = true;
                                        }
                                    }
                                }
                                g.room_of(&rid)
                            };
                            info!("Reconnect offer: rebinding new PC to existing player [{rid}]");
                            // 猶予中の切断UIを解除させるため、Reconnected をルームへ即通知する
                            // （PlayerConnectionLostNotification とペアの通知）。
                            if was_disconnected {
                                if let Some(room_id) = room_id {
                                    connection::broadcast_json(
                                        &game_for_loop,
                                        room_id,
                                        &payload::JsonMessage::JSONPlayerReconnectedNotification(
                                            payload::JSONPlayerReconnectedNotification {
                                                room_id,
                                                player_id: rid,
                                            },
                                        ),
                                    )
                                    .await;
                                }
                            }
                        } else {
                            warn!(
                                "Reconnect offer for unknown player or mismatched reconnect_secret [{rid}]; treating as new connection."
                            );
                        }
                    }

                    // player_id が確定した(新規、または上の再接続で確定)ので、チケット検証済みの
                    // アカウントがあればここで初めて Game 側へ反映する（設計 v2.2.2 §7.2）。
                    if let Some(account) = account_shared.lock().await.clone() {
                        let final_pid = *player_id_for_loop.lock().await;
                        game_for_loop.write().await.set_account(final_pid, account);
                    }

                    let offer = match RTCSessionDescription::offer(sdp) {
                        Ok(o) => o,
                        Err(e) => {
                            debug!("Failed to parse SDP offer: {e}");
                            continue;
                        }
                    };

                    if let Err(e) = peer_connection.set_remote_description(offer).await {
                        debug!("Failed to set remote description: {e}");
                        continue;
                    }

                    let answer = match peer_connection.create_answer(None).await {
                        Ok(a) => a,
                        Err(e) => {
                            debug!("Failed to create SDP answer: {e}");
                            continue;
                        }
                    };

                    if let Err(e) = peer_connection.set_local_description(answer.clone()).await {
                        debug!("Failed to set local description: {e}");
                        continue;
                    }

                    let response = match serde_json::to_string(&signaling::SignalMessage::Answer {
                        sdp: answer.sdp,
                    }) {
                        Ok(r) => r,
                        Err(e) => {
                            debug!("Failed to serialize signaling message: {e}");
                            continue;
                        }
                    };

                    let _ = ws_sender
                        .lock()
                        .await
                        .send(Message::Text(response.into()))
                        .await;
                }
                signaling::SignalMessage::Candidate {
                    candidate,
                    sdp_mid,
                    sdp_m_line_index,
                    user_id: _,
                } => {
                    if !version_authenticated {
                        warn!("Received offer before client authentication; ignoring.");
                        continue;
                    }
                    let init = RTCIceCandidateInit {
                        candidate,
                        sdp_mid,
                        sdp_mline_index: sdp_m_line_index,
                        username_fragment: None,
                    };
                    if let Err(e) = peer_connection.add_ice_candidate(init).await {
                        warn!("Failed to add ICE candidate: {e}");
                    }
                }
                signaling::SignalMessage::Auth { version, protocol, ticket } => {
                    if let Err(message) = check_auth(&version, protocol, &state.allow_versions) {
                        error!(
                            "Rejected connection with unsupported client (version={version}, protocol={protocol:?}): {message}"
                        );

                        let _ = ws_sender.lock().await.send(Message::Text(
                            serde_json::to_string(&signaling::SignalMessage::AuthResult {
                                success: false,
                                message: Some(message),
                                rtc_peer_ice_config: None,
                                protocol: Some(PROTOCOL_VERSION),
                                account: None,
                                reconnect_secret: None,
                                ticket_error: None,
                            })
                            .unwrap()
                            .into(),
                        )).await;

                        break;
                    } else {
                        info!("Client authenticated with version: {version}, protocol: {protocol:?}");

                        // online アカウント化(設計 v2.2.2 §7.2): チケットがあれば検証する。
                        // TICKET_PUBLIC_KEY未設定(=ticket_verifierがNone)ならそもそも検証せず、
                        // チケットの有無を問わず常にゲスト扱い(エラーも出さない)。
                        // 不正/期限切れでも接続自体は拒否せずゲストとして続行し、ticket_error だけ返す。
                        let mut ticket_error: Option<String> = None;
                        if let (Some(t), Some(verifier)) = (&ticket, &state.ticket_verifier) {
                            match verifier.lock().await.verify(t, now_unix_secs()) {
                                Ok(acc) => {
                                    info!("Ticket verified for account [{}]", acc.discord_id);
                                    *account_shared.lock().await = Some(acc);
                                }
                                Err(e) => {
                                    warn!("Ticket verification failed: {e}");
                                    ticket_error = Some(e.to_string());
                                }
                            }
                        }
                        let account_info = account_shared
                            .lock()
                            .await
                            .as_ref()
                            .map(signaling::AccountInfo::from);

                        // 再接続の乗っ取り対策(設計 v2.2.2 §7.3): この接続自身の現在のplayer_id
                        // (Offer未処理ならそのまま確定IDになる)へシークレットを発行して紐付ける。
                        let reconnect_secret = generate_reconnect_secret();
                        {
                            let pid = *player_id_for_loop.lock().await;
                            game_for_loop
                                .write()
                                .await
                                .set_reconnect_secret(pid, reconnect_secret.clone());
                        }

                        let ice_config: Option<String> = {
                            // 空文字列は「未設定」として扱う。env::var は空文字でも Ok("") を返すため、
                            // .ok() だけだと Environment=CF_TURN_TOKEN_ID= のような空設定でも
                            // 「設定されている」ルートに入り、Cloudflareへ壊れたリクエストを送って
                            // 静かに失敗する(TURN/STUNどちらも無い状態でクライアントに渡ってしまう)。
                            let token_id = env::var("CF_TURN_TOKEN_ID")
                                .ok()
                                .filter(|s| !s.is_empty());
                            let token_secret = env::var("CF_TURN_API_TOKEN")
                                .ok()
                                .filter(|s| !s.is_empty());

                            if let (Some(id), Some(secret)) = (token_id, token_secret) {
                                let url = Url::parse(format!("https://rtc.live.cloudflare.com/v1/turn/keys/{}/credentials/generate-ice-servers", id).as_str()).unwrap();
                                let mut map: std::collections::HashMap<&str, i32> =
                                    std::collections::HashMap::new();
                                map.insert("ttl", 86400);

                                let header = format!("Bearer {}", secret);
                                let mut headers = HeaderMap::new();
                                headers.insert(
                                    header::AUTHORIZATION,
                                    HeaderValue::from_str(&header).unwrap(),
                                );
                                headers.insert(
                                    header::CONTENT_TYPE,
                                    HeaderValue::from_static("application/json"),
                                );

                                let client = reqwest::Client::new();
                                let res = client.post(url).headers(headers).json(&map).send().await;
                                if let Ok(r) = res {
                                    if r.status().is_success() {
                                        let text = r.text().await;
                                        if let Ok(t) = text {
                                            Some(t)
                                        } else {
                                            error!("Failed to read ICE server response text");
                                            Some(default_stun_ice_config_json())
                                        }
                                    } else {
                                        error!("Failed to get ICE servers: HTTP {}", r.status());
                                        Some(default_stun_ice_config_json())
                                    }
                                } else {
                                    error!(
                                        "Failed to send request for ICE servers: {}",
                                        res.err().unwrap()
                                    );
                                    Some(default_stun_ice_config_json())
                                }
                            } else {
                                warn!(
                                    "CF_TURN_TOKEN_ID/CF_TURN_API_TOKEN not set; falling back to STUN-only ICE config"
                                );
                                Some(default_stun_ice_config_json())
                            }
                        };

                        let _ = ws_sender
                            .lock()
                            .await
                            .send(Message::Text(
                                serde_json::to_string(&signaling::SignalMessage::AuthResult {
                                    success: true,
                                    message: None,
                                    rtc_peer_ice_config: ice_config,
                                    protocol: Some(PROTOCOL_VERSION),
                                    account: account_info,
                                    reconnect_secret: Some(reconnect_secret),
                                    ticket_error,
                                })
                                .unwrap()
                                .into(),
                            ))
                            .await;

                        version_authenticated = true;
                    }
                }
                _ => {
                    debug!("Unexpected signaling message type: {signal:?}");
                    continue;
                }
            }
        }
    }

    // ★ シグナリングWSが閉じても PeerConnection は閉じない。
    //   WSは「確立後に閉じてよい」設計（client connection.ts 参照）で、DataChannel は
    //   生き続けるため。ここで close すると対戦中に WS がアイドル切断された瞬間に
    //   全員の接続が切れる。リソース解放は PeerConnection が Failed になった時に行う。
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    raise_fd_limit();

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();

    let file_appender = rolling::never("logs", "server.log");
    let (file_writer, _guard) = tracing_appender::non_blocking(file_appender);

    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_level(true)
        .with_writer(std::io::stdout);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_level(true)
        .with_ansi(false)
        .with_writer(file_writer);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(stdout_layer)
        .with(file_layer)
        .init();

    info!("Starting server...");

    let http_addr = env::var("HTTP_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());

    let mut m = MediaEngine::default();
    m.register_default_codecs()?;
    // ICE タイムアウト。切断検知を早めたいが、短すぎると低品質な回線
    // (高レイテンシ・パケロス)でチェックが完了する前にFailedになってしまう
    // (大学WIFI等での接続断の主因だった。旧設定は 5s/8s = 13s で機械的に打ち切っていた)。
    // disconnected: 一過性とみなす猶予 / failed: これを過ぎたら終端(Failed) / keepalive: 疎通確認間隔
    let mut s = webrtc::api::setting_engine::SettingEngine::default();
    // EC2にIPv6の実経路が無い環境だと、ICEエージェントがデフォルトで試みるudp6候補の
    // 収集が "Network is unreachable" で全滅し、リンクローカルアドレスへのbindも
    // EINVALで失敗する(実機ログで確認済み)。IPv4のみに絞ってこの無駄な失敗試行と
    // 収集フェーズの混乱を避ける。
    s.set_network_types(vec![webrtc::ice::network_type::NetworkType::Udp4]);
    // ICE_UDP_PORT_RANGE="50000-50100" のように指定すると、ICE候補のUDPポートを
    // この範囲に固定する。EC2等でセキュリティグループのUDP開放範囲を絞るために必要。
    // NAT_1TO1_IP にパブリックIP(Elastic IP)を指定すると、host候補をそのIPに
    // 書き換えて広告する(EC2はNAT越しなのでこれがないと私有IPしか候補に出ない)。
    if let Ok(range) = env::var("ICE_UDP_PORT_RANGE") {
        if let Some((min, max)) = range.split_once('-') {
            let min: u16 = min.trim().parse().expect("invalid ICE_UDP_PORT_RANGE min");
            let max: u16 = max.trim().parse().expect("invalid ICE_UDP_PORT_RANGE max");
            s.set_udp_network(webrtc::ice::udp_network::UDPNetwork::Ephemeral(
                webrtc::ice::udp_network::EphemeralUDP::new(min, max)
                    .expect("invalid ICE_UDP_PORT_RANGE"),
            ));
            info!("ICE UDP port range set to {min}-{max}");
        }
    }
    if let Ok(ip) = env::var("NAT_1TO1_IP") {
        s.set_nat_1to1_ips(
            vec![ip.clone()],
            webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType::Host,
        );
        info!("NAT 1:1 IP set to {ip}");
    }
    s.set_ice_timeouts(
        Some(std::time::Duration::from_secs(10)),
        Some(std::time::Duration::from_secs(20)),
        Some(std::time::Duration::from_secs(2)),
    );
    let api = Arc::new(
        APIBuilder::new()
            .with_media_engine(m)
            .with_setting_engine(s)
            .build(),
    );
    // サーバー自身の PeerConnection には STUN を渡さない。NAT_1TO1_IP で host候補が
    // 既にパブリックIPへ書き換わっており、STUNで求まる srflx候補は同じIPの重複でしかない。
    // それでも ICE_UDP_PORT_RANGE の限られたポートを srflxサーバー数(=6)だけ余分に消費し、
    // 1接続あたりの消費ポート数を7倍にしてポート枯渇を早めていた
    // (2026-07-26に発生した障害の原因の一つ)。クライアントへ配る rtc_peer_ice_config の
    // STUN/TURN(DEFAULT_STUN_URLS / default_stun_ice_config_json)とは別物なので、
    // クライアント側の到達性には影響しない。
    let config = RTCConfiguration::default();

    // Game は読み取り(中継時の RTCDataChannel 取得)が高頻度・書き込み(ルーム操作)が低頻度
    // なので RwLock を使い、中継の読み取りロックを全ルーム並行に取れるようにする(Issue #8)。
    let game = Arc::new(RwLock::new(game::Game::default()));

    // online アカウント化(設計 v2.2.2 §7)。TICKET_PUBLIC_KEY が無い/1件も有効な鍵として
    // パースできない場合は None にし、Auth処理はチケット検証自体を行わず全員ゲスト扱いにする
    // （鍵未発行のローカル開発や移行期間でオンライン対戦自体を止めないため）。
    let ticket_verifier: Option<Arc<Mutex<account::TicketVerifier>>> = {
        let raw_keys = env::var("TICKET_PUBLIC_KEY").unwrap_or_default();
        let audience = env::var("TICKET_AUDIENCE").unwrap_or_default();
        let keys = account::parse_public_keys(&raw_keys);
        if keys.is_empty() {
            info!(
                "TICKET_PUBLIC_KEY not set (or contains no valid keys); online accounts disabled, all connections are guests"
            );
            None
        } else {
            info!(
                "Ticket verification enabled: {} public key(s), audience={audience:?}",
                keys.len()
            );
            Some(Arc::new(Mutex::new(account::TicketVerifier::new(
                keys, audience,
            ))))
        }
    };

    let state = AppState {
        game,
        api,
        config,
        allow_versions: env::var("ALLOW_VERSIONS")
            .unwrap_or_else(|_| "0.0.0".to_string())
            .split(',')
            .map(|s| s.trim().to_string())
            .collect(),
        ticket_verifier,
    };

    // 定期スイープ: 60秒ごとに、全員のreliableチャンネルが無応答になったルームを破棄する。
    // disconnect_player の PeerConnection state 遷移トリガーを取りこぼした場合の保険。
    // ついでにチケットのjti再利用防止表(account::TicketVerifier)の期限切れエントリも掃除する。
    {
        let game = Arc::clone(&state.game);
        let ticket_verifier = state.ticket_verifier.clone();
        tokio::spawn(async move {
            let mut ticker =
                tokio::time::interval(std::time::Duration::from_secs(ROOM_SWEEP_INTERVAL_SECS));
            loop {
                ticker.tick().await;
                let closed = game.write().await.sweep_unresponsive_rooms();
                for (player_id, reliable, unreliable) in closed {
                    warn!(
                        "Swept unresponsive room member [{player_id}] (no response within {ROOM_SWEEP_INTERVAL_SECS}s sweep interval)"
                    );
                    spawn_close_channels(player_id, reliable, unreliable);
                }
                if let Some(verifier) = &ticket_verifier {
                    verifier.lock().await.sweep_expired(now_unix_secs());
                }
            }
        });
    }

    let app = Router::new()
        .route("/", get(hello))
        .route("/ws", get(ws_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&http_addr).await?;
    info!("Listening on {http_addr}...");
    axum::serve(listener, app).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow_versions() -> Vec<String> {
        vec!["2.1.0".to_string(), "2.1.1".to_string(), "2.2.0".to_string()]
    }

    #[test]
    fn protocol_match_is_allowed() {
        assert!(check_auth("2.2.1", Some(PROTOCOL_VERSION), &allow_versions()).is_ok());
    }

    #[test]
    fn protocol_mismatch_is_rejected_even_if_version_is_allowed() {
        // versionがALLOW_VERSIONSに載っていても、protocolが一致しなければ拒否する
        // （完全一致方式。範囲許可はしない）。
        assert!(check_auth("2.1.1", Some(PROTOCOL_VERSION + 1), &allow_versions()).is_err());
    }

    #[test]
    fn no_protocol_falls_back_to_allow_versions() {
        // 旧クライアント（2.2.0以前）はprotocolを送らない＝移行期間はversion文字列で判定する。
        assert!(check_auth("2.1.1", None, &allow_versions()).is_ok());
        assert!(check_auth("1.3.0", None, &allow_versions()).is_err());
    }

    #[test]
    fn no_protocol_and_empty_allow_versions_is_rejected() {
        assert!(check_auth("2.2.0", None, &[]).is_err());
    }
}
