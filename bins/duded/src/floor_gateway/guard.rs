//! The one place a dynamic Floor request is admitted.
//!
//! Every route that is not a static file (sign-in, every POST, the stream, the terminal and the engine sign-in) is
//! resolved by [`Route::of`] and admitted by [`admit`], and a route's handler can only be called
//! with what [`admit`] returns. A route cannot skip a check by forgetting it, because the checks
//! are not in the routes.
//!
//! Admission checks, in this order for a POST that has a session:
//! 1. the listener's exact Host and Origin;
//! 2. the peer: no process on the other end of the connection is inside an agent, so a refused
//!    peer holding stolen credentials neither authenticates nor keeps the session alive;
//! 3. the session cookie, which must name one live device. A cookie that does not is answered at
//!    once, with the reason when the daemon remembers one, because a reason grants nothing;
//! 4. the signature, which must be that device's, made for this request, this origin and this
//!    minute, and not seen before. It covers the body, so the body is read first and must have no
//!    NUL byte and be valid UTF-8.
//!
//! Sign-in reads its body first, then checks the peer, and only then does the exchange spend the
//! one-time link, so a refused agent never spends it. A socket checks the peer before it takes a
//! slot. A check or a sign-out of the session checks the peer too, then looks the session up
//! without extending it: an unknown session is an answer, not a refusal, and the only one that
//! says so. The route's switch is part of resolving it: a route that is off is not a route. The head's shape (framing,
//! Content-Type, Content-Length against the route's cap) is checked by the parsers in the parent
//! module before any body is read.

use std::io;

use dudes_proto::FloorPeerRefusal;

use super::{
    ANSWER_PATH, ActionRoute, CHAT_CANCEL_PATH, CHAT_FILE_PATH, CHAT_SEND_PATH, Catalog, Clock,
    DEVICES_KEEP_PATH, DEVICES_PATH, DEVICES_REMOVE_PATH, DIFF_PATH, DeviceVerb, ENGINE_LOGIN_PATH,
    FloorSessions, GIT_PATH, GOAL_PATH, Handle, INSPECT_RECOVERY_PATH, LOOK_PATH,
    MEMORY_ENTRY_PATH, MEMORY_PAGE_PATH, ONBOARD_PATH, PAUSE_PATH, PROJECT_ADD_PATH,
    PROJECT_CHOOSE_PATH, PROJECT_PREVIEW_PATH, PROJECT_REMOVE_PATH, RECEIPT_RECOVERY_PATH,
    RENAME_PATH, RENEW_PATH, RESUME_PATH, RETIRE_PATH, ReceiptSession, RecoveryRoute, Response,
    SESSION_CHECK_PATH, SESSION_END_PATH, SETTINGS_PATH, SIGN_IN_PATH, STREAM_PATH,
    SUBMIT_RECOVERY_PATH, SessionIdentity, SocketAddr, SocketRoute, SocketSession, TERMINAL_PATH,
    TicketRoute, action_json, error_response,
};
use crate::floor_peer::Verdict;
use crate::floor_session::{Presentation, Refusal};

/// Every dynamic path, whether or not its switch is on. A test holds this equal to the paths the
/// gateway source names, so a route added without being listed fails the build.
pub(super) const DYNAMIC_PATHS: &[&str] = &[
    SIGN_IN_PATH,
    SESSION_CHECK_PATH,
    SESSION_END_PATH,
    STREAM_PATH,
    TERMINAL_PATH,
    ENGINE_LOGIN_PATH,
    INSPECT_RECOVERY_PATH,
    SUBMIT_RECOVERY_PATH,
    RECEIPT_RECOVERY_PATH,
    ONBOARD_PATH,
    ANSWER_PATH,
    PAUSE_PATH,
    RESUME_PATH,
    GOAL_PATH,
    RENEW_PATH,
    RENAME_PATH,
    LOOK_PATH,
    RETIRE_PATH,
    SETTINGS_PATH,
    PROJECT_PREVIEW_PATH,
    PROJECT_ADD_PATH,
    PROJECT_CHOOSE_PATH,
    PROJECT_REMOVE_PATH,
    GIT_PATH,
    DIFF_PATH,
    MEMORY_PAGE_PATH,
    MEMORY_ENTRY_PATH,
    CHAT_SEND_PATH,
    CHAT_CANCEL_PATH,
    CHAT_FILE_PATH,
    DEVICES_PATH,
    DEVICES_REMOVE_PATH,
    DEVICES_KEEP_PATH,
    "/floor/ticket",
    "/floor/ticket/edit",
    "/floor/ticket/assign",
    "/floor/ticket/accept",
    "/floor/ticket/stop",
    "/floor/ticket/remove",
    "/floor/ticket/read",
];

#[derive(Clone, Copy)]
enum Kind {
    SignIn,
    Standing(StandingRoute),
    Action(ActionRoute),
    Socket(SocketRoute),
}

/// The two routes that ask about the session itself rather than act for it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum StandingRoute {
    /// Is the session the page holds still known?
    Check,
    /// Sign the session out.
    End,
}

/// A dynamic route that is on. Only [`Route::of`] makes one.
#[derive(Clone, Copy)]
pub(super) struct Route {
    kind: Kind,
    path: &'static str,
}

impl Route {
    /// The dynamic route a request names, when its switch is on.
    pub(super) fn of(method: &str, path: &str, catalog: Catalog<'_>) -> Option<Self> {
        let path = DYNAMIC_PATHS.iter().copied().find(|known| *known == path)?;
        let kind = if method == "POST" && path == SIGN_IN_PATH {
            Kind::SignIn
        } else if method == "POST" && path == SESSION_CHECK_PATH {
            Kind::Standing(StandingRoute::Check)
        } else if method == "POST" && path == SESSION_END_PATH {
            Kind::Standing(StandingRoute::End)
        } else if let Some(route) = ActionRoute::of(method, path, catalog) {
            Kind::Action(route)
        } else {
            Kind::Socket(SocketRoute::of(method, path, catalog)?)
        };
        Some(Self { kind, path })
    }

    pub(super) const fn action(self) -> Option<ActionRoute> {
        match self.kind {
            Kind::Action(route) => Some(route),
            Kind::SignIn | Kind::Standing(_) | Kind::Socket(_) => None,
        }
    }

    pub(super) const fn standing(self) -> Option<StandingRoute> {
        match self.kind {
            Kind::Standing(route) => Some(route),
            Kind::SignIn | Kind::Action(_) | Kind::Socket(_) => None,
        }
    }

    pub(super) const fn is_sign_in(self) -> bool {
        matches!(self.kind, Kind::SignIn)
    }

    pub(super) const fn socket(self) -> Option<SocketRoute> {
        match self.kind {
            Kind::Socket(route) => Some(route),
            Kind::SignIn | Kind::Standing(_) | Kind::Action(_) => None,
        }
    }

    /// Whether the route carries a request body, and so a Content-Length to cap.
    pub(super) const fn has_body(self) -> bool {
        !matches!(self.kind, Kind::Socket(_))
    }

    /// Whether the route changes something, or gives the sender control of a terminal. Signing out
    /// changes something; asking about the session does not. Where the
    /// operating system cannot say who owns a connection, only the routes that do not are served.
    const fn acting(self) -> bool {
        match self.kind {
            Kind::SignIn
            | Kind::Standing(StandingRoute::Check)
            | Kind::Socket(SocketRoute::Stream) => false,
            Kind::Standing(StandingRoute::End)
            | Kind::Socket(SocketRoute::Terminal | SocketRoute::EngineLogin) => true,
            Kind::Action(route) => !matches!(
                route,
                ActionRoute::Git
                    | ActionRoute::Diff
                    | ActionRoute::Memory(_)
                    | ActionRoute::Recovery(RecoveryRoute::Inspect | RecoveryRoute::Receipt)
                    | ActionRoute::Ticket(TicketRoute::Read)
                    | ActionRoute::Devices(DeviceVerb::List)
            ),
        }
    }
}

/// What a page presents to show a request is its own: the origin it signed for, the time, a
/// one-time request id and the signature. Strings as sent; their form is checked at admission.
#[derive(Clone)]
pub(super) struct Signed {
    pub(super) origin: String,
    pub(super) time: String,
    pub(super) request: String,
    pub(super) signature: String,
}

/// A request head whose shape the parsers accepted.
#[derive(Clone)]
pub(super) struct Head {
    pub(super) host: String,
    pub(super) origin: String,
    /// The method and target exactly as the request line carried them. The signature covers both.
    pub(super) method: String,
    pub(super) target: String,
    /// The session cookie, when the head carried a well-formed one.
    pub(super) cookie: Option<String>,
    /// What the page presented, when it presented all four values.
    pub(super) signed: Option<Signed>,
    pub(super) content_length: usize,
    /// The WebSocket accept value for a socket upgrade; empty for everything else.
    pub(super) accept: String,
}

impl Head {
    /// The head as the registry checks it, over the body that came with it.
    pub(super) fn presentation<'a>(&'a self, cookie: &'a str, body: &'a [u8]) -> Presentation<'a> {
        let request = Presentation::new(
            &self.host,
            &self.origin,
            cookie,
            &self.method,
            &self.target,
            body,
        );
        match &self.signed {
            Some(signed) => request.signed(
                &signed.origin,
                &signed.time,
                &signed.request,
                &signed.signature,
            ),
            None => request,
        }
    }
}

/// The one-time link's request, admitted. The exchange itself follows.
pub(super) struct SignIn {
    pub(super) head: Head,
}

/// What a request has earned. Handlers take these and nothing less.
pub(super) enum Admitted {
    SignIn(SignIn),
    /// The session a check or a sign-out named, or `None` when the daemon does not know it.
    Standing(Option<SessionIdentity>),
    Session(SessionIdentity),
    Receipt(ReceiptSession),
    Socket(SocketSession),
}

/// Both ends of the connection: the client's and the listener's.
pub(super) type Endpoints = (SocketAddr, SocketAddr);

/// What admission reads, besides the request.
#[derive(Clone, Copy)]
pub(super) struct Guard<'a> {
    pub(super) catalog: Catalog<'a>,
    pub(super) sessions: &'a FloorSessions,
    pub(super) handle: &'a Handle,
    pub(super) clock: &'a (dyn Clock + Send + Sync),
}

/// Why a request was not admitted.
pub(super) enum Denied {
    /// Answer with this and close.
    Response(Response<'static>),
    /// The body never arrived in time, or the daemon is stopping: close without an answer.
    Closed,
    /// The connection failed while the body was read.
    Failed(io::Error),
}

impl From<Response<'static>> for Denied {
    fn from(response: Response<'static>) -> Self {
        Self::Response(response)
    }
}

/// An admitted request and the body that came with it.
pub(super) struct Admission {
    pub(super) admitted: Admitted,
    pub(super) body: Vec<u8>,
}

impl Guard<'_> {
    /// Let a request through, or say why not.
    ///
    /// `read_body` is called once, after the session has been shown to exist and before the body is
    /// checked, so a sender with no session is refused without the daemon waiting for its body.
    /// `endpoints` are the connection's two addresses, when the transport has them; without them
    /// the peer cannot be found, and the request is refused.
    pub(super) fn admit(
        self,
        route: Route,
        head: &Head,
        read_body: impl FnOnce() -> Result<Vec<u8>, Denied>,
        endpoints: Option<Endpoints>,
    ) -> Result<Admission, Denied> {
        let forbidden = || Denied::Response(error_response(403, "Forbidden"));
        let sessions = self.sessions;
        if !sessions.authority_matches(&head.host, &head.origin) {
            return Err(forbidden());
        }
        match route.kind {
            Kind::SignIn => {
                let body = read_body()?;
                clean_body(&body)?;
                self.peer(route, endpoints)?;
                Ok(Admission {
                    admitted: Admitted::SignIn(SignIn { head: head.clone() }),
                    body,
                })
            }
            Kind::Standing(_) => {
                self.peer(route, endpoints)?;
                let body = read_body()?;
                clean_body(&body)?;
                let identity = match head.cookie.as_deref() {
                    None => None,
                    Some(cookie) => {
                        match sessions.authorize_standing(&head.presentation(cookie, &body)) {
                            Ok(identity) => Some(identity),
                            Err(Refusal::Signature) => return Err(signature_refused()),
                            Err(Refusal::Gone | Refusal::Busy) => None,
                        }
                    }
                };
                Ok(Admission {
                    admitted: Admitted::Standing(identity),
                    body,
                })
            }
            Kind::Socket(socket) => {
                self.peer(route, endpoints)?;
                let cookie = head.cookie.as_deref().ok_or_else(forbidden)?;
                let authorizer = sessions.authorizer();
                let request = head.presentation(cookie, &[]);
                let admitted = match socket {
                    SocketRoute::Stream => authorizer
                        .authorize_stream(&request)
                        .map(SocketSession::Stream),
                    SocketRoute::Terminal => authorizer
                        .authorize_terminal(&request)
                        .map(SocketSession::Terminal),
                    SocketRoute::EngineLogin => authorizer
                        .authorize_login(&request)
                        .map(SocketSession::Login),
                }
                .map(Admitted::Socket)
                .map_err(|refusal| self.refused(head, refusal))?;
                Ok(Admission {
                    admitted,
                    body: Vec::new(),
                })
            }
            Kind::Action(action) => {
                self.peer(route, endpoints)?;
                let cookie = head.cookie.as_deref().ok_or_else(forbidden)?;
                let reads_only = action == ActionRoute::Recovery(RecoveryRoute::Receipt);
                if !sessions.knows(cookie, !reads_only) {
                    return Err(self.refused(head, Refusal::Gone));
                }
                let body = read_body()?;
                if action != ActionRoute::ChatFile {
                    clean_body(&body)?;
                }
                let request = head.presentation(cookie, &body);
                let admitted = if action == ActionRoute::Recovery(RecoveryRoute::Receipt) {
                    sessions.authorize_receipt(&request).map(Admitted::Receipt)
                } else {
                    sessions.authorize_onboard(&request).map(Admitted::Session)
                }
                .map_err(|refusal| self.refused(head, refusal))?;
                Ok(Admission { admitted, body })
            }
        }
    }

    /// The answer for a request that earned no device. A signature that did not verify is its own
    /// answer. A cookie that names no live device is answered with the reason when it names one
    /// that was removed, expired or replaced, so the page can tell the person. The cookie alone
    /// earns that, and nothing else about the device is said.
    fn refused(self, head: &Head, refusal: Refusal) -> Denied {
        if refusal == Refusal::Signature {
            return signature_refused();
        }
        let gone = (refusal == Refusal::Gone)
            .then(|| {
                head.cookie
                    .as_deref()
                    .and_then(|cookie| self.sessions.gone_reason(cookie))
            })
            .flatten();
        Denied::Response(match gone {
            Some(reason) => action_json(
                403,
                "Forbidden",
                &serde_json::json!({ "session": "gone", "reason": reason.code() }),
            ),
            None => error_response(403, "Forbidden"),
        })
    }

    /// Find the process on the other end of the connection and refuse it when it is inside an
    /// agent, or cannot be found. The refusal is journaled, at most once a minute per agent.
    fn peer(self, route: Route, endpoints: Option<Endpoints>) -> Result<(), Response<'static>> {
        let verdict = endpoints.map_or(
            Verdict::Refused {
                reason: FloorPeerRefusal::NoOwner,
                agent: None,
            },
            |(client, server)| self.catalog.peer.check(client, server),
        );
        let Verdict::Refused { reason, agent } = verdict else {
            return Ok(());
        };
        if reason == FloorPeerRefusal::Unsupported && !route.acting() {
            return Ok(());
        }
        if self.catalog.peer.should_journal(agent, self.clock.now()) {
            self.handle.floor_peer_refused(route.path, agent, reason);
        }
        Err(error_response(403, "Forbidden"))
    }
}

/// The answer for a request from a live device that did not prove it: the page retries once with a
/// new time and id, and a second refusal is an error it shows, not a sign-out.
fn signature_refused() -> Denied {
    Denied::Response(action_json(
        401,
        "Unauthorized",
        &serde_json::json!({ "signature": "refused" }),
    ))
}

/// A body with a NUL byte or invalid UTF-8 is not something any route's parser should see.
fn clean_body(body: &[u8]) -> Result<(), Response<'static>> {
    if body.contains(&0) || std::str::from_utf8(body).is_err() {
        return Err(error_response(400, "Bad Request"));
    }
    Ok(())
}
