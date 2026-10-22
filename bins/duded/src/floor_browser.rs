//! A browser for tests: it holds a P-256 key as the Floor page does, enrols it with a link, and
//! signs each request and socket handshake the way the page does. Only tests and the `testing`
//! feature compile it, so the daemon carries no signing code of its own.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use dudes_proto::record::DeviceId;
use p256::ecdsa::signature::Signer as _;
use p256::ecdsa::{Signature, SigningKey};
use p256::pkcs8::EncodePublicKey as _;
use sha2::{Digest as _, Sha256};

use crate::floor_session::Presentation;

/// The four values a page puts on a request to show it is its own.
#[derive(Clone, Debug)]
pub struct Signed {
    /// The origin the page signed for.
    pub origin: String,
    /// The time in milliseconds, as decimal digits.
    pub time: String,
    /// The one-time request id.
    pub request: String,
    /// The signature.
    pub signature: String,
}

impl Signed {
    /// The HTTP header lines for these values, each ending in a line break.
    #[must_use]
    pub fn header_lines(&self) -> String {
        format!(
            "X-Dudes-Origin: {}\r\nX-Dudes-Time: {}\r\nX-Dudes-Request: {}\r\nX-Dudes-Signature: {}\r\n",
            self.origin, self.time, self.request, self.signature
        )
    }

    /// The socket protocol entries for these values, comma separated, to follow a route's own.
    #[must_use]
    pub fn protocol_entries(&self) -> String {
        format!(
            "dudes-sig.{}, dudes-time.{}, dudes-id.{}",
            self.signature, self.time, self.request
        )
    }
}

/// A browser's key and the counter that keeps its request ids apart.
pub struct Browser {
    signing: SigningKey,
    key: String,
    device: DeviceId,
    counter: AtomicU64,
}

impl Browser {
    /// A browser whose key comes from `seed`, so a test can name the same browser twice.
    #[must_use]
    pub fn seeded(seed: u64) -> Self {
        let mut counter = 0_u32;
        loop {
            let bytes = Sha256::digest(
                [
                    b"test browser".as_slice(),
                    &seed.to_be_bytes(),
                    &counter.to_be_bytes(),
                ]
                .concat(),
            );
            if let Ok(signing) = SigningKey::from_slice(&bytes) {
                return Self::with_key(signing);
            }
            counter += 1;
        }
    }

    /// A browser with a key of its own.
    #[must_use]
    pub fn new() -> Self {
        let mut seed = [0_u8; 8];
        getrandom::fill(&mut seed).expect("the operating system's entropy source");
        Self::seeded(u64::from_be_bytes(seed))
    }

    fn with_key(signing: SigningKey) -> Self {
        let der = signing
            .verifying_key()
            .to_public_key_der()
            .expect("a verifying key encodes as SPKI");
        let device = DeviceId::new(Sha256::digest(der.as_bytes()).into());
        Self {
            signing,
            key: URL_SAFE_NO_PAD.encode(der.as_bytes()),
            device,
            counter: AtomicU64::new(0),
        }
    }

    /// The device id this key will have once it is enrolled.
    #[must_use]
    pub const fn device(&self) -> DeviceId {
        self.device
    }

    /// The public key as the exchange carries it.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The exchange body for `link`, named `name`.
    #[must_use]
    pub fn enrolment(&self, link: &str, name: &str) -> String {
        serde_json::json!({ "link": link, "key": self.key, "name": name }).to_string()
    }

    /// The header lines that sign the exchange `body` for this key at `time_ms`, as the page does.
    #[must_use]
    pub fn enrolment_lines(&self, body: &str, origin: &str, time_ms: i64) -> String {
        self.sign("POST", "/floor/session", origin, body.as_bytes(), time_ms)
            .header_lines()
    }

    /// The time now in milliseconds, for a test that runs against the system clock.
    #[must_use]
    pub fn now_ms() -> i64 {
        i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("after 1970")
                .as_millis(),
        )
        .expect("the time fits")
    }

    /// Sign a request. The id is new every time.
    #[must_use]
    pub fn sign(
        &self,
        method: &str,
        target: &str,
        origin: &str,
        body: &[u8],
        time_ms: i64,
    ) -> Signed {
        let mut id = [0_u8; 16];
        id[..8].copy_from_slice(&self.device.bytes()[..8]);
        id[8..].copy_from_slice(&self.counter.fetch_add(1, Ordering::Relaxed).to_be_bytes());
        self.sign_with_id(method, target, origin, body, time_ms, id)
    }

    /// Sign a request under a chosen id, to send the same one twice.
    #[must_use]
    pub fn sign_with_id(
        &self,
        method: &str,
        target: &str,
        origin: &str,
        body: &[u8],
        time_ms: i64,
        id: [u8; 16],
    ) -> Signed {
        let device = self.device.to_string();
        let time = time_ms.to_string();
        let request = URL_SAFE_NO_PAD.encode(id);
        let body = Sha256::digest(body);
        let fields: [&[u8]; 8] = [
            b"dudes-floor-request-v1",
            device.as_bytes(),
            method.as_bytes(),
            target.as_bytes(),
            origin.as_bytes(),
            &body,
            time.as_bytes(),
            request.as_bytes(),
        ];
        let mut message = Vec::new();
        for field in fields {
            message.extend_from_slice(
                &u32::try_from(field.len())
                    .expect("a field fits")
                    .to_be_bytes(),
            );
            message.extend_from_slice(field);
        }
        let signature: Signature = self.signing.sign(&message);
        Signed {
            origin: origin.to_owned(),
            time,
            request,
            signature: URL_SAFE_NO_PAD.encode(signature.to_bytes()),
        }
    }

    /// A request ready for the registry to check, signed at `time_ms`.
    #[must_use]
    #[expect(
        clippy::too_many_arguments,
        reason = "a test helper that spells out every part of the request"
    )]
    pub fn presentation<'a>(
        &self,
        host: &'a str,
        origin: &'a str,
        cookie: &'a str,
        method: &'a str,
        target: &'a str,
        body: &'a [u8],
        time_ms: i64,
    ) -> Presentation<'a> {
        let signed = self.sign(method, target, origin, body, time_ms);
        Presentation::new(host, origin, cookie, method, target, body).signed(
            &signed.origin,
            &signed.time,
            &signed.request,
            &signed.signature,
        )
    }
}

impl Default for Browser {
    fn default() -> Self {
        Self::new()
    }
}

/// Where a page reads the time it signs with.
type Now = Arc<dyn Fn() -> i64 + Send + Sync>;

/// A signed-in page: its browser, and the cookie the Floor gave it. It signs with the system
/// clock unless a test gives it the clock its daemon runs on.
pub struct Page {
    /// The address of the Floor listener.
    pub addr: SocketAddr,
    /// The cookie value.
    pub cookie: String,
    /// The browser's key.
    pub browser: Browser,
    now: Now,
}

impl Page {
    /// A page for a cookie the Floor already gave `browser`.
    #[must_use]
    pub fn new(addr: SocketAddr, cookie: String, browser: Browser) -> Self {
        Self {
            addr,
            cookie,
            browser,
            now: Arc::new(Browser::now_ms),
        }
    }

    /// Sign with this clock, in milliseconds, instead of the system's. A test whose daemon runs
    /// on a fake clock gives its page the same one.
    #[must_use]
    pub fn on_clock(mut self, now: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        self.now = Arc::new(now);
        self
    }

    /// The time this page signs with now.
    #[must_use]
    pub fn now_ms(&self) -> i64 {
        (self.now)()
    }

    /// The values this page signs `method target` with, over `body`, at its time.
    #[must_use]
    pub fn signed(&self, method: &str, target: &str, body: &[u8]) -> Signed {
        self.browser
            .sign(method, target, &self.origin(), body, self.now_ms())
    }

    /// The origin this page signs for.
    #[must_use]
    pub fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The head of a signed socket handshake for `path`, offering `route_protocol` and the
    /// signature entries. The caller writes it to a connection and reads the answer.
    #[must_use]
    pub fn upgrade_head(&self, path: &str, route_protocol: &str) -> String {
        let signed = self.signed("GET", path, &[]);
        self.upgrade_head_with(path, route_protocol, Some(&signed))
    }

    /// A handshake with the values given, or none.
    #[must_use]
    pub fn upgrade_head_with(
        &self,
        path: &str,
        route_protocol: &str,
        signed: Option<&Signed>,
    ) -> String {
        let host = self.addr.to_string();
        let protocols = signed.map_or_else(
            || route_protocol.to_owned(),
            |signed| format!("{route_protocol}, {}", signed.protocol_entries()),
        );
        format!(
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Protocol: {protocols}\r\nCookie: dudes_floor_session={}\r\n\r\n",
            self.cookie
        )
    }
}
