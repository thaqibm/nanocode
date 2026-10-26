use nanocode::{
    claude::{ClaudeHomeLogin, subscription::ClaudeSubscriptionStatus},
    codex::auth::{ChatGptLogin, chatgpt_auth_file},
};
use nanocode_tui::transcript::NoticeLevel;
use tokio::sync::mpsc::UnboundedSender;

use crate::app::{App, AppEvent};

impl App {
    pub(crate) fn login(&mut self, argument: &str) {
        let _span = tracing::info_span!(target: "nanocode", "nc.action", action.name = "login", session.id = self.agent.as_ref().map(|agent| agent.session_id())).entered();
        let tx = self.tx.clone();
        match argument.trim() {
            "codex" | "chatgpt" | "openai" => {
                tokio::spawn(codex(tx));
            }
            "claude" | "anthropic" => {
                tokio::spawn(claude(tx));
            }
            name if self.host.config.providers.contains_key(name) => {
                self.pending_login = Some(name.to_owned());
            }
            _ => {
                let providers = self
                    .host
                    .config
                    .providers
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>();
                self.notice(
                    NoticeLevel::Info,
                    format!(
                        "usage: /login {}",
                        ["codex", "claude"]
                            .into_iter()
                            .chain(providers)
                            .collect::<Vec<_>>()
                            .join("|")
                    ),
                );
            }
        }
    }
}

impl App {
    pub(crate) fn provider_login(
        &mut self,
        name: &str,
        terminal: &mut nanocode_tui::terminal::TerminalSession,
    ) {
        let config = crate::config::Config {
            providers: self.host.config.providers.clone(),
            ..crate::config::Config::default()
        };
        drop(terminal.suspend());
        let result = crate::credentials::login(&config, &self.home, name);
        drop(terminal.resume());
        match result {
            Ok(path) => {
                self.notice(
                    NoticeLevel::Info,
                    format!(
                        "saved the {name} key to {}; reloading to use it",
                        path.display()
                    ),
                );
                self.request_reload(false);
            }
            Err(error) => self.notice(NoticeLevel::Error, format!("login {name}: {error}")),
        }
    }
}

async fn codex(tx: UnboundedSender<AppEvent>) {
    let notice = |level, text| drop(tx.send(AppEvent::Notice(level, text)));
    let path = match chatgpt_auth_file() {
        Ok(path) => path,
        Err(error) => return notice(NoticeLevel::Error, format!("login codex: {error}")),
    };
    let login = match ChatGptLogin::start(path).await {
        Ok(login) => login,
        Err(error) => return notice(NoticeLevel::Error, format!("login codex: {error}")),
    };
    notice(
        NoticeLevel::Info,
        browse("ChatGPT", login.authorization_url()),
    );
    match login.complete().await {
        Ok(status) => notice(
            NoticeLevel::Info,
            format!(
                "signed in to ChatGPT as {}",
                status.email.unwrap_or(status.account_id)
            ),
        ),
        Err(error) => notice(NoticeLevel::Error, format!("login codex: {error}")),
    }
}

async fn claude(tx: UnboundedSender<AppEvent>) {
    let notice = |level, text| drop(tx.send(AppEvent::Notice(level, text)));
    let login = match ClaudeHomeLogin::start().await {
        Ok(login) => login,
        Err(error) => return notice(NoticeLevel::Error, format!("login claude: {error}")),
    };
    notice(
        NoticeLevel::Info,
        browse("Claude", login.authorization_url()),
    );
    match login.complete().await {
        Ok(ClaudeSubscriptionStatus::Authenticated { .. }) => {
            notice(NoticeLevel::Info, "signed in to Claude".to_owned());
        }
        Ok(status) => notice(
            NoticeLevel::Error,
            format!("login claude ended as {status:?}"),
        ),
        Err(error) => notice(NoticeLevel::Error, format!("login claude: {error}")),
    }
}

fn browse(service: &str, url: &str) -> String {
    if crate::extras::open_url(url) {
        format!("opened {service} sign-in in your browser; if it did not appear, visit {url}")
    } else {
        format!("visit {url} to sign in to {service}")
    }
}
