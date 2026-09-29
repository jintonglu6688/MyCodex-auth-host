//! GUI login IDs refer to native OAuth flows; credentials stay in the manager.
use super::{accounts, gui::valid_id, Host, Result};
use chrono::{TimeZone, Utc};
use futures::executor::block_on;
use serde_json::{json, Value};

pub(super) struct Login {
    id: String,
    target: Option<String>,
    device: String,
    browser: Option<crate::proxy::providers::codex_oauth_auth::BrowserLogin>,
    method: String,
    user_code: String,
    expires: i64,
    next_poll: i64,
    interval: i64,
    started: i64,
    status: &'static str,
    error: Option<&'static str>,
    account: Option<Value>,
}

impl Login {
    fn summary(&self) -> Value {
        json!({"loginId":self.id,"targetAccountId":self.target,"status":self.status,
            "userCode":if self.status=="pending" {&self.user_code} else {""},
            "method":self.method,
            "verificationUri":if self.method == "browser" { self.browser.as_ref().map(|b| b.url.as_str()).unwrap_or("") } else { "https://auth.openai.com/codex/device" },
            "expiresAt":Utc.timestamp_millis_opt(self.expires).single().map(|t|t.to_rfc3339()),
            "error":self.error,"account":self.account})
    }
    fn finish(&mut self, status: &'static str, error: Option<&'static str>) {
        self.status = status;
        self.error = error;
        self.browser = None;
        self.device.clear();
        self.user_code.clear();
    }
}

fn account_summary(host: &Host, account: &Value) -> Value {
    let manager = &host.state.codex_oauth_manager;
    let id = account["id"].as_str().unwrap_or("");
    json!({"id":id,"email":account["login"],"label":account["login"],
        "workspaceId":block_on(manager.chatgpt_account_id_for_account(id)).ok(),
        "isDefault":block_on(manager.default_account_id()).as_deref()==Some(id),
        "needsReauthentication":account["reauthRequired"]==true})
}

pub(super) fn login_pending(host: &Host) -> Result<bool> {
    Ok(host
        .gui
        .lock()
        .map_err(|_| "session_failed")?
        .logins
        .values()
        .any(|login| login.status == "pending" && login.expires > Utc::now().timestamp_millis()))
}

pub(super) fn handle(host: &Host, method: &str, params: &Value) -> Result<Value> {
    let state = &host.state;
    match method {
        "gui/account/list" => {
            let accounts = accounts::handle(state, "account/list", &json!({}))?;
            let session = host.gui.lock().map_err(|_| "session_failed")?;
            let active = session
                .logins
                .values()
                .filter(|l| l.status == "pending")
                .max_by_key(|l| l.started);
            let last = session.logins.values().max_by_key(|l| l.started);
            Ok(
                json!({"browserLoginAvailable":cfg!(windows),"accounts":accounts["accounts"].as_array().ok_or("invalid_response")?.iter().map(|a|account_summary(host,a)).collect::<Vec<_>>(),
                "activeLogin":active.map(Login::summary),"lastLogin":last.map(Login::summary)}),
            )
        }
        "gui/account/delete" => accounts::handle(state, "account/remove", params),
        "gui/account/login/start" => {
            let id = params["loginId"].as_str().ok_or("invalid_params")?;
            valid_id(id)?;
            let target = params
                .get("targetAccountId")
                .map(|v| v.as_str().ok_or("invalid_params"))
                .transpose()?
                .map(str::to_owned);
            let method = params["method"].as_str().unwrap_or("device");
            if method != "device" && method != "browser" {
                return Err("invalid_params");
            }
            if method == "browser" && !cfg!(windows) {
                return Err("unsupported_target");
            }
            let mut session = host.gui.lock().map_err(|_| "session_failed")?;
            if let Some(login) = session.logins.get(id) {
                if login.target != target || login.method != method {
                    return Err("login_id_conflict");
                }
                return Ok(login.summary());
            }
            if session.logins.len() >= 32 {
                let oldest = session
                    .logins
                    .iter()
                    .filter(|(_, l)| {
                        l.status != "starting"
                            && (l.status != "pending" || l.expires <= Utc::now().timestamp_millis())
                    })
                    .min_by_key(|(_, l)| l.started)
                    .map(|(id, _)| id.clone());
                if let Some(id) = oldest {
                    session.logins.remove(&id);
                } else {
                    return Err("login_capacity_reached");
                }
            }
            let now = Utc::now().timestamp_millis();
            let mut login = Login {
                id: id.into(),
                target: target.clone(),
                device: String::new(),
                browser: None,
                method: method.into(),
                user_code: String::new(),
                expires: now,
                next_poll: now,
                interval: 5000,
                started: now,
                status: "starting",
                error: None,
                account: None,
            };
            let mut native = json!({});
            if let Some(target) = target {
                native["accountId"] = json!(target);
            }
            if method == "browser" {
                match block_on(
                    state
                        .codex_oauth_manager
                        .start_browser_flow(login.target.as_deref()),
                ) {
                    Ok(flow) => {
                        login.device = flow.id.clone();
                        login.expires = flow.expires;
                        login.browser = Some(flow);
                        login.interval = 1000;
                        login.status = "pending";
                    }
                    Err(error) => login.finish("failed", Some(accounts::auth_error(error))),
                }
            } else {
                match accounts::handle(state, "account/login/start", &native) {
                    Ok(code) => {
                        login.device = code["deviceCode"]
                            .as_str()
                            .ok_or("invalid_response")?
                            .into();
                        login.user_code =
                            code["userCode"].as_str().ok_or("invalid_response")?.into();
                        login.expires =
                            now + code["expiresIn"].as_i64().unwrap_or(900).clamp(1, 86400) * 1000;
                        login.interval = code["interval"].as_i64().unwrap_or(5).clamp(1, 60) * 1000;
                        login.next_poll = now + login.interval;
                        login.status = "pending";
                    }
                    Err(code) => login.finish("failed", Some(code)),
                }
            }
            let response = login.summary();
            session.logins.insert(id.into(), login);
            Ok(response)
        }
        "gui/account/login/get" | "gui/account/login/cancel" => {
            let id = params["loginId"].as_str().ok_or("invalid_params")?;
            valid_id(id)?;
            let mut session = host.gui.lock().map_err(|_| "session_failed")?;
            let login = session.logins.get_mut(id).ok_or("login_not_found")?;
            if login.status != "pending" {
                return Ok(login.summary());
            }
            let native = json!({"deviceCode":login.device});
            if method == "gui/account/login/cancel" {
                accounts::handle(state, "account/login/cancel", &native)?;
                login.finish("cancelled", None);
            } else if Utc::now().timestamp_millis() >= login.expires {
                let _ = accounts::handle(state, "account/login/cancel", &native);
                login.finish("expired", Some("login_expired"));
            } else if Utc::now().timestamp_millis() >= login.next_poll {
                login.next_poll = Utc::now().timestamp_millis() + login.interval;
                let result = if let Some(browser) = login.browser.as_mut() {
                    block_on(
                        state
                            .codex_oauth_manager
                            .poll_browser_login(browser, || async {
                                state.proxy_service.lock_switch_for_app("codex").await
                            }),
                    )
                    .map(|account| match account {
                        Some(account) => {
                            json!({"status":"authorized","account":accounts::summary(&account)})
                        }
                        None => json!({"status":"pending"}),
                    })
                    .map_err(accounts::auth_error)
                } else {
                    accounts::handle(state, "account/login/poll", &native)
                };
                match result {
                    Ok(value) if value["status"] == "authorized" => {
                        login.account = Some(account_summary(host, &value["account"]));
                        login.finish("completed", None);
                    }
                    Ok(_) => (),
                    Err(code) => {
                        let _ = accounts::handle(state, "account/login/cancel", &native);
                        login.finish(
                            if code == "login_expired" {
                                "expired"
                            } else {
                                "failed"
                            },
                            Some(code),
                        );
                    }
                }
            }
            Ok(login.summary())
        }
        _ => Err("method_not_supported"),
    }
}
