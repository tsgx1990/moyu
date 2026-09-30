//! 邀请码(`moyuinv1...`)与 join-request 信封 —— 纯逻辑,零 I/O。
//! bech32 用 `Bech32` 变体(非 Bech32m);经典 90 字符上限不适用于该 API
//! (真实上限 ~1023,nostr 的多 relay `nprofile` 同法),我们的码必超 90。

use bech32::{Bech32, Hrp};
use serde::{Deserialize, Serialize};

pub const INVITE_KIND_CONTACT: u8 = 0;
pub const INVITE_KIND_WORKSPACE: u8 = 1;
const HRP_STR: &str = "moyuinv";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteToken {
    pub v: u8,
    pub kind: u8,
    pub inviter: [u8; 32],
    pub relays: Vec<String>,
    pub label: Option<String>,
    /// `cgka_traits::types::GroupId` bytes -- opaque and NOT fixed-32 (MDK's
    /// `byte_id!` wraps a `Vec<u8>`; OpenMLS's default `GroupId::random`
    /// generates 16 random bytes -- `openmls-0.8.1/src/group/mod.rs:73`,
    /// verified against the vendored source, not the 32-byte Nostr-pubkey
    /// size an earlier draft of this struct assumed). Length-prefixed in the
    /// wire format below so it never re-hardcodes a byte count.
    pub ws_gid: Option<Vec<u8>>,
    pub ws_name: Option<String>,
    pub secret: Option<[u8; 16]>,
}

#[derive(Debug)]
pub enum InviteError {
    Bech32(String),
    Hrp(String),
    Truncated,
    BadVersion(u8),
    BadKind(u8),
    MissingWorkspaceFields,
    Utf8,
    TooManyRelays(usize),
    StringTooLong(usize),
    TrailingData,
}

impl std::fmt::Display for InviteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InviteError::Bech32(e) => write!(f, "invite code is not valid bech32: {e}"),
            InviteError::Hrp(h) => write!(
                f,
                "not a moyu invite code (prefix `{h}`, expected `moyuinv`)"
            ),
            InviteError::Truncated => write!(f, "invite code is truncated/corrupt"),
            InviteError::BadVersion(v) => write!(f, "unsupported invite version {v}"),
            InviteError::BadKind(k) => write!(f, "unknown invite kind {k}"),
            InviteError::MissingWorkspaceFields => {
                write!(f, "workspace invite missing gid/name/secret")
            }
            InviteError::Utf8 => write!(f, "invite code has invalid UTF-8 in a string field"),
            InviteError::TooManyRelays(n) => write!(f, "too many relays ({n}, max 255)"),
            InviteError::StringTooLong(n) => {
                write!(f, "string field too long ({n} bytes, max 65535)")
            }
            InviteError::TrailingData => write!(f, "invite code has unexpected trailing data"),
        }
    }
}
impl std::error::Error for InviteError {}

// ---- TLV payload：紧凑、版本化、自描述 ----
// 布局：v(1) kind(1) inviter(32)
//       relay_count(1) [ len(2 LE) bytes ]*
//       flags(1: bit0=label bit1=ws_gid bit2=ws_name bit3=secret)
//       [label: len(2) bytes]? [ws_gid: len(2) bytes]? [ws_name: len(2) bytes]? [secret: 16]?
fn put_str(buf: &mut Vec<u8>, s: &str) {
    let b = s.as_bytes();
    buf.extend_from_slice(&(b.len() as u16).to_le_bytes());
    buf.extend_from_slice(b);
}
fn put_bytes(buf: &mut Vec<u8>, b: &[u8]) {
    buf.extend_from_slice(&(b.len() as u16).to_le_bytes());
    buf.extend_from_slice(b);
}
fn take(buf: &[u8], off: &mut usize, n: usize) -> Result<Vec<u8>, InviteError> {
    let end = off.checked_add(n).ok_or(InviteError::Truncated)?;
    if end > buf.len() {
        return Err(InviteError::Truncated);
    }
    let out = buf[*off..end].to_vec();
    *off = end;
    Ok(out)
}
fn take_str(buf: &[u8], off: &mut usize) -> Result<String, InviteError> {
    let lb = take(buf, off, 2)?;
    let len = u16::from_le_bytes([lb[0], lb[1]]) as usize;
    let b = take(buf, off, len)?;
    String::from_utf8(b).map_err(|_| InviteError::Utf8)
}
fn take_bytes(buf: &[u8], off: &mut usize) -> Result<Vec<u8>, InviteError> {
    let lb = take(buf, off, 2)?;
    let len = u16::from_le_bytes([lb[0], lb[1]]) as usize;
    take(buf, off, len)
}

fn to_payload(t: &InviteToken) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.push(t.v);
    buf.push(t.kind);
    buf.extend_from_slice(&t.inviter);
    buf.push(t.relays.len() as u8);
    for r in &t.relays {
        put_str(&mut buf, r);
    }
    let mut flags = 0u8;
    if t.label.is_some() {
        flags |= 1;
    }
    if t.ws_gid.is_some() {
        flags |= 2;
    }
    if t.ws_name.is_some() {
        flags |= 4;
    }
    if t.secret.is_some() {
        flags |= 8;
    }
    buf.push(flags);
    if let Some(l) = &t.label {
        put_str(&mut buf, l);
    }
    if let Some(g) = &t.ws_gid {
        put_bytes(&mut buf, g);
    }
    if let Some(n) = &t.ws_name {
        put_str(&mut buf, n);
    }
    if let Some(s) = &t.secret {
        buf.extend_from_slice(s);
    }
    buf
}

fn from_payload(buf: &[u8]) -> Result<InviteToken, InviteError> {
    let mut off = 0usize;
    let v = take(buf, &mut off, 1)?[0];
    if v != 1 {
        return Err(InviteError::BadVersion(v));
    }
    let kind = take(buf, &mut off, 1)?[0];
    if kind != INVITE_KIND_CONTACT && kind != INVITE_KIND_WORKSPACE {
        return Err(InviteError::BadKind(kind));
    }
    let inviter: [u8; 32] = take(buf, &mut off, 32)?
        .try_into()
        .map_err(|_| InviteError::Truncated)?;
    let n = take(buf, &mut off, 1)?[0] as usize;
    let mut relays = Vec::with_capacity(n);
    for _ in 0..n {
        relays.push(take_str(buf, &mut off)?);
    }
    let flags = take(buf, &mut off, 1)?[0];
    let label = if flags & 1 != 0 {
        Some(take_str(buf, &mut off)?)
    } else {
        None
    };
    let ws_gid = if flags & 2 != 0 {
        Some(take_bytes(buf, &mut off)?)
    } else {
        None
    };
    let ws_name = if flags & 4 != 0 {
        Some(take_str(buf, &mut off)?)
    } else {
        None
    };
    let secret = if flags & 8 != 0 {
        Some(
            take(buf, &mut off, 16)?
                .try_into()
                .map_err(|_| InviteError::Truncated)?,
        )
    } else {
        None
    };
    if off != buf.len() {
        return Err(InviteError::TrailingData);
    }
    if kind == INVITE_KIND_WORKSPACE
        && (ws_gid.as_ref().is_none_or(|g| g.is_empty()) || ws_name.is_none() || secret.is_none())
    {
        return Err(InviteError::MissingWorkspaceFields);
    }
    Ok(InviteToken {
        v,
        kind,
        inviter,
        relays,
        label,
        ws_gid,
        ws_name,
        secret,
    })
}

pub fn encode_token(t: &InviteToken) -> Result<String, InviteError> {
    if t.relays.len() > u8::MAX as usize {
        return Err(InviteError::TooManyRelays(t.relays.len()));
    }
    for s in t
        .relays
        .iter()
        .chain(t.label.iter())
        .chain(t.ws_name.iter())
    {
        if s.len() > u16::MAX as usize {
            return Err(InviteError::StringTooLong(s.len()));
        }
    }
    if let Some(g) = &t.ws_gid
        && g.len() > u16::MAX as usize
    {
        return Err(InviteError::StringTooLong(g.len()));
    }
    let hrp = Hrp::parse(HRP_STR).expect("static hrp");
    bech32::encode::<Bech32>(hrp, &to_payload(t)).map_err(|e| InviteError::Bech32(e.to_string()))
}

pub fn decode_token(s: &str) -> Result<InviteToken, InviteError> {
    let (hrp, data) = bech32::decode(s.trim()).map_err(|e| InviteError::Bech32(e.to_string()))?;
    if hrp.as_str() != HRP_STR {
        return Err(InviteError::Hrp(hrp.as_str().to_owned()));
    }
    from_payload(&data)
}

pub fn new_secret_hex() -> String {
    // 复用 identity.rs 已用的 nostr RNG 路径,零新依赖。
    // `SecretKey::to_secret_bytes() -> [u8;32]` 是稳定公开 API(identity.rs:250 已用);
    // 取前 16 字节 hex。**不要**用 to_secret_hex()(nostr 0.44 无此名,已核实)。
    let k = nostr::Keys::generate();
    let bytes = k.secret_key().to_secret_bytes();
    hex::encode(&bytes[..16])
}

// ---- join-request 信封(普通 kind-9 DM content) ----
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinRequest {
    pub ws_gid_hex: String,
    pub ws_name: String,
    pub secret_hex: String,
}

pub fn build_join_request_content(r: &JoinRequest) -> String {
    serde_json::json!({
        "v": 1,
        "moyu": { "type": "join-request", "ws_gid": r.ws_gid_hex, "ws_name": r.ws_name, "secret": r.secret_hex }
    }).to_string()
}

pub fn parse_join_request(plaintext: &str) -> Option<JoinRequest> {
    let v: serde_json::Value = serde_json::from_str(plaintext).ok()?;
    let m = v.get("moyu")?;
    if m.get("type").and_then(|t| t.as_str()) != Some("join-request") {
        return None;
    }
    Some(JoinRequest {
        ws_gid_hex: m.get("ws_gid")?.as_str()?.to_owned(),
        ws_name: m.get("ws_name")?.as_str()?.to_owned(),
        secret_hex: m.get("secret")?.as_str()?.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_ws() -> InviteToken {
        InviteToken {
            v: 1,
            kind: INVITE_KIND_WORKSPACE,
            inviter: [7u8; 32],
            relays: vec!["wss://relay.damus.io".into(), "ws://127.0.0.1:7777".into()],
            label: Some("alice".into()),
            // 16 bytes -- matches OpenMLS's actual `GroupId::random` size, not
            // the 32-byte Nostr-pubkey size (regression coverage for the
            // fixed-32 bug this length-prefixed encoding replaced).
            ws_gid: Some(vec![9u8; 16]),
            ws_name: Some("eng".into()),
            secret: Some([3u8; 16]),
        }
    }

    #[test]
    fn workspace_token_round_trips() {
        let t = sample_ws();
        let s = encode_token(&t).unwrap();
        assert!(s.starts_with("moyuinv1"));
        assert!(
            s.len() > 90,
            "code must exceed classic bech32 90-char cap ({} chars)",
            s.len()
        );
        assert_eq!(decode_token(&s).unwrap(), t);
    }

    #[test]
    fn contact_token_round_trips() {
        let t = InviteToken {
            v: 1,
            kind: INVITE_KIND_CONTACT,
            inviter: [1u8; 32],
            relays: vec!["wss://nos.lol".into()],
            label: None,
            ws_gid: None,
            ws_name: None,
            secret: None,
        };
        assert_eq!(decode_token(&encode_token(&t).unwrap()).unwrap(), t);
    }

    #[test]
    fn rejects_wrong_hrp() {
        // a valid npub bech32 is `npub1...`, wrong HRP.
        // (a throwaway key minted for this test; nobody holds its secret)
        let npub = "npub135fkg569gqfjdwj2sllxnt27fcc5nhhu4ahgfqsmkqr6jl799wespmaxwa";
        assert!(matches!(decode_token(npub), Err(InviteError::Hrp(_))));
    }

    #[test]
    fn rejects_corrupt() {
        assert!(decode_token("moyuinv1qqqq").is_err());
        assert!(decode_token("not a code").is_err());
    }

    #[test]
    fn secret_hex_is_16_bytes() {
        let s = new_secret_hex();
        assert_eq!(s.len(), 32);
        assert!(hex::decode(&s).is_ok());
        assert_ne!(new_secret_hex(), new_secret_hex());
    }

    #[test]
    fn join_request_round_trips() {
        let jr = JoinRequest {
            ws_gid_hex: "ab".repeat(32),
            ws_name: "eng".into(),
            secret_hex: "cd".repeat(16),
        };
        let content = build_join_request_content(&jr);
        assert_eq!(parse_join_request(&content), Some(jr));
    }

    #[test]
    fn encode_rejects_oversized() {
        let mut t = sample_ws();
        t.relays = vec!["wss://x".to_string(); 300];
        assert!(matches!(
            encode_token(&t),
            Err(InviteError::TooManyRelays(_))
        ));
        let mut t2 = sample_ws();
        t2.ws_name = Some("a".repeat(70_000));
        assert!(matches!(
            encode_token(&t2),
            Err(InviteError::StringTooLong(_))
        ));
        let mut t3 = sample_ws();
        t3.ws_gid = Some(vec![0u8; 70_000]);
        assert!(matches!(
            encode_token(&t3),
            Err(InviteError::StringTooLong(_))
        ));
    }

    #[test]
    fn ws_gid_length_agnostic_and_empty_rejected() {
        // non-16-byte gids round-trip fine (codec is length-agnostic)
        for len in [8usize, 24, 32] {
            let mut t = sample_ws();
            t.ws_gid = Some(vec![5u8; len]);
            let s = encode_token(&t).unwrap();
            assert_eq!(decode_token(&s).unwrap(), t);
        }
        // empty gid = missing workspace field
        let mut t = sample_ws();
        t.ws_gid = Some(Vec::new());
        let s = encode_token(&t).unwrap();
        assert!(matches!(
            decode_token(&s),
            Err(InviteError::MissingWorkspaceFields)
        ));
    }

    #[test]
    fn decode_rejects_trailing_data() {
        let t = sample_ws();
        let mut payload = to_payload(&t);
        payload.push(0xFF);
        let hrp = bech32::Hrp::parse(HRP_STR).unwrap();
        let s = bech32::encode::<bech32::Bech32>(hrp, &payload).unwrap();
        assert!(matches!(decode_token(&s), Err(InviteError::TrailingData)));
    }

    #[test]
    fn parse_join_request_ignores_plain_and_channel_envelope() {
        assert_eq!(parse_join_request("hello"), None);
        // channel envelope uses integer moyu:1, must NOT match
        assert_eq!(
            parse_join_request(r#"{"moyu":1,"ch":"general","body":"hi"}"#),
            None
        );
    }

    #[test]
    fn inviter_relay_survives_invite_round_trip() {
        // A contact invite must carry the inviter's relay set so the invitee's
        // `join` (config::merge_relays) unions it in and both land on the same
        // relay -- e.g. a self-hosted one nobody else would know about.
        let relays = vec!["wss://relay.example.com".to_string()];
        let token = InviteToken {
            v: 1,
            kind: INVITE_KIND_CONTACT,
            inviter: [7u8; 32],
            relays: relays.clone(),
            label: None,
            ws_gid: None,
            ws_name: None,
            secret: None,
        };
        let encoded = encode_token(&token).expect("encode");
        let decoded = decode_token(&encoded).expect("decode");
        assert_eq!(decoded.relays, relays);
    }
}
