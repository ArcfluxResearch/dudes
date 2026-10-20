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
//! 3. the session cookie and the session secret, which must both name one live session;
//! 4. the body: no NUL byte and valid UTF-8.
//!
//! A proof of the daemon to the page needs no session and no secret, since it is asked for before
//! the secret is sent: it checks the Host and Origin and the peer, and answers every cookie in the
//! same shape.
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
    ANSWER_PATH, ActionRoute, CHAT_CANCEL_PATH, CHAT_SEND_PATH, Catalog, Clock, DIFF_PATH,
    ENGINE_LOGIN_PATH, FloorSessions, GIT_PATH, GOAL_PATH, Handle, INSPECT_RECOVERY_PATH,
    LOOK_PATH, MEMORY_ENTRY_PATH, MEMORY_PAGE_PATH, ONBOARD_PATH, PAUSE_PATH, PROJECT_ADD_PATH,
    PROJECT_PREVIEW_PATH, PROJECT_REMOVE_PATH, RECEIPT_RECOVERY_PATH, RENAME_PATH, RENEW_PATH,
    RESUME_PATH, RETIRE_PATH, ReceiptSession, RecoveryRoute, Response, SESSION_CHECK_PATH,
    SESSION_END_PATH, SESSION_PROVE_PATH, SETTINGS_PATH, SIGN_IN_PATH, STREAM_PATH,
    SUBMIT_RECOVERY_PATH, SessionIdentity, SocketAddr, SocketRoute, SocketSession, TERMINAL_PATH,
    TicketRoute, error_response,
};
use crate::floor_peer::Verdict;

/// Every dynamic path, whether or not its switch is on. A test holds this equal to the paths the
/// gateway source names, so a route added without being listed fails the build.
pub(super) const DYNAMIC_PATHS: &[&str] = &[
    SIGN_IN_PATH,
    SESSION_CHECK_PATH,
    SESSION_END_PATH,
    SESSION_PROVE_PATH,
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
    PROJECT_REMOVE_PATH,
    GIT_PATH,
    DIFF_PATH,
    MEMORY_PAGE_PATH,
    MEMORY_ENTRY_PATH,
    CHAT_SEND_PATH,
    CHAT_CANCEL_PATH,
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
    Proof,
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
        } else if method == "POST" && path == SESSION_PROVE_PATH {
            Kind::Proof
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
            Kind::SignIn | Kind::Proof | Kind::Standing(_) | Kind::Socket(_) => None,
        }
    }

    pub(super) const fn standing(self) -> Option<StandingRoute> {
        match self.kind {
            Kind::Standing(route) => Some(route),
            Kind::SignIn | Kind::Proof | Kind::Action(_) | Kind::Socket(_) => None,
        }
    }

    pub(super) const fn is_sign_in(self) -> bool {
        matches!(self.kind, Kind::SignIn)
    }

    pub(super) const fn is_proof(self) -> bool {
        matches!(self.kind, Kind::Proof)
    }

    pub(super) const fn socket(self) -> Option<SocketRoute> {
        match self.kind {
            Kind::Socket(route) => Some(route),
            Kind::SignIn | Kind::Proof | Kind::Standing(_) | Kind::Action(_) => None,
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
            | Kind::Proof
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
            ),
        }
    }
}

/// The secret the page sent, beside the cookie the browser sent.
pub(super) struct Credentials {
    pub(super) cookie: String,
    pub(super) secret: String,
}

/// A request head whose shape the parsers accepted.
pub(super) struct Head {
    pub(super) host: String,
    pub(super) origin: String,
    /// The session cookie, when the head carried a well-formed one. Only a proof reads it alone;
    /// every other route that acts reads it with the secret, in `credentials`.
    pub(super) cookie: Option<String>,
    pub(super) credentials: Option<Credentials>,
    pub(super) content_length: usize,
    /// The WebSocket accept value for a socket upgrade; empty for everything else.
    pub(super) accept: String,
}

/// The one-time link's request, admitted. The exchange itself follows.
pub(super) struct SignIn {
    pub(super) host: String,
    pub(super) origin: String,
}

/// A request for the daemon's proof of itself, with the cookie it named. The cookie is only a
/// name: it is never checked here, so a request without one is answered like one for a session the
/// daemon does not know.
pub(super) struct ProofAsk {
    pub(super) cookie: Option<String>,
}

/// What a request has earned. Handlers take these and nothing less.
pub(super) enum Admitted {
    SignIn(SignIn),
    Proof(ProofAsk),
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
                    admitted: Admitted::SignIn(SignIn {
                        host: head.host.clone(),
                        origin: head.origin.clone(),
                    }),
                    body,
                })
            }
            Kind::Proof => {
                self.peer(route, endpoints)?;
                let body = read_body()?;
                clean_body(&body)?;
                Ok(Admission {
                    admitted: Admitted::Proof(ProofAsk {
                        cookie: head.cookie.clone(),
                    }),
                    body,
                })
            }
            Kind::Standing(_) => {
                self.peer(route, endpoints)?;
                let body = read_body()?;
                clean_body(&body)?;
                let identity = head.credentials.as_ref().and_then(|credentials| {
                    sessions.authorize_standing(
                        &head.host,
                        &head.origin,
                        &credentials.cookie,
                        &credentials.secret,
                    )
                });
                Ok(Admission {
                    admitted: Admitted::Standing(identity),
                    body,
                })
            }
            Kind::Socket(socket) => {
                self.peer(route, endpoints)?;
                let credentials = head.credentials.as_ref().ok_or_else(forbidden)?;
                let authorizer = sessions.authorizer();
                let (host, origin) = (&head.host, &head.origin);
                let (cookie, secret) = (&credentials.cookie, &credentials.secret);
                let admitted = match socket {
                    SocketRoute::Stream => authorizer
                        .authorize_stream(host, origin, cookie, secret)
                        .map(SocketSession::Stream),
                    SocketRoute::Terminal => authorizer
                        .authorize_terminal(host, origin, cookie, secret)
                        .map(SocketSession::Terminal),
                    SocketRoute::EngineLogin => authorizer
                        .authorize_login(host, origin, cookie, secret)
                        .map(SocketSession::Login),
                }
                .map(Admitted::Socket)
                .ok_or_else(forbidden)?;
                Ok(Admission {
                    admitted,
                    body: Vec::new(),
                })
            }
            Kind::Action(action) => {
                self.peer(route, endpoints)?;
                let credentials = head.credentials.as_ref().ok_or_else(forbidden)?;
                let (host, origin) = (&head.host, &head.origin);
                let (cookie, secret) = (&credentials.cookie, &credentials.secret);
                let admitted = if action == ActionRoute::Recovery(RecoveryRoute::Receipt) {
                    sessions
                        .authorize_receipt(host, origin, cookie, secret)
                        .map(Admitted::Receipt)
                } else {
                    sessions
                        .authorize_onboard(host, origin, cookie, secret)
                        .map(Admitted::Session)
                }
                .ok_or_else(forbidden)?;
                let body = read_body()?;
                clean_body(&body)?;
                Ok(Admission { admitted, body })
            }
        }
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

/// A body with a NUL byte or invalid UTF-8 is not something any route's parser should see.
fn clean_body(body: &[u8]) -> Result<(), Response<'static>> {
    if body.contains(&0) || std::str::from_utf8(body).is_err() {
        return Err(error_response(400, "Bad Request"));
    }
    Ok(())
}
