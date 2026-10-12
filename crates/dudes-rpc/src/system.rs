//! `system.ping`: the one method this crate ships.
//!
//! It exists so the transport is provable without the method surface — framing, dispatch and the
//! seam can all be exercised end to end before a single stateful method is written — and because
//! the daemon's own singleton check is a `system.ping` against a socket that may or may not still
//! have a daemon behind it.
//!
//! It is also the one method a peer that has proved nothing may call, which
//! is what keeps a daemon probeable by a client that carries no credential at all.

use dudes_proto::rpc::RpcError;
use dudes_proto::rpc::system::PingResult;
use serde_json::Value;

use crate::dispatch::Call;

/// A `system.ping` handler that answers with whatever `state` reports.
///
/// A closure rather than a value: the lsn moves with every append, and a handler that answered with
/// a snapshot taken at registration would report a daemon that had stopped writing.
pub fn ping(
    state: impl Fn() -> PingResult + Send + Sync + 'static,
) -> impl Fn(Call<'_>) -> Result<Value, RpcError> + Send + Sync + 'static {
    move |_call| {
        serde_json::to_value(state()).map_err(|err| {
            crate::dispatch::error(
                dudes_proto::rpc::ErrorCode::InternalError,
                format!("could not encode the ping result: {err}"),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use dudes_proto::rpc::Method;
    use dudes_types::Lsn;

    use super::*;
    use crate::auth::PeerRole;

    /// The handler reports the state as it is at call time, not as it was at registration — which
    /// is the whole reason it takes a closure. Two calls across a change to the lsn is the control.
    #[test]
    fn ping_reports_the_state_at_call_time() {
        let lsn = std::sync::atomic::AtomicU64::new(1);
        let handler = ping(move || PingResult {
            version: "0.5.0".to_owned(),
            socket: "/run/duded.sock".into(),
            lsn: Lsn(lsn.fetch_add(1, std::sync::atomic::Ordering::SeqCst)),
        });

        let role = PeerRole::Unproven;
        let outbound = std::sync::Arc::new(crate::outbound::Outbound::sink());
        let call = || Call {
            goal_replacement: None,
            method: Method::SystemPing.into(),
            params: None,
            role: &role,
            outbound: &outbound,
            hang_up: crate::server::HangUp::never(),
        };

        let first: PingResult = serde_json::from_value(handler(call()).unwrap()).unwrap();
        let second: PingResult = serde_json::from_value(handler(call()).unwrap()).unwrap();
        assert_eq!(first.lsn, Lsn(1));
        assert_eq!(second.lsn, Lsn(2), "the second call saw the newer state");
        assert_eq!(first.version, "0.5.0");
    }
}
