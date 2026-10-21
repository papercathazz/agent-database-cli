use crate::config::{list_supported_databases, load_config, resolve_config_path};
use crate::daemon::{client::send_daemon_request, control::start_daemon};
use crate::types::{AppConfig, DaemonAction, DaemonRequest, MetadataRequest, QueryResult};
use anyhow::Result;
use serde_json::{json, Value};

pub async fn run_list(group: Option<&str>, tag: Option<&str>) -> Result<Value> {
    let config_path = resolve_config_path()?;
    let connections = load_config(Some(config_path.clone()))
        .map(|config| listed_connections(&config, group, tag))
        .unwrap_or_default();
    let configured: Vec<_> = connections.iter().map(|entry| &entry["name"]).collect();
    Ok(json!({
        "supported": list_supported_databases(),
        "configured": configured,
        "connections": connections,
        "configPath": config_path,
    }))
}

// 保留 configured 名称数组，额外返回安全的展示元数据，不暴露连接 URL 或凭据。
fn listed_connections(config: &AppConfig, group: Option<&str>, tag: Option<&str>) -> Vec<Value> {
    let mut entries: Vec<_> = config.databases.iter().collect();
    entries.sort_by(|left, right| left.0.cmp(right.0));
    entries
        .into_iter()
        .filter(|(_, db)| group.is_none_or(|value| db.group.as_deref() == Some(value)))
        .filter(|(_, db)| tag.is_none_or(|value| db.tags.iter().any(|tag| tag == value)))
        .map(|(name, db)| {
            json!({
                "name": name,
                "type": db.db_type,
                "group": db.group,
                "tags": db.tags,
            })
        })
        .collect()
}
#[cfg(test)]
mod classification_tests {
    use super::*;

    #[test]
    fn listing_filters_metadata_without_exposing_credentials() {
        let config: AppConfig = serde_json::from_value(json!({"databases": {
            "prod": {"type":"mysql","url":"mysql://user:secret@host/app","group":" 生产 ","tags":[" 应用 ","上海","应用"]},
            "test": {"type":"postgres","url":"postgres://host/app","group":"测试","tags":["应用"]},
            "old": {"type":"redis","url":"redis://localhost"}
        }})).unwrap();
        let all = listed_connections(&config, None, None);
        assert_eq!(all.len(), 3);
        assert_eq!(
            all[0],
            json!({"name":"old","type":"redis","group":null,"tags":[]})
        );
        assert!(!serde_json::to_string(&all).unwrap().contains("secret"));
        assert!(all.iter().all(|entry| entry.get("url").is_none()));
        assert_eq!(listed_connections(&config, Some("生产"), None).len(), 1);
        assert_eq!(listed_connections(&config, None, Some("应用")).len(), 2);
        assert_eq!(
            listed_connections(&config, Some("生产"), Some("上海"))[0]["name"],
            "prod"
        );
        assert!(listed_connections(&config, Some("测试"), Some("上海")).is_empty());
        assert!(listed_connections(&config, None, Some("APP")).is_empty());
    }
}

pub async fn run_test(db: &str) -> Result<Value> {
    run_via_daemon(DaemonAction::Test, Some(db), None, None).await
}

pub async fn run_execute(db: &str, command: &str) -> Result<QueryResult> {
    let data = run_via_daemon(DaemonAction::Execute, Some(db), Some(command), None).await?;
    Ok(serde_json::from_value(data)?)
}

pub async fn run_metadata(db: &str, request: MetadataRequest) -> Result<QueryResult> {
    let data = run_via_daemon(DaemonAction::Metadata, Some(db), None, Some(request)).await?;
    Ok(serde_json::from_value(data)?)
}

pub async fn run_reset(db: &str) -> Result<Value> {
    run_via_daemon(DaemonAction::Reset, Some(db), None, None).await
}

async fn run_via_daemon(
    action: DaemonAction,
    db: Option<&str>,
    command: Option<&str>,
    metadata: Option<MetadataRequest>,
) -> Result<Value> {
    let config_path = resolve_config_path()?.display().to_string();
    start_daemon().await?;
    let response = send_daemon_request(&DaemonRequest {
        action,
        db: db.map(ToString::to_string),
        command: command.map(ToString::to_string),
        metadata,
        config_path: Some(config_path),
    })
    .await?;
    if !response.ok {
        anyhow::bail!(response
            .error
            .unwrap_or_else(|| "daemon 执行失败".to_string()));
    }
    Ok(response.data.unwrap_or_else(|| json!({})))
}
