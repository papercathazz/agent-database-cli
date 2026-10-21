use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DatabaseType {
    Mysql,
    Postgres,
    Redis,
    Oracle,
    Mongodb,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    Json,
    Table,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OracleDriver {
    Oracle,
    Sqlcl,
    #[serde(alias = "oracledb")]
    Oracledb,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseConfig {
    #[serde(rename = "type")]
    pub db_type: DatabaseType,
    #[serde(
        default,
        deserialize_with = "deserialize_group",
        skip_serializing_if = "Option::is_none"
    )]
    pub group: Option<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_tags",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub tags: Vec<String>,
    pub url: String,
    pub password_ref: Option<String>,
    pub redis_cluster: Option<RedisClusterConfig>,
    pub ssh_tunnel: Option<SshTunnelConfig>,
    pub database: Option<String>,
    pub oracle_driver: Option<OracleDriver>,
    pub sqlcl_path: Option<String>,
    pub java_home: Option<String>,
    pub readonly: Option<bool>,
    pub blacklist: Option<Vec<String>>,
    pub keep_alive_seconds: Option<u64>,
}

// 分类元数据在反序列化时归一化；旧配置缺失或 null 字段时保持兼容。
fn deserialize_group<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Ok(Option::<String>::deserialize(deserializer)?
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty()))
}

fn deserialize_tags<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    let mut seen = HashSet::new();
    Ok(Option::<Vec<String>>::deserialize(deserializer)?
        .unwrap_or_default()
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty() && seen.insert(value.clone()))
        .collect())
}
#[cfg(test)]
mod classification_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn metadata_is_optional_normalized_and_type_checked() {
        let base = json!({ "type": "mysql", "url": "mysql://localhost/app" });
        let legacy: DatabaseConfig = serde_json::from_value(base.clone()).unwrap();
        assert!(legacy.group.is_none());
        assert!(legacy.tags.is_empty());
        let mut value = base.clone();
        value["group"] = json!(" 生产环境 ");
        value["tags"] = json!([" 应用 ", "应用", "", "上海", "app", "App"]);
        let db: DatabaseConfig = serde_json::from_value(value).unwrap();
        assert_eq!(db.group.as_deref(), Some("生产环境"));
        assert_eq!(db.tags, ["应用", "上海", "app", "App"]);
        for empty in [json!(null), json!([])] {
            let mut value = base.clone();
            value["group"] = json!(" ");
            value["tags"] = empty;
            let db: DatabaseConfig = serde_json::from_value(value).unwrap();
            assert!(db.group.is_none());
            assert!(db.tags.is_empty());
        }
        for (key, invalid) in [
            ("group", json!([])),
            ("tags", json!("应用")),
            ("tags", json!([1])),
        ] {
            let mut value = base.clone();
            value[key] = invalid;
            assert!(serde_json::from_value::<DatabaseConfig>(value).is_err());
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RedisClusterConfig {
    pub nodes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_address_map: Option<HashMap<String, RedisNodeAddress>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RedisNodeAddress {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SshTunnelConfig {
    pub host: String,
    pub port: Option<u16>,
    pub username: String,
    pub password: Option<String>,
    pub password_ref: Option<String>,
    pub private_key_path: Option<String>,
    pub private_key: Option<String>,
    pub passphrase: Option<String>,
    pub passphrase_ref: Option<String>,
    pub ready_timeout: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppConfig {
    pub databases: HashMap<String, DatabaseConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MetadataRequest {
    #[serde(rename = "type")]
    pub request_type: MetadataType,
    pub table: Option<String>,
    pub pattern: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MetadataType {
    Tables,
    Columns,
    Collections,
    Keys,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryResult {
    pub rows: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fields: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row_count: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DaemonAction {
    Test,
    Execute,
    Metadata,
    Reset,
    Status,
    Stop,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonRequest {
    pub action: DaemonAction,
    pub db: Option<String>,
    pub command: Option<String>,
    pub metadata: Option<MetadataRequest>,
    pub config_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DaemonResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
