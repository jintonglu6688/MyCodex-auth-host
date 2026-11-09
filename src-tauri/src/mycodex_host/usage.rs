//! Usage (quota) queries for saved providers other than ChatGPT accounts. The
//! settings live in the row's native `usage_script` metadata and the native
//! CC Switch queries run them; nothing here touches the live Codex config.
use super::gui::{valid_id, WriteGate};
use super::{auto_capture, gui_provider as form, runtime_paths, Host, Result};
use crate::provider::{UsageResult, UsageScript};
use crate::proxy::providers::copilot_auth::CopilotAuthManager;
use crate::proxy::providers::xai_oauth_auth::XaiOAuthManager;
use crate::services::{balance, coding_plan};
use crate::{AppState, AppType, Provider};
use futures::executor::block_on;
use serde_json::{json, Value};
use std::ops::RangeInclusive;
use std::sync::Arc;

/// Templates offered to MyCodex. GitHub Copilot, xAI and official subscription
/// quotas belong to other account systems and are refused.
const TEMPLATES: [&str; 6] = [
    "token_plan",
    "balance",
    "newapi",
    "general",
    "sub2api",
    "custom",
];

// Script templates as in CC Switch's UsageScriptModal (English fallbacks).
const GENERAL: &str = r#"({
  request: {
    url: "{{baseUrl}}/user/balance",
    method: "GET",
    headers: {
      "Authorization": "Bearer {{apiKey}}",
      "User-Agent": "cc-switch/1.0"
    }
  },
  extractor: function(response) {
    return {
      isValid: response.is_active || true,
      remaining: response.balance,
      unit: "USD"
    };
  }
})"#;

const NEW_API: &str = r#"({
  request: {
    url: "{{baseUrl}}/api/user/self",
    method: "GET",
    headers: {
      "Content-Type": "application/json",
      "Authorization": "Bearer {{accessToken}}",
      "User-Agent": "cc-switch/1.0",
      "New-Api-User": "{{userId}}"
    },
  },
  extractor: function (response) {
    if (response.success && response.data) {
      return {
        planName: response.data.group || "Default",
        remaining: response.data.quota / 500000,
        used: response.data.used_quota / 500000,
        total: (response.data.quota + response.data.used_quota) / 500000,
        unit: "USD",
      };
    }
    return {
      isValid: false,
      invalidMessage: response.message || "Query failed"
    };
  },
})"#;

const SUB2API: &str = r#"({
  request: {
    url: "{{baseUrl}}".replace(/\/+$/, "").replace(/\/v1$/, "") + "/v1/usage",
    method: "GET",
    headers: {
      "Authorization": "Bearer {{apiKey}}",
      "User-Agent": "cc-switch/1.0"
    }
  },
  extractor: function (response) {
    var quota = response.quota || {};
    var result = {
      isValid: response.isValid !== false,
      unit: response.unit || quota.unit || "USD"
    };
    if (typeof response.planName === "string") result.planName = response.planName;
    if (typeof response.remaining === "number" && response.remaining >= 0) {
      result.remaining = response.remaining;
    }
    if (quota.limit > 0) {
      result.used = quota.used;
      result.total = quota.limit;
    }
    if (!result.isValid) {
      result.invalidMessage = response.status ? String(response.status) : "Query failed";
    }
    return result;
  },
})"#;

pub(super) fn handle(host: &Host, method: &str, params: &Value) -> Result<Value> {
    let state = &host.state;
    match method {
        "gui/usage/get" => view(&usable(state, params)?),
        "gui/usage/save" => save(state, params),
        "gui/usage/query" => query(state, params),
        _ => Err("method_not_supported"),
    }
}

fn usable(state: &AppState, params: &Value) -> Result<Provider> {
    let id = params["providerId"].as_str().ok_or("invalid_params")?;
    valid_id(id)?;
    let provider = form::stored(state, id)?;
    // ChatGPT accounts report their quota through the App Server.
    if form::kind(&provider)? == "chatgpt" {
        return Err("usage_unsupported");
    }
    Ok(provider)
}

/// The row's settings when they use a template offered here.
fn script(provider: &Provider) -> Option<&UsageScript> {
    provider
        .meta
        .as_ref()
        .and_then(|m| m.usage_script.as_ref())
        .filter(|s| {
            s.template_type
                .as_deref()
                .is_some_and(|t| TEMPLATES.contains(&t))
        })
}

/// Whether the provider list should query this row's usage.
pub(super) fn enabled(provider: &Provider) -> bool {
    script(provider).is_some_and(|s| s.enabled)
}

fn present(value: &Option<String>) -> bool {
    value.as_deref().is_some_and(|v| !v.is_empty())
}

fn view(provider: &Provider) -> Result<Value> {
    let (base, _) = provider.resolve_usage_credentials(&AppType::Codex);
    let detected = if coding_plan::detect_provider_id(&base).is_some() {
        json!("token_plan")
    } else if balance::is_supported(&base) {
        json!("balance")
    } else {
        Value::Null
    };
    // Secrets are reported only as present or absent.
    let config = script(provider).map(|s| {
        json!({
            "enabled": s.enabled,
            "templateType": s.template_type,
            "baseUrl": s.base_url,
            "hasApiKey": present(&s.api_key),
            "hasAccessToken": present(&s.access_token),
            "userId": s.user_id,
            "timeout": s.timeout.unwrap_or(10),
            "autoQueryInterval": s.auto_query_interval.unwrap_or(5),
            "code": (s.template_type.as_deref() == Some("custom")).then(|| s.code.clone()),
            "zhipuTeam": s.coding_plan_provider.as_deref() == Some("zhipu_team"),
            "hasAccessKey": present(&s.access_key_id) && present(&s.secret_access_key),
            "teamOrganizationId": s.team_organization_id,
            "teamProjectId": s.team_project_id,
        })
    });
    // The coding plan the address names decides which extra credentials apply.
    let plan = coding_plan::detect_provider_id(&base);
    Ok(
        json!({"providerId":provider.id,"providerName":provider.name,"config":config,"detected":detected,"planProvider":plan}),
    )
}

fn save(state: &AppState, params: &Value) -> Result<Value> {
    // Only the row's metadata changes, so this can be stored beside routed requests.
    let gate = WriteGate::enter(state)?;
    if !gate.beside() {
        auto_capture::before_write(state)?;
    }
    let mut provider = usable(state, params)?;
    let template = params["templateType"]
        .as_str()
        .filter(|t| TEMPLATES.contains(t))
        .ok_or("invalid_params")?;
    let previous = script(&provider).cloned();
    let text = |key: &str, max: usize| -> Result<Option<String>> {
        match params.get(key).filter(|v| !v.is_null()) {
            None => Ok(None),
            Some(value) => {
                let value = value.as_str().ok_or("invalid_params")?.trim();
                if value.len() > max {
                    return Err("invalid_params");
                }
                Ok((!value.is_empty()).then(|| value.to_string()))
            }
        }
    };
    let number = |key: &str, default: u64, range: RangeInclusive<u64>| -> Result<u64> {
        match params.get(key).filter(|v| !v.is_null()) {
            None => Ok(default),
            Some(value) => value
                .as_u64()
                .filter(|n| range.contains(n))
                .ok_or("invalid_params"),
        }
    };
    // An omitted secret keeps the saved one; clearing is explicit.
    let secret = |key: &str, clear: &str, saved: Option<String>| -> Result<Option<String>> {
        if params[clear].as_bool() == Some(true) {
            return Ok(None);
        }
        Ok(text(key, 4096)?.or(saved))
    };
    let base_url = text("baseUrl", 2048)?;
    if let Some(url) = &base_url {
        let url = url::Url::parse(url).map_err(|_| "invalid_base_url")?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err("invalid_base_url");
        }
    }
    let code = match template {
        "general" => GENERAL.to_string(),
        "newapi" => NEW_API.to_string(),
        "sub2api" => SUB2API.to_string(),
        "custom" => text("code", 65536)?.ok_or("invalid_params")?,
        _ => String::new(),
    };
    let (provider_base, _) = provider.resolve_usage_credentials(&AppType::Codex);
    let kept = |pick: fn(&UsageScript) -> Option<String>| previous.as_ref().and_then(pick);
    let plan = coding_plan::detect_provider_id(&provider_base);
    // Zhipu's team plan shares the personal plan's address, so only the user can choose it.
    let team = template == "token_plan"
        && plan == Some("zhipu")
        && params["zhipuTeam"].as_bool() == Some(true);
    let script = UsageScript {
        enabled: params["enabled"].as_bool().ok_or("invalid_params")?,
        language: "javascript".into(),
        code,
        timeout: Some(number("timeout", 10, 2..=30)?),
        api_key: secret("apiKey", "clearApiKey", kept(|s| s.api_key.clone()))?,
        base_url,
        access_token: secret(
            "accessToken",
            "clearAccessToken",
            kept(|s| s.access_token.clone()),
        )?,
        user_id: text("userId", 128)?,
        template_type: Some(template.into()),
        auto_query_interval: Some(number("autoQueryInterval", 5, 0..=1440)?),
        coding_plan_provider: if team {
            Some("zhipu_team".into())
        } else {
            plan.map(str::to_string)
                .or_else(|| kept(|s| s.coding_plan_provider.clone()))
        },
        // Volcengine signs usage queries with the account AccessKey, not the inference key.
        access_key_id: secret(
            "accessKeyId",
            "clearAccessKey",
            kept(|s| s.access_key_id.clone()),
        )?,
        secret_access_key: secret(
            "secretAccessKey",
            "clearAccessKey",
            kept(|s| s.secret_access_key.clone()),
        )?,
        team_organization_id: if team {
            text("teamOrganizationId", 128)?
        } else {
            kept(|s| s.team_organization_id.clone())
        },
        team_project_id: if team {
            text("teamProjectId", 128)?
        } else {
            kept(|s| s.team_project_id.clone())
        },
    };
    provider
        .meta
        .get_or_insert_with(Default::default)
        .usage_script = Some(script);
    state
        .db
        .save_provider("codex", &provider)
        .map_err(|_| "save_outcome_unknown")?;
    view(&provider)
}

fn query(state: &AppState, params: &Value) -> Result<Value> {
    let provider = usable(state, params)?;
    let script = script(&provider)
        .filter(|s| s.enabled)
        .ok_or("usage_not_configured")?;
    // The refused templates are the only users of these account stores, so an
    // empty store in a directory that is never created is enough.
    let unused = runtime_paths()
        .ok_or("invalid_state")?
        .data_dir
        .join("usage-unused-accounts");
    let copilot = crate::commands::CopilotAuthState(Arc::new(tokio::sync::RwLock::new(
        CopilotAuthManager::new(unused.clone()),
    )));
    let xai = crate::commands::XaiOAuthState(Arc::new(tokio::sync::RwLock::new(
        XaiOAuthManager::new(unused),
    )));
    let result = block_on(crate::commands::query_provider_usage_inner(
        state,
        &copilot,
        &xai,
        AppType::Codex,
        &provider.id,
    ))
    // A transport failure is reported like any other failed query.
    .unwrap_or_else(|error| UsageResult {
        success: false,
        data: None,
        error: Some(error),
    });
    let (_, provider_key) = provider.resolve_usage_credentials(&AppType::Codex);
    let secrets = [
        Some(provider_key),
        script.api_key.clone(),
        script.access_token.clone(),
        script.access_key_id.clone(),
        script.secret_access_key.clone(),
    ];
    let mut value = serde_json::to_value(result).map_err(|_| "invalid_response")?;
    redact(&mut value, &secrets);
    Ok(value)
}

/// Upstream messages can echo a request; keep credentials and long bodies out of the reply.
fn redact(value: &mut Value, secrets: &[Option<String>]) {
    match value {
        Value::String(text) => {
            for secret in secrets.iter().flatten().filter(|s| s.len() >= 4) {
                *text = text.replace(secret.as_str(), "***");
            }
            if let Some((cut, _)) = text.char_indices().nth(500) {
                text.truncate(cut);
                text.push('…');
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| redact(item, secrets)),
        Value::Object(fields) => fields.values_mut().for_each(|item| redact(item, secrets)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_hides_every_secret_and_bounds_text() {
        let mut value =
            json!({"error":"bad key sk-secret-1 and tok-2","data":[{"extra":"x".repeat(600)}]});
        redact(
            &mut value,
            &[
                Some("sk-secret-1".into()),
                Some("tok-2".into()),
                Some("ab".into()),
                None,
            ],
        );
        assert_eq!(value["error"], "bad key *** and ***");
        assert_eq!(
            value["data"][0]["extra"].as_str().unwrap().chars().count(),
            501
        );
    }
}
