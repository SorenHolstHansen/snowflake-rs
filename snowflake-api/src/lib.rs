#![doc(
    issue_tracker_base_url = "https://github.com/mycelial/snowflake-rs/issues",
    test(no_crate_inject)
)]
#![doc = include_str!("../README.md")]
#![warn(clippy::all, clippy::pedantic)]
#![allow(
clippy::must_use_candidate,
clippy::missing_errors_doc,
clippy::module_name_repetitions,
clippy::struct_field_names,
clippy::future_not_send, // This one seems like something we should eventually fix
clippy::missing_panics_doc
)]

use std::fmt::{Display, Formatter};
use std::io::{self};
use std::sync::Arc;

use arrow::error::ArrowError;
use arrow::ipc::reader::StreamReader;
use arrow::record_batch::RecordBatch;
use base64::Engine;
use bytes::{Buf, Bytes};
use futures::future::try_join_all;
use regex::Regex;
use reqwest_middleware::ClientWithMiddleware;
use serde_json::Value;
use thiserror::Error;

use responses::{ExecResponse, ExecRestResponse, ProcessedRestResponse, QueryContext};
use session::{AuthError, Session};

use crate::connection::QueryType;
use crate::connection::{Connection, ConnectionError};
use crate::requests::{EmptyRequest, ExecRequest};
use crate::responses::{BaseRestResponse, ExecResponseRowType, SnowflakeType};
use crate::session::AuthError::MissingEnvArgument;

pub mod connection;
#[cfg(feature = "polars")]
mod polars;
mod put;
mod requests;
pub mod responses;
mod session;
mod utils;

#[derive(Error, Debug)]
pub enum SnowflakeApiError {
    #[error(transparent)]
    RequestError(#[from] ConnectionError),

    #[error(transparent)]
    AuthError(#[from] AuthError),

    #[error(transparent)]
    ResponseDeserializationError(#[from] base64::DecodeError),

    #[error(transparent)]
    ArrowError(#[from] arrow::error::ArrowError),

    #[error("S3 bucket path in PUT request is invalid: `{0}`")]
    InvalidBucketPath(String),

    #[error("Couldn't extract filename from the local path: `{0}`")]
    InvalidLocalPath(String),

    #[error(transparent)]
    LocalIoError(#[from] io::Error),

    #[error(transparent)]
    ObjectStoreError(#[from] object_store::Error),

    #[error(transparent)]
    ObjectStorePathError(#[from] object_store::path::Error),

    #[error(transparent)]
    TokioTaskJoinError(#[from] tokio::task::JoinError),

    #[error(
        "Snowflake API error. Code: `{code:?}`. Message: `{message:?}`. QueryId: `{query_id:?}`"
    )]
    ApiError {
        code: String,
        message: String,
        query_id: String,
    },

    #[error("Snowflake API empty response could mean that query wasn't executed correctly or API call was faulty")]
    EmptyResponse,

    #[error("No usable rowsets were included in the response")]
    BrokenResponse,

    #[error("Following feature is not implemented yet: {0}")]
    Unimplemented(String),

    #[error("Unexpected API response")]
    UnexpectedResponse,

    #[error("Unexpected Async Query response")]
    UnexpectedAsyncQueryResponse,

    #[error(transparent)]
    GlobPatternError(#[from] glob::PatternError),

    #[error(transparent)]
    GlobError(#[from] glob::GlobError),
}

#[derive(Debug)]
pub struct EmptyJsonResult {
    pub schema: Option<Vec<FieldSchema>>,
    pub query_id: String,
    pub send_result_time: usize,
    pub query_context: QueryContext,
}

/// Even if Arrow is specified as a return type non-select queries
/// will return Json array of arrays: `[[42, "answer"], [43, "non-answer"]]`.
#[derive(Debug)]
pub struct JsonResult {
    // todo: can it _only_ be a json array of arrays or something else too?
    pub value: serde_json::Value,
    /// Field ordering matches the array ordering
    pub schema: Vec<FieldSchema>,
    pub query_id: String,
    pub send_result_time: usize,
    pub query_context: QueryContext,
}

impl Display for JsonResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.value)
    }
}

#[derive(Debug)]
pub struct BytesResult {
    pub chunks: Vec<Bytes>,
    pub query_id: String,
    pub send_result_time: usize,
    pub query_context: QueryContext,
}

/// Based on the [`ExecResponseRowType`]
#[derive(Debug)]
pub struct FieldSchema {
    pub name: String,
    // todo: is it a good idea to expose internal response struct to the user?
    pub type_: SnowflakeType,
    pub scale: Option<i64>,
    pub precision: Option<i64>,
    pub nullable: bool,
}

impl From<ExecResponseRowType> for FieldSchema {
    fn from(value: ExecResponseRowType) -> Self {
        FieldSchema {
            name: value.name,
            type_: value.type_,
            scale: value.scale,
            precision: value.precision,
            nullable: value.nullable,
        }
    }
}

#[derive(Debug)]
pub struct ArrowResult {
    pub batches: Vec<RecordBatch>,
    pub query_id: String,
    pub send_result_time: usize,
    pub query_context: QueryContext,
}

/// Container for query result.
/// Arrow is returned by-default for all SELECT statements,
/// unless there is session configuration issue or it's a different statement type.
#[derive(Debug)]
pub enum QueryResult {
    Arrow(ArrowResult),
    Json(JsonResult),
    Empty(EmptyJsonResult),
}

/// Raw query result
/// Can be transformed into [`QueryResult`]
pub enum RawQueryResult {
    /// Arrow IPC chunks
    /// see: <https://arrow.apache.org/docs/format/Columnar.html#serialization-and-interprocess-communication-ipc>
    Bytes(BytesResult),
    /// Json payload is deserialized,
    /// as it's already a part of REST response
    Json(JsonResult),
    Empty(EmptyJsonResult),
}

impl RawQueryResult {
    pub fn deserialize_arrow(self) -> Result<QueryResult, ArrowError> {
        match self {
            RawQueryResult::Bytes(bytes_result) => Self::flat_bytes_to_batches(bytes_result.chunks)
                .map(|batches| {
                    QueryResult::Arrow(ArrowResult {
                        batches,
                        query_id: bytes_result.query_id,
                        send_result_time: bytes_result.send_result_time,
                        query_context: bytes_result.query_context,
                    })
                }),
            RawQueryResult::Json(j) => Ok(QueryResult::Json(j)),
            RawQueryResult::Empty(e) => Ok(QueryResult::Empty(e)),
        }
    }

    fn flat_bytes_to_batches(bytes: Vec<Bytes>) -> Result<Vec<RecordBatch>, ArrowError> {
        let mut res = vec![];
        for b in bytes {
            let mut batches = Self::bytes_to_batches(b)?;
            res.append(&mut batches);
        }
        Ok(res)
    }

    fn bytes_to_batches(bytes: Bytes) -> Result<Vec<RecordBatch>, ArrowError> {
        let record_batches = StreamReader::try_new(bytes.reader(), None)?;
        record_batches.into_iter().collect()
    }
}

pub struct AuthArgs {
    pub account_identifier: String,
    pub warehouse: Option<String>,
    pub database: Option<String>,
    pub schema: Option<String>,
    pub username: String,
    pub role: Option<String>,
    pub auth_type: AuthType,
}

impl AuthArgs {
    pub fn from_env() -> Result<AuthArgs, SnowflakeApiError> {
        let auth_type = if let Ok(password) = std::env::var("SNOWFLAKE_PASSWORD") {
            Ok(AuthType::Password(PasswordArgs { password }))
        } else if let Ok(private_key_pem) = std::env::var("SNOWFLAKE_PRIVATE_KEY") {
            Ok(AuthType::Certificate(CertificateArgs { private_key_pem }))
        } else {
            Err(MissingEnvArgument(
                "SNOWFLAKE_PASSWORD or SNOWFLAKE_PRIVATE_KEY".to_owned(),
            ))
        };

        Ok(AuthArgs {
            account_identifier: std::env::var("SNOWFLAKE_ACCOUNT")
                .map_err(|_| MissingEnvArgument("SNOWFLAKE_ACCOUNT".to_owned()))?,
            warehouse: std::env::var("SNOWLFLAKE_WAREHOUSE").ok(),
            database: std::env::var("SNOWFLAKE_DATABASE").ok(),
            schema: std::env::var("SNOWFLAKE_SCHEMA").ok(),
            username: std::env::var("SNOWFLAKE_USER")
                .map_err(|_| MissingEnvArgument("SNOWFLAKE_USER".to_owned()))?,
            role: std::env::var("SNOWFLAKE_ROLE").ok(),
            auth_type: auth_type?,
        })
    }
}

pub enum AuthType {
    Password(PasswordArgs),
    Certificate(CertificateArgs),
}

pub struct PasswordArgs {
    pub password: String,
}

pub struct CertificateArgs {
    pub private_key_pem: String,
}

#[must_use]
pub struct SnowflakeApiBuilder {
    pub auth: AuthArgs,
    client: Option<ClientWithMiddleware>,
}

impl SnowflakeApiBuilder {
    pub fn new(auth: AuthArgs) -> Self {
        Self { auth, client: None }
    }

    pub fn with_client(mut self, client: ClientWithMiddleware) -> Self {
        self.client = Some(client);
        self
    }

    pub fn build(self) -> Result<SnowflakeApi, SnowflakeApiError> {
        let connection = match self.client {
            Some(client) => Arc::new(Connection::new_with_middware(client)),
            None => Arc::new(Connection::new()?),
        };

        let session = match self.auth.auth_type {
            AuthType::Password(args) => Session::password_auth(
                Arc::clone(&connection),
                &self.auth.account_identifier,
                self.auth.warehouse.as_deref(),
                self.auth.database.as_deref(),
                self.auth.schema.as_deref(),
                &self.auth.username,
                self.auth.role.as_deref(),
                &args.password,
            ),
            AuthType::Certificate(args) => Session::cert_auth(
                Arc::clone(&connection),
                &self.auth.account_identifier,
                self.auth.warehouse.as_deref(),
                self.auth.database.as_deref(),
                self.auth.schema.as_deref(),
                &self.auth.username,
                self.auth.role.as_deref(),
                &args.private_key_pem,
            ),
        };

        let account_identifier = self.auth.account_identifier.to_uppercase();

        Ok(SnowflakeApi::new(
            Arc::clone(&connection),
            session,
            account_identifier,
        ))
    }
}

/// Snowflake API, keeps connection pool and manages session for you
pub struct SnowflakeApi {
    connection: Arc<Connection>,
    session: Session,
    account_identifier: String,
}

impl SnowflakeApi {
    /// Create a new `SnowflakeApi` object with an existing connection and session.
    pub fn new(connection: Arc<Connection>, session: Session, account_identifier: String) -> Self {
        Self {
            connection,
            session,
            account_identifier,
        }
    }
    /// Initialize object with password auth. Authentication happens on the first request.
    pub fn with_password_auth(
        account_identifier: &str,
        warehouse: Option<&str>,
        database: Option<&str>,
        schema: Option<&str>,
        username: &str,
        role: Option<&str>,
        password: &str,
    ) -> Result<Self, SnowflakeApiError> {
        let connection = Arc::new(Connection::new()?);

        let session = Session::password_auth(
            Arc::clone(&connection),
            account_identifier,
            warehouse,
            database,
            schema,
            username,
            role,
            password,
        );

        let account_identifier = account_identifier.to_uppercase();
        Ok(Self::new(
            Arc::clone(&connection),
            session,
            account_identifier,
        ))
    }

    /// Initialize object with private certificate auth. Authentication happens on the first request.
    pub fn with_certificate_auth(
        account_identifier: &str,
        warehouse: Option<&str>,
        database: Option<&str>,
        schema: Option<&str>,
        username: &str,
        role: Option<&str>,
        private_key_pem: &str,
    ) -> Result<Self, SnowflakeApiError> {
        let connection = Arc::new(Connection::new()?);

        let session = Session::cert_auth(
            Arc::clone(&connection),
            account_identifier,
            warehouse,
            database,
            schema,
            username,
            role,
            private_key_pem,
        );

        let account_identifier = account_identifier.to_uppercase();
        Ok(Self::new(
            Arc::clone(&connection),
            session,
            account_identifier,
        ))
    }

    pub fn from_env() -> Result<Self, SnowflakeApiError> {
        SnowflakeApiBuilder::new(AuthArgs::from_env()?).build()
    }

    /// Closes the current session, this is necessary to clean up temporary objects (tables, functions, etc)
    /// which are Snowflake session dependent.
    /// If another request is made the new session will be initiated.
    pub async fn close_session(&mut self) -> Result<(), SnowflakeApiError> {
        self.session.close().await?;
        Ok(())
    }

    /// Execute a single query against API.
    /// If statement is PUT, then file will be uploaded to the Snowflake-managed storage
    pub async fn exec(&self, sql: &str) -> Result<ExecRestResponse, SnowflakeApiError> {
        let base_rest_res = self.exec_raw(sql).await?;
        Ok(into_resp_type!(
            &base_rest_res,
            base_rest_res.data.deserialize_arrow()?
        ))
    }

    /// Executes a single query against API.
    /// If statement is PUT, then file will be uploaded to the Snowflake-managed storage
    /// Returns raw bytes in the Arrow response
    pub async fn exec_raw(&self, sql: &str) -> Result<ProcessedRestResponse, SnowflakeApiError> {
        let put_re = Regex::new(r"(?i)^(?:/\*.*\*/\s*)*put\s+").unwrap();

        // put commands go through a different flow and result is side-effect
        if put_re.is_match(sql) {
            log::info!("Detected PUT query");
            self.exec_put(sql).await
        } else {
            self.exec_arrow_raw(sql).await
        }
    }

    async fn exec_put(&self, sql: &str) -> Result<ProcessedRestResponse, SnowflakeApiError> {
        let resp = self
            .run_sql::<ExecResponse>(sql, QueryType::JsonQuery)
            .await?;
        log::debug!("Got PUT response: {:?}", resp);

        match resp {
            ExecResponse::Query(_) => Err(SnowflakeApiError::UnexpectedResponse),
            ExecResponse::PutGet(pg) => {
                let res = into_resp_type!(
                    &pg,
                    RawQueryResult::Empty(EmptyJsonResult {
                        schema: None,
                        query_id: pg.data.query_id.clone(),
                        send_result_time: pg.data.send_result_time,
                        query_context: pg.data.query_context.clone()
                    })
                );
                put::put(pg).await?;
                Ok(res)
            }
            ExecResponse::Error(e) => Err(SnowflakeApiError::ApiError {
                code: e.data.error_code,
                message: e.message.unwrap_or_default(),
                query_id: e.data.query_id,
            }),
        }
    }

    /// Useful for debugging to get the straight query response
    #[cfg(debug_assertions)]
    pub async fn exec_response(&mut self, sql: &str) -> Result<ExecResponse, SnowflakeApiError> {
        self.run_sql::<ExecResponse>(sql, QueryType::ArrowQuery)
            .await
    }

    /// Useful for debugging to get raw JSON response
    #[cfg(debug_assertions)]
    pub async fn exec_json(&mut self, sql: &str) -> Result<serde_json::Value, SnowflakeApiError> {
        self.run_sql::<serde_json::Value>(sql, QueryType::JsonQuery)
            .await
    }

    async fn exec_arrow_raw(&self, sql: &str) -> Result<ProcessedRestResponse, SnowflakeApiError> {
        let resp = self
            .run_sql::<ExecResponse>(sql, QueryType::ArrowQuery)
            .await?;
        log::debug!("Got query response: {:?}", resp);

        let orig_resp = match resp {
            // processable response
            ExecResponse::Query(qr) => Ok(qr),
            ExecResponse::PutGet(_) => Err(SnowflakeApiError::UnexpectedResponse),
            ExecResponse::Error(e) => Err(SnowflakeApiError::ApiError {
                code: e.data.error_code,
                message: e.message.unwrap_or_default(),
                query_id: e.data.query_id,
            }),
        }?;
        let mut resp = orig_resp.clone();
        while resp.is_async() {
            let async_data = resp.data.as_async()?;
            resp = match self
                .poll::<ExecResponse>(&async_data.get_result_url)
                .await?
            {
                ExecResponse::Query(qr) => qr,
                ExecResponse::PutGet(_) => return Err(SnowflakeApiError::UnexpectedResponse),
                ExecResponse::Error(e) => {
                    return Err(SnowflakeApiError::ApiError {
                        code: e.data.error_code,
                        message: e.message.unwrap_or_default(),
                        query_id: e.data.query_id,
                    })
                }
            };
        }

        // if response was empty, base64 data is empty string
        // todo: still return empty arrow batch with proper schema? (schema always included)
        // should be safe to ? here, as we've checked for async resp before
        let sync_data = resp.data.as_sync()?;
        let raw_query_res = if sync_data.returned == 0 {
            log::debug!("Got response with 0 rows");
            let schema = if let Some(rowtype) = sync_data.rowtype {
                Some(rowtype.into_iter().map(Into::into).collect())
            } else {
                None
            };
            RawQueryResult::Empty(EmptyJsonResult {
                schema,
                query_id: sync_data.query_id,
                send_result_time: sync_data.send_result_time,
                query_context: sync_data.query_context,
            })
        } else if let Some(value) = sync_data.rowset {
            log::debug!("Got JSON response");
            let mut values: Vec<Value> = serde_json::from_value(value).unwrap();
            for chunk in sync_data.chunks.iter() {
                let bytes = self
                    .connection
                    .get_chunk(&chunk.url, &sync_data.chunk_headers)
                    .await?;
                // Add a '[' at the beginning and ']' at the end of the byte stream
                let mut bytes_with_brackets = Vec::new();
                bytes_with_brackets.push(b'['); // Add opening bracket
                bytes_with_brackets.extend_from_slice(&bytes); // Add the original bytes
                bytes_with_brackets.push(b']'); // Add closing bracket

                // Replace the invalid ',' between arrays with the valid array separator ", "
                let json_str = String::from_utf8_lossy(&bytes_with_brackets);
                let json_str = json_str.replace("], [", "],[");

                // Now deserialize as a `Vec<Value>` since the entire data is now a valid JSON array
                let chunk_values: Vec<Value> = serde_json::from_str(&json_str).unwrap();
                values.extend(chunk_values);
            }
            // NOTE: json response could be chunked too. however, go clients should receive arrow by-default,
            // unless user sets session variable to return json. This case was added for debugging and status
            // information being passed through that fields.
            let value = serde_json::to_value(values).unwrap();
            RawQueryResult::Json(JsonResult {
                value,
                query_id: sync_data.query_id,
                send_result_time: sync_data.send_result_time,
                query_context: sync_data.query_context,
                schema: sync_data
                    .rowtype
                    .unwrap()
                    .into_iter()
                    .map(Into::into)
                    .collect(),
            })
        } else if let Some(base64) = sync_data.rowset_base64 {
            // fixme: is it possible to give streaming interface?
            let mut chunks = try_join_all(sync_data.chunks.iter().map(|chunk| {
                self.connection
                    .get_chunk(&chunk.url, &sync_data.chunk_headers)
            }))
            .await?;

            // fixme: should base64 chunk go first?
            // fixme: if response is chunked is it both base64 + chunks or just chunks?
            if !base64.is_empty() {
                log::debug!("Got base64 encoded response");
                let bytes = Bytes::from(base64::engine::general_purpose::STANDARD.decode(base64)?);
                chunks.push(bytes);
            }

            RawQueryResult::Bytes(BytesResult {
                chunks,
                query_id: sync_data.query_id,
                send_result_time: sync_data.send_result_time,
                query_context: sync_data.query_context,
            })
        } else {
            return Err(SnowflakeApiError::BrokenResponse);
        };
        Ok(into_resp_type!(&orig_resp, raw_query_res))
    }

    async fn run_sql<R: serde::de::DeserializeOwned>(
        &self,
        sql_text: &str,
        query_type: QueryType,
    ) -> Result<R, SnowflakeApiError> {
        log::debug!("Executing: {}", sql_text);

        let parts = self.session.get_token().await?;

        let body = ExecRequest {
            sql_text: sql_text.to_string(),
            async_exec: false,
            sequence_id: parts.sequence_id,
            is_internal: false,
        };

        let resp = self
            .connection
            .request::<R>(
                query_type,
                &self.account_identifier,
                &[],
                Some(&parts.session_token_auth_header),
                body,
                None,
            )
            .await?;

        Ok(resp)
    }

    async fn poll<R: serde::de::DeserializeOwned>(
        &self,
        get_result_url: &str,
    ) -> Result<R, SnowflakeApiError> {
        log::debug!("Polling: {}", get_result_url);

        let parts = self.session.get_token().await?;
        let resp = self
            .connection
            .request::<R>(
                QueryType::ArrowQuery,
                &self.account_identifier,
                &[],
                Some(&parts.session_token_auth_header),
                EmptyRequest,
                Some(get_result_url),
            )
            .await?;

        Ok(resp)
    }
}
