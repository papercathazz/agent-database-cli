use crate::types::{DatabaseConfig, DatabaseType};
use anyhow::Result;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::collections::HashSet;

static SQL_READ_COMMANDS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    ["select", "show", "describe", "desc", "explain", "with"]
        .into_iter()
        .collect()
});
static SQL_WRITE_COMMANDS: &[&str] = &[
    "insert", "update", "delete", "merge", "replace", "drop", "truncate", "alter", "create",
    "grant", "revoke", "call", "copy", "load", "vacuum", "analyze",
];
static SQL_WRITE_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    [
        // PostgreSQL SELECT INTO 会创建新表，首词仍是 select，不能按普通查询放行。
        r"(?i)(^|[^\p{L}\p{N}_$])select\s+.+\s+into\s+[^\s(]",
        // CTE 后接写操作时首词是 with，也需要按写操作拒绝。
        r"(?i)(^|[^\p{L}\p{N}_$])with\s+.+\)\s*(insert|update|delete|merge)\b",
    ]
    .into_iter()
    .map(|pattern| Regex::new(pattern).expect("SQL 写入模式正则必须合法"))
    .collect()
});
static REDIS_READ_COMMANDS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
        "get",
        "ping",
        "mget",
        "exists",
        "ttl",
        "pttl",
        "type",
        "strlen",
        "scan",
        "hget",
        "hgetall",
        "hmget",
        "hexists",
        "hlen",
        "hkeys",
        "hvals",
        "lrange",
        "llen",
        "lindex",
        "smembers",
        "scard",
        "sismember",
        "zrange",
        "zrevrange",
        "zcard",
        "zscore",
    ]
    .into_iter()
    .collect()
});
static MONGO_READ_COMMANDS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
        "find",
        "findone",
        "aggregate",
        "count",
        "countdocuments",
        "estimateddocumentcount",
        "distinct",
    ]
    .into_iter()
    .collect()
});
static MONGO_AGGREGATE_WRITE_STAGES: Lazy<HashSet<&'static str>> =
    Lazy::new(|| ["$out", "$merge"].into_iter().collect());

/// 编辑器黑名单输入框的推荐补全项。
///
/// 这是「值得拉黑的高危命令」推荐清单，不是完整枚举：真正决定命令能否执行的判定在
/// `assert_command_allowed` 与 `is_read_only_command`；Redis / MongoDB 采用「只读白名单，
/// 其余视为写」的判定方式，本来也无法穷举，所以推荐项与判定表用途不同、允许不一致。
/// SQL 侧这里取判定表 `SQL_WRITE_COMMANDS` 的并集，避免漏掉已知写命令。
static RECOMMENDED_BLACKLIST_SQL: &[&str] = &[
    "drop", "truncate", "delete", "update", "insert", "merge", "alter", "create", "replace",
    "grant", "revoke", "call", "copy", "load", "vacuum", "analyze",
];
static RECOMMENDED_BLACKLIST_REDIS: &[&str] = &[
    "flushall", "flushdb", "del", "unlink", "set", "mset", "expire", "rename", "hset", "lpush",
    "rpush", "sadd", "zadd", "keys",
];
static RECOMMENDED_BLACKLIST_MONGODB: &[&str] = &[
    "insertOne",
    "insertMany",
    "updateOne",
    "updateMany",
    "replaceOne",
    "deleteOne",
    "deleteMany",
    "findAndModify",
    "findOneAndUpdate",
    "findOneAndDelete",
    "drop",
    "dropDatabase",
    "createIndex",
    "dropIndex",
    "$out",
    "$merge",
];

pub fn recommended_blacklists() -> Value {
    serde_json::json!({
        "sql": RECOMMENDED_BLACKLIST_SQL,
        "redis": RECOMMENDED_BLACKLIST_REDIS,
        "mongodb": RECOMMENDED_BLACKLIST_MONGODB,
    })
}

pub fn normalize_command(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn get_command_head(command: &str, db_type: &DatabaseType) -> Result<String> {
    let normalized = normalize_command(command);
    if *db_type == DatabaseType::Mongodb {
        return Ok(get_mongo_command_name(&normalized)?.to_lowercase());
    }
    Ok(normalized
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_end_matches(';')
        .to_lowercase())
}

pub fn assert_command_allowed(config: &DatabaseConfig, command: &str) -> Result<()> {
    let normalized = normalize_command(command);
    let head = get_command_head(&normalized, &config.db_type)?;
    assert_not_blacklisted(config, &normalized, &head)?;
    // 默认只读，只有显式 readonly=false 才允许写操作。
    if config.readonly.unwrap_or(true) && !is_read_only_command(&config.db_type, &normalized)? {
        let rejected = if head.is_empty() { normalized } else { head };
        anyhow::bail!("只读模式拒绝执行命令: {rejected}");
    }
    Ok(())
}

fn assert_not_blacklisted(config: &DatabaseConfig, normalized: &str, head: &str) -> Result<()> {
    let command_for_blacklist = if is_sql_database(&config.db_type) {
        strip_sql_literals_and_comments(normalized)
    } else {
        normalized.to_string()
    };
    for item in config.blacklist.as_deref().unwrap_or(&[]) {
        let black = normalize_command(item).to_lowercase();
        if black.is_empty() {
            continue;
        }
        if head == black || has_blacklisted_keyword(&command_for_blacklist, &black) {
            anyhow::bail!("黑名单拒绝执行命令: {item}");
        }
    }
    Ok(())
}

fn has_blacklisted_keyword(command: &str, keyword: &str) -> bool {
    let escaped = regex::escape(keyword).replace(r"\ ", r"\s+");
    let re = Regex::new(&format!(
        r"(?i)(^|[^\p{{L}}\p{{N}}_$]){}($|[^\p{{L}}\p{{N}}_$])",
        escaped
    ))
    .expect("黑名单正则必须合法");
    re.is_match(command)
}

fn is_sql_database(db_type: &DatabaseType) -> bool {
    matches!(
        db_type,
        DatabaseType::Mysql | DatabaseType::Postgres | DatabaseType::Oracle
    )
}

pub fn is_read_only_command(db_type: &DatabaseType, command: &str) -> Result<bool> {
    let head = get_command_head(command, db_type)?;
    match db_type {
        DatabaseType::Redis => Ok(REDIS_READ_COMMANDS.contains(head.as_str())),
        DatabaseType::Mongodb => is_mongo_read_only_command(command),
        DatabaseType::Mysql | DatabaseType::Postgres | DatabaseType::Oracle => {
            if !SQL_READ_COMMANDS.contains(head.as_str()) {
                return Ok(false);
            }
            let sanitized = strip_sql_literals_and_comments(command);
            Ok(!SQL_WRITE_PATTERNS
                .iter()
                .any(|pattern| pattern.is_match(&sanitized))
                && !SQL_WRITE_COMMANDS
                    .iter()
                    .any(|keyword| has_blacklisted_keyword(&sanitized, keyword)))
        }
    }
}

fn is_mongo_read_only_command(command: &str) -> Result<bool> {
    let parsed: Value = serde_json::from_str(command)
        .map_err(|_| anyhow::anyhow!("MongoDB 命令必须是合法 JSON"))?;
    let object = parsed
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("MongoDB 命令必须是对象"))?;
    let (operation, payload) = object
        .iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("MongoDB 命令 JSON 不能为空"))?;
    let operation = operation.to_lowercase();
    if !MONGO_READ_COMMANDS.contains(operation.as_str()) {
        return Ok(false);
    }
    if operation == "aggregate" && mongo_payload_has_write_stage(payload) {
        return Ok(false);
    }
    Ok(true)
}

fn mongo_payload_has_write_stage(value: &Value) -> bool {
    let Some(pipeline) = value.get("pipeline").and_then(Value::as_array) else {
        return false;
    };
    pipeline.iter().any(|stage| {
        stage.as_object().is_some_and(|object| {
            object
                .keys()
                .any(|key| MONGO_AGGREGATE_WRITE_STAGES.contains(key.as_str()))
        })
    })
}

fn strip_sql_literals_and_comments(command: &str) -> String {
    let chars: Vec<char> = command.chars().collect();
    let mut result = String::new();
    let mut index = 0;
    while index < chars.len() {
        let char = chars[index];
        let next = chars.get(index + 1).copied();
        if (char == 'q' || char == 'Q') && next == Some('\'') {
            if let Some(end) = find_oracle_quoted_literal_end(&chars, index) {
                push_spaces(&mut result, end - index);
                index = end;
                continue;
            }
        }
        if char == '-' && next == Some('-') {
            let end = find_line_end(&chars, index + 2);
            push_spaces(&mut result, end - index);
            index = end;
            continue;
        }
        if char == '#' {
            let end = find_line_end(&chars, index + 1);
            push_spaces(&mut result, end - index);
            index = end;
            continue;
        }
        if char == '/' && next == Some('*') {
            let end = find_block_comment_end(&chars, index + 2);
            push_spaces(&mut result, end - index);
            index = end;
            continue;
        }
        if char == '\'' || char == '"' || char == '`' {
            let end = find_quoted_token_end(&chars, index, char, char);
            push_spaces(&mut result, end - index);
            index = end;
            continue;
        }
        if char == '[' {
            let end = find_quoted_token_end(&chars, index, '[', ']');
            push_spaces(&mut result, end - index);
            index = end;
            continue;
        }
        result.push(char);
        index += 1;
    }
    result
}

fn push_spaces(result: &mut String, count: usize) {
    result.extend(std::iter::repeat_n(' ', count));
}

fn find_line_end(chars: &[char], start: usize) -> usize {
    chars[start..]
        .iter()
        .position(|c| *c == '\n')
        .map(|offset| start + offset)
        .unwrap_or(chars.len())
}

fn find_block_comment_end(chars: &[char], start: usize) -> usize {
    let mut index = start;
    while index + 1 < chars.len() {
        if chars[index] == '*' && chars[index + 1] == '/' {
            return index + 2;
        }
        index += 1;
    }
    chars.len()
}

fn find_quoted_token_end(chars: &[char], start: usize, _open: char, close: char) -> usize {
    let mut index = start + 1;
    while index < chars.len() {
        if chars[index] == close {
            if chars.get(index + 1) == Some(&close) {
                index += 2;
                continue;
            }
            return index + 1;
        }
        if chars[index] == '\\' && close != ']' {
            index += 2;
            continue;
        }
        index += 1;
    }
    chars.len()
}

fn find_oracle_quoted_literal_end(chars: &[char], start: usize) -> Option<usize> {
    let open = *chars.get(start + 2)?;
    let close = match open {
        '[' => ']',
        '(' => ')',
        '{' => '}',
        '<' => '>',
        other => other,
    };
    let mut index = start + 3;
    while index + 1 < chars.len() {
        if chars[index] == close && chars[index + 1] == '\'' {
            return Some(index + 2);
        }
        index += 1;
    }
    None
}

fn get_mongo_command_name(command: &str) -> Result<String> {
    let parsed: Value = serde_json::from_str(command)
        .map_err(|_| anyhow::anyhow!("MongoDB 命令必须是合法 JSON"))?;
    let object = parsed
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("MongoDB 命令必须是对象"))?;
    object
        .keys()
        .next()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("MongoDB 命令 JSON 不能为空"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_recommendation_covers_every_sql_write_command() {
        for command in SQL_WRITE_COMMANDS {
            assert!(
                RECOMMENDED_BLACKLIST_SQL.contains(command),
                "推荐黑名单缺少写命令: {command}"
            );
        }
    }

    #[test]
    fn redis_ping_is_read_only() {
        assert!(is_read_only_command(&DatabaseType::Redis, "PING").unwrap());
    }

    #[test]
    fn redis_keys_is_not_read_only() {
        assert!(!is_read_only_command(&DatabaseType::Redis, "KEYS *").unwrap());
    }

    #[test]
    fn postgres_select_into_is_not_read_only() {
        assert!(!is_read_only_command(
            &DatabaseType::Postgres,
            "select * into new_table from users"
        )
        .unwrap());
    }

    #[test]
    fn cte_followed_by_insert_is_not_read_only() {
        assert!(!is_read_only_command(
            &DatabaseType::Postgres,
            "with rows as (select * from users) insert into audit select * from rows"
        )
        .unwrap());
    }

    #[test]
    fn mongo_aggregate_with_out_is_not_read_only() {
        assert!(!is_read_only_command(
            &DatabaseType::Mongodb,
            r#"{"aggregate":{"collection":"users","pipeline":[{"$match":{}},{"$out":"backup"}]}}"#
        )
        .unwrap());
    }

    #[test]
    fn mongo_aggregate_with_merge_is_not_read_only() {
        assert!(!is_read_only_command(
            &DatabaseType::Mongodb,
            r#"{"aggregate":{"collection":"users","pipeline":[{"$merge":{"into":"backup"}}]}}"#
        )
        .unwrap());
    }

    #[test]
    fn mongo_find_is_read_only() {
        assert!(is_read_only_command(
            &DatabaseType::Mongodb,
            r#"{"find":{"collection":"users","filter":{}}}"#
        )
        .unwrap());
    }
}
