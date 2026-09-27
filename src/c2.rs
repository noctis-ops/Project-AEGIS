//! Layer 5 update: Arabic, command-only Telegram C2 microservice.

use crate::{domain::Fixed, execution::ExecutionMode, risk::BreakerLevel};
use crossbeam_channel::{unbounded, Receiver, Sender};
use serde::Deserialize;
use std::{
    collections::VecDeque,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::{sync::mpsc, time::sleep};
use tracing::warn;

#[derive(Debug, Clone, Copy)]
pub enum C2Command {
    Status,
    HaltConfirmed,
    Resume,
    SetRiskFraction(f64),
    ChangeMode(ExecutionMode),
}

#[derive(Debug, Clone)]
pub struct SystemStatus {
    pub equity: Fixed,
    pub exposure: Fixed,
    pub daily_pnl: Fixed,
    pub breaker: BreakerLevel,
    pub market_connected: bool,
    pub user_stream_connected: bool,
    pub mode: ExecutionMode,
}

impl SystemStatus {
    #[must_use]
    pub fn arabic(&self) -> String {
        format!(
            "📊 حالة AEGIS\nالرصيد: {} USDT\nالتعرض: {} USDT\nربح/خسارة اليوم: {} USDT\nقاطع الحماية: {:?}\nبيانات السوق: {}\nبيانات الحساب: {}\nالوضع: {:?}",
            self.equity,
            self.exposure,
            self.daily_pnl,
            self.breaker,
            connected(self.market_connected),
            connected(self.user_stream_connected),
            self.mode,
        )
    }
}

fn connected(value: bool) -> &'static str {
    if value {
        "متصل"
    } else {
        "منقطع"
    }
}

pub trait StatusProvider: Send + Sync {
    fn status(&self) -> SystemStatus;
}

#[derive(Clone)]
pub struct SharedStatus(Arc<RwLock<SystemStatus>>);

impl SharedStatus {
    #[must_use]
    pub fn new(status: SystemStatus) -> Self {
        Self(Arc::new(RwLock::new(status)))
    }

    pub fn update(&self, update: impl FnOnce(&mut SystemStatus)) {
        if let Ok(mut status) = self.0.write() {
            update(&mut status);
        }
    }
}

impl StatusProvider for SharedStatus {
    fn status(&self) -> SystemStatus {
        self.0.read().expect("status lock").clone()
    }
}

#[derive(Clone)]
pub struct NotificationSender(Sender<String>);

impl NotificationSender {
    /// Fire-and-forget: Telegram latency can never block trading threads.
    pub fn send(&self, arabic_message: impl Into<String>) {
        let _ = self.0.send(arabic_message.into());
    }
}

pub struct TelegramC2 {
    token: String,
    allowed_user_id: i64,
    notification_chat_id: i64,
    confirmation_code: String,
    commands: mpsc::Sender<C2Command>,
    notifications: Receiver<String>,
    status: Arc<dyn StatusProvider>,
    http: reqwest::Client,
}

impl TelegramC2 {
    pub fn new(
        token: String,
        allowed_user_id: i64,
        notification_chat_id: i64,
        confirmation_code: String,
        commands: mpsc::Sender<C2Command>,
        status: Arc<dyn StatusProvider>,
    ) -> anyhow::Result<(Self, NotificationSender)> {
        anyhow::ensure!(!token.is_empty(), "Telegram token must not be empty");
        anyhow::ensure!(
            !confirmation_code.is_empty(),
            "C2 confirmation code must be injected at runtime"
        );
        let (sender, receiver) = unbounded();
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(25))
            .build()?;
        Ok((
            Self {
                token,
                allowed_user_id,
                notification_chat_id,
                confirmation_code,
                commands,
                notifications: receiver,
                status,
                http,
            },
            NotificationSender(sender),
        ))
    }

    pub async fn run(self) -> anyhow::Result<()> {
        let notification_http = self.http.clone();
        let notification_token = self.token.clone();
        let notification_receiver = self.notifications.clone();
        let notification_chat_id = self.notification_chat_id;
        tokio::spawn(async move {
            notification_loop(
                notification_http,
                notification_token,
                notification_chat_id,
                notification_receiver,
            )
            .await;
        });

        let mut offset = 0_i64;
        let mut pending_confirmation: Option<C2Command> = None;
        loop {
            let url = format!("https://api.telegram.org/bot{}/getUpdates", self.token);
            let response = self
                .http
                .get(url)
                .query(&[("offset", offset.to_string()), ("timeout", "20".to_owned())])
                .send()
                .await;
            let updates = match response {
                Ok(response) => match response.json::<TelegramResponse<Vec<Update>>>().await {
                    Ok(response) if response.ok => response.result,
                    Ok(_) => {
                        warn!("Telegram returned an unsuccessful response");
                        sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                    Err(error) => {
                        warn!(%error, "invalid Telegram response");
                        sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                },
                Err(error) => {
                    warn!(%error, "Telegram polling failed");
                    sleep(Duration::from_secs(2)).await;
                    continue;
                }
            };
            for update in updates {
                offset = offset.max(update.update_id + 1);
                let Some(message) = update.message else {
                    continue;
                };
                let Some(from) = message.from else {
                    continue;
                };
                if from.id != self.allowed_user_id {
                    warn!(user_id = from.id, "rejected non-whitelisted C2 sender");
                    continue;
                }
                let text = message.text.unwrap_or_default();
                if pending_confirmation.is_some() && text.trim() == self.confirmation_code {
                    let command = pending_confirmation.take().expect("pending command");
                    self.commands.send(command).await?;
                    self.send_message(message.chat.id, "✅ تم تأكيد الأمر وإرساله للنظام.")
                        .await?;
                    continue;
                }
                let reply = self.handle_command(&text, &mut pending_confirmation).await;
                self.send_message(message.chat.id, &reply).await?;
            }
        }
    }

    async fn handle_command(&self, text: &str, pending: &mut Option<C2Command>) -> String {
        let parts: Vec<&str> = text.split_whitespace().collect();
        match parts.as_slice() {
            ["/status"] => {
                let _ = self.commands.send(C2Command::Status).await;
                self.status.status().arabic()
            }
            ["/halt"] => {
                *pending = Some(C2Command::HaltConfirmed);
                "⚠️ هل أنت متأكد من إيقاف النظام وإغلاق المراكز؟ أرسل رمز التأكيد اليومي.".to_owned()
            }
            ["/resume"] => {
                let _ = self.commands.send(C2Command::Resume).await;
                "🔄 تم طلب الاستئناف. لن يبدأ التداول إلا بعد فحص الصحة وفترة التبريد.".to_owned()
            }
            ["/risk", percent] => match percent.parse::<f64>() {
                Ok(percent) if percent > 0.0 && percent <= 5.0 => {
                    let fraction = percent / 100.0;
                    let _ = self
                        .commands
                        .send(C2Command::SetRiskFraction(fraction))
                        .await;
                    format!("✅ تم إرسال طلب ضبط المخاطرة إلى {percent}%.")
                }
                _ => "❌ النسبة يجب أن تكون أكبر من 0 وأقل من أو تساوي 5.".to_owned(),
            },
            ["/mode", "shadow"] => {
                let _ = self
                    .commands
                    .send(C2Command::ChangeMode(ExecutionMode::Shadow))
                    .await;
                "✅ سيتم التحويل إلى الوضع الشبحي عبر إعادة تشغيل آمنة.".to_owned()
            }
            ["/mode", "live"] => {
                *pending = Some(C2Command::ChangeMode(ExecutionMode::Live));
                "⚠️ التحويل للوضع الحقيقي يتطلب رمز التأكيد ثم إعادة تشغيل آمنة.".to_owned()
            }
            _ => "الأوامر: /status، /halt، /resume، /risk [نسبة]، /mode [shadow/live]".to_owned(),
        }
    }

    async fn send_message(&self, chat_id: i64, text: &str) -> anyhow::Result<()> {
        send_telegram(&self.http, &self.token, chat_id, text).await
    }
}

async fn notification_loop(
    http: reqwest::Client,
    token: String,
    chat_id: i64,
    receiver: Receiver<String>,
) {
    let mut retry = VecDeque::with_capacity(1_000);
    loop {
        for message in receiver.try_iter() {
            if retry.len() == 1_000 {
                retry.pop_front();
            }
            retry.push_back(message);
        }
        if let Some(message) = retry.pop_front() {
            if let Err(error) = send_telegram(&http, &token, chat_id, &message).await {
                warn!(%error, "Telegram notification failed; queued for retry");
                retry.push_front(message);
                sleep(Duration::from_secs(2)).await;
            }
        } else {
            sleep(Duration::from_millis(100)).await;
        }
    }
}

async fn send_telegram(
    http: &reqwest::Client,
    token: &str,
    chat_id: i64,
    text: &str,
) -> anyhow::Result<()> {
    let url = format!("https://api.telegram.org/bot{token}/sendMessage");
    http.post(url)
        .json(&serde_json::json!({ "chat_id": chat_id, "text": text }))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

#[derive(Debug, Deserialize)]
struct TelegramResponse<T> {
    ok: bool,
    result: T,
}

#[derive(Debug, Deserialize)]
struct Update {
    update_id: i64,
    message: Option<MessagePayload>,
}

#[derive(Debug, Deserialize)]
struct MessagePayload {
    chat: Chat,
    #[serde(rename = "from")]
    from: Option<User>,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Chat {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct User {
    id: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_is_localized() {
        let status = SystemStatus {
            equity: Fixed::from_f64(100.0),
            exposure: Fixed::ZERO,
            daily_pnl: Fixed::from_f64(1.0),
            breaker: BreakerLevel::Green,
            market_connected: true,
            user_stream_connected: false,
            mode: ExecutionMode::Shadow,
        };
        let text = status.arabic();
        assert!(text.contains("الرصيد"));
        assert!(text.contains("منقطع"));
    }
}
