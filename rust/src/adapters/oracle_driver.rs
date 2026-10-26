use super::deadline::{log_cleanup, query_timeout, timeout_error};
use super::DatabaseAdapter;
use crate::types::{MetadataRequest, MetadataType, QueryResult};
use anyhow::Result;
use async_trait::async_trait;
use oracle::{Connection, Version};
use serde_json::{Map, Value};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::task;
use url::Url;

pub struct OracleAdapter {
    url: String,
}

impl OracleAdapter {
    pub fn new(url: String) -> Self {
        Self { url }
    }

    async fn query(&mut self, command: &str) -> Result<QueryResult> {
        let url = self.url.clone();
        let command = command.to_string();
        let state = Arc::new(Mutex::new(CancelState {
            cancelled: false,
            connection: None,
        }));
        let mut cancel = CancelQuery(Some(state.clone()));
        let deadline = Instant::now() + query_timeout();
        let result = tokio::time::timeout(
            query_timeout(),
            task::spawn_blocking(move || {
                {
                    if state.lock().expect("Oracle 取消锁中毒").cancelled {
                        return Err(timeout_error());
                    }
                }
                let connection = Arc::new(connect_oracle(&url)?);
                {
                    let mut state = state.lock().expect("Oracle 取消锁中毒");
                    if state.cancelled {
                        return Err(timeout_error());
                    }
                    state.connection = Some(connection.clone());
                }
                let result = execute_query(&connection, &command, deadline, &state);
                state.lock().expect("Oracle 取消锁中毒").connection = None;
                result
            }),
        )
        .await;
        match result {
            Ok(result) => {
                cancel.0 = None;
                result?
            }
            Err(_) => Err(timeout_error()),
        }
    }
}

struct CancelState {
    cancelled: bool,
    connection: Option<Arc<Connection>>,
}

/// 外层 timeout 丢弃 future 并不能停止 spawn_blocking，须显式通知 OCI 取消执行。
struct CancelQuery(Option<Arc<Mutex<CancelState>>>);

impl Drop for CancelQuery {
    fn drop(&mut self) {
        if let Some(state) = self.0.take() {
            let connection = {
                let mut state = state.lock().expect("Oracle 取消锁中毒");
                state.cancelled = true;
                state.connection.clone()
            };
            if let Some(connection) = connection {
                task::spawn_blocking(move || {
                    log_cleanup(connection.break_execution().map_err(Into::into))
                });
            }
        }
    }
}

/// OCI 的 call_timeout 是单次往返限制，每次读取前按总截止时间缩短它。
fn prepare_call(
    connection: &Connection,
    client_version: &Version,
    deadline: Instant,
    state: &Mutex<CancelState>,
) -> Result<()> {
    if state.lock().expect("Oracle 取消锁中毒").cancelled {
        return Err(timeout_error());
    }
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(timeout_error)?;
    configure_call_timeout(client_version, remaining, |duration| {
        connection
            .set_call_timeout(Some(duration))
            .map_err(Into::into)
    })?;
    Ok(())
}

/// ODPI-C 要求 Client >= 18.1；旧客户端仍由外层期限和 break_execution 负责取消。
/// 这里只跳过不支持的接口，新客户端上的真实设置错误继续向上传递。
fn configure_call_timeout(
    client_version: &Version,
    remaining: Duration,
    set_timeout: impl FnOnce(Duration) -> Result<()>,
) -> Result<()> {
    if client_version >= &Version::new(18, 1, 0, 0, 0) {
        set_timeout(remaining.max(Duration::from_millis(1)))?;
    }
    Ok(())
}

#[async_trait]
impl DatabaseAdapter for OracleAdapter {
    async fn connect(&mut self) -> Result<()> {
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<()> {
        Ok(())
    }

    async fn test(&mut self) -> Result<()> {
        self.execute("select 1 from dual").await.map(|_| ())
    }

    async fn execute(&mut self, command: &str) -> Result<QueryResult> {
        self.query(command).await
    }

    async fn metadata(&mut self, request: MetadataRequest) -> Result<QueryResult> {
        match request.request_type {
            MetadataType::Tables => {
                self.query("select table_name from user_tables order by table_name")
                    .await
            }
            MetadataType::Columns => {
                let table = request
                    .table
                    .ok_or_else(|| anyhow::anyhow!("columns 元信息查询必须提供 --table"))?
                    .replace('\'', "''")
                    .to_uppercase();
                self.query(&format!("select table_name, column_name, data_type from user_tab_columns where table_name = '{}' order by column_id", table)).await
            }
            _ => anyhow::bail!("当前数据库不支持元信息类型: {:?}", request.request_type),
        }
    }
}

fn connect_oracle(url: &str) -> Result<Connection> {
    let parsed = Url::parse(url)?;
    let user = percent_decode(parsed.username())?;
    let password = percent_decode(parsed.password().unwrap_or(""))?;
    let port = parsed.port().unwrap_or(1521);
    let service = parsed.path().trim_start_matches('/');
    let connect_string = format!(
        "{}:{}{}",
        parsed.host_str().unwrap_or("localhost"),
        port,
        if service.is_empty() {
            String::new()
        } else {
            format!("/{service}")
        }
    );
    Ok(Connection::connect(user, password, connect_string)?)
}

fn percent_decode(value: &str) -> Result<String> {
    Ok(url::form_urlencoded::parse(value.as_bytes())
        .next()
        .map(|(k, _)| k.into_owned())
        .unwrap_or_else(|| value.to_string()))
}

fn execute_query(
    connection: &Connection,
    command: &str,
    deadline: Instant,
    state: &Mutex<CancelState>,
) -> Result<QueryResult> {
    // 检查客户端库版本，而非数据库服务端版本；每次查询只读取一次。
    let client_version = Version::client()?;
    prepare_call(connection, &client_version, deadline, state)?;
    let mut rows = connection.query(command, &[])?;
    let column_names = rows
        .column_info()
        .iter()
        .map(|info| info.name().to_string())
        .collect::<Vec<_>>();
    let mut values = Vec::new();
    loop {
        prepare_call(connection, &client_version, deadline, state)?;
        let Some(row_result) = rows.next() else {
            break;
        };
        let row = row_result?;
        let mut object = Map::new();
        for (index, column) in column_names.iter().enumerate() {
            // LOB 转换可能再次访问数据库，也必须遵守剩余期限。
            prepare_call(connection, &client_version, deadline, state)?;
            let value: Option<String> = row.get(index)?;
            object.insert(
                column.clone(),
                value.map(Value::String).unwrap_or(Value::Null),
            );
        }
        values.push(Value::Object(object));
    }
    Ok(QueryResult {
        row_count: Some(values.len() as u64),
        rows: values,
        fields: Some(column_names),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_clients_skip_unsupported_call_timeout() {
        for version in [
            Version::new(11, 2, 0, 4, 0),
            Version::new(12, 1, 0, 2, 0),
            Version::new(12, 2, 0, 1, 0),
            Version::new(18, 0, 0, 0, 0),
        ] {
            configure_call_timeout(&version, Duration::from_secs(1), |_| {
                panic!("旧客户端 {version} 不应调用 set_call_timeout")
            })
            .unwrap();
        }
    }

    #[test]
    fn supported_clients_set_remaining_timeout_and_propagate_errors() {
        for version in [Version::new(18, 1, 0, 0, 0), Version::new(19, 3, 0, 0, 0)] {
            let mut called = false;
            configure_call_timeout(&version, Duration::from_secs(2), |duration| {
                called = true;
                assert_eq!(duration, Duration::from_secs(2));
                Ok(())
            })
            .unwrap();
            assert!(called);
        }
        let error = configure_call_timeout(
            &Version::new(18, 1, 0, 0, 0),
            Duration::from_secs(1),
            |_| anyhow::bail!("OCI 设置失败"),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "OCI 设置失败");
    }
}
