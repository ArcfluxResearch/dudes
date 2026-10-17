use dudes_proto::rpc::recovery::{
    MAX_RECOVERY_REQUEST_BYTES, RecoveryInspectParams, RecoveryReceiptParams, RecoverySubmitParams,
};
use dudes_proto::rpc::{CurrentMethod, ErrorCode, RpcError};
use serde::Deserialize;
use serde::de::{DeserializeOwned, IgnoredAny};

use crate::dispatch::error;

#[derive(Debug)]
pub(crate) enum RecoveryParams {
    Inspect(RecoveryInspectParams),
    Submit(Box<RecoverySubmitParams>),
    Receipt(RecoveryReceiptParams),
}

pub(crate) const fn is_method(method: CurrentMethod) -> bool {
    matches!(
        method,
        CurrentMethod::RecoveryInspect
            | CurrentMethod::RecoverySubmit
            | CurrentMethod::RecoveryReceipt
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope<T> {
    #[serde(rename = "jsonrpc")]
    _jsonrpc: IgnoredAny,
    #[serde(rename = "id")]
    _id: IgnoredAny,
    #[serde(rename = "method")]
    _method: IgnoredAny,
    params: T,
    #[serde(rename = "v", default)]
    version: Option<u16>,
    #[serde(rename = "cred", default)]
    _credential: IgnoredAny,
}

pub(crate) fn decode(line: &str, method: CurrentMethod) -> Result<RecoveryParams, RpcError> {
    if line.len() > MAX_RECOVERY_REQUEST_BYTES {
        return Err(error(
            ErrorCode::InvalidParams,
            "recovery frame exceeds 4096 bytes",
        ));
    }
    if line.bytes().find(|byte| !byte.is_ascii_whitespace()) != Some(b'{') {
        return Err(error(
            ErrorCode::InvalidRequest,
            "recovery requires an object envelope",
        ));
    }
    let envelope = serde_json::from_str::<Envelope<Option<IgnoredAny>>>(line)
        .map_err(|_| error(ErrorCode::InvalidRequest, "invalid recovery envelope"))?;
    if envelope
        .version
        .is_some_and(|version| version != dudes_proto::CONTRACT_VERSION)
    {
        return Err(error(
            ErrorCode::InvalidRequest,
            "unsupported recovery contract version",
        ));
    }
    match method {
        CurrentMethod::RecoveryInspect => params(line).map(RecoveryParams::Inspect),
        CurrentMethod::RecoverySubmit => {
            params(line).map(|params| RecoveryParams::Submit(Box::new(params)))
        }
        CurrentMethod::RecoveryReceipt => params(line).map(RecoveryParams::Receipt),
        _ => Err(error(ErrorCode::InvalidRequest, "invalid recovery method")),
    }
}

fn params<T: DeserializeOwned>(line: &str) -> Result<T, RpcError> {
    serde_json::from_str::<Envelope<T>>(line)
        .map(|frame| frame.params)
        .map_err(|_| error(ErrorCode::InvalidParams, "invalid recovery params"))
}
