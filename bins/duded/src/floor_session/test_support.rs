//! What the session tests share: a browser that has joined, and the calls that sign for it.

use std::ops::Deref;

use super::{
    Exchange, ExchangeRefusal, FloorSessionAuthorizer, FloorSessions, Presentation, ReceiptSession,
    SessionIdentity, StreamSession,
};
use crate::floor_browser::Browser;

/// A browser that exchanged a link, with the key that signs for it.
pub(crate) struct Joined {
    pub(crate) exchange: Exchange,
    pub(crate) browser: Browser,
}

impl Deref for Joined {
    type Target = Exchange;

    fn deref(&self) -> &Exchange {
        &self.exchange
    }
}

impl Joined {
    /// The request the registry would see from this browser at `now_ms`, signed for `host`.
    pub(crate) fn present<'a>(
        &'a self,
        host: &'a str,
        origin: &'a str,
        now_ms: i64,
    ) -> Presentation<'a> {
        self.browser.presentation(
            host,
            origin,
            &self.exchange.cookie,
            "POST",
            "/floor/test",
            &[],
            now_ms,
        )
    }
}

impl FloorSessions {
    /// The registry's clock, in milliseconds. It reads the clock under the registry lock, so a test
    /// that watches clock reads takes its time before it starts watching.
    pub(crate) fn now_ms(&self) -> i64 {
        self.0.registry.lock_at_now().now.unix_millis()
    }

    /// Exchange `link` as a new browser with a key of its own.
    pub(crate) fn join(&self, host: &str, origin: &str, link: &str) -> Option<Joined> {
        self.join_as(host, origin, link, Browser::new(), "Chrome on macOS")
            .ok()
    }

    /// Exchange `link` as `browser`, naming it `name`.
    pub(crate) fn join_as(
        &self,
        host: &str,
        origin: &str,
        link: &str,
        browser: Browser,
        name: &str,
    ) -> Result<Joined, ExchangeRefusal> {
        let body = browser.enrolment(link, name);
        let exchange = self.exchange_signed(host, origin, &browser, &body)?;
        Ok(Joined { exchange, browser })
    }

    /// Send the exchange `body`, signed by `browser` at the registry's time.
    pub(crate) fn exchange_signed(
        &self,
        host: &str,
        origin: &str,
        browser: &Browser,
        body: &str,
    ) -> Result<Exchange, ExchangeRefusal> {
        self.exchange(&browser.presentation(
            host,
            origin,
            "",
            "POST",
            "/floor/session",
            body.as_bytes(),
            self.now_ms(),
        ))
    }

    /// Sign a new browser in and admit its first action, for a test that needs only a session.
    pub(crate) fn admitted_for_test(&self, host: &str, origin: &str) -> (SessionIdentity, Joined) {
        let link = self.issue().expect("a link");
        let who = self.join(host, origin, &link.bootstrap).expect("a device");
        let identity = self.enter(host, origin, &who).expect("an admitted action");
        (identity, who)
    }

    /// Admit a signed action at the registry's time.
    pub(crate) fn enter(&self, host: &str, origin: &str, who: &Joined) -> Option<SessionIdentity> {
        self.authorize_onboard(&who.present(host, origin, self.now_ms()))
            .ok()
    }

    /// Admit a check or a sign-out at the registry's time.
    pub(crate) fn standing(
        &self,
        host: &str,
        origin: &str,
        who: &Joined,
    ) -> Option<SessionIdentity> {
        self.authorize_standing(&who.present(host, origin, self.now_ms()))
            .ok()
    }

    /// Admit a receipt reader at the registry's time.
    pub(crate) fn receipt(&self, host: &str, origin: &str, who: &Joined) -> Option<ReceiptSession> {
        self.authorize_receipt(&who.present(host, origin, self.now_ms()))
            .ok()
    }
}

impl FloorSessionAuthorizer {
    fn now_ms(&self) -> i64 {
        self.0.registry.lock_at_now().now.unix_millis()
    }

    pub(crate) fn stream(&self, host: &str, origin: &str, who: &Joined) -> Option<StreamSession> {
        self.authorize_stream(&who.present(host, origin, self.now_ms()))
            .ok()
    }
}
