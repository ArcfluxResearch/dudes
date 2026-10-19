//! The one place a dynamic Floor request is admitted.
//!
//! Every route that is not a static file (sign-in, every POST, the stream and the terminal) is
//! resolved by [`Route::of`] and admitted by [`admit`], and a route's handler can only be called
//! with what [`admit`] returns. A route cannot skip a check by forgetting it, because the checks
//! are not in the routes.
//!
//! Admission checks, in this order for a POST:
//! 1. the listener's exact Host and Origin;
//! 2. the session cookie and the session secret, which must both name one live session;
//! 3. the body: no NUL byte and valid UTF-8;
//! 4. the peer: the process on the other end of the connection is not an agent.
//!
//! Sign-in and the sockets have no session to check before the peer, so for them the peer comes
//! first and a refused agent never spends a one-time link or takes a socket slot. The route's
//! switch is part of resolving it: a route that is off is not a route. The head's shape (framing,
//! Content-Type, Content-Length against the route's cap) is checked by the parsers in the parent
//! module before any body is read.

use dudes_proto::FloorPeerRefusal;

use super::*;
use crate::floor_peer::Verdict;

/// Every dynamic path, whether or not its switch is on. A test holds this equal to the paths the
/// gateway source names, so a route added without being listed fails the build.
pub(super) const DYNAMIC_PATHS: &[&str] = &[
    SIGN_IN_PATH,
    STREAM_PATH,
    TERMINAL_PATH,
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
    Action(ActionRoute),
    Socket(SocketRoute),
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
            Kind::SignIn | Kind::Socket(_) => None,
        }
    }

    pub(super) const fn is_sign_in(self) -> bool {
        matches!(self.kind, Kind::SignIn)
    }

    pub(super) const fn socket(self) -> Option<SocketRoute> {
        match self.kind {
            Kind::Socket(route) => Some(route),
            Kind::SignIn | Kind::Action(_) => None,
        }
    }

    /// Whether the route carries a request body, and so a Content-Length to cap.
    pub(super) const fn has_body(self) -> bool {
        !matches!(self.kind, Kind::Socket(_))
    }

    /// Whether the route changes something, or gives the sender control of a terminal. Where the
    /// operating system cannot say who owns a connection, only the routes that do not are served.
    const fn acting(self) -> bool {
        match self.kind {
            Kind::SignIn | Kind::Socket(SocketRoute::Stream) => false,
            Kind::Socket(SocketRoute::Terminal) => true,
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

/// What a request has earned. Handlers take these and nothing less.
pub(super) enum Admitted {
    SignIn(SignIn),
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

impl Guard<'_> {
    /// Let a request through, or say why not. `endpoints` are the connection's two addresses, when
    /// the transport has them; without them the peer cannot be found, and the request is refused.
    pub(super) fn admit(
        self,
        route: Route,
        head: &Head,
        body: &[u8],
        endpoints: Option<Endpoints>,
    ) -> Result<Admitted, Response<'static>> {
        let forbidden = || error_response(403, "Forbidden");
        let sessions = self.sessions;
        if !sessions.authority_matches(&head.host, &head.origin) {
            return Err(forbidden());
        }
        match route.kind {
            Kind::SignIn => {
                clean_body(body)?;
                self.peer(route, endpoints)?;
                Ok(Admitted::SignIn(SignIn {
                    host: head.host.clone(),
                    origin: head.origin.clone(),
                }))
            }
            Kind::Socket(socket) => {
                self.peer(route, endpoints)?;
                let credentials = head.credentials.as_ref().ok_or_else(forbidden)?;
                let authorizer = sessions.authorizer();
                let (host, origin) = (&head.host, &head.origin);
                let (cookie, secret) = (&credentials.cookie, &credentials.secret);
                match socket {
                    SocketRoute::Stream => authorizer
                        .authorize_stream(host, origin, cookie, secret)
                        .map(SocketSession::Stream),
                    SocketRoute::Terminal => authorizer
                        .authorize_terminal(host, origin, cookie, secret)
                        .map(SocketSession::Terminal),
                }
                .map(Admitted::Socket)
                .ok_or_else(forbidden)
            }
            Kind::Action(action) => {
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
                clean_body(body)?;
                self.peer(route, endpoints)?;
                Ok(admitted)
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
