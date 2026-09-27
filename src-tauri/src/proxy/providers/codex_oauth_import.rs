//! Read-only live credential capture into the existing account store.
//! This module never reads/writes CodexHome, refreshes tokens, or records a live marker.
use super::*;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExistingLoginIdentity {
    pub subject: String,
    pub workspace_id: String,
}

/// JWT decoding establishes local identity only, not server-side validity.
pub(crate) fn existing_login_identity(
    auth: &Value,
) -> Result<ExistingLoginIdentity, CodexOAuthError> {
    let invalid = || CodexOAuthError::ExistingLoginInvalid;
    let workspace_id =
        crate::codex_config::extract_codex_managed_oauth_account_id(auth).ok_or_else(invalid)?;
    if auth
        .get("OPENAI_API_KEY")
        .is_some_and(|value| !value.is_null())
    {
        return Err(invalid());
    }
    nonempty_token(auth, "refresh_token")?;
    let id_token = nonempty_token(auth, "id_token")?;
    let subject =
        crate::codex_config::extract_codex_id_token_subject(id_token).ok_or_else(invalid)?;
    for token in [id_token, nonempty_token(auth, "access_token")?] {
        if let Some(claims) = jwt_payload(token) {
            if claims
                .get("sub")
                .is_some_and(|value| value.as_str().map(str::trim) != Some(subject.as_str()))
            {
                return Err(invalid());
            }
            for value in [
                claims.get("chatgpt_account_id"),
                claims.pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id"),
            ]
            .into_iter()
            .flatten()
            .filter(|value| !value.is_null())
            {
                if value.as_str().map(str::trim) != Some(workspace_id.as_str()) {
                    return Err(invalid());
                }
            }
        }
    }
    Ok(ExistingLoginIdentity {
        subject,
        workspace_id,
    })
}

fn nonempty_token<'a>(auth: &'a Value, field: &str) -> Result<&'a str, CodexOAuthError> {
    auth.get("tokens")
        .and_then(|tokens| tokens.get(field))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or(CodexOAuthError::ExistingLoginInvalid)
}

fn jwt_payload(token: &str) -> Option<Value> {
    let parts: Vec<_> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).ok()?).ok()
}

fn account_identity(account: &CodexAccountData) -> Option<ExistingLoginIdentity> {
    Some(ExistingLoginIdentity {
        subject: crate::codex_config::extract_codex_id_token_subject(account.id_token.as_deref()?)?,
        workspace_id: account.chatgpt_account_id.clone()?,
    })
}

impl CodexOAuthManager {
    pub(crate) async fn existing_account_identity(
        &self,
        id: &str,
    ) -> Option<ExistingLoginIdentity> {
        self.accounts
            .read()
            .await
            .get(id)
            .and_then(account_identity)
    }

    pub(crate) async fn import_existing_login(
        &self,
        auth: &Value,
    ) -> Result<String, CodexOAuthError> {
        let identity = existing_login_identity(auth)?;
        let refresh_token = nonempty_token(auth, "refresh_token")?;
        let id_token = nonempty_token(auth, "id_token")?;
        let access_token = nonempty_token(auth, "access_token")?;
        let observed = match auth.get("last_refresh").filter(|value| !value.is_null()) {
            Some(value) => chrono::DateTime::parse_from_rfc3339(
                value
                    .as_str()
                    .ok_or(CodexOAuthError::ExistingLoginInvalid)?,
            )
            .map_err(|_| CodexOAuthError::ExistingLoginInvalid)?
            .timestamp_millis(),
            None => 0,
        };
        if observed < 0 || observed > chrono::Utc::now().timestamp_millis() {
            return Err(CodexOAuthError::ExistingLoginInvalid);
        }
        // Import is local and brief. The existing lifecycle write guard excludes
        // refresh, device-login commits, removal and other imports in one lock.
        let _lifecycle = self.lifecycle_lock.write().await;
        let _persist = self.storage_lock.lock().await;
        let mut accounts = self.accounts.read().await.clone();
        self.validate_import_store(&accounts)?;
        let mut matches = accounts
            .values()
            .filter(|account| account_identity(account).as_ref() == Some(&identity));
        let existing = matches.next().cloned();
        if matches.next().is_some() {
            return Err(CodexOAuthError::ExistingLoginConflict);
        }
        let account_id = existing
            .as_ref()
            .map(|account| account.account_id.clone())
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let mut data = existing.clone().unwrap_or_else(|| CodexAccountData {
            account_id: account_id.clone(),
            chatgpt_account_id: Some(identity.workspace_id),
            email: parse_jwt_claims(id_token).and_then(|claims| claims.email),
            refresh_token: refresh_token.to_string(),
            authenticated_at: chrono::Utc::now().timestamp(),
            id_token: Some(id_token.to_string()),
            token_updated_at_ms: observed,
        });
        let mut replace_cache = existing.is_none();
        if let Some(existing) = existing.as_ref() {
            let cached = self.access_tokens.read().await.get(&account_id).cloned();
            let material_changed = existing.refresh_token != refresh_token
                || existing.id_token.as_deref() != Some(id_token)
                || cached
                    .as_ref()
                    .is_some_and(|cached| cached.token != access_token);
            if material_changed {
                if existing.token_updated_at_ms <= 0
                    || observed <= 0
                    || observed == existing.token_updated_at_ms
                {
                    return Err(CodexOAuthError::ExistingLoginConflict);
                }
                if observed < existing.token_updated_at_ms {
                    return Ok(account_id); // Do not roll back a newer archived generation.
                }
                data.refresh_token = refresh_token.to_string();
                data.id_token = Some(id_token.to_string());
                data.token_updated_at_ms = observed;
                replace_cache = true;
            } else {
                data.token_updated_at_ms = data.token_updated_at_ms.max(observed);
                // Unknown generation stays unknown; importing never manufactures "now".
                replace_cache = cached.is_none();
            }
        }
        accounts.insert(account_id.clone(), data.clone());
        let default = self
            .resolve_default_account_id()
            .await
            .or_else(|| Some(account_id.clone()));
        if existing.as_ref() != Some(&data) {
            let store = CodexOAuthStore {
                version: 2,
                accounts,
                default_account_id: default.clone(),
            };
            let content = serde_json::to_string_pretty(&store).map_err(|_| {
                CodexOAuthError::ParseError("Codex account store serialization failed".into())
            })?;
            self.write_store_atomic(&content)?;
        }
        let mut accounts = self.accounts.write().await;
        accounts.insert(account_id.clone(), data.clone());
        if replace_cache {
            let mut cache = self.access_tokens.write().await;
            cache.remove(&account_id);
            // Without a truthful generation time, normal use must obtain a fresh bundle.
            if observed > 0 && data.token_updated_at_ms == observed {
                if let Some(expires_at_ms) = jwt_payload(access_token)
                    .and_then(|value| value.get("exp").and_then(Value::as_i64))
                    .and_then(|seconds| seconds.checked_mul(1000))
                    .filter(|expiry| *expiry > chrono::Utc::now().timestamp_millis())
                {
                    cache.insert(
                        account_id.clone(),
                        CachedAccessToken {
                            token: access_token.to_string(),
                            expires_at_ms,
                            obtained_at_ms: data.token_updated_at_ms,
                        },
                    );
                }
            }
        }
        drop(accounts);
        *self.default_account_id.write().await = default;
        Ok(account_id)
    }

    // new() logs load errors and leaves empty memory. Capture must not turn a
    // corrupt/unreadable or externally changed store into a successful empty import.
    fn validate_import_store(
        &self,
        accounts: &HashMap<String, CodexAccountData>,
    ) -> Result<(), CodexOAuthError> {
        let invalid = || {
            CodexOAuthError::ParseError(
                "Codex account store is invalid or changed; restart after repairing it".into(),
            )
        };
        if !self.storage_path.try_exists()? {
            return if accounts.is_empty() {
                Ok(())
            } else {
                Err(invalid())
            };
        }
        let raw: Value =
            serde_json::from_slice(&fs::read(&self.storage_path)?).map_err(|_| invalid())?;
        if !raw.get("accounts").is_some_and(Value::is_object) {
            return Err(invalid());
        }
        let store: CodexOAuthStore = serde_json::from_value(raw).map_err(|_| invalid())?;
        if !matches!(store.version, 1 | 2)
            || store.accounts != *accounts
            || store
                .accounts
                .iter()
                .any(|(id, data)| id.trim().is_empty() || id != &data.account_id)
        {
            return Err(invalid());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn jwt(value: Value) -> String {
        format!(
            "{}.{}.",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#),
            URL_SAFE_NO_PAD.encode(value.to_string())
        )
    }

    fn login(subject: &str, workspace: &str, generation: &str, date: Option<&str>) -> Value {
        let mut auth = json!({
            "OPENAI_API_KEY": null,
            "tokens": {
                "account_id": workspace,
                "id_token": jwt(json!({"sub": subject, "email": "same@example.invalid",
                    "chatgpt_account_id": workspace, "generation": generation})),
                "refresh_token": format!("refresh-{generation}"),
                "access_token": jwt(json!({"exp": 4102444800_i64, "generation": generation,
                    "https://api.openai.com/auth": {"chatgpt_account_id":workspace}})),
            },
        });
        if let Some(date) = date {
            auth["last_refresh"] = json!(date);
        }
        auth
    }

    #[tokio::test]
    async fn existing_login_is_idempotent_and_deduplicates_subject_and_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let manager = CodexOAuthManager::new(temp.path().to_path_buf());
        let auth = login("one", "workspace", "r0", Some("2026-01-01T00:00:00Z"));
        let before = auth.clone();
        let id = manager.import_existing_login(&auth).await.unwrap();
        assert_eq!(auth, before);
        assert!(Uuid::parse_str(&id).is_ok());
        assert_eq!(manager.import_existing_login(&auth).await.unwrap(), id);
        let reopened = CodexOAuthManager::new(temp.path().to_path_buf());
        assert_eq!(reopened.import_existing_login(&auth).await.unwrap(), id);
        for other in [
            login("two", "workspace", "r0", None),
            login("one", "other", "r0", None),
        ] {
            assert_ne!(reopened.import_existing_login(&other).await.unwrap(), id);
        }
        assert_eq!(reopened.accounts.read().await.len(), 3);
        let identity = reopened.existing_account_identity(&id).await.unwrap();
        assert_eq!(identity.subject, "one");
        assert_eq!(identity.workspace_id, "workspace");
        assert_eq!(
            manager.accounts.read().await[&id].token_updated_at_ms,
            1767225600000
        );
        assert_eq!(
            manager.access_tokens.read().await[&id].obtained_at_ms,
            1767225600000
        );
    }

    #[tokio::test]
    async fn existing_login_orders_complete_generations_and_preserves_newer_archive() {
        let temp = tempfile::tempdir().unwrap();
        let manager = CodexOAuthManager::new(temp.path().to_path_buf());
        let old = login("one", "workspace", "r0", Some("2026-01-01T00:00:00Z"));
        let newer = login("one", "workspace", "r1", Some("2026-01-02T00:00:00Z"));
        let id = manager.import_existing_login(&old).await.unwrap();
        assert_eq!(manager.import_existing_login(&newer).await.unwrap(), id);
        let saved = fs::read(&manager.storage_path).unwrap();
        assert_eq!(manager.import_existing_login(&old).await.unwrap(), id);
        assert_eq!(fs::read(&manager.storage_path).unwrap(), saved);
        let accounts = manager.accounts.read().await;
        assert_eq!(accounts[&id].refresh_token, "refresh-r1");
        assert_eq!(
            accounts[&id].id_token.as_deref(),
            newer["tokens"]["id_token"].as_str()
        );
        drop(accounts);
        assert_eq!(
            manager.access_tokens.read().await[&id].token,
            newer["tokens"]["access_token"].as_str().unwrap()
        );
        for date in [None, Some("2026-01-02T00:00:00Z")] {
            let conflict = login("one", "workspace", "conflict", date);
            assert!(matches!(
                manager.import_existing_login(&conflict).await,
                Err(CodexOAuthError::ExistingLoginConflict)
            ));
            assert_eq!(fs::read(&manager.storage_path).unwrap(), saved);
        }
    }

    #[tokio::test]
    async fn existing_login_unknown_time_stays_unknown_and_conflicts_remain_ambiguous() {
        let temp = tempfile::tempdir().unwrap();
        let manager = CodexOAuthManager::new(temp.path().to_path_buf());
        let auth = login("one", "workspace", "r0", None);
        let id = manager.import_existing_login(&auth).await.unwrap();
        manager.import_existing_login(&auth).await.unwrap();
        manager
            .adopt_account_refresh_token(
                &id,
                "refresh-r0".into(),
                Some(auth["tokens"]["id_token"].as_str().unwrap().into()),
                None,
            )
            .await
            .unwrap();
        assert_eq!(manager.accounts.read().await[&id].token_updated_at_ms, 0);
        assert!(manager.access_tokens.read().await.is_empty());
        let newer = login("one", "workspace", "r1", Some("2026-01-02T00:00:00Z"));
        for _ in 0..2 {
            assert!(matches!(
                manager.import_existing_login(&newer).await,
                Err(CodexOAuthError::ExistingLoginConflict)
            ));
            assert_eq!(manager.accounts.read().await[&id].token_updated_at_ms, 0);
        }
    }

    #[tokio::test]
    async fn existing_login_rejects_corrupt_store_and_failed_write_without_publication() {
        let temp = tempfile::tempdir().unwrap();
        let auth = login("one", "workspace", "r0", None);
        for raw in ["{broken", "{}", r#"{"version":3,"accounts":{}}"#] {
            let path = temp.path().join("codex_oauth_auth.json");
            fs::write(&path, raw).unwrap();
            let manager = CodexOAuthManager::new(temp.path().to_path_buf());
            assert!(manager.import_existing_login(&auth).await.is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), raw);
            assert!(manager.accounts.read().await.is_empty());
        }
        let blocked = temp.path().join("not-a-directory");
        fs::write(&blocked, "occupied").unwrap();
        let manager = CodexOAuthManager::new(blocked);
        assert!(manager.import_existing_login(&auth).await.is_err());
        assert!(manager.accounts.read().await.is_empty());
    }

    #[tokio::test]
    async fn existing_login_concurrent_imports_publish_one_account() {
        let temp = tempfile::tempdir().unwrap();
        let manager = CodexOAuthManager::new(temp.path().to_path_buf());
        let auth = login("one", "workspace", "r0", None);
        let (one, two) = tokio::join!(
            manager.import_existing_login(&auth),
            manager.import_existing_login(&auth)
        );
        assert_eq!(one.unwrap(), two.unwrap());
        assert_eq!(manager.accounts.read().await.len(), 1);
    }

    #[test]
    fn existing_login_rejects_incomplete_contradictory_and_non_chatgpt_material() {
        let auth = login("one", "workspace", "r0", None);
        for mode in [
            json!("apikey"),
            json!("chatgptAuthTokens"),
            json!("unknown"),
            json!(3),
        ] {
            let mut invalid = auth.clone();
            invalid["auth_mode"] = mode;
            assert!(existing_login_identity(&invalid).is_err());
        }
        for key in ["access_token", "refresh_token", "id_token", "account_id"] {
            let mut invalid = auth.clone();
            invalid["tokens"].as_object_mut().unwrap().remove(key);
            assert!(existing_login_identity(&invalid).is_err());
        }
        let mut invalid = auth.clone();
        invalid["tokens"]["account_id"] = json!("other");
        assert!(existing_login_identity(&invalid).is_err());
        for subject in [json!("other-user"), json!(null), json!(3)] {
            let mut invalid = auth.clone();
            invalid["tokens"]["access_token"] = json!(jwt(json!({
                "sub": subject, "chatgpt_account_id": "workspace"
            })));
            assert!(existing_login_identity(&invalid).is_err());
        }
        let mut matching = auth.clone();
        matching["tokens"]["access_token"] = json!(jwt(json!({
            "sub": "one", "chatgpt_account_id": "workspace"
        })));
        assert!(existing_login_identity(&matching).is_ok());
        for key in ["PROXY_MANAGED", "api-secret", ""] {
            let mut invalid = auth.clone();
            invalid["OPENAI_API_KEY"] = json!(key);
            assert!(existing_login_identity(&invalid).is_err());
        }
        for mode in [json!(null), json!("chatgpt")] {
            let mut valid = auth.clone();
            valid["auth_mode"] = mode;
            assert!(existing_login_identity(&valid).is_ok());
        }
    }
}
