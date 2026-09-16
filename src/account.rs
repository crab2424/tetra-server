//! online アカウント化: Cloudflare Worker が発行する短寿命チケットの検証。
//! 設計: source_assets/memory/v2.2.2/tetlabo-discord-integration-design.md §7
//!
//! チケット形式: base64url(payloadJSON) + "." + base64url(Ed25519署名(payloadJSON のバイト列))
//! payload: { v, sub(discord_id), name, avatarUrl, aud(接続先ホスト), iat, exp, jti }

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::Deserialize;
use std::collections::HashMap;

/// exp/iat の時計ずれ許容(秒)。Worker とサーバーのクロックが多少ずれていても弾かないため。
const CLOCK_SKEW_SECS: i64 = 30;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub discord_id: String,
    pub name: String,
    pub avatar_url: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum TicketError {
    Malformed,
    BadSignature,
    UnsupportedVersion(u32),
    WrongAudience,
    Expired,
    Replayed,
}

impl std::fmt::Display for TicketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => write!(f, "malformed ticket"),
            Self::BadSignature => write!(f, "invalid ticket signature"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported ticket version: {v}"),
            Self::WrongAudience => write!(f, "ticket audience mismatch"),
            Self::Expired => write!(f, "ticket expired"),
            Self::Replayed => write!(f, "ticket already used"),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TicketPayload {
    v: u32,
    sub: String,
    name: String,
    avatar_url: String,
    aud: String,
    #[allow(dead_code)] // 表示専用(発行時刻)。検証には使わない(§7参照)。
    iat: i64,
    exp: i64,
    jti: String,
}

/// `TICKET_PUBLIC_KEY` (raw 32byte を標準base64・カンマ区切り) をパースする。
/// 不正な値は黙って無視する(鍵ローテーション中に片方が空/壊れていても他方で継続できるように)。
pub fn parse_public_keys(env_value: &str) -> Vec<VerifyingKey> {
    env_value
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .filter_map(|s| {
            let bytes = STANDARD.decode(s).ok()?;
            let bytes: [u8; 32] = bytes.try_into().ok()?;
            VerifyingKey::from_bytes(&bytes).ok()
        })
        .collect()
}

fn parse_ticket(ticket: &str) -> Result<(Vec<u8>, Signature), TicketError> {
    let (payload_b64, sig_b64) = ticket.split_once('.').ok_or(TicketError::Malformed)?;
    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| TicketError::Malformed)?;
    let sig_bytes = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|_| TicketError::Malformed)?;
    let sig_bytes: [u8; 64] = sig_bytes.try_into().map_err(|_| TicketError::Malformed)?;
    Ok((payload_bytes, Signature::from_bytes(&sig_bytes)))
}

/// チケット検証器。公開鍵(ローテーション用に複数可)・自ホスト名(audience)・jti再利用防止表を保持する。
pub struct TicketVerifier {
    public_keys: Vec<VerifyingKey>,
    audience: String,
    seen_jti: HashMap<String, i64>,
}

impl TicketVerifier {
    pub fn new(public_keys: Vec<VerifyingKey>, audience: String) -> Self {
        Self {
            public_keys,
            audience,
            seen_jti: HashMap::new(),
        }
    }

    /// `now`(epoch秒)を渡すことでテスト容易性を確保する。
    /// 検査順序: 署名 → v(バージョン) → aud(発行先ホスト) → exp(±30秒許容) → jti(未使用)。
    pub fn verify(&mut self, ticket: &str, now: i64) -> Result<Account, TicketError> {
        let (payload_bytes, signature) = parse_ticket(ticket)?;

        if !self
            .public_keys
            .iter()
            .any(|key| key.verify_strict(&payload_bytes, &signature).is_ok())
        {
            return Err(TicketError::BadSignature);
        }

        let payload: TicketPayload =
            serde_json::from_slice(&payload_bytes).map_err(|_| TicketError::Malformed)?;

        if payload.v != 1 {
            return Err(TicketError::UnsupportedVersion(payload.v));
        }
        if payload.aud != self.audience {
            return Err(TicketError::WrongAudience);
        }
        if now > payload.exp + CLOCK_SKEW_SECS {
            return Err(TicketError::Expired);
        }
        if self.seen_jti.contains_key(&payload.jti) {
            return Err(TicketError::Replayed);
        }
        self.seen_jti.insert(payload.jti, payload.exp);

        Ok(Account {
            discord_id: payload.sub,
            name: payload.name,
            avatar_url: payload.avatar_url,
        })
    }

    /// 期限切れの jti エントリを掃除する(既存の60秒ルームスイープに便乗して呼ぶ)。
    pub fn sweep_expired(&mut self, now: i64) {
        self.seen_jti.retain(|_, exp| *exp + CLOCK_SKEW_SECS >= now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{SigningKey, Signer};

    const TEST_AUD: &str = "tetlabo-canary.duckdns.org";

    fn signing_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn make_ticket(key: &SigningKey, patch: impl FnOnce(&mut serde_json::Value)) -> String {
        let mut payload = serde_json::json!({
            "v": 1,
            "sub": "123456789",
            "name": "Alice",
            "avatarUrl": "https://cdn.discordapp.com/embed/avatars/0.png",
            "aud": TEST_AUD,
            "iat": 1_000,
            "exp": 1_120,
            "jti": "nonce-1",
        });
        patch(&mut payload);
        let payload_bytes = serde_json::to_vec(&payload).unwrap();
        let signature = key.sign(&payload_bytes);
        format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(&payload_bytes),
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        )
    }

    fn verifier(key: &SigningKey) -> TicketVerifier {
        TicketVerifier::new(vec![key.verifying_key()], TEST_AUD.to_string())
    }

    #[test]
    fn valid_ticket_is_accepted() {
        let key = signing_key(1);
        let ticket = make_ticket(&key, |_| {});
        let account = verifier(&key).verify(&ticket, 1_050).unwrap();
        assert_eq!(account.discord_id, "123456789");
        assert_eq!(account.name, "Alice");
        assert_eq!(account.avatar_url, "https://cdn.discordapp.com/embed/avatars/0.png");
    }

    #[test]
    fn tampered_payload_is_rejected() {
        let key = signing_key(1);
        let ticket = make_ticket(&key, |_| {});
        // 署名対象のバイト列を変えると署名検証で弾かれる(名前を書き換えるなりすまし対策)。
        let (payload_b64, sig_b64) = ticket.split_once('.').unwrap();
        let mut payload_bytes = URL_SAFE_NO_PAD.decode(payload_b64).unwrap();
        let last = payload_bytes.len() - 2;
        payload_bytes[last] ^= 0xFF;
        let tampered = format!("{}.{}", URL_SAFE_NO_PAD.encode(&payload_bytes), sig_b64);
        assert_eq!(
            verifier(&key).verify(&tampered, 1_050),
            Err(TicketError::BadSignature)
        );
    }

    #[test]
    fn signature_from_wrong_key_is_rejected() {
        let signer = signing_key(1);
        let other = signing_key(2);
        let ticket = make_ticket(&signer, |_| {});
        assert_eq!(
            verifier(&other).verify(&ticket, 1_050),
            Err(TicketError::BadSignature)
        );
    }

    #[test]
    fn key_rotation_accepts_either_configured_key() {
        let old_key = signing_key(1);
        let new_key = signing_key(2);
        let ticket = make_ticket(&old_key, |_| {});
        // ローテーション中: 新旧両方の公開鍵を許可しておけば旧鍵で発行されたチケットも通る。
        let mut v = TicketVerifier::new(
            vec![new_key.verifying_key(), old_key.verifying_key()],
            TEST_AUD.to_string(),
        );
        assert!(v.verify(&ticket, 1_050).is_ok());
    }

    #[test]
    fn wrong_audience_is_rejected() {
        let key = signing_key(1);
        let ticket = make_ticket(&key, |p| {
            p["aud"] = serde_json::Value::String("evil.example.com".to_string());
        });
        assert_eq!(
            verifier(&key).verify(&ticket, 1_050),
            Err(TicketError::WrongAudience)
        );
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let key = signing_key(1);
        let ticket = make_ticket(&key, |p| {
            p["v"] = serde_json::Value::Number(2.into());
        });
        assert_eq!(
            verifier(&key).verify(&ticket, 1_050),
            Err(TicketError::UnsupportedVersion(2))
        );
    }

    #[test]
    fn expired_ticket_is_rejected_outside_clock_skew() {
        let key = signing_key(1);
        let ticket = make_ticket(&key, |_| {}); // exp = 1120
        // ちょうど許容誤差(30秒)以内はまだ許可
        assert!(verifier(&key).verify(&ticket, 1_150).is_ok());
    }

    #[test]
    fn expired_ticket_is_rejected_after_clock_skew() {
        let key = signing_key(1);
        let ticket = make_ticket(&key, |_| {}); // exp = 1120
        assert_eq!(
            verifier(&key).verify(&ticket, 1_151),
            Err(TicketError::Expired)
        );
    }

    #[test]
    fn replayed_jti_is_rejected_on_second_use() {
        let key = signing_key(1);
        let ticket = make_ticket(&key, |_| {});
        let mut v = verifier(&key);
        assert!(v.verify(&ticket, 1_050).is_ok());
        assert_eq!(v.verify(&ticket, 1_060), Err(TicketError::Replayed));
    }

    #[test]
    fn sweep_expired_removes_old_jti_entries_only() {
        let key = signing_key(1);
        let mut v = verifier(&key);
        let ticket_a = make_ticket(&key, |p| {
            p["jti"] = serde_json::Value::String("a".to_string());
            p["exp"] = serde_json::Value::Number(1_000.into());
        });
        let ticket_b = make_ticket(&key, |p| {
            p["jti"] = serde_json::Value::String("b".to_string());
            p["exp"] = serde_json::Value::Number(5_000.into());
        });
        v.verify(&ticket_a, 900).unwrap();
        v.verify(&ticket_b, 900).unwrap();

        // a は期限切れになったので掃除で消える → 同じjtiを別チケットで再利用できる(再利用防止表の意味でのみ再利用可能、
        // ticket_a自体は既にexpも過ぎているので別途Expiredで弾かれる。ここではmapの掃除自体を検証する)。
        v.sweep_expired(1_100);
        assert!(!v.seen_jti.contains_key("a"));
        assert!(v.seen_jti.contains_key("b"));
    }
}
