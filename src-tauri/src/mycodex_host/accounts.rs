//! Codex-only projection of the original auth commands, without Tauri State.
use super::{lifecycle, Result};
use crate::proxy::providers::codex_oauth_auth::CodexOAuthError;
use crate::proxy::providers::copilot_auth::GitHubAccount;
use crate::{AppState, AppType};
use futures::executor::block_on;
use serde_json::{json, Value};

pub(super) fn handle(state: &AppState, method: &str, params: &Value) -> Result<Value> {
    let manager = &state.codex_oauth_manager;
    match method {
        "account/list" => {
            let status = block_on(manager.get_status());
            Ok(json!({"defaultAccountId":status.default_account_id,
                "accounts":status.accounts.iter().map(summary).collect::<Vec<_>>()}))
        }
        "account/login/start" => {
            let target = params
                .get("accountId")
                .map(|v| v.as_str().ok_or("invalid_params"))
                .transpose()?;
            let code = block_on(manager.start_device_flow(target)).map_err(auth_error)?;
            // Only the interactive login reply contains the device/user codes.
            // They are never included in diagnostics or account summaries.
            Ok(
                json!({"deviceCode":code.device_code,"userCode":code.user_code,
                "verificationUri":code.verification_uri,"expiresIn":code.expires_in,"interval":code.interval}),
            )
        }
        "account/login/poll" => {
            let code = params["deviceCode"].as_str().ok_or("invalid_params")?;
            let result = block_on(manager.poll_for_token(code, || async {
                state
                    .proxy_service
                    .lock_switch_for_app(AppType::Codex.as_str())
                    .await
            }));
            match result {
                Ok(Some(account)) => Ok(json!({"status":"authorized","account":summary(&account)})),
                Ok(None) | Err(CodexOAuthError::AuthorizationPending) => {
                    Ok(json!({"status":"pending"}))
                }
                Err(error) => Err(auth_error(error)),
            }
        }
        "account/login/cancel" => {
            let code = params["deviceCode"].as_str().ok_or("invalid_params")?;
            Ok(json!({"cancelled":block_on(manager.cancel_device_flow(code))}))
        }
        "account/remove" => {
            let id = params["accountId"].as_str().ok_or("invalid_params")?;
            let _guard = lifecycle::mutation()?;
            block_on(crate::remove_codex_oauth_account_with_switch_lock(
                state, id,
            ))
            .map_err(|_| "account_remove_failed")?;
            Ok(json!({"removed":true}))
        }
        "account/default" => {
            let id = params["accountId"].as_str().ok_or("invalid_params")?;
            let _guard = lifecycle::mutation()?;
            block_on(manager.set_default_account(id)).map_err(auth_error)?;
            Ok(json!({"defaultAccountId":id}))
        }
        _ => Err("method_not_supported"),
    }
}

fn summary(account: &GitHubAccount) -> Value {
    json!({"id":account.id,"login":account.login,"reauthRequired":account.reauth_required})
}

fn auth_error(error: CodexOAuthError) -> &'static str {
    match error {
        CodexOAuthError::AccessDenied => "login_denied",
        CodexOAuthError::ExpiredToken => "login_expired",
        CodexOAuthError::DuplicateAccount => "account_already_exists",
        CodexOAuthError::RefreshTokenInvalid => "reauth_required",
        CodexOAuthError::AccountNotFound(_) => "account_not_found",
        CodexOAuthError::AccountUnavailable(_) => "account_unavailable",
        CodexOAuthError::NetworkError(_) => "login_network_failed",
        _ => "login_failed",
    }
}
