//! The daemon-wide live secret set and the matcher over it.
//!
//! One [`Redactor`] is shared by every reader of agent text. Its handle exposes a scrub and the
//! calls that change the set, and nothing that reads a secret back: the agent-token plaintext
//! the supervisor mints is added here by the mint and cannot be taken out again, so a caller that
//! holds a handle can hide a value but never learn one.
//!
//! **What matches.** A secret matches as written, as lower- or upper-case hex, percent-encoded
//! (the RFC 3986 set, the `encodeURIComponent` set and the `+`-for-space form), and as base64 or
//! base64url at each of the three alignments it can take inside a longer base64 string, with the
//! edge characters that depend on neighbouring bytes trimmed. A match becomes [`REDACTED`]. A
//! secret that is split, reversed or compressed is not matched.
//!
//! **Order.** Text is matched as written, then stripped of terminal escapes, then matched again,
//! so neither an escape that swallows a character nor an escape inside a secret hides it. Callers
//! that cut text scrub first and cut afterwards: a cut inside a secret would keep its first part.
//!
//! **Streams.** [`StreamScrubber`] holds back the tail of its buffer for as long as that tail is a
//! prefix of some form in the set, so a known value is never released in part. Ordinary text has
//! no such tail and is released at once. A completed match is held too while a longer form could
//! still continue it. The hold never exceeds the longest form: past that, text is replaced and
//! released, so a run of one repeated byte against a secret made of it costs time linear in its
//! length, and the only part of such a run that can leave is a fragment of the secret shorter
//! than it.
//!
//! **Limits.** A secret longer than 64 KiB is matched as written only. A source holds at most
//! 4,096 secrets, secret-named values first; any beyond that are dropped and the first time this
//! happens is logged without the values.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::Bound;
use std::sync::{Arc, Mutex, PoisonError};

use aho_corasick::{AhoCorasick, AhoCorasickKind, MatchKind};
use dudes_providers::launch::EnvVar;
use dudes_types::{AgentId, TurnId};

/// What a match is replaced with.
pub const REDACTED: &str = "(redacted)";

/// The shortest secret the matcher accepts. A shorter value would match ordinary words.
const MIN_SECRET_BYTES: usize = 8;
/// The shortest base64 form worth matching once its alignment-dependent edges are trimmed.
const MIN_BASE64_FORM: usize = 8;
/// The longest secret that gets every encoded form. A longer one is held as written only, so one
/// huge value cannot make the set unbounded. It covers a PEM private key with room to spare.
const MAX_ENCODED_SECRET_BYTES: usize = 64 * 1024;
/// The most secrets one source may hold, far above any real environment. The rest are dropped,
/// and the values handed first are the ones kept.
const MAX_SECRETS_PER_SOURCE: usize = 4096;
/// The longest unfinished escape sequence a stream carries to the next piece. Past it the
/// sequence is text.
const ESCAPE_CARRY_BYTES: usize = 4 * 1024;
/// A kept tail at least this long that never completed is redacted at the end of a stream. A
/// shorter one is released: it is too short to name its secret.
const MIN_PARTIAL_REDACTION_BYTES: usize = 8;

/// The daemon-wide live secret set. Cheap to clone; every clone shares one set.
#[derive(Clone)]
pub struct Redactor {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    sources: BTreeMap<String, BTreeSet<String>>,
    holders: BTreeSet<Holder>,
    matcher: Arc<Matcher>,
    generation: u64,
    cap_logged: bool,
}

impl fmt::Debug for Redactor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.lock();
        f.debug_struct("Redactor")
            .field("sources", &inner.sources.len())
            .field("forms", &inner.matcher.forms.len())
            .finish()
    }
}

impl Redactor {
    /// An empty set. It strips terminal escapes and redacts nothing else.
    ///
    /// Only tests build one: a daemon shares the set its child sink owns, so a reader that was
    /// never wired to it cannot compile.
    #[cfg(any(test, feature = "testing"))]
    #[expect(
        clippy::new_without_default,
        reason = "a Default would let a missed wiring compile"
    )]
    #[must_use]
    pub fn new() -> Self {
        Self::for_run()
    }

    /// The set a daemon run starts with. Only the owner of a run makes one; every reader is
    /// handed a clone of it.
    ///
    /// One production caller makes the run's set, the child sink in the daemon; a test in this
    /// crate's `tests/` fails when a second appears.
    #[must_use]
    pub fn for_run() -> Self {
        Self {
            inner: Arc::default(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn matcher(&self) -> Arc<Matcher> {
        Arc::clone(&self.lock().matcher)
    }

    /// A number that changes whenever the set does. A cached scrub is good while it is the same.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.lock().generation
    }

    /// Replace everything `source` holds with `values`. An empty `values` ends the source.
    ///
    /// A value stays in the set until the last source naming it has ended. Past
    /// [`MAX_SECRETS_PER_SOURCE`] values the first ones given are kept, so a caller lists its most
    /// certain secrets first.
    pub fn set_source<I>(&self, source: &str, values: I)
    where
        I: IntoIterator<Item = String>,
    {
        self.lock().set_source(source, values);
    }

    /// End `source`.
    pub fn clear_source(&self, source: &str) {
        self.set_source(source, std::iter::empty());
    }

    /// Hold the secrets in the environment `holder` launched with, until [`Self::release`].
    ///
    /// The set is rebuilt at each launch, so a value that changed between two launches is held
    /// for as long as a live child still has it.
    pub fn hold_env(&self, holder: Holder, vars: &[EnvVar]) {
        let values = env_secrets(vars);
        let mut inner = self.lock();
        inner.set_source(&holder.source(), values);
        inner.holders.insert(holder);
    }

    /// The holder's child is gone: its environment values stop being held, unless another source
    /// still names them.
    pub fn release(&self, holder: Holder) {
        let mut inner = self.lock();
        inner.holders.remove(&holder);
        inner.set_source(&holder.source(), std::iter::empty());
    }

    /// `text` with every known secret replaced and without terminal escape sequences.
    #[must_use]
    pub fn scrub<'a>(&self, text: &'a str) -> Cow<'a, str> {
        scrub_with(&self.matcher(), text)
    }

    /// Scrub only while this exact child's environment is held, including an empty environment.
    /// The same captured matcher runs before and after normalization; the caller must establish
    /// that the input belongs to this holder before using it.
    #[must_use]
    pub fn scrub_held(
        &self,
        holder: Holder,
        text: &str,
        normalize: impl FnOnce(&str) -> String,
    ) -> Option<String> {
        let matcher = {
            let inner = self.lock();
            if !inner.holders.contains(&holder) {
                return None;
            }
            Arc::clone(&inner.matcher)
        };
        let normalized = normalize(&scrub_with(&matcher, text));
        Some(scrub_with(&matcher, &normalized).into_owned())
    }

    /// A scrubber for one stream of text.
    #[must_use]
    pub fn stream(&self) -> StreamScrubber {
        StreamScrubber {
            redactor: self.clone(),
            raw: Hold::default(),
            escape: String::new(),
            clean: Hold::default(),
        }
    }
}

impl Inner {
    fn set_source<I>(&mut self, source: &str, values: I)
    where
        I: IntoIterator<Item = String>,
    {
        let mut kept = BTreeSet::new();
        let mut dropped = 0usize;
        for value in values {
            if value.len() < MIN_SECRET_BYTES {
                continue;
            }
            if kept.len() < MAX_SECRETS_PER_SOURCE {
                kept.insert(value);
            } else if !kept.contains(&value) {
                dropped += 1;
            }
        }
        let values = kept;
        if dropped > 0 && !std::mem::replace(&mut self.cap_logged, true) {
            tracing::warn!(
                source,
                dropped,
                kept = MAX_SECRETS_PER_SOURCE,
                "a secret source is over its cap; the values beyond it are not scrubbed"
            );
        }
        let changed = if values.is_empty() {
            self.sources.remove(source).is_some()
        } else if self.sources.get(source) == Some(&values) {
            false
        } else {
            self.sources.insert(source.to_owned(), values);
            true
        };
        if changed {
            self.matcher = Arc::new(Matcher::of(self.sources.values().flatten()));
            self.generation += 1;
        }
    }
}

fn scrub_with<'a>(matcher: &Matcher, text: &'a str) -> Cow<'a, str> {
    let raw = matcher
        .replace(text)
        .map_or(Cow::Borrowed(text), Cow::Owned);
    let stripped = strip_ansi(raw);
    match matcher.replace(&stripped) {
        Some(replaced) => Cow::Owned(replaced),
        None => stripped,
    }
}

/// Who launched a child whose environment is held.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Holder {
    /// A managed child, for the turn it runs.
    Turn(TurnId),
    /// An attached terminal's child, for the agent it serves.
    Pty(AgentId),
}

impl Holder {
    fn source(self) -> String {
        match self {
            Self::Turn(turn) => format!("child-turn:{turn}"),
            Self::Pty(agent) => format!("child-pty:{agent}"),
        }
    }
}

/// Scrubs text that arrives in pieces.
///
/// Each [`Self::push`] answers the part of the stream that can no longer become a secret, and
/// keeps the rest. [`Self::finish`] answers what is kept. Two passes run in turn: the first over
/// the text as written, so an escape that swallows a character cannot hide a secret's start, and
/// the second over the text without its escapes, so an escape inside a secret cannot split it.
pub struct StreamScrubber {
    redactor: Redactor,
    raw: Hold,
    /// An escape sequence the last piece began and did not finish.
    escape: String,
    clean: Hold,
}

impl fmt::Debug for StreamScrubber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamScrubber")
            .field(
                "held",
                &(self.raw.text.len() + self.escape.len() + self.clean.text.len()),
            )
            .finish_non_exhaustive()
    }
}

impl StreamScrubber {
    /// Add `piece` and answer the text that is now safe to release.
    pub fn push(&mut self, piece: &str) -> String {
        let matcher = self.redactor.matcher();
        let released = self.raw.push(&matcher, piece);
        self.strip_and_hold(&matcher, &released)
    }

    /// Release what is kept. A kept tail of at least eight bytes that begins some secret is
    /// replaced, since it names the secret's start; a shorter one goes out as written.
    pub fn finish(&mut self) -> String {
        let matcher = self.redactor.matcher();
        let released = self.raw.finish(&matcher);
        let mut out = self.strip_and_hold(&matcher, &released);
        self.escape.clear();
        out.push_str(&self.clean.finish(&matcher));
        out
    }

    fn strip_and_hold(&mut self, matcher: &Matcher, released: &str) -> String {
        let mut input = std::mem::take(&mut self.escape);
        input.push_str(released);
        let (clean, incomplete) = split_ansi(&input);
        incomplete.clone_into(&mut self.escape);
        self.clean.push(matcher, &clean)
    }
}

/// Text kept as written because its end could still become a secret.
#[derive(Default)]
struct Hold {
    text: String,
}

impl Hold {
    fn push(&mut self, matcher: &Matcher, piece: &str) -> String {
        self.text.push_str(piece);
        if matcher.forms.is_empty() {
            return std::mem::take(&mut self.text);
        }
        let spans = matcher.spans(&self.text);
        let mut cut = matcher.hold_start(&self.text);
        // A match that reaches into the kept tail may be the start of a longer secret.
        if let Some(&(start, _)) = spans.iter().find(|&&(_, end)| end > cut) {
            cut = cut.min(start);
        }
        // Never keep more than the longest form: a run of overlapping matches would otherwise be
        // held and rescanned whole on every push.
        let mut floor = self.text.len().saturating_sub(matcher.longest);
        while !self.text.is_char_boundary(floor) {
            floor += 1;
        }
        cut = cut.max(floor);
        let kept = self.text.split_off(cut);
        let head = std::mem::replace(&mut self.text, kept);
        let in_head: Vec<_> = spans
            .iter()
            .filter(|&&(start, _)| start < cut)
            .map(|&(start, end)| (start, end.min(cut)))
            .collect();
        render(&head, &in_head)
    }

    fn finish(&mut self, matcher: &Matcher) -> String {
        let text = std::mem::take(&mut self.text);
        let mut spans = matcher.spans(&text);
        let cut = matcher.hold_start(&text);
        if text.len() - cut >= MIN_PARTIAL_REDACTION_BYTES {
            spans.push((cut, text.len()));
            spans = merge(spans);
        }
        render(&text, &spans)
    }
}

#[derive(Default)]
struct Matcher {
    automaton: Option<AhoCorasick>,
    forms: BTreeSet<String>,
    longest: usize,
}

impl Matcher {
    fn of<'a>(secrets: impl Iterator<Item = &'a String>) -> Self {
        let mut forms = BTreeSet::new();
        for secret in secrets {
            add_forms(secret, &mut forms);
        }
        let longest = forms.iter().map(String::len).max().unwrap_or(0);
        let automaton = (!forms.is_empty()).then(|| {
            // The automatic choice builds a DFA, which takes quadratic time on a long repetitive
            // secret; the contiguous NFA builds in linear time and searches as fast here.
            AhoCorasick::builder()
                .match_kind(MatchKind::Standard)
                .kind(Some(AhoCorasickKind::ContiguousNFA))
                .build(&forms)
                .expect("the forms are few and non-empty, far under the automaton's limits")
        });
        Self {
            automaton,
            forms,
            longest,
        }
    }

    /// Every stretch of `text` some form covers, overlaps joined.
    fn spans(&self, text: &str) -> Vec<(usize, usize)> {
        let Some(automaton) = &self.automaton else {
            return Vec::new();
        };
        merge(
            automaton
                .find_overlapping_iter(text)
                .map(|found| (found.start(), found.end()))
                .collect(),
        )
    }

    /// `text` with every form replaced, or `None` when none occurs.
    fn replace(&self, text: &str) -> Option<String> {
        let spans = self.spans(text);
        (!spans.is_empty()).then(|| render(text, &spans))
    }

    /// Where the longest suffix of `text` that some longer form starts with begins, or the length
    /// of `text` when there is none. A suffix that is itself a whole form counts when a longer
    /// form continues it.
    fn hold_start(&self, text: &str) -> usize {
        if self.forms.is_empty() {
            return text.len();
        }
        let floor = text.len().saturating_sub(self.longest.saturating_sub(1));
        for start in floor..text.len() {
            if !text.is_char_boundary(start) {
                continue;
            }
            let tail = &text[start..];
            if self
                .forms
                .range::<str, _>((Bound::Included(tail), Bound::Unbounded))
                .take_while(|form| form.starts_with(tail))
                .any(|form| form.len() > tail.len())
            {
                return start;
            }
        }
        text.len()
    }
}

fn merge(mut spans: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    spans.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
    for (start, end) in spans {
        match merged.last_mut() {
            Some((_, close)) if start <= *close => *close = (*close).max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// `text` with each of the merged, ordered `spans` replaced.
fn render(text: &str, spans: &[(usize, usize)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for &(start, end) in spans {
        if end > text.len() {
            break;
        }
        out.push_str(&text[copied..start]);
        out.push_str(REDACTED);
        copied = end;
    }
    out.push_str(&text[copied..]);
    out
}

fn add_forms(secret: &str, forms: &mut BTreeSet<String>) {
    forms.insert(secret.to_owned());
    if secret.len() > MAX_ENCODED_SECRET_BYTES {
        return;
    }
    let secret = secret.as_bytes();
    forms.insert(hex(secret, false));
    forms.insert(hex(secret, true));
    for upper in [false, true] {
        for (keep, plus) in [
            (rfc3986_unreserved as fn(u8) -> bool, false),
            (uri_component_unreserved, false),
            (rfc3986_unreserved, true),
        ] {
            let encoded = percent(secret, upper, keep, plus);
            if encoded.as_bytes() != secret {
                forms.insert(encoded);
            }
        }
    }
    for lead in 0..3usize {
        let mut padded = vec![0u8; lead];
        padded.extend_from_slice(secret);
        let encoded = base64(&padded);
        let front = [0, 2, 3][lead];
        let back = usize::from(!(lead + secret.len()).is_multiple_of(3));
        let Some(form) = encoded.get(front..encoded.len().saturating_sub(back)) else {
            continue;
        };
        if form.len() < MIN_BASE64_FORM {
            continue;
        }
        forms.insert(form.to_owned());
        forms.insert(form.replace('+', "-").replace('/', "_"));
    }
}

fn hex(bytes: &[u8], upper: bool) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        if upper {
            write!(out, "{byte:02X}").expect("formatting a String");
        } else {
            write!(out, "{byte:02x}").expect("formatting a String");
        }
    }
    out
}

fn rfc3986_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

/// What JavaScript's `encodeURIComponent` leaves raw.
fn uri_component_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'-' | b'.' | b'_' | b'~' | b'!' | b'*' | b'\'' | b'(' | b')'
        )
}

fn percent(bytes: &[u8], upper: bool, keep: fn(u8) -> bool, plus_for_space: bool) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len());
    for &byte in bytes {
        if keep(byte) {
            out.push(char::from(byte));
        } else if plus_for_space && byte == b' ' {
            out.push('+');
        } else if upper {
            write!(out, "%{byte:02X}").expect("formatting a String");
        } else {
            write!(out, "%{byte:02x}").expect("formatting a String");
        }
    }
    out
}

/// Unpadded standard base64.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let group = chunk.iter().enumerate().fold(0u32, |acc, (at, &byte)| {
            acc | (u32::from(byte) << (16 - 8 * at))
        });
        for at in 0..=chunk.len() {
            out.push(char::from(
                ALPHABET[((group >> (18 - 6 * at)) & 63) as usize],
            ));
        }
    }
    out
}

/// `text` without terminal escape sequences.
fn strip_ansi(text: Cow<'_, str>) -> Cow<'_, str> {
    if !text.contains(['\u{1b}', '\u{9b}']) {
        return text;
    }
    Cow::Owned(split_ansi(&text).0)
}

/// How far an escape sequence runs.
enum Escape {
    /// It ends after this many bytes.
    Done(usize),
    /// The text ends inside it.
    Open,
    /// It ran past [`ESCAPE_CARRY_BYTES`] with no end.
    TooLong,
}

/// `text` without its complete escape sequences, and the unfinished sequence it ends in.
///
/// A sequence that has run [`ESCAPE_CARRY_BYTES`] without ending is not one: its introducer is
/// dropped and the rest is text, so a stray `ESC ]` cannot swallow the output after it.
fn split_ansi(text: &str) -> (String, &str) {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while let Some(found) = text[at..].find(['\u{1b}', '\u{9b}']) {
        let start = at + found;
        out.push_str(&text[at..start]);
        let rest = &text[start..];
        // An introducer left directly before a replaced secret goes alone, so the sequence it
        // would open cannot eat the marker.
        let introducer = match rest.as_bytes() {
            [0x1b, b'[' | b']', ..] => 2,
            [0x1b, ..] => 1,
            _ => '\u{9b}'.len_utf8(),
        };
        if rest[introducer..].starts_with(REDACTED) {
            at = start + introducer;
            continue;
        }
        match escape_end(rest) {
            Escape::Done(len) => at = start + len,
            Escape::Open => return (out, rest),
            Escape::TooLong => {
                at = start + rest.chars().next().map_or(1, char::len_utf8);
            }
        }
    }
    out.push_str(&text[at..]);
    (out, "")
}

/// Where the escape sequence that opens `rest` ends.
fn escape_end(rest: &str) -> Escape {
    let mut chars = rest.char_indices();
    let Some((_, introducer)) = chars.next() else {
        return Escape::Open;
    };
    if introducer == '\u{9b}' {
        return csi_end(chars);
    }
    match chars.next() {
        None => Escape::Open,
        Some((_, '[')) => csi_end(chars),
        Some((_, ']')) => loop {
            match chars.next() {
                None => return Escape::Open,
                Some((at, _)) if at > ESCAPE_CARRY_BYTES => return Escape::TooLong,
                Some((at, c @ ('\u{7}' | '\u{9c}'))) => return Escape::Done(at + c.len_utf8()),
                Some((_, '\u{1b}')) => {
                    return chars
                        .next()
                        .map_or(Escape::Open, |(at, c)| Escape::Done(at + c.len_utf8()));
                }
                Some(_) => {}
            }
        },
        Some((at, c)) => Escape::Done(at + c.len_utf8()),
    }
}

/// Consume parameter and intermediate bytes through the final byte.
fn csi_end(chars: std::str::CharIndices<'_>) -> Escape {
    for (at, c) in chars {
        if at > ESCAPE_CARRY_BYTES {
            return Escape::TooLong;
        }
        if ('\u{40}'..='\u{7e}').contains(&c) {
            return Escape::Done(at + c.len_utf8());
        }
    }
    Escape::Open
}

/// Environment values a child can print that are secrets by the daemon's measured rule.
///
/// A value of at least 12 bytes whose name has an underscore-separated part ending in KEY, TOKEN,
/// SECRET, PASS, PASSWORD, PASSWD, CREDENTIAL, CREDENTIALS, AUTH, BEARER, COOKIE, SESSION or
/// PRIVATE (case ignored) is a secret whole, and so is each token run inside it. In any other
/// value, each run of at least 16 letters, digits, `+`, `-` or `_` holding a letter and a digit,
/// at 3.0 bits per character or more, is one; PATH, MANPATH, INFOPATH, HOME, PWD, OLDPWD, TMPDIR
/// and `XDG_*` are exempt from that second rule. Secrets under these bounds are a stated
/// residual.
#[must_use]
pub fn env_secrets<'a>(vars: impl IntoIterator<Item = &'a EnvVar>) -> Vec<String> {
    let mut named = Vec::new();
    let mut runs = Vec::new();
    for var in vars {
        let (Some(name), Some(value)) = (var.name().to_str(), var.expose_value().to_str()) else {
            continue;
        };
        if secret_named(name) {
            if value.len() >= 12 {
                named.push(value.to_owned());
                // A credential helper wraps the token (`x-access-token:ghp_...`), and a child
                // prints the token alone.
                named.extend(token_runs(value));
            }
            continue;
        }
        if exempt_from_runs(name) {
            continue;
        }
        runs.extend(token_runs(value));
    }
    // Secret-named values come first: they are the ones a source cap must never drop.
    let mut seen = BTreeSet::new();
    named
        .into_iter()
        .chain(runs)
        .filter(|value| seen.insert(value.clone()))
        .collect()
}

const SECRET_SUFFIXES: &[&str] = &[
    "KEY",
    "TOKEN",
    "SECRET",
    "PASS",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "CREDENTIALS",
    "AUTH",
    "BEARER",
    "COOKIE",
    "SESSION",
    "PRIVATE",
];

fn secret_named(name: &str) -> bool {
    name.split('_').any(|part| {
        let part = part.to_ascii_uppercase();
        SECRET_SUFFIXES.iter().any(|suffix| part.ends_with(suffix))
    })
}

fn exempt_from_runs(name: &str) -> bool {
    matches!(
        name,
        "PATH" | "MANPATH" | "INFOPATH" | "HOME" | "PWD" | "OLDPWD" | "TMPDIR"
    ) || name.starts_with("XDG_")
}

fn token_runs(value: &str) -> impl Iterator<Item = String> + '_ {
    value
        .split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '_')))
        .filter(|run| {
            run.len() >= 16
                && run.bytes().any(|b| b.is_ascii_alphabetic())
                && run.bytes().any(|b| b.is_ascii_digit())
                && bits_per_char(run) >= 3.0
        })
        .map(str::to_owned)
}

#[allow(clippy::cast_precision_loss)]
fn bits_per_char(run: &str) -> f64 {
    let mut counts = [0u32; 256];
    for byte in run.bytes() {
        counts[usize::from(byte)] += 1;
    }
    let total = run.len() as f64;
    counts
        .iter()
        .filter(|&&n| n > 0)
        .map(|&n| {
            let p = f64::from(n) / total;
            -p * p.log2()
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "tok_9fQ2LmZx81KdPvR4sWbN7hYcA3eJuT6o";

    fn redactor(secrets: &[&str]) -> Redactor {
        let redactor = Redactor::new();
        redactor.set_source("test", secrets.iter().map(|s| (*s).to_owned()));
        redactor
    }

    #[test]
    fn held_subject_capture_distinguishes_empty_context_from_ended_context() {
        let redactor = Redactor::new();
        let holder = Holder::Turn(TurnId::NIL);
        assert_eq!(redactor.scrub_held(holder, "hello", str::to_owned), None);
        redactor.hold_env(holder, &[]);
        assert_eq!(
            redactor.scrub_held(holder, "hello", str::to_owned),
            Some("hello".into())
        );
        redactor.release(holder);
        assert_eq!(redactor.scrub_held(holder, "hello", str::to_owned), None);
    }

    #[test]
    fn held_subject_capture_scrubs_again_after_normalization_with_the_same_matcher() {
        let redactor = Redactor::new();
        let holder = Holder::Turn(TurnId::NIL);
        redactor.hold_env(holder, &vars(&[("API_TOKEN", TOKEN)]));
        let (head, tail) = TOKEN.split_at(10);
        let text = format!("{head}\u{202e}{tail}");
        let scrubbed = redactor.scrub_held(holder, &text, |text| {
            redactor.release(holder);
            text.replace('\u{202e}', "")
        });
        assert_eq!(scrubbed, Some(REDACTED.into()));
        assert_eq!(redactor.scrub(TOKEN), TOKEN);
    }

    fn b64(bytes: &[u8]) -> String {
        let mut text = base64(bytes);
        while !text.len().is_multiple_of(4) {
            text.push('=');
        }
        text
    }

    #[test]
    fn a_planted_token_is_replaced_in_every_form() {
        let spaced = "tok 9fQ2/LmZx81KdPvR4sW";
        let redactor = redactor(&[TOKEN, spaced]);
        let forms = [
            TOKEN.to_owned(),
            hex(TOKEN.as_bytes(), false),
            hex(TOKEN.as_bytes(), true),
            percent(spaced.as_bytes(), false, rfc3986_unreserved, false),
            percent(spaced.as_bytes(), true, rfc3986_unreserved, false),
        ];
        for form in forms {
            let text = format!("before {form} after");
            assert_eq!(redactor.scrub(&text), "before (redacted) after", "{form}");
        }
    }

    #[test]
    fn base64_matches_at_all_three_alignments_and_for_basic_auth() {
        let redactor = redactor(&[TOKEN]);
        for lead in ["", "x", "xy"] {
            let encoded = b64(format!("{lead}{TOKEN}").as_bytes());
            let url = encoded.replace('+', "-").replace('/', "_");
            for form in [encoded, url] {
                let text = format!("data={form}&more");
                let scrubbed = redactor.scrub(&text);
                assert!(
                    scrubbed.contains(REDACTED) && !scrubbed.contains(&form[4..form.len() - 4]),
                    "alignment {lead:?}: {scrubbed}"
                );
            }
        }
        let basic = b64(format!("user:{TOKEN}").as_bytes());
        let header = format!("Authorization: Basic {basic}");
        let scrubbed = redactor.scrub(&header);
        assert!(scrubbed.starts_with("Authorization: Basic "));
        assert!(scrubbed.contains(REDACTED));
        assert!(!scrubbed.contains(&basic[8..basic.len() - 8]));
    }

    #[test]
    fn a_token_inside_terminal_codes_is_found_and_the_codes_are_gone() {
        let redactor = redactor(&[TOKEN]);
        let (head, tail) = TOKEN.split_at(10);
        let text = format!("\u{1b}[31mkey \u{1b}[1m{head}\u{1b}[0m{tail}\u{1b}]0;title\u{7}!");
        assert_eq!(redactor.scrub(&text), "key (redacted)!");
    }

    #[test]
    fn a_secret_split_across_stream_pieces_is_never_released_in_part() {
        let redactor = redactor(&[TOKEN]);
        let text = format!("the value is {TOKEN} and then more text");
        for size in 1..=text.len() {
            let mut stream = redactor.stream();
            let mut out = String::new();
            let mut at = 0;
            while at < text.len() {
                let mut end = (at + size).min(text.len());
                while !text.is_char_boundary(end) {
                    end += 1;
                }
                let released = stream.push(&text[at..end]);
                assert!(
                    !released.contains(&TOKEN[..6]),
                    "piece size {size} released {released:?}"
                );
                out.push_str(&released);
                at = end;
            }
            out.push_str(&stream.finish());
            assert_eq!(
                out, "the value is (redacted) and then more text",
                "size {size}"
            );
        }
    }

    #[test]
    fn a_long_value_is_held_until_the_matcher_has_seen_all_of_it() {
        let pem = format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----",
            "MIIEvQIBADANBgkqhkiG9w0BAQEFAASC".repeat(40)
        );
        let redactor = redactor(&[&pem]);
        let mut stream = redactor.stream();
        let mut out = String::new();
        for piece in pem.as_bytes().chunks(64) {
            let released = stream.push(std::str::from_utf8(piece).unwrap());
            assert!(!released.contains("MIIE"), "released part of the key");
            out.push_str(&released);
        }
        out.push_str(&stream.push(" done"));
        out.push_str(&stream.finish());
        assert_eq!(out, "(redacted) done");
    }

    #[test]
    fn ordinary_text_streams_with_no_delay() {
        let redactor = redactor(&[TOKEN]);
        let mut stream = redactor.stream();
        assert_eq!(
            stream.push("Running the tests now. "),
            "Running the tests now. "
        );
        assert_eq!(stream.push("All 42 passed."), "All 42 passed.");
        assert_eq!(stream.finish(), "");
    }

    #[test]
    fn a_kept_prefix_of_eight_bytes_that_never_completes_is_redacted_at_the_end() {
        let redactor = redactor(&[TOKEN]);
        let mut stream = redactor.stream();
        assert_eq!(stream.push("see tok_9fQ2"), "see ");
        assert_eq!(stream.finish(), REDACTED);
    }

    #[test]
    fn a_shorter_kept_prefix_is_released_at_the_end() {
        let redactor = redactor(&[TOKEN]);
        let mut stream = redactor.stream();
        assert_eq!(stream.push("see tok_9fQ"), "see ");
        assert_eq!(stream.finish(), "tok_9fQ");
    }

    fn streamed(redactor: &Redactor, text: &str, size: usize) -> (String, Vec<String>) {
        let mut stream = redactor.stream();
        let mut out = String::new();
        let mut released = Vec::new();
        let mut at = 0;
        while at < text.len() {
            let mut end = (at + size).min(text.len());
            while !text.is_char_boundary(end) {
                end += 1;
            }
            let piece = stream.push(&text[at..end]);
            out.push_str(&piece);
            released.push(piece);
            at = end;
        }
        out.push_str(&stream.finish());
        (out, released)
    }

    #[test]
    fn a_secret_that_prefixes_a_longer_secret_never_defeats_the_hold_back() {
        // The short secret ends in a byte that no form starts with, so only the longer form can
        // make the stream wait once the short one has completed.
        for short in ["tok_9fQ2LmZx81Km", "tok_9fQ2LmZx81Kd"] {
            let long = format!("{short}PvR4sWbN7hYc");
            let redactor = redactor(&[short, &long]);
            for text in [
                format!("a {long} b"),
                format!("a {short} b"),
                format!("a {short}Pv"),
                format!("a {short}PvR4sWbN7hY"),
            ] {
                for size in 1..=text.len() {
                    let (out, released) = streamed(&redactor, &text, size);
                    for piece in &released {
                        assert!(
                            !piece.contains("tok_9f") && !piece.contains("PvR4"),
                            "{short} size {size} released {piece:?}"
                        );
                    }
                    assert!(
                        !out.contains("tok_9f") && !out.contains("PvR4"),
                        "{short} size {size}: {out:?}"
                    );
                }
            }
            for size in 1..=("a  b".len() + long.len()) {
                let (out, _) = streamed(&redactor, &format!("a {long} b"), size);
                assert_eq!(out, "a (redacted) b", "{short} size {size}");
                let (out, _) = streamed(&redactor, &format!("a {short} b"), size);
                assert_eq!(out, "a (redacted) b", "{short} size {size}");
            }
        }
    }

    #[test]
    fn a_run_of_overlapping_matches_is_held_no_longer_than_the_longest_form() {
        let secret = "ab".repeat(8);
        let redactor = redactor(&[&secret]);
        let longest = redactor.matcher().longest;
        let mut stream = redactor.stream();
        let started = std::time::Instant::now();
        let mut out = String::new();
        let run = "ab".repeat(50_000);
        for piece in run.as_bytes().chunks(64) {
            out.push_str(&stream.push(std::str::from_utf8(piece).unwrap()));
            assert!(
                stream.raw.text.len() <= longest,
                "{}",
                stream.raw.text.len()
            );
        }
        out.push_str(&stream.finish());
        assert!(out.contains(REDACTED));
        assert!(!out.contains(&secret), "a whole secret left");
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn an_escape_before_a_token_does_not_hide_its_first_characters() {
        let redactor = redactor(&[TOKEN]);
        for lead in ["\u{1b}", "\u{1b}[", "\u{9b}"] {
            let text = format!("x {lead}{TOKEN} y");
            let scrubbed = redactor.scrub(&text);
            assert!(
                !scrubbed.contains("9fQ2") && !scrubbed.contains("tok_"),
                "{lead:?}: {scrubbed:?}"
            );
            for size in 1..=text.len() {
                let (out, released) = streamed(&redactor, &text, size);
                assert!(
                    !out.contains("9fQ2") && !released.concat().contains("tok_"),
                    "{lead:?} size {size}: {out:?}"
                );
            }
        }
    }

    #[test]
    fn an_unterminated_escape_does_not_swallow_the_output_after_it() {
        let redactor = redactor(&[TOKEN]);
        let mut stream = redactor.stream();
        let mut out = stream.push("\u{1b}]");
        let filler = "z".repeat(1024 * 1024);
        let started = std::time::Instant::now();
        for piece in filler.as_bytes().chunks(4096) {
            out.push_str(&stream.push(std::str::from_utf8(piece).unwrap()));
        }
        out.push_str(&stream.finish());
        assert!(out.ends_with(&filler[..4096]), "the output vanished");
        assert!(out.len() > filler.len() - 8192);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn an_escape_introducer_before_a_token_leaves_the_marker_whole() {
        let redactor = redactor(&[TOKEN]);
        for lead in ["\u{1b}[", "\u{9b}", "\u{1b}", "\u{1b}]"] {
            let text = format!("x {lead}{TOKEN} y");
            assert_eq!(redactor.scrub(&text), "x (redacted) y", "{lead:?}");
            for size in 1..=text.len() {
                let (out, _) = streamed(&redactor, &text, size);
                assert_eq!(out, "x (redacted) y", "{lead:?} size {size}");
            }
        }
    }

    #[test]
    fn a_source_keeps_thousands_of_secrets_and_the_first_given_past_its_cap() {
        let redactor = Redactor::new();
        let values: Vec<String> = (0..MAX_SECRETS_PER_SOURCE + 50)
            .map(|n| format!("zq{n:06}-secret-value"))
            .collect();
        redactor.set_source("many", values.clone());
        assert_eq!(redactor.scrub(&values[0]), REDACTED);
        assert_eq!(
            redactor.scrub(&values[MAX_SECRETS_PER_SOURCE - 1]),
            REDACTED
        );
        let dropped = &values[MAX_SECRETS_PER_SOURCE];
        assert_eq!(redactor.scrub(dropped), dropped.as_str());
    }

    #[test]
    fn a_secret_named_value_survives_an_environment_full_of_other_tokens() {
        let crowd = (0..MAX_SECRETS_PER_SOURCE + 100)
            .map(|n| format!("Q{n:05}ZxKdPvR4sWbN7h"))
            .collect::<Vec<_>>()
            .join(" ");
        let env = vars(&[
            ("CROWD", &crowd),
            ("service_api_key", "abcdefghijklmnop0123"),
        ]);
        let held = env_secrets(&env);
        assert!(held.len() > MAX_SECRETS_PER_SOURCE, "{}", held.len());
        assert_eq!(held[0], "abcdefghijklmnop0123");
        let redactor = Redactor::new();
        redactor.hold_env(Holder::Pty(AgentId::NIL), &env);
        assert_eq!(redactor.scrub("abcdefghijklmnop0123"), REDACTED);
    }

    #[test]
    fn a_secret_of_sixty_kib_gets_its_encoded_forms_and_builds_quickly() {
        let mut seed = 0x2545_f491_u32;
        let secret: String = (0..60 * 1024)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                char::from(
                    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"
                        [(seed >> 24) as usize % 62],
                )
            })
            .collect();
        let started = std::time::Instant::now();
        let redactor = redactor(&[&secret]);
        let built = started.elapsed();
        assert!(redactor.matcher().forms.len() > 4);
        let text = format!(
            "<{}|{}>",
            hex(secret.as_bytes(), false),
            b64(secret.as_bytes())
        );
        assert_eq!(redactor.scrub(&text), "<(redacted)|(redacted)>");
        assert!(built < std::time::Duration::from_secs(2), "{built:?}");
    }

    #[test]
    fn the_uri_component_and_plus_forms_match() {
        let spaced = "tok 9fQ2/LmZx81Kd!*'()PvR4";
        let redactor = redactor(&[spaced]);
        let component = "tok%209fQ2%2FLmZx81Kd!*'()PvR4";
        let form = "tok+9fQ2%2FLmZx81Kd%21%2A%27%28%29PvR4";
        for text in [component, form] {
            assert_eq!(
                redactor.scrub(&format!("q={text}&")),
                "q=(redacted)&",
                "{text}"
            );
        }
    }

    #[test]
    fn a_secret_shorter_than_eight_bytes_is_not_held() {
        let redactor = redactor(&["abcdefg"]);
        assert_eq!(redactor.scrub("abcdefg"), "abcdefg");
    }

    #[test]
    fn the_set_changes_the_generation_and_a_huge_value_gets_only_its_raw_form() {
        let redactor = Redactor::new();
        let before = redactor.generation();
        let huge = "Ab1_".repeat(MAX_ENCODED_SECRET_BYTES);
        redactor.set_source("huge", [huge.clone()]);
        assert_ne!(redactor.generation(), before);
        assert_eq!(redactor.matcher().forms.len(), 1);
        assert_eq!(redactor.scrub(&format!("<{huge}>")), "<(redacted)>");
        redactor.set_source("huge", [huge]);
        let again = redactor.generation();
        redactor.set_source("huge", [format!("{TOKEN}{TOKEN}")]);
        assert_ne!(redactor.generation(), again);
    }

    #[test]
    fn a_large_message_scrubs_in_well_under_a_second() {
        let secrets: Vec<String> = (0..64)
            .map(|n| format!("secret-value-{n:04}-xYz9QwErTy"))
            .collect();
        let refs: Vec<&str> = secrets.iter().map(String::as_str).collect();
        let redactor = redactor(&refs);
        let text = format!(
            "{} {} {}",
            "lorem ipsum ".repeat(80_000),
            secrets[7],
            "tail"
        );
        let started = std::time::Instant::now();
        let out = redactor.scrub(&text);
        assert!(out.ends_with("(redacted) tail"));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_value_leaves_when_its_last_source_ends() {
        let redactor = Redactor::new();
        redactor.set_source("one", [TOKEN.to_owned()]);
        redactor.set_source("two", [TOKEN.to_owned()]);
        redactor.clear_source("one");
        assert_eq!(redactor.scrub(TOKEN), REDACTED);
        redactor.clear_source("two");
        assert_eq!(redactor.scrub(TOKEN), TOKEN);
    }

    fn vars(pairs: &[(&str, &str)]) -> Vec<EnvVar> {
        pairs.iter().map(|(n, v)| EnvVar::new(*n, *v)).collect()
    }

    #[test]
    fn secret_named_values_of_twelve_bytes_are_held() {
        let env = vars(&[
            ("GITHUB_TOKEN", "ghp_abcdefghijklmnop"),
            ("SSH_AUTH_SOCK", "/private/tmp/launchd-1/Listeners"),
            ("GIT_AUTHOR_NAME", "Alex Nguyen the Third"),
            ("API_KEY", "1234567"),
            ("SESSION_DRIVER", "database"),
        ]);
        let held = env_secrets(&env);
        assert!(held.contains(&"ghp_abcdefghijklmnop".to_owned()));
        assert!(held.contains(&"/private/tmp/launchd-1/Listeners".to_owned()));
        assert!(!held.iter().any(|v| v.contains("Alex")));
        assert!(!held.contains(&"1234567".to_owned()), "the 7-byte residual");
        assert!(!held.contains(&"database".to_owned()));
    }

    #[test]
    fn an_inner_token_of_a_secret_named_value_is_held_alone() {
        let env = vars(&[(
            "GIT_ASKPASS_TOKEN",
            "x-access-token:ghp_Zq81KdPvR4sWbN7hYcA3eJ",
        )]);
        let held = env_secrets(&env);
        assert!(
            held.contains(&"ghp_Zq81KdPvR4sWbN7hYcA3eJ".to_owned()),
            "{held:?}"
        );
        assert!(held.contains(&"x-access-token:ghp_Zq81KdPvR4sWbN7hYcA3eJ".to_owned()));
    }

    #[test]
    fn a_random_looking_run_in_any_other_value_is_held() {
        let env = vars(&[(
            "CURL_OPTS",
            "-H Authorization:Bearer x=9fQ2LmZx81KdPvR4sWbN7hYc end",
        )]);
        assert_eq!(env_secrets(&env), ["9fQ2LmZx81KdPvR4sWbN7hYc"]);
    }

    #[test]
    fn ordinary_environment_values_stay_visible() {
        let ls_colors = "rs=0:di=01;34:ln=01;36:mh=00:pi=40;33:so=01;35:do=01;35:bd=40;33;01:cd=40;33;01:or=40;31;01:mi=00:su=37;41:sg=30;43:ca=00:tw=30;42:ow=34;42:st=37;44:ex=01;32:*.tar=01;31:*.tgz=01;31:*.zip=01;31";
        let env = vars(&[
            ("NODE_ENV", "production"),
            ("LS_COLORS", ls_colors),
            ("NPM_CONFIG_REGISTRY", "https://registry.npmjs.org/"),
            (
                "TMPDIR",
                "/var/folders/zz/zyxvpxvq6csfxvn_n0000000000000/T/",
            ),
            (
                "PATH",
                "/usr/local/bin:/usr/bin:/opt/homebrew/Cellar/node/24.1.0/bin",
            ),
            ("XDG_DATA_DIRS", "/usr/local/share/9fQ2LmZx81KdPvR4sWbN7hYc"),
        ]);
        assert_eq!(env_secrets(&env), Vec::<String>::new());
    }
}
