//! The daemon-wide live secret set and the matcher over it.
//!
//! One [`Redactor`] is shared by every reader of agent text. Its handle exposes a scrub and the
//! calls that change the set, and nothing that reads a secret back: the agent-token plaintext
//! the supervisor mints is added here by the mint and cannot be taken out again, so a caller that
//! holds a handle can hide a value but never learn one.
//!
//! **What matches.** A secret matches as written, as lower- or upper-case hex, percent-encoded,
//! and as base64 or base64url at each of the three alignments it can take inside a longer base64
//! string, with the edge characters that depend on neighbouring bytes trimmed. A match becomes
//! [`REDACTED`]. A secret that is split, reversed or compressed is not matched.
//!
//! **Streams.** [`StreamScrubber`] holds back the tail of its buffer for as long as that tail is a
//! prefix of some form in the set, so a known value is never released in part. Ordinary text has
//! no such tail and is released at once.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::Bound;
use std::sync::{Arc, Mutex, PoisonError};

use dudes_providers::launch::EnvVar;
use dudes_types::{AgentId, TurnId};

/// What a match is replaced with.
pub const REDACTED: &str = "(redacted)";

/// The shortest secret the matcher accepts. A shorter value would match ordinary words.
const MIN_SECRET_BYTES: usize = 4;
/// The shortest base64 form worth matching once its alignment-dependent edges are trimmed.
const MIN_BASE64_FORM: usize = 8;

/// The daemon-wide live secret set. Cheap to clone; every clone shares one set.
#[derive(Clone, Default)]
pub struct Redactor {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    sources: BTreeMap<String, BTreeSet<String>>,
    matcher: Arc<Matcher>,
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
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn matcher(&self) -> Arc<Matcher> {
        Arc::clone(&self.lock().matcher)
    }

    /// Replace everything `source` holds with `values`. An empty `values` ends the source.
    ///
    /// A value stays in the set until the last source naming it has ended.
    pub fn set_source<I>(&self, source: &str, values: I)
    where
        I: IntoIterator<Item = String>,
    {
        let values: BTreeSet<String> = values
            .into_iter()
            .filter(|value| value.len() >= MIN_SECRET_BYTES)
            .collect();
        let mut inner = self.lock();
        let changed = if values.is_empty() {
            inner.sources.remove(source).is_some()
        } else if inner.sources.get(source) == Some(&values) {
            false
        } else {
            inner.sources.insert(source.to_owned(), values);
            true
        };
        if changed {
            inner.matcher = Arc::new(Matcher::of(inner.sources.values().flatten()));
        }
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
        self.set_source(&holder.source(), env_secrets(vars));
    }

    /// The holder's child is gone: its environment values stop being held, unless another source
    /// still names them.
    pub fn release(&self, holder: Holder) {
        self.clear_source(&holder.source());
    }

    /// `text` without terminal escape sequences and with every known secret replaced.
    #[must_use]
    pub fn scrub<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let matcher = self.matcher();
        let stripped = strip_ansi(text);
        match matcher.replace(&stripped) {
            Some(replaced) => Cow::Owned(replaced),
            None => stripped,
        }
    }

    /// A scrubber for one stream of text.
    #[must_use]
    pub fn stream(&self) -> StreamScrubber {
        StreamScrubber {
            redactor: self.clone(),
            escape: String::new(),
            held: String::new(),
        }
    }
}

/// Who launched a child whose environment is held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
/// keeps the rest. [`Self::finish`] answers what is kept.
pub struct StreamScrubber {
    redactor: Redactor,
    /// An escape sequence the last piece began and did not finish.
    escape: String,
    /// Scrubbed text that is a prefix of some form in the set.
    held: String,
}

impl fmt::Debug for StreamScrubber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamScrubber")
            .field("held", &(self.held.len() + self.escape.len()))
            .finish_non_exhaustive()
    }
}

impl StreamScrubber {
    /// Add `piece` and answer the text that is now safe to release.
    pub fn push(&mut self, piece: &str) -> String {
        let matcher = self.redactor.matcher();
        let mut input = std::mem::take(&mut self.escape);
        input.push_str(piece);
        let (clean, incomplete) = split_ansi(&input);
        let mut text = std::mem::take(&mut self.held);
        text.push_str(&clean);
        incomplete.clone_into(&mut self.escape);
        if let Some(replaced) = matcher.replace(&text) {
            text = replaced;
        }
        let keep = matcher.held_tail(&text);
        self.held = text.split_off(text.len() - keep);
        text
    }

    /// Release what is kept. A kept tail never became a whole secret, so it goes out as written.
    pub fn finish(&mut self) -> String {
        self.escape.clear();
        let text = std::mem::take(&mut self.held);
        self.redactor.scrub(&text).into_owned()
    }
}

#[derive(Default)]
struct Matcher {
    forms: BTreeSet<String>,
    longest: usize,
}

impl Matcher {
    fn of<'a>(secrets: impl Iterator<Item = &'a String>) -> Self {
        let mut forms = BTreeSet::new();
        for secret in secrets {
            add_forms(secret.as_bytes(), &mut forms);
        }
        let longest = forms.iter().map(String::len).max().unwrap_or(0);
        Self { forms, longest }
    }

    /// `text` with every form replaced, or `None` when none occurs.
    fn replace(&self, text: &str) -> Option<String> {
        let mut spans: Vec<(usize, usize)> = Vec::new();
        for form in &self.forms {
            if form.len() > text.len() {
                continue;
            }
            let mut from = 0;
            while let Some(at) = text[from..].find(form.as_str()) {
                let start = from + at;
                spans.push((start, start + form.len()));
                from = start + text[start..].chars().next().map_or(1, char::len_utf8);
            }
        }
        if spans.is_empty() {
            return None;
        }
        spans.sort_unstable();
        let mut out = String::with_capacity(text.len());
        let mut copied = 0;
        let mut current: Option<(usize, usize)> = None;
        for (start, end) in spans {
            match current {
                Some((open, close)) if start <= close => current = Some((open, close.max(end))),
                Some((open, close)) => {
                    out.push_str(&text[copied..open]);
                    out.push_str(REDACTED);
                    copied = close;
                    current = Some((start, end));
                }
                None => current = Some((start, end)),
            }
        }
        if let Some((open, close)) = current {
            out.push_str(&text[copied..open]);
            out.push_str(REDACTED);
            copied = close;
        }
        out.push_str(&text[copied..]);
        Some(out)
    }

    /// How many bytes at the end of `text` are a proper prefix of some form.
    fn held_tail(&self, text: &str) -> usize {
        if self.forms.is_empty() {
            return 0;
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
                .next()
                .is_some_and(|form| form.len() > tail.len() && form.starts_with(tail))
            {
                return tail.len();
            }
        }
        0
    }
}

fn add_forms(secret: &[u8], forms: &mut BTreeSet<String>) {
    if let Ok(raw) = std::str::from_utf8(secret) {
        forms.insert(raw.to_owned());
    }
    forms.insert(hex(secret, false));
    forms.insert(hex(secret, true));
    for upper in [false, true] {
        let encoded = percent(secret, upper);
        if encoded.as_bytes() != secret {
            forms.insert(encoded);
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

fn percent(bytes: &[u8], upper: bool) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len());
    for &byte in bytes {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
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
fn strip_ansi(text: &str) -> Cow<'_, str> {
    if !text.contains(['\u{1b}', '\u{9b}']) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(split_ansi(text).0)
}

/// `text` without its complete escape sequences, and the unfinished sequence it ends in.
fn split_ansi(text: &str) -> (String, &str) {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        match c {
            '\u{9b}' => {
                if !skip_csi(&mut chars) {
                    return (out, &text[start..]);
                }
            }
            '\u{1b}' => match chars.next() {
                None => return (out, &text[start..]),
                Some((_, '[')) => {
                    if !skip_csi(&mut chars) {
                        return (out, &text[start..]);
                    }
                }
                Some((_, ']')) => loop {
                    match chars.next() {
                        None => return (out, &text[start..]),
                        Some((_, '\u{7}' | '\u{9c}')) => break,
                        Some((_, '\u{1b}')) => {
                            if chars.next().is_none() {
                                return (out, &text[start..]);
                            }
                            break;
                        }
                        Some(_) => {}
                    }
                },
                Some(_) => {}
            },
            c => out.push(c),
        }
    }
    (out, "")
}

/// Consume parameter and intermediate bytes through the final byte; `false` when the text ends
/// first.
fn skip_csi(chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>) -> bool {
    for (_, c) in chars.by_ref() {
        if ('\u{40}'..='\u{7e}').contains(&c) {
            return true;
        }
    }
    false
}

/// Environment values a child can print that are secrets by the daemon's measured rule.
///
/// A value of at least 12 bytes whose name has an underscore-separated part ending in KEY, TOKEN,
/// SECRET, PASS, PASSWORD, PASSWD, CREDENTIAL, CREDENTIALS, AUTH, BEARER, COOKIE, SESSION or
/// PRIVATE (case ignored) is a secret whole. In any other value, each run of at least 16 letters,
/// digits, `+`, `-` or `_` holding a letter and a digit, at 3.0 bits per character or more, is
/// one; PATH, MANPATH, INFOPATH, HOME, PWD, OLDPWD, TMPDIR and `XDG_*` are exempt from that
/// second rule. Secrets under these bounds are a stated residual.
#[must_use]
pub fn env_secrets<'a>(vars: impl IntoIterator<Item = &'a EnvVar>) -> Vec<String> {
    let mut out = BTreeSet::new();
    for var in vars {
        let (Some(name), Some(value)) = (var.name().to_str(), var.expose_value().to_str()) else {
            continue;
        };
        if secret_named(name) {
            if value.len() >= 12 {
                out.insert(value.to_owned());
            }
            continue;
        }
        if exempt_from_runs(name) {
            continue;
        }
        out.extend(token_runs(value));
    }
    out.into_iter().collect()
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
            percent(spaced.as_bytes(), false),
            percent(spaced.as_bytes(), true),
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
    fn a_kept_prefix_that_never_completes_is_released_at_the_end() {
        let redactor = redactor(&[TOKEN]);
        let mut stream = redactor.stream();
        assert_eq!(stream.push("see tok_9fQ2"), "see ");
        assert_eq!(stream.finish(), "tok_9fQ2");
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
