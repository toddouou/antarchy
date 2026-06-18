//! Stripe **hosted Checkout** integration for buying gems with real USD. **Dormant by default:**
//! with no `HIVE_STRIPE_SECRET_KEY` set, `api::buy_gems` short-circuits to a dev-mode response and
//! never calls this module — so the gem store UI is testable without a Stripe account. All network
//! I/O runs on the **tokio runtime** (the REST handler awaits it); the sim thread never does it.
//!
//! **Money-safety model:** pricing and gem amounts are server-authoritative — they live ONLY in
//! [`GEM_PACKS`] below; the client sends just a pack `id`. Gems are credited ONLY from a
//! signature-verified `checkout.session.completed` webhook (see [`verify_webhook`]), never from the
//! browser success-redirect, so a tampered request can't mint gems.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::config::{public_base_url, stripe_secret_key, stripe_webhook_secret, current_ms};

type HmacSha256 = Hmac<Sha256>;

/// A purchasable gem pack. `cents` is the USD price in cents (Stripe's `unit_amount`); `gems` is the
/// amount credited on a completed payment. **This table is the single source of truth for pricing.**
pub struct GemPack {
    pub id:   &'static str,
    pub gems: u64,
    pub cents: u64,
    pub name: &'static str,
}

/// The 6-tier gem ladder. Edit prices/amounts here — the client display list mirrors these ids.
pub const GEM_PACKS: &[GemPack] = &[
    GemPack { id: "pouch",   gems: 100,    cents: 99,    name: "Pouch of Gems" },
    GemPack { id: "handful", gems: 550,    cents: 499,   name: "Handful of Gems" },
    GemPack { id: "stack",   gems: 1_200,  cents: 999,   name: "Stack of Gems" },
    GemPack { id: "chest",   gems: 2_500,  cents: 1_999, name: "Chest of Gems" },
    GemPack { id: "vault",   gems: 6_500,  cents: 4_999, name: "Vault of Gems" },
    GemPack { id: "hoard",   gems: 14_000, cents: 9_999, name: "Hoard of Gems" },
];

/// Look up a pack by id (the only thing the client is trusted to send).
pub fn pack(id: &str) -> Option<&'static GemPack> {
    GEM_PACKS.iter().find(|p| p.id == id)
}

/// Create a Stripe hosted Checkout Session for `uid` buying `pack`, returning the redirect URL the
/// client should navigate to. The amount + gem count come from `pack` (never the client); `metadata`
/// carries the uid + gems so the webhook can credit the right account. Caller must have already
/// confirmed `stripe_secret_key()` is set.
pub async fn create_checkout_session(uid: u32, pack: &GemPack) -> Result<String, String> {
    let key = stripe_secret_key().ok_or("Stripe is not configured")?;
    let base = public_base_url().unwrap_or_default();

    // Stripe's API is form-encoded; nested objects use bracketed keys. Inline `price_data` avoids
    // needing pre-created Price objects in the dashboard — the price lives entirely in code.
    let product_name = format!("{} ({} gems)", pack.name, pack.gems);
    let unit_amount = pack.cents.to_string();
    let uid_s = uid.to_string();
    let gems_s = pack.gems.to_string();
    let success_url = format!("{base}/play?gems=ok");
    let cancel_url  = format!("{base}/play?gems=cancel");
    let form: Vec<(&str, &str)> = vec![
        ("mode", "payment"),
        ("line_items[0][quantity]", "1"),
        ("line_items[0][price_data][currency]", "usd"),
        ("line_items[0][price_data][unit_amount]", &unit_amount),
        ("line_items[0][price_data][product_data][name]", &product_name),
        ("client_reference_id", &uid_s),
        ("metadata[uid]", &uid_s),
        ("metadata[gems]", &gems_s),
        ("metadata[pack]", pack.id),
        ("success_url", &success_url),
        ("cancel_url", &cancel_url),
    ];

    let resp = reqwest::Client::new()
        .post("https://api.stripe.com/v1/checkout/sessions")
        .bearer_auth(key)
        .form(&form)
        .send()
        .await
        .map_err(|e| format!("Stripe request failed: {e}"))?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        eprintln!("[stripe] checkout session error {status}: {text}");
        return Err("Could not start checkout — please try again".into());
    }
    let val: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| "Bad response from Stripe".to_string())?;
    val.get("url").and_then(|u| u.as_str()).map(|s| s.to_string())
        .ok_or_else(|| "Stripe did not return a checkout URL".into())
}

/// Verify a `Stripe-Signature` header against the raw request body using the webhook signing secret,
/// returning the parsed event JSON on success. Returns `None` (reject) when the secret is unset, the
/// header is malformed, the timestamp is older than the tolerance (replay guard), or no `v1`
/// signature matches (constant-time compare). This is the webhook's ONLY authentication.
pub fn verify_webhook(payload: &[u8], sig_header: &str) -> Option<serde_json::Value> {
    let secret = stripe_webhook_secret()?;

    // Header form: `t=<unix_secs>,v1=<hex>,v1=<hex>,...` (Stripe may send more than one v1).
    let mut ts: Option<&str> = None;
    let mut v1s: Vec<&str> = Vec::new();
    for part in sig_header.split(',') {
        let mut it = part.splitn(2, '=');
        match (it.next(), it.next()) {
            (Some("t"), Some(v))  => ts = Some(v.trim()),
            (Some("v1"), Some(v)) => v1s.push(v.trim()),
            _ => {}
        }
    }
    let ts = ts?;
    if v1s.is_empty() { return None; }

    // Replay guard: reject signatures whose timestamp is more than ~5 min from now.
    let ts_secs: u64 = ts.parse().ok()?;
    let now_secs = current_ms() / 1000;
    if now_secs.abs_diff(ts_secs) > 300 { return None; }

    // signed_payload = "{t}.{raw_body}"; expected = HMAC-SHA256(secret, signed_payload).
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(ts.as_bytes());
    mac.update(b".");
    mac.update(payload);
    let expected = mac.finalize().into_bytes();

    let matched = v1s.iter().any(|v1| {
        match hex::decode(v1) {
            Ok(bytes) => bytes.len() == expected.len()
                && bool::from(bytes.as_slice().ct_eq(expected.as_slice())),
            Err(_) => false,
        }
    });
    if !matched { return None; }

    serde_json::from_slice(payload).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign(secret: &str, ts: &str, body: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(ts.as_bytes());
        mac.update(b".");
        mac.update(body.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    #[test]
    fn webhook_accepts_valid_rejects_tampered_and_stale() {
        // OnceLock caches on first read — set before any verify_webhook call. No other test reads it.
        std::env::set_var("HIVE_STRIPE_WEBHOOK_SECRET", "whsec_test_unit");
        let secret = "whsec_test_unit";
        let body = r#"{"type":"checkout.session.completed","data":{"object":{"metadata":{"uid":"7","gems":"550"}}}}"#;

        let now = (current_ms() / 1000).to_string();
        let header = format!("t={now},v1={}", sign(secret, &now, body));
        let ev = verify_webhook(body.as_bytes(), &header).expect("valid signature accepted");
        assert_eq!(ev.pointer("/data/object/metadata/gems").and_then(|v| v.as_str()), Some("550"));

        // A body the attacker edited after signing must fail (signature no longer matches).
        let tampered = body.replace("550", "9999999");
        assert!(verify_webhook(tampered.as_bytes(), &header).is_none(), "tampered body rejected");

        // Replay guard: a correctly-signed but old timestamp is rejected.
        let old = (current_ms() / 1000 - 10_000).to_string();
        let old_header = format!("t={old},v1={}", sign(secret, &old, body));
        assert!(verify_webhook(body.as_bytes(), &old_header).is_none(), "stale timestamp rejected");

        // Garbage header → reject, no panic.
        assert!(verify_webhook(body.as_bytes(), "not-a-signature").is_none());
    }

    #[test]
    fn pack_lookup_is_server_authoritative() {
        assert_eq!(pack("pouch").unwrap().gems, 100);
        assert_eq!(pack("hoard").unwrap().cents, 9_999);
        assert!(pack("free_money").is_none(), "unknown pack id is not purchasable");
    }
}
