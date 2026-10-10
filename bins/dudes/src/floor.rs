//! The two URLs a minted Floor session becomes, and the check that refuses anything else.
//!
//! # Why a CLI composes this at all
//!
//! Gives the operator `{base_url, bootstrap, expires_in_s}`. Nothing on the wire is
//! the URL itself, so somebody has to join the origin, the app's route and the fragment the
//! browser reads. Doing it here rather than in every operator's head is the whole of this verb.
//!
//! # This module launches nothing
//!
//! The fragment-bearing URL has one permitted disclosure: printed once by the requesting CLI.
//! It never leaves this process for another program.
//!
//! # Two URLs, named so that misusing one is visible
//!
//! [`FloorUrl::page`] carries no secret and may be printed, logged or repeated. `printed_once`
//! carries the bearer and goes only where the paragraph above says. They are separate fields rather
//! than one string plus a comment, because the whole of the leak rule is *which of these two a
//! caller reaches for*.
//!
//! # The two constants, and why they are pinned rather than shared
//!
//! [`BASE_PATH`] is the gateway's configured public root and [`FRAGMENT_PREFIX`] is what the
//! browser app slices the bearer out of. Their owners are `duded` and `floor/src/session.ts`, and
//! this crate can depend on neither: `dudes` is a client that links no daemon, and TypeScript is
//! not a dependency. Copying them here with `tests/floor_open.rs` asserting both against their
//! owning source is the arrangement that keeps one source of truth each *and* makes drift red —
//! moving them into `dudes-proto` would put two values on the contract that no wire message
//! carries.

use std::fmt;

use dudes_proto::rpc::floor::SessionIssueResult;

/// The gateway's configured public root, owned by `duded::floor_gateway::BASE_PATH`.
pub const BASE_PATH: &str = "/floor/";

/// The fragment the browser app reads the bearer out of, owned by `floor/src/session.ts`.
pub const FRAGMENT_PREFIX: &str = "#bootstrap=";

/// The scheme the loopback gateway serves. Plain HTTP is accepted only on loopback,
/// and there is no loopback TLS origin for this CLI to meet.
const SCHEME: &str = "http://";

/// The two URLs one issued session becomes.
///
/// **`Debug` is written by hand, not derived**, and that is the point of the type rather than a
/// detail of it. A derived `Debug` prints every field, so it hands the bearer to whatever the
/// formatting reaches — and it is reachable today: `expect_err` on a `Result<FloorUrl, _>` formats
/// the `Ok` value into its panic message, which the refusal tests below do on every failure. The
/// module header says `page` "may be printed, logged or repeated" and `printed_once` may not; a
/// derive is exactly the thing that ignores that sentence. `dudes_proto::rpc::floor`'s
/// `SessionIssueResult` redacts its own `bootstrap` the same way, for the same reason.
#[derive(Clone, PartialEq, Eq)]
pub struct FloorUrl {
    /// The address of the app's own page. Carries no secret.
    pub page: String,
    /// The fragment-bearing URL, which this CLI prints once.
    pub printed_once: String,
}

impl fmt::Debug for FloorUrl {
    /// `page` legible, `printed_once` redacted whole.
    ///
    /// Redacted whole rather than truncated: `printed_once` is `page` plus the fragment, so a
    /// prefix would disclose only what the neighbouring field already shows, and a suffix is the
    /// bearer. There is nothing in between worth printing.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FloorUrl")
            .field("page", &self.page)
            .field("printed_once", &"<redacted>")
            .finish()
    }
}

/// Why an answered `floor.session.issue` could not become a URL.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FloorError {
    /// The origin the daemon reported is not a loopback `http://host:port`.
    ///
    /// Invariant 10: *"No listener binds outside loopback."* A daemon answering anything else is
    /// not serving a Dudes Floor this CLI can vouch for, so the URL is refused rather than opened
    /// or printed. The value is echoed because it is the daemon's own configuration and carries no
    /// secret — the bearer is a separate field and never reaches this message.
    #[error(
        "the daemon reported the Floor origin {0}, which is not a loopback http://host:port; invariant 10 admits no other listener"
    )]
    NotLoopback(String),
}

/// The URLs for one issued session, or the reason there are none.
///
/// # Errors
///
/// [`FloorError::NotLoopback`] when `base_url` is not `http://` followed by a socket address whose
/// IP is loopback. The bearer's own shape is not re-checked here: `SessionIssueResult` refuses a
/// non-canonical one at deserialization (`dudes_proto::rpc::floor`), so a second check would be a
/// second copy of a rule the contract already owns.
pub fn urls(result: &SessionIssueResult) -> Result<FloorUrl, FloorError> {
    let page = page_url(&result.base_url)?;
    let printed_once = format!("{page}{FRAGMENT_PREFIX}{}", result.bootstrap);
    Ok(FloorUrl { page, printed_once })
}

/// `base_url` joined to the app's route, once the origin has been proved loopback.
fn page_url(base_url: &str) -> Result<String, FloorError> {
    let refuse = || FloorError::NotLoopback(base_url.to_owned());
    let authority = base_url.strip_prefix(SCHEME).ok_or_else(refuse)?;
    // **A socket address, not a host.** The daemon builds this origin as `http://{addr}` from the
    // listener's own `SocketAddr`, so an exact parse is what the daemon actually produces — and it
    // is what refuses `localhost` (a name a resolver decides, not an address), a bare host with no
    // port, and anything carrying a path, a query or credentials, because none of those parse.
    let address: std::net::SocketAddr = authority.parse().map_err(|_| refuse())?;
    if !address.ip().is_loopback() {
        return Err(refuse());
    }
    Ok(format!("{SCHEME}{address}{BASE_PATH}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A canonical bearer: 42 base64url characters and one of the 15 that encode a final 32nd byte.
    const BEARER: &str = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFg";

    fn issued(base_url: &str) -> SessionIssueResult {
        let value = serde_json::json!({
            "base_url": base_url,
            "bootstrap": BEARER,
            "expires_in_s": 60,
        });
        serde_json::from_value(value).expect("the contract accepts this result")
    }

    /// The composed URL is the gate's: origin, app route, fragment, bearer — in that order.
    #[test]
    fn the_printed_url_is_the_app_route_with_the_bearer_in_the_fragment() {
        let urls = urls(&issued("http://127.0.0.1:53211")).expect("a loopback origin");
        assert_eq!(urls.page, "http://127.0.0.1:53211/floor/");
        assert_eq!(
            urls.printed_once,
            format!("http://127.0.0.1:53211/floor/#bootstrap={BEARER}")
        );
    }

    /// The page URL is the half that may be repeated, so it must not carry the bearer.
    #[test]
    fn the_page_url_carries_no_bearer() {
        let urls = urls(&issued("http://127.0.0.1:53211")).expect("a loopback origin");
        assert!(
            !urls.page.contains(BEARER),
            "the page URL carries the bearer: {}",
            urls.page
        );
    }

    /// **Formatting the struct discloses no bearer, and still says which URL is which.**
    ///
    /// The firing control is restoring `#[derive(Debug)]`: the derive prints every field, so
    /// `printed_once` — and the bearer inside it — lands wherever the formatting goes. This is not
    /// a hypothetical sink. `Result::expect_err` requires `T: Debug` and formats the `Ok` value
    /// into its panic message, so the two refusal tests below would print the bearer to the test
    /// log on the day they failed.
    ///
    /// The `page` assertion is the control on the same string: a `Debug` that had simply stopped
    /// emitting field values would satisfy an absence check while telling a reader nothing.
    #[test]
    fn debug_redacts_the_bearer_and_keeps_the_page_legible() {
        let urls = urls(&issued("http://127.0.0.1:53211")).expect("a loopback origin");
        let shown = format!("{urls:?}");

        assert!(
            !shown.contains(BEARER),
            "the bearer reached a Debug rendering: {shown}"
        );
        assert!(
            shown.contains("<redacted>"),
            "the redaction is not visible as one: {shown}"
        );
        // The control, on the same rendering that just proved an absence.
        assert!(
            shown.contains(&urls.page),
            "the page is not legible, so the absence above proves nothing: {shown}"
        );
        assert!(shown.contains("FloorUrl"), "{shown}");
    }

    /// **The error renderings carry the daemon's own origin and never the bearer.**
    ///
    /// The other half of the no-bearer rule. `FloorError` holds `base_url`, which is configuration rather than a
    /// secret, so its derived `Debug` and its `thiserror` `Display` are both left alone. This pins
    /// that reading instead of asserting it in a comment, and checks both renderings, because
    /// `Display` is what reaches the operator's terminal and `Debug` is what reaches a panic
    /// message.
    #[test]
    fn the_error_renderings_carry_the_origin_and_no_bearer() {
        let error = urls(&issued("http://10.0.0.5:8080")).expect_err("not loopback");
        for shown in [format!("{error}"), format!("{error:?}")] {
            assert!(!shown.contains(BEARER), "{shown}");
            assert!(shown.contains("10.0.0.5:8080"), "{shown}");
        }
    }

    /// Every loopback origin the daemon can produce is accepted.
    ///
    /// This is the control row for [`a_non_loopback_origin_is_refused`]: deleting the
    /// `is_loopback` check turns that test red and leaves this one green, so both are run.
    #[test]
    fn a_loopback_origin_is_accepted() {
        for origin in [
            "http://127.0.0.1:53211",
            // 127.0.0.0/8 is loopback in full, not just its first address.
            "http://127.0.0.2:1",
            "http://[::1]:53211",
        ] {
            let urls = urls(&issued(origin)).unwrap_or_else(|error| panic!("{origin}: {error}"));
            assert!(
                urls.printed_once.starts_with(origin),
                "{origin} produced {}",
                urls.printed_once
            );
        }
    }

    /// Anything that is not a loopback `http://host:port` is refused, and the message says so.
    #[test]
    fn a_non_loopback_origin_is_refused() {
        for origin in [
            // A routable address: the case invariant 10 exists for.
            "http://10.0.0.5:8080",
            "http://0.0.0.0:53211",
            "http://[2001:db8::1]:53211",
            // A name, not an address: what it resolves to is a resolver's answer, not the
            // daemon's.
            "http://localhost:53211",
            // The wrong scheme, including one that merely starts with the right letters.
            "https://127.0.0.1:53211",
            "httpx://127.0.0.1:53211",
            "file:///etc/passwd",
            // Well-formed loopback, but not an authority this CLI can join a route onto.
            "http://127.0.0.1",
            "http://127.0.0.1:53211/floor/",
            "http://user@127.0.0.1:53211",
            "",
        ] {
            let error = urls(&issued(origin)).expect_err(origin);
            assert_eq!(error, FloorError::NotLoopback(origin.to_owned()));
            assert!(
                error.to_string().contains("invariant 10"),
                "the refusal does not cite the invariant: {error}"
            );
        }
    }

    /// The refusal names the origin and never the bearer.
    #[test]
    fn a_refusal_discloses_no_bearer() {
        let error = urls(&issued("http://10.0.0.5:8080")).expect_err("not loopback");
        let message = error.to_string();
        assert!(message.contains("10.0.0.5:8080"), "{message}");
        assert!(!message.contains(BEARER), "{message}");
    }
}
