//! Transactional email via Resend. **Dormant by default:** with no `HIVE_RESEND_API_KEY` /
//! `HIVE_EMAIL_FROM` set, the code/link is logged to the server console instead of sent — so the full
//! verify / reset flow is testable in dev without a provider. All sends run on the **tokio runtime**
//! (the REST handler awaits them AFTER the sim-thread reply); the sim thread never does network I/O.

use crate::config::{email_from, public_base_url, resend_api_key};

/// Send a 6-digit verification code to `to`. Best-effort — failures are logged, not propagated (the
/// user can re-request a code).
pub async fn send_code(to: &str, code: &str) {
    let subject = "Your Antarchy verification code";
    let html = format!(
        "<div style=\"font-family:system-ui,Segoe UI,sans-serif;max-width:480px;margin:auto\">\
         <h2 style=\"color:#111\">antarchy.fun</h2>\
         <p>Your verification code is:</p>\
         <p style=\"font-size:34px;font-weight:800;letter-spacing:8px;color:#111\">{code}</p>\
         <p style=\"color:#888;font-size:13px\">It expires in 15 minutes. If you didn't request this, ignore this email.</p></div>");
    send(to, subject, &html, &format!("verification code {code}")).await;
}

/// Send a password-reset link to `to`.
pub async fn send_reset(to: &str, token: &str) {
    let base = public_base_url().unwrap_or_default();
    let link = format!("{base}/reset?token={token}");
    let subject = "Reset your Antarchy password";
    let html = format!(
        "<div style=\"font-family:system-ui,Segoe UI,sans-serif;max-width:480px;margin:auto\">\
         <h2 style=\"color:#111\">antarchy.fun</h2>\
         <p>Reset your password with the link below:</p>\
         <p><a href=\"{link}\" style=\"color:#c0392b\">{link}</a></p>\
         <p style=\"color:#888;font-size:13px\">This link expires in 1 hour. If you didn't request it, ignore this email.</p></div>");
    send(to, subject, &html, &format!("reset link {link}")).await;
}

/// Tell an existing account that someone tried to register again with their email. Sent on the
/// enumeration-safe registration path so a would-be registrant gets the same "check your email"
/// response whether or not the address is taken.
pub async fn send_register_exists_notice(to: &str) {
    let base = public_base_url().unwrap_or_default();
    let subject = "Someone tried to sign up with your email";
    let html = format!(
        "<div style=\"font-family:system-ui,Segoe UI,sans-serif;max-width:480px;margin:auto\">\
         <h2 style=\"color:#111\">antarchy.fun</h2>\
         <p>Someone just tried to create a new account with this email, but you already have one.</p>\
         <p>If it was you, just <a href=\"{base}/\" style=\"color:#c0392b\">log in</a> (or reset your \
         password). If not, you can safely ignore this email.</p></div>");
    send(to, subject, &html, "register-exists notice").await;
}

async fn send(to: &str, subject: &str, html: &str, dev_desc: &str) {
    let (Some(key), Some(from)) = (resend_api_key(), email_from()) else {
        println!("[email:DEV] to={to} · {subject} · {dev_desc}  \
                  (set HIVE_RESEND_API_KEY + HIVE_EMAIL_FROM to actually send)");
        let _ = std::io::Write::flush(&mut std::io::stdout()); // visible immediately when piped
        return;
    };
    let body = serde_json::json!({ "from": from, "to": [to], "subject": subject, "html": html });
    match reqwest::Client::new()
        .post("https://api.resend.com/emails")
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => {
            let status = resp.status();
            let txt = resp.text().await.unwrap_or_default();
            eprintln!("[email] Resend error {status}: {txt}");
        }
        Err(e) => eprintln!("[email] send to {to} failed: {e}"),
    }
}
