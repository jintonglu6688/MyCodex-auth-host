//! Loopback browser OAuth; account commits reuse the native login generation guards.
use super::*;
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    routing::get,
    Router,
};
use sha2::{Digest, Sha256};

type CallbackResult = Arc<std::sync::Mutex<Option<Result<String, CodexOAuthError>>>>;

pub(crate) struct BrowserLogin {
    pub id: String,
    pub url: String,
    pub expires: i64,
    verifier: String,
    redirect: String,
    callback: CallbackResult,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl BrowserLogin {
    fn stop(&mut self) {
        if let Ok(mut result) = self.callback.lock() {
            *result = Some(Err(CodexOAuthError::ExpiredToken));
        }
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

impl Drop for BrowserLogin {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Clone)]
struct CallbackState {
    nonce: String,
    host: String,
    result: CallbackResult,
}

#[derive(Deserialize)]
struct CallbackQuery {
    state: String,
    code: Option<String>,
    error: Option<String>,
}

async fn callback(
    State(flow): State<CallbackState>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> (
    StatusCode,
    [(axum::http::HeaderName, &'static str); 2],
    &'static str,
) {
    let headers_out = [
        (axum::http::header::CACHE_CONTROL, "no-store"),
        (axum::http::header::REFERRER_POLICY, "no-referrer"),
    ];
    if headers.get("host").and_then(|h| h.to_str().ok()) != Some(flow.host.as_str())
        || query.state != flow.nonce
        || query.code.is_some() == query.error.is_some()
        || query
            .code
            .as_ref()
            .is_some_and(|code| code.is_empty() || code.len() > 4096)
    {
        return (
            StatusCode::BAD_REQUEST,
            headers_out,
            "Invalid authorization response.",
        );
    }
    let Ok(mut result) = flow.result.lock() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            headers_out,
            "Authorization unavailable.",
        );
    };
    if result.is_some() {
        return (
            StatusCode::CONFLICT,
            headers_out,
            "Authorization already received.",
        );
    }
    *result = Some(query.code.ok_or(CodexOAuthError::AccessDenied));
    (StatusCode::OK, headers_out, "Authorization received. Return to MyCodex to check the sign-in result. 授权信息已收到，请返回 MyCodex 查看登录结果。")
}

impl CodexOAuthManager {
    pub(crate) async fn start_browser_flow(
        &self,
        target: Option<&str>,
    ) -> Result<BrowserLogin, CodexOAuthError> {
        let listener =
            match tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 1455)).await {
                Ok(listener) => listener,
                Err(_) => tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 1457))
                    .await
                    .map_err(|_| CodexOAuthError::IoError("browser_callback_unavailable".into()))?,
            };
        self.browser_flow_on(listener, target).await
    }

    async fn browser_flow_on(
        &self,
        listener: tokio::net::TcpListener,
        target: Option<&str>,
    ) -> Result<BrowserLogin, CodexOAuthError> {
        let (epoch, target, generation) = self.begin_login(target).await?;
        let id = format!("browser-{}", Uuid::new_v4());
        let nonce = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let verifier = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let host = format!("127.0.0.1:{}", listener.local_addr()?.port());
        let redirect = format!("http://{host}/auth/callback");
        let mut url = reqwest::Url::parse("https://auth.openai.com/oauth/authorize").unwrap();
        url.query_pairs_mut().extend_pairs([
            ("response_type", "code"),
            ("client_id", CODEX_CLIENT_ID),
            ("redirect_uri", &redirect),
            (
                "scope",
                "openid profile email offline_access api.connectors.read api.connectors.invoke",
            ),
            ("state", &nonce),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("originator", CODEX_OAUTH_ORIGINATOR),
        ]);
        let expires = chrono::Utc::now().timestamp_millis() + 900_000;
        // An opaque browser ID participates in the same cancellation and account
        // generation table as device codes. It is never sent to a device endpoint.
        self.register_pending_device_code(
            id.clone(),
            String::new(),
            expires,
            epoch,
            target,
            generation,
        )
        .await?;
        let result = Arc::new(std::sync::Mutex::new(None));
        let app = Router::new()
            .route("/auth/callback", get(callback))
            .with_state(CallbackState {
                nonce,
                host,
                result: result.clone(),
            });
        let (shutdown, receiver) = tokio::sync::oneshot::channel();
        let expiry_result = result.clone();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = tokio::time::timeout(Duration::from_secs(900), receiver).await;
                    if let Ok(mut result) = expiry_result.lock() {
                        *result = Some(Err(CodexOAuthError::ExpiredToken));
                    }
                })
                .await;
        });
        Ok(BrowserLogin {
            id,
            url: url.into(),
            expires,
            verifier,
            redirect,
            callback: result,
            shutdown: Some(shutdown),
        })
    }

    pub(crate) async fn poll_browser_login<BeforeCommit, CommitFuture, CommitGuard>(
        &self,
        flow: &mut BrowserLogin,
        before_commit: BeforeCommit,
    ) -> Result<Option<GitHubAccount>, CodexOAuthError>
    where
        BeforeCommit: FnOnce() -> CommitFuture,
        CommitFuture: std::future::Future<Output = CommitGuard>,
    {
        let entry = self
            .pending_device_codes
            .read()
            .await
            .get(&flow.id)
            .cloned()
            .ok_or(CodexOAuthError::ExpiredToken)?;
        if entry.expires_at_ms <= chrono::Utc::now().timestamp_millis() {
            return Err(CodexOAuthError::ExpiredToken);
        }
        let response = flow
            .callback
            .lock()
            .map_err(|_| CodexOAuthError::ExpiredToken)?
            .take();
        let Some(response) = response else {
            return Ok(None);
        };
        flow.stop();
        let tokens = self
            .exchange_code_for_tokens(&response?, &flow.verifier, &flow.redirect)
            .await?;
        self.commit_login_tokens(tokens, &flow.id, &entry, before_commit)
            .await
            .map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn start(manager: &CodexOAuthManager, target: Option<&str>) -> BrowserLogin {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        manager.browser_flow_on(listener, target).await.unwrap()
    }

    fn tokens(subject: &str) -> OAuthTokenResponse {
        let claims = serde_json::json!({"sub":subject,"chatgpt_account_id":"workspace","email":"test@example.invalid"});
        OAuthTokenResponse {
            access_token: "synthetic-access".into(),
            refresh_token: Some("synthetic-refresh".into()),
            id_token: Some(format!(
                "{}.{}.",
                URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#),
                URL_SAFE_NO_PAD.encode(claims.to_string())
            )),
            expires_in: Some(3600),
        }
    }

    #[tokio::test]
    async fn browser_callback_validates_state_host_and_replay_without_saving_accounts() {
        let temp = tempfile::tempdir().unwrap();
        let manager = CodexOAuthManager::new(temp.path().into());
        let mut flow = start(&manager, None).await;
        let url = reqwest::Url::parse(&flow.url).unwrap();
        let params: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(url.host_str(), Some("auth.openai.com"));
        assert_eq!(params["redirect_uri"], flow.redirect);
        assert_eq!(params["code_challenge_method"], "S256");
        assert_eq!(
            params["code_challenge"],
            URL_SAFE_NO_PAD.encode(Sha256::digest(flow.verifier.as_bytes()))
        );
        assert!(manager
            .poll_browser_login(&mut flow, || async {})
            .await
            .unwrap()
            .is_none());
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let callback_url = format!(
            "{}?state={}&code=synthetic-code",
            flow.redirect, params["state"]
        );
        assert_eq!(
            client
                .get(&callback_url)
                .header("host", "untrusted.invalid")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            client
                .get(format!("{}?state=wrong&code=synthetic-code", flow.redirect))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert!(flow.callback.lock().unwrap().is_none());
        let response = client.get(&callback_url).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert!(!response.text().await.unwrap().contains("synthetic-code"));
        assert_eq!(
            client.get(&callback_url).send().await.unwrap().status(),
            StatusCode::CONFLICT
        );
        assert!(manager.list_accounts().await.is_empty());
        drop(flow);
        tokio::time::timeout(Duration::from_secs(2), async {
            while client.get(&callback_url).send().await.is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn browser_commit_reuses_identity_generation_and_cancellation_guards() {
        let temp = tempfile::tempdir().unwrap();
        let manager = CodexOAuthManager::new(temp.path().into());
        let flow = start(&manager, None).await;
        let entry = manager.pending_device_codes.read().await[&flow.id].clone();
        let account = manager
            .commit_login_tokens(tokens("user-a"), &flow.id, &entry, || async {})
            .await
            .unwrap();
        assert_eq!(manager.list_accounts().await.len(), 1);
        assert!(!manager
            .pending_device_codes
            .read()
            .await
            .contains_key(&flow.id));
        for scenario in ["cancel", "newer", "identity", "success"] {
            let flow = start(&manager, Some(&account.id)).await;
            let entry = manager.pending_device_codes.read().await[&flow.id].clone();
            let newer = if scenario == "newer" {
                Some(start(&manager, Some(&account.id)).await)
            } else {
                None
            };
            if scenario == "cancel" {
                assert!(manager.cancel_device_flow(&flow.id).await);
            }
            let result = manager
                .commit_login_tokens(
                    tokens(if scenario == "identity" {
                        "user-b"
                    } else {
                        "user-a"
                    }),
                    &flow.id,
                    &entry,
                    || async {},
                )
                .await;
            if scenario == "success" {
                assert_eq!(result.unwrap().id, account.id);
            } else {
                assert!(result.is_err(), "{scenario} must not overwrite the account");
            }
            assert_eq!(manager.list_accounts().await.len(), 1);
            drop(newer);
        }
    }

    #[tokio::test]
    async fn browser_denial_expiry_and_cancel_do_not_exchange_or_commit_tokens() {
        let temp = tempfile::tempdir().unwrap();
        let manager = CodexOAuthManager::new(temp.path().into());
        let mut flow = start(&manager, None).await;
        *flow.callback.lock().unwrap() = Some(Err(CodexOAuthError::AccessDenied));
        assert!(matches!(
            manager.poll_browser_login(&mut flow, || async {}).await,
            Err(CodexOAuthError::AccessDenied)
        ));
        manager
            .pending_device_codes
            .write()
            .await
            .get_mut(&flow.id)
            .unwrap()
            .expires_at_ms = 0;
        assert!(matches!(
            manager.poll_browser_login(&mut flow, || async {}).await,
            Err(CodexOAuthError::ExpiredToken)
        ));
        manager.cancel_device_flow(&flow.id).await;
        assert!(matches!(
            manager.poll_browser_login(&mut flow, || async {}).await,
            Err(CodexOAuthError::ExpiredToken)
        ));
        assert!(manager.list_accounts().await.is_empty());
        assert!(!manager.storage_path.exists());
    }
}
