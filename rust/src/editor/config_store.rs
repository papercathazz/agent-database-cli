use crate::config::validate_config;
use crate::editor::http::ApiFailure;
use crate::editor::now_millis;
use crate::secrets::decrypt_secret;
use crate::security::recommended_blacklists;
use crate::types::AppConfig;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

/// 配置文件不存在时页面展示的初始内容。
pub const EMPTY_CONFIG_RAW: &str = "{\n  \"databases\": {}\n}\n";

/// 读取配置文件原文交给页面。
///
/// 只做结构校验，不触发 `load_config` 的明文密文化迁移，
/// 避免页面只是打开就把磁盘内容改掉。
pub fn load_config_payload(path: &Path) -> std::result::Result<Value, ApiFailure> {
    let display = path.display().to_string();
    if !path.exists() {
        let data: Value =
            serde_json::from_str(EMPTY_CONFIG_RAW).expect("内置空配置模板必须是合法 JSON");
        return Ok(json!({
            "path": display,
            "exists": false,
            "raw": EMPTY_CONFIG_RAW,
            "data": data,
            "issues": [],
            "recommendedBlacklist": recommended_blacklists(),
        }));
    }
    let raw = fs::read_to_string(path)
        .map_err(|error| ApiFailure::internal(format!("读取配置文件失败: {error}")))?;
    match serde_json::from_str::<Value>(&raw) {
        Ok(data) => Ok(json!({
            "path": display,
            "exists": true,
            "raw": raw,
            "data": data,
            "issues": validate_raw_config(&raw),
            "recommendedBlacklist": recommended_blacklists(),
        })),
        Err(error) => Ok(json!({
            "path": display,
            "exists": true,
            "raw": raw,
            "data": Value::Null,
            "issues": [format!("配置文件不是合法 JSON: {error}")],
            "recommendedBlacklist": recommended_blacklists(),
        })),
    }
}

/// 保存配置文件：结构校验通过后先备份再原子替换。
pub fn save_config_payload(path: &Path, body: &Value) -> std::result::Result<Value, ApiFailure> {
    let raw = body
        .get("raw")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiFailure::bad_request("请求体缺少 raw 字符串字段"))?;
    let config: AppConfig = serde_json::from_str(raw)
        .map_err(|error| ApiFailure::bad_request(format!("配置结构不合法: {error}")))?;
    validate_config(&config).map_err(|error| ApiFailure::bad_request(error.to_string()))?;
    let backup = write_config_file(path, raw)
        .map_err(|error| ApiFailure::internal(format!("写入配置文件失败: {error}")))?;
    Ok(json!({
        "saved": true,
        "path": path.display().to_string(),
        "bytes": raw.len(),
        "backup": backup.map(|path| path.display().to_string()),
        "savedAt": now_millis(),
    }))
}

/// 按需解密配置里的密文，只允许读取当前配置真正引用过的 ref，
/// 避免这个接口变成任意密文读取入口。
pub fn load_secret_payload(
    config_path: &Path,
    body: &Value,
) -> std::result::Result<Value, ApiFailure> {
    let reference = body
        .get("ref")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiFailure::bad_request("请求体缺少 ref 字符串字段"))?;
    if !references_secret(config_path, reference) {
        return Err(ApiFailure::bad_request("配置里不存在该密钥引用"));
    }
    let value = decrypt_secret(config_path, reference)
        .map_err(|error| ApiFailure::bad_request(error.to_string()))?;
    Ok(json!({ "ref": reference, "value": value }))
}

fn references_secret(config_path: &Path, reference: &str) -> bool {
    let Ok(raw) = fs::read_to_string(config_path) else {
        return false;
    };
    let Ok(root) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };
    let Some(databases) = root.get("databases").and_then(Value::as_object) else {
        return false;
    };
    databases.values().any(|database| {
        if database.get("passwordRef").and_then(Value::as_str) == Some(reference) {
            return true;
        }
        let Some(tunnel) = database.get("sshTunnel") else {
            return false;
        };
        tunnel.get("passwordRef").and_then(Value::as_str) == Some(reference)
            || tunnel.get("passphraseRef").and_then(Value::as_str) == Some(reference)
    })
}

pub fn validate_raw_config(raw: &str) -> Vec<String> {
    match serde_json::from_str::<AppConfig>(raw) {
        Ok(config) => match validate_config(&config) {
            Ok(()) => Vec::new(),
            Err(error) => vec![error.to_string()],
        },
        Err(error) => vec![format!("配置结构不合法: {error}")],
    }
}

fn write_config_file(path: &Path, raw: &str) -> Result<Option<PathBuf>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建配置目录失败: {}", parent.display()))?;
    }
    // 覆盖已有文件时保留原权限，避免把收紧过的配置文件放宽成默认权限
    let existing_permissions = fs::metadata(path).ok().map(|meta| meta.permissions());
    let backup = if path.exists() {
        let backup = sibling_path(path, ".bak");
        fs::copy(path, &backup)
            .with_context(|| format!("备份配置文件失败: {}", backup.display()))?;
        Some(backup)
    } else {
        None
    };
    let tmp = sibling_path(path, ".tmp");
    fs::write(&tmp, raw.as_bytes())
        .with_context(|| format!("写入临时文件失败: {}", tmp.display()))?;
    if let Some(permissions) = existing_permissions {
        let _ = fs::set_permissions(&tmp, permissions);
    }
    fs::rename(&tmp, path).with_context(|| format!("替换配置文件失败: {}", path.display()))?;
    Ok(backup)
}

fn sibling_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::TempDir;
    use crate::secrets::encrypt_secret;

    /// 用例里的临时目录都走这个守卫，跑完自动删掉
    fn temp_dir(name: &str) -> TempDir {
        TempDir::new(&format!("editor-{name}"))
    }

    #[test]
    fn classification_roundtrip_preserves_secret_refs_and_rejects_wrong_types() {
        let dir = temp_dir("classification");
        let path = dir.path().join("config.json");
        let mut data = json!({"databases":{"local":{
            "type":"mysql","url":"mysql://user@localhost/app",
            "passwordRef":"agentdbcli:local:url","group":"生产环境","tags":["应用","上海"],
            "readonly":true,"blacklist":["drop"]
        }}});
        save_config_payload(&path, &json!({"raw":data.to_string()})).unwrap();
        assert_eq!(load_config_payload(&path).unwrap()["data"], data);
        data["databases"]["local"]["group"] = json!("测试环境");
        data["databases"]["local"]["tags"] = json!(["应用"]);
        save_config_payload(&path, &json!({"raw":data.to_string()})).unwrap();
        assert_eq!(load_config_payload(&path).unwrap()["data"], data);
        let saved = fs::read_to_string(&path).unwrap();
        data["databases"]["local"]["tags"] = json!([1]);
        assert!(save_config_payload(&path, &json!({"raw":data.to_string()})).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), saved);
        data["databases"]["local"]
            .as_object_mut()
            .unwrap()
            .remove("group");
        data["databases"]["local"]
            .as_object_mut()
            .unwrap()
            .remove("tags");
        save_config_payload(&path, &json!({"raw":data.to_string()})).unwrap();
        assert_eq!(load_config_payload(&path).unwrap()["data"], data);
    }

    #[test]
    fn secret_payload_only_serves_referenced_refs() {
        let dir = temp_dir("secret");
        let path = dir.path().join("config.json");
        fs::write(
            &path,
            "{\"databases\":{\"local\":{\"type\":\"mysql\",\"url\":\"mysql://user:@127.0.0.1:3306/app\",\"passwordRef\":\"agentdbcli:local:url\"}}}",
        )
        .expect("写入配置失败");

        let unknown = load_secret_payload(&path, &json!({ "ref": "agentdbcli:other:url" }))
            .expect_err("未被引用的 ref 必须被拒绝");
        assert_eq!(unknown.status, 400);

        let missing = load_secret_payload(&path, &json!({ "ref": "agentdbcli:local:url" }))
            .expect_err("没有密文时必须失败");
        assert_eq!(missing.status, 400);

        encrypt_secret(&path, "agentdbcli:local:url", "s3cret").expect("加密失败");
        let payload = load_secret_payload(&path, &json!({ "ref": "agentdbcli:local:url" }))
            .expect("解密失败");
        assert_eq!(payload["value"], json!("s3cret"));
    }

    #[test]
    fn missing_config_returns_empty_template() {
        let path = temp_dir("missing").path().join("config.json");
        let payload = load_config_payload(&path).expect("读取配置失败");
        assert_eq!(payload["exists"], json!(false));
        assert_eq!(payload["data"]["databases"], json!({}));
        assert_eq!(payload["issues"], json!([]));
    }

    #[test]
    fn load_config_payload_keeps_raw_text_untouched() {
        let dir = temp_dir("raw");
        let path = dir.path().join("config.json");
        let raw = "{\n  \"databases\": {\n    \"local\": {\n      \"type\": \"mysql\",\n      \"url\": \"mysql://user:@127.0.0.1:3306/app\",\n      \"passwordRef\": \"agentdbcli:local:url\"\n    }\n  }\n}\n";
        fs::write(&path, raw).expect("写入配置失败");

        let payload = load_config_payload(&path).expect("读取配置失败");
        // 页面只读原文，不得解密或改写 passwordRef
        assert_eq!(payload["raw"], json!(raw));
        assert_eq!(payload["issues"], json!([]));
    }

    #[test]
    fn save_config_writes_file_and_keeps_backup() {
        let dir = temp_dir("save");
        let path = dir.path().join("config.json");
        fs::write(&path, "{\n  \"databases\": {}\n}\n").expect("写入初始配置失败");
        let body = json!({ "raw": "{\n  \"databases\": {\n    \"local\": {\n      \"type\": \"mysql\",\n      \"url\": \"mysql://user@127.0.0.1:3306/app\"\n    }\n  }\n}\n" });

        let payload = save_config_payload(&path, &body).expect("保存配置失败");
        assert_eq!(payload["saved"], json!(true));
        assert_eq!(
            payload["backup"],
            json!(path.with_file_name("config.json.bak").display().to_string())
        );
        assert!(fs::read_to_string(&path)
            .expect("读取新配置失败")
            .contains("127.0.0.1:3306"));
        assert!(path.with_file_name("config.json.bak").is_file());
        assert!(!path.with_file_name("config.json.tmp").exists());
    }

    #[test]
    fn save_config_rejects_invalid_payload() {
        let dir = temp_dir("reject");
        let path = dir.path().join("config.json");
        assert!(save_config_payload(&path, &json!({ "raw": "{ not json" })).is_err());

        let missing_url = json!({ "raw": "{\"databases\":{\"local\":{\"type\":\"mysql\"}}}" });
        let failure = save_config_payload(&path, &missing_url).expect_err("缺少 url 必须被拒绝");
        assert_eq!(failure.status, 400);
        assert!(failure.message.contains("url"));

        let empty_cluster = json!({ "raw": "{\"databases\":{\"local\":{\"type\":\"redis\",\"url\":\"redis://127.0.0.1:6379\",\"redisCluster\":{\"nodes\":[]}}}}" });
        let failure = save_config_payload(&path, &empty_cluster).expect_err("空集群节点必须被拒绝");
        assert_eq!(failure.status, 400);
        // 校验失败时不能改动磁盘内容
        assert!(!path.exists());
    }

    #[test]
    fn validate_raw_config_reports_config_rule_errors() {
        let issues = validate_raw_config("{\"databases\":{\"cache\":{\"type\":\"redis\",\"url\":\"redis://127.0.0.1:6379\",\"keepAliveSeconds\":0}}}");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("keepAliveSeconds"));
        assert!(validate_raw_config("{\"databases\":{}}").is_empty());
    }
}
