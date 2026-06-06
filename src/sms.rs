//! Phone verification (SMS). **v1 stub** — no provider is wired yet, so codes are logged to the
//! server console and `config::sms_enabled()` defaults off (which makes the phone-verification step
//! OPTIONAL: accounts finalize on email verification alone). When a provider (e.g. Twilio) is added,
//! wire the real send here behind `sms_enabled()`; the rest of the flow already routes through it.
//!
//! Runs on the tokio runtime (the REST handler awaits it after the sim reply), never the sim thread.

use crate::config::sms_enabled;

/// "Send" a 6-digit code to `to`. Until a provider is wired this only logs — but logging lets the
/// optional phone step be exercised end-to-end in dev.
pub async fn send_code(to: &str, code: &str) {
    if sms_enabled() {
        // TODO: real SMS provider (Twilio/etc). Enabled but unwired → still just logs.
        println!("[sms:STUB] would send code {code} to {to} (no SMS provider wired yet)");
    } else {
        println!("[sms:DEV] to={to} · verification code {code} \
                  (HIVE_SMS_ENABLED off → phone step is optional)");
    }
    let _ = std::io::Write::flush(&mut std::io::stdout()); // visible immediately when piped
}
