use std::collections::HashMap;

use serde::Deserialize;

use crate::{QueryResult, RawQueryResult, SnowflakeApiError};

#[allow(clippy::large_enum_variant)]
#[derive(Deserialize, Debug)]
#[serde(untagged)]
pub enum ExecResponse {
    Query(QueryExecResponse),
    PutGet(PutGetExecResponse),
    Error(ExecErrorResponse),
}

impl ExecResponse {
    pub fn is_async(&self) -> bool {
        match self {
            Self::Query(q) => q.is_async(),
            Self::PutGet(p) => p.is_async(),
            Self::Error(_) => false,
        }
    }
}

// todo: add close session response, which should be just empty?
#[allow(clippy::large_enum_variant)]
#[derive(Deserialize, Debug)]
#[serde(untagged)]
pub enum AuthResponse {
    Login(LoginResponse),
    Auth(AuthenticatorResponse),
    Renew(RenewSessionResponse),
    Close(CloseSessionResponse),
    Error(AuthErrorResponse),
}

#[derive(Deserialize, Debug, Clone)]
pub struct BaseRestResponse<D> {
    // null for auth
    pub code: Option<String>,
    pub message: Option<String>,
    pub success: bool,
    pub data: D,
}

impl<D> BaseRestResponse<D> {
    pub fn is_async(&self) -> bool {
        self.code == Some("333333".to_owned()) || self.code == Some("333334".to_owned())
    }
}

pub type PutGetExecResponse = BaseRestResponse<PutGetResponseData>;
pub type QueryExecResponse = BaseRestResponse<QueryExecResponseData>;
pub type ExecErrorResponse = BaseRestResponse<ExecErrorResponseData>;
pub type AuthErrorResponse = BaseRestResponse<AuthErrorResponseData>;
pub type AuthenticatorResponse = BaseRestResponse<AuthenticatorResponseData>;
pub type LoginResponse = BaseRestResponse<LoginResponseData>;
pub type RenewSessionResponse = BaseRestResponse<RenewSessionResponseData>;
// Data should be always `null` on successful close session response
pub type CloseSessionResponse = BaseRestResponse<Option<()>>;
pub type ProcessedRestResponse = BaseRestResponse<RawQueryResult>;
pub type ExecRestResponse = BaseRestResponse<QueryResult>;

#[macro_export]
macro_rules! into_resp_type {
    ($base_res:expr, $data:expr) => {
        BaseRestResponse {
            code: $base_res.code.clone(),
            message: $base_res.message.clone(),
            success: $base_res.success,
            data: $data,
        }
    };
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ExecErrorResponseData {
    pub age: i64,
    pub error_code: String,
    pub internal_error: bool,

    // come when query is invalid
    pub line: Option<i64>,
    pub pos: Option<i64>,

    // fixme: only valid for exec query response error? present in any exec query response?
    pub query_id: String,
    pub sql_state: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AuthErrorResponseData {
    pub authn_method: Option<String>,
    pub error_code: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct NameValueParameter {
    pub name: String,
    pub value: serde_json::Value,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct LoginResponseData {
    pub session_id: i64,
    pub token: String,
    pub master_token: String,
    pub server_version: String,
    #[serde(default)]
    pub parameters: Vec<NameValueParameter>,
    pub session_info: SessionInfo,
    pub master_validity_in_seconds: i64,
    pub validity_in_seconds: i64,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub database_name: Option<String>,
    pub schema_name: Option<String>,
    pub warehouse_name: Option<String>,
    pub role_name: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AuthenticatorResponseData {
    pub token_url: String,
    pub sso_url: String,
    pub proof_key: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RenewSessionResponseData {
    pub session_token: String,
    pub validity_in_seconds_s_t: i64,
    pub master_token: String,
    pub validity_in_seconds_m_t: i64,
    pub session_id: i64,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum QueryExecResponseData {
    Sync(SyncQueryExecResponseData),
    Async(AsyncQueryExecResponseData),
}

impl QueryExecResponseData {
    pub fn as_sync(self) -> Result<SyncQueryExecResponseData, SnowflakeApiError> {
        if let Self::Sync(data) = self {
            Ok(data)
        } else {
            Err(SnowflakeApiError::UnexpectedAsyncQueryResponse)
        }
    }

    pub fn as_async(self) -> Result<AsyncQueryExecResponseData, SnowflakeApiError> {
        if let Self::Async(data) = self {
            Ok(data)
        } else {
            Err(SnowflakeApiError::UnexpectedAsyncQueryResponse)
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AsyncQueryExecResponseData {
    pub query_id: String,
    pub get_result_url: String,
    pub query_aborts_after_secs: i64,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct QueryContextEntry {
    pub id: isize,
    pub timestamp: usize,
    // pub priority: usize,
    // pub context: String
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct QueryContext {
    pub entries: Vec<QueryContextEntry>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SyncQueryExecResponseData {
    pub parameters: Option<Vec<NameValueParameter>>,
    pub rowtype: Option<Vec<ExecResponseRowType>>,
    // default for non-SELECT queries
    // GET / PUT has their own response format
    pub rowset: Option<serde_json::Value>,
    // only exists when binary response is given, eg Arrow
    // default for all SELECT queries
    // is base64-encoded Arrow IPC payload
    pub rowset_base64: Option<String>,
    pub total: i64,
    pub returned: i64,    // unused in .NET
    pub query_id: String, // unused in .NET
    pub database_provider: Option<String>,
    pub final_database_name: Option<String>, // unused in .NET
    pub final_schema_name: Option<String>,
    pub final_warehouse_name: Option<String>, // unused in .NET
    pub final_role_name: Option<String>,      // unused in .NET
    // only present on SELECT queries
    pub number_of_binds: Option<i32>, // unused in .NET
    // todo: deserialize into enum
    pub statement_type_id: Option<i64>,
    pub version: Option<i64>,
    // if response is chunked
    #[serde(default)] // soft-default to empty Vec if not present
    pub chunks: Vec<ExecResponseChunk>,
    // x-amz-server-side-encryption-customer-key, when chunks are present for download
    pub qrmk: Option<String>,
    #[serde(default)] // chunks are present
    pub chunk_headers: HashMap<String, String>,
    // multi-statement response, comma-separated
    pub result_ids: Option<String>,
    // `progressDesc`, and `queryAbortAfterSecs` are not used but exist in .NET
    // `queryResultFormat` also exist
    pub send_result_time: usize,
    pub query_context: QueryContext,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ExecResponseRowType {
    pub name: String,
    #[serde(rename = "byteLength")]
    pub byte_length: Option<i64>,
    // unused in .NET
    pub length: Option<i64>,
    #[serde(rename = "type")]
    pub type_: SnowflakeType,
    pub scale: Option<i64>,
    pub precision: Option<i64>,
    pub nullable: bool,
}

// fixme: is it good idea to keep this as an enum if more types could be added in future?
#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub enum SnowflakeType {
    Fixed,
    Real,
    Text,
    Date,
    Variant,
    TimestampLtz,
    TimestampNtz,
    TimestampTz,
    Object,
    Binary,
    Time,
    Boolean,
    Array,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ExecResponseChunk {
    pub url: String,
    pub row_count: i32,
    pub uncompressed_size: i64,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PutGetResponseData {
    // `kind`, `operation` are present in Go implementation, but not in .NET
    pub command: CommandType,
    pub local_location: Option<String>,
    // inconsistent case naming
    #[serde(rename = "src_locations", default)]
    pub src_locations: Vec<String>,
    // file upload parallelism
    pub parallel: usize, // fixme: originally i32, handle this in parsing somehow?
    // file size threshold, small ones are should be uploaded with given parallelism
    pub threshold: i64,
    // doesn't need compression if source is already compressed
    pub auto_compress: bool,
    pub overwrite: bool,
    // maps to one of the predefined compression algos
    // todo: support different compression formats?
    pub source_compression: String,
    pub stage_info: PutGetStageInfo,
    pub encryption_material: EncryptionMaterialVariant,
    // GCS specific. If you request multiple files?
    #[serde(default)]
    pub presigned_urls: Vec<String>,
    #[serde(default)]
    pub parameters: Vec<NameValueParameter>,
    pub statement_type_id: Option<i64>,
    pub query_id: String,
    pub send_result_time: usize,
    pub query_context: QueryContext,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "UPPERCASE")]
pub enum CommandType {
    Upload,
    Download,
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
pub enum PutGetStageInfo {
    Aws(AwsPutGetStageInfo),
    Azure(AzurePutGetStageInfo),
    Gcs(GcsPutGetStageInfo),
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AwsPutGetStageInfo {
    pub location_type: String,
    pub location: String,
    pub region: String,
    pub creds: AwsCredentials,
    // FIPS endpoint
    pub end_point: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub struct AwsCredentials {
    pub aws_key_id: String,
    pub aws_secret_key: String,
    pub aws_token: String,
    pub aws_id: String,
    pub aws_key: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct GcsPutGetStageInfo {
    pub location_type: String,
    pub location: String,
    pub storage_account: String,
    pub creds: GcsCredentials,
    pub presigned_url: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub struct GcsCredentials {
    pub gcs_access_token: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AzurePutGetStageInfo {
    pub location_type: String,
    pub location: String,
    pub storage_account: String,
    pub creds: AzureCredentials,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub struct AzureCredentials {
    pub azure_sas_token: String,
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
pub enum EncryptionMaterialVariant {
    Single(PutGetEncryptionMaterial),
    Multiple(Vec<PutGetEncryptionMaterial>),
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PutGetEncryptionMaterial {
    // base64 encoded
    pub query_stage_master_key: String,
    pub query_id: String,
    pub smk_id: i64,
}
