//! Notifications for background runs.
//!
//! An unattended run that nobody sees is an unattended run nobody trusts, so
//! the agent can announce its result two ways:
//!
//! * a **desktop notification** on the machine it ran on, and
//! * a **webhook** — Telegram, Discord, Slack, or any endpoint that accepts a
//!   JSON POST — for when the machine is headless or you are not at it.
//!
//! Both are best-effort. A notification failure never changes the exit code or
//! the outcome of a run: the playlist was still written, and failing a cron job
//! because a chat server was down would be its own kind of bug.

use crate::config::{NotificationConfig, WebhookKind};
use crate::error::Result;
use serde_json::json;

/// What happened, in a form both channels can render.
#[derive(Debug, Clone)]
pub struct Notification {
    pub title: String,
    pub body: String,
    pub success: bool,
    /// Link to the playlist, when there is one.
    pub url: Option<String>,
}

impl Notification {
    pub fn success(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            success: true,
            url: None,
        }
    }

    pub fn failure(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            success: false,
            url: None,
        }
    }

    pub fn with_url(mut self, url: Option<String>) -> Self {
        self.url = url;
        self
    }

    /// One-line plain-text rendering, used by every webhook flavour.
    pub fn plain(&self) -> String {
        let mut text = format!("{}\n{}", self.title, self.body);
        if let Some(url) = &self.url {
            text.push('\n');
            text.push_str(url);
        }
        text
    }
}

/// Deliver on every enabled channel. Never fails the caller.
pub async fn send(cfg: &NotificationConfig, http: &reqwest::Client, notification: &Notification) {
    let wanted = if notification.success {
        cfg.on_success
    } else {
        cfg.on_failure
    };
    if !wanted {
        return;
    }

    if cfg.desktop {
        if let Err(e) = desktop(notification) {
            tracing::debug!(error = %e, "desktop notification unavailable");
        }
    }

    if let Err(e) = webhook(cfg, http, notification).await {
        tracing::warn!(error = %e, "webhook notification failed");
    }
}

// ---------------------------------------------------------------------------
// Desktop
// ---------------------------------------------------------------------------

/// Show a desktop notification using whatever the platform provides.
///
/// Implemented by shelling out rather than by linking a notification crate:
/// the crate pulls in D-Bus, Cocoa and WinRT bindings for a feature that is
/// entirely optional, and every one of these commands ships with the OS.
fn desktop(notification: &Notification) -> Result<()> {
    use std::process::{Command, Stdio};

    let urgency = if notification.success {
        "normal"
    } else {
        "critical"
    };
    let summary = notification.title.clone();
    let body = notification.plain();

    let mut command = if cfg!(target_os = "macos") {
        // `display notification` is the only scriptable path that does not
        // require an app bundle.
        let script = format!(
            "display notification {} with title {}",
            applescript_string(&notification.body),
            applescript_string(&summary)
        );
        let mut c = Command::new("osascript");
        c.arg("-e").arg(script);
        c
    } else if cfg!(target_os = "windows") {
        // Windows has no built-in CLI toast; PowerShell's balloon tip is the
        // dependency-free option that works without an installed app id.
        let script = format!(
            "[reflection.assembly]::LoadWithPartialName('System.Windows.Forms') | Out-Null; \
             $n = New-Object System.Windows.Forms.NotifyIcon; \
             $n.Icon = [System.Drawing.SystemIcons]::Information; \
             $n.BalloonTipTitle = {}; $n.BalloonTipText = {}; \
             $n.Visible = $true; $n.ShowBalloonTip(10000); Start-Sleep -Seconds 10; $n.Dispose()",
            powershell_string(&summary),
            powershell_string(&notification.body)
        );
        let mut c = Command::new("powershell");
        c.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
        c
    } else {
        let mut c = Command::new("notify-send");
        c.args([
            "--app-name=spotify-agent",
            &format!("--urgency={urgency}"),
            &summary,
            &body,
        ]);
        c
    };

    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| crate::error::AgentError::io("desktop notification", e))
}

/// AppleScript string literal: backslashes and quotes must be escaped.
fn applescript_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// PowerShell single-quoted string: only the quote itself needs escaping, and
/// nothing inside is interpolated — which is what makes it injection-safe.
fn powershell_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

// ---------------------------------------------------------------------------
// Webhook
// ---------------------------------------------------------------------------

async fn webhook(
    cfg: &NotificationConfig,
    http: &reqwest::Client,
    notification: &Notification,
) -> Result<()> {
    let Some(url) = &cfg.webhook_url else {
        return Ok(());
    };
    if url.is_empty() {
        return Ok(());
    }
    let url = url.expose().to_string();

    let kind = match cfg.webhook_kind {
        WebhookKind::Auto => detect(&url),
        explicit => explicit,
    };

    let body = match kind {
        WebhookKind::Telegram => {
            let Some(chat_id) = cfg.telegram_chat_id.as_ref().filter(|c| !c.is_empty()) else {
                return Err(crate::error::AgentError::config(
                    "notifications.telegram_chat_id is required for a Telegram webhook",
                ));
            };
            json!({
                "chat_id": chat_id,
                "text": notification.plain(),
                "disable_web_page_preview": false,
            })
        }
        // Discord and Slack both accept a bare `content`/`text` field.
        WebhookKind::Discord => json!({ "content": notification.plain() }),
        WebhookKind::Slack => json!({ "text": notification.plain() }),
        WebhookKind::Generic | WebhookKind::Auto => json!({
            "text": notification.plain(),
            "title": notification.title,
            "body": notification.body,
            "success": notification.success,
            "url": notification.url,
        }),
    };

    let response = http
        .post(&url)
        .timeout(std::time::Duration::from_secs(
            cfg.timeout_secs.clamp(1, 120),
        ))
        .json(&body)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        // The body may echo the bot token in an error; only the status is logged.
        return Err(crate::error::AgentError::Api {
            service: "webhook",
            status: status.as_u16(),
            message: "webhook rejected the notification".into(),
        });
    }
    Ok(())
}

/// Infer the flavour from the URL so `webhook_kind = "auto"` works unattended.
fn detect(url: &str) -> WebhookKind {
    let lowered = url.to_ascii_lowercase();
    if lowered.contains("api.telegram.org") {
        WebhookKind::Telegram
    } else if lowered.contains("discord.com") || lowered.contains("discordapp.com") {
        WebhookKind::Discord
    } else if lowered.contains("hooks.slack.com") {
        WebhookKind::Slack
    } else {
        WebhookKind::Generic
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::secret::Secret;

    #[test]
    fn detects_the_common_webhook_hosts() {
        assert_eq!(
            detect("https://api.telegram.org/bot123/sendMessage"),
            WebhookKind::Telegram
        );
        assert_eq!(
            detect("https://discord.com/api/webhooks/1/x"),
            WebhookKind::Discord
        );
        assert_eq!(
            detect("https://hooks.slack.com/services/x"),
            WebhookKind::Slack
        );
        assert_eq!(detect("https://example.com/hook"), WebhookKind::Generic);
    }

    #[test]
    fn applescript_and_powershell_strings_are_escaped() {
        assert_eq!(applescript_string("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(powershell_string("it's"), "'it''s'");
        // A quote-heavy title must not be able to terminate the literal early.
        assert!(powershell_string("'; rm -rf /; '").starts_with('\''));
    }

    #[test]
    fn plain_text_includes_the_url_when_present() {
        let n = Notification::success("Done", "30 tracks")
            .with_url(Some("https://open.spotify.com/playlist/x".into()));
        let text = n.plain();
        assert!(text.contains("Done"));
        assert!(text.contains("30 tracks"));
        assert!(text.contains("open.spotify.com"));
    }

    #[tokio::test]
    async fn telegram_without_a_chat_id_is_a_config_error() {
        let cfg = NotificationConfig {
            desktop: false,
            webhook_url: Some(Secret::new("https://api.telegram.org/bot1/sendMessage")),
            telegram_chat_id: None,
            ..Default::default()
        };
        let error = webhook(
            &cfg,
            &reqwest::Client::new(),
            &Notification::success("t", "b"),
        )
        .await
        .expect_err("missing chat id");
        assert!(error.to_string().contains("telegram_chat_id"), "{error}");
    }

    #[tokio::test]
    async fn no_webhook_configured_is_a_no_op() {
        let cfg = NotificationConfig {
            desktop: false,
            webhook_url: None,
            ..Default::default()
        };
        assert!(
            webhook(
                &cfg,
                &reqwest::Client::new(),
                &Notification::success("t", "b")
            )
            .await
            .is_ok()
        );
    }
}
