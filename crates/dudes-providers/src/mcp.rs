//! MCP servers the operator declares in `[mcp.servers.<name>]` and opts each agent in to at hire.
//!
//! Three stages, each a type:
//!
//! 1. [`declare`] turns one table, as the daemon's file parser read it, into a [`McpServer`]: a
//!    program the engine starts, or an http endpoint it connects to. The rules are pure over the
//!    file. No key holds a secret value: a declaration names environment variables.
//! 2. [`LoadedServers::new`] runs once when the daemon starts. It takes the values of those variables,
//!    which [`split_secrets`] removed from what every other child inherits, and the absolute path
//!    the daemon found for each bare `command`, and remembers which servers cannot run and why.
//! 3. [`LoadedServers::admit`] applies the hire checks, in their order, to a list of names. A hire runs
//!    it before anything is journaled, and every managed launch runs it again against the recorded
//!    names, so a definition changed under a restart cannot launch what the hire would refuse. Its
//!    answer, a list of [`McpLaunch`], is what an adapter composes into the launch.
//!
//! Dudes reads nothing back from a server. A server's traffic is the agent's, inside a turn.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt;
use std::path::Path;

use dudes_types::McpServerName;
use serde::Deserialize;

use crate::launch::{DAEMON_ENV_ALLOWLIST, EnvVar, PREDICATE_ENV_ALLOWLIST};

/// The prefixes a declared variable may not start with: Dudes' own names and each engine's.
const RESERVED_PREFIXES: &[&str] = &["DUDES_", "CLAUDE", "ANTHROPIC_", "CODEX_", "OPENAI_", "PI_"];

/// Whether a declaration may not name `name`, compared without regard to ASCII case.
///
/// A declared name leaves every other child's environment ([`split_secrets`]), so a name a child
/// needs is refused: the daemon's and a predicate's allowlists, `USER`, which the daemon supplies
/// for the Claude CLI's keychain login, and every engine's billing names, whose presence decides
/// which credential an engine bills. The prefixes keep a declared name from colliding with a name
/// an engine or the supervisor sets.
#[must_use]
pub fn reserved_variable(name: &str) -> bool {
    let same = |other: &&str| name.eq_ignore_ascii_case(other);
    RESERVED_PREFIXES.iter().any(|prefix| {
        name.as_bytes()
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
    }) || [
        crate::claude::config::BILLING_ENV_VARS,
        crate::codex::managed::BILLING_ENV_VARS,
        crate::pi::managed::BILLING_ENV_VARS,
        DAEMON_ENV_ALLOWLIST,
        PREDICATE_ENV_ALLOWLIST,
        &["USER"],
    ]
    .iter()
    .any(|list| list.iter().any(same))
}

/// An environment variable a declaration names: `[A-Z_][A-Z0-9_]*`, outside
/// [`reserved_variable`].
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VariableName(String);

impl VariableName {
    fn parse(server: &McpServerName, text: String) -> Result<Self, DeclarationError> {
        let mut bytes = text.bytes();
        let shaped = bytes
            .next()
            .is_some_and(|b| b.is_ascii_uppercase() || b == b'_')
            && bytes.all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
        if !shaped {
            return Err(DeclarationError::server(server, Problem::VariableShape));
        }
        if reserved_variable(&text) {
            return Err(DeclarationError::server(
                server,
                Problem::VariableReserved(text),
            ));
        }
        Ok(Self(text))
    }

    /// The name as declared.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for VariableName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One declared server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpServer {
    /// A program the engine starts and speaks to on its standard input and output.
    Program(ProgramServer),
    /// An endpoint the engine connects to.
    Http(HttpServer),
}

/// A program server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgramServer {
    /// A bare name or an absolute path as declared; in a [`McpLaunch`], always the absolute path
    /// the daemon found.
    pub command: String,
    /// The arguments after the program, each one element.
    pub args: Vec<String>,
    /// The variables the program reads its secrets from, sorted and without repeats.
    pub secret_env: Vec<VariableName>,
}

/// An http server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpServer {
    /// `https`, or `http` to a loopback host.
    pub url: String,
    /// What the engine sends to authenticate.
    pub auth: HttpAuth,
}

/// An http server's credential, by variable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpAuth {
    /// Nothing Dudes supplies. The engine may still present a sign-in of its own for the URL.
    None,
    /// `Authorization: Bearer <value>`.
    Bearer(VariableName),
    /// Each header's whole value, by header name.
    Headers(BTreeMap<String, VariableName>),
}

impl McpServer {
    /// Every variable this server's definition names, in the order its launch adds them.
    #[must_use]
    pub fn variables(&self) -> Vec<&VariableName> {
        match self {
            Self::Program(program) => program.secret_env.iter().collect(),
            Self::Http(HttpServer { auth, .. }) => match auth {
                HttpAuth::None => Vec::new(),
                HttpAuth::Bearer(name) => vec![name],
                HttpAuth::Headers(headers) => headers.values().collect(),
            },
        }
    }
}

/// One `[mcp.servers.<name>]` table as written, before [`declare`] judges it.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Declaration {
    command: Option<String>,
    args: Option<Vec<String>>,
    secret_env: Option<Vec<String>>,
    url: Option<String>,
    bearer_env: Option<String>,
    /// Header name to the variable that holds the header's value.
    #[serde(rename = "header_env")]
    headers: Option<BTreeMap<String, String>>,
}

/// The one `args` entry a program server may use to name the agent's folder. Only a whole entry
/// equal to this token is replaced, at each launch, by the agent's recorded working directory;
/// nothing is interpolated into any other text.
pub const AGENT_DIR_ARG: &str = "{agent_dir}";

/// Every declared server, by name.
pub type DeclaredServers = BTreeMap<McpServerName, McpServer>;

/// A refused declaration. It names the server, and the variable when a reserved one is at fault;
/// it never repeats any other text from the file, because a value pasted where a name belongs
/// would otherwise reach the daemon's error output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeclarationError {
    server: Option<McpServerName>,
    problem: Problem,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Problem {
    Malformed,
    Kind,
    Mixed,
    Interpolation,
    Placeholder,
    Command,
    Url,
    Header,
    NoHeader,
    BothAuth,
    VariableShape,
    VariableReserved(String),
}

impl DeclarationError {
    fn server(server: &McpServerName, problem: Problem) -> Self {
        Self {
            server: Some(server.clone()),
            problem,
        }
    }

    /// The table named `name` is not a table of known keys with values of the right types, or
    /// `name` is not a server name.
    #[must_use]
    pub fn malformed(name: &str) -> Self {
        Self {
            server: McpServerName::parse(name).ok(),
            problem: Problem::Malformed,
        }
    }
}

impl std::error::Error for DeclarationError {}

impl fmt::Display for DeclarationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some(server) = &self.server else {
            return f.write_str(
                "an [mcp.servers] table has a name that is not 1 to 32 lowercase letters and \
                 digits in runs joined by single underscores, starting with a letter",
            );
        };
        let problem = match &self.problem {
            Problem::Malformed => "has an unknown key or a value of the wrong type".to_owned(),
            Problem::Kind => "needs exactly one of `command` and `url`".to_owned(),
            Problem::Mixed => "mixes a program server's keys (`command`, `args`, `secret_env`) \
                               with an http server's (`url`, `bearer_env`, `header_env`)"
                .to_owned(),
            Problem::Interpolation => {
                "holds a NUL or `${`, which an engine would expand from its environment".to_owned()
            }
            Problem::Placeholder => format!(
                "has an `args` entry that holds `{AGENT_DIR_ARG}` beside other text; the token \
                 is valid only as a whole entry"
            ),
            Problem::Command => {
                "has a `command` that is not a bare name or an absolute path".to_owned()
            }
            Problem::Url => "has a `url` that is not https, or http to 127.0.0.1, ::1 or \
                             localhost, with no user information"
                .to_owned(),
            Problem::Header => {
                "has a `header_env` header name that is not an HTTP token".to_owned()
            }
            Problem::NoHeader => "has an empty `header_env`".to_owned(),
            Problem::BothAuth => "sets both `bearer_env` and `header_env`".to_owned(),
            Problem::VariableShape => {
                "names a variable that is not uppercase letters, digits and underscores, not \
                 starting with a digit"
                    .to_owned()
            }
            Problem::VariableReserved(name) => format!(
                "names `{name}`, which Dudes, an engine or the daemon's own children need for \
                 themselves"
            ),
        };
        write!(f, "[mcp.servers.{server}] {problem}")
    }
}

/// Judge one `[mcp.servers.<name>]` table.
///
/// # Errors
///
/// [`DeclarationError`] for a name outside the rules, neither or both of `command` and `url`, keys
/// of both kinds, a string holding a NUL or `${`, a `command` that is neither a bare name nor an
/// absolute path, a `url` that is not https (or http to a loopback host) or carries user
/// information, a header name that is not an HTTP token, both `bearer_env` and `header_env`, or a
/// variable name that is malformed or reserved.
pub fn declare(
    name: &str,
    declaration: Declaration,
) -> Result<(McpServerName, McpServer), DeclarationError> {
    let server = McpServerName::parse(name).map_err(|_| DeclarationError::malformed(name))?;
    let fail = |problem| DeclarationError::server(&server, problem);
    let Declaration {
        command,
        args,
        secret_env,
        url,
        bearer_env,
        headers: header_env,
    } = declaration;
    // Claude expands `${NAME}` in `command`, `args`, `env`, `url` and `headers`, so an operator
    // string holding one would be interpolated by the engine. The only `${NAME}` in a launch is
    // one an adapter composes.
    let interpolated = command
        .iter()
        .chain(args.iter().flatten())
        .chain(secret_env.iter().flatten())
        .chain(&url)
        .chain(&bearer_env)
        .chain(
            header_env
                .iter()
                .flat_map(|headers| headers.iter().flat_map(|(k, v)| [k, v])),
        )
        .any(|text| text.contains('\0') || text.contains("${"));
    if interpolated {
        return Err(fail(Problem::Interpolation));
    }
    if args
        .iter()
        .flatten()
        .any(|arg| arg.contains(AGENT_DIR_ARG) && arg != AGENT_DIR_ARG)
    {
        return Err(fail(Problem::Placeholder));
    }
    let variable = |text: String| VariableName::parse(&server, text);
    let declared = match (command, url) {
        (Some(command), None) => {
            if bearer_env.is_some() || header_env.is_some() {
                return Err(fail(Problem::Mixed));
            }
            let written = Path::new(&command);
            let bare = written.file_name() == Some(OsStr::new(&command));
            if command.is_empty() || !(written.is_absolute() || bare) {
                return Err(fail(Problem::Command));
            }
            let secret_env: BTreeSet<VariableName> = secret_env
                .unwrap_or_default()
                .into_iter()
                .map(variable)
                .collect::<Result<_, _>>()?;
            McpServer::Program(ProgramServer {
                command,
                args: args.unwrap_or_default(),
                secret_env: secret_env.into_iter().collect(),
            })
        }
        (None, Some(url)) => {
            if args.is_some() || secret_env.is_some() {
                return Err(fail(Problem::Mixed));
            }
            if !url_allowed(&url) {
                return Err(fail(Problem::Url));
            }
            let auth = match (bearer_env, header_env) {
                (Some(_), Some(_)) => return Err(fail(Problem::BothAuth)),
                (Some(bearer), None) => HttpAuth::Bearer(variable(bearer)?),
                (None, Some(headers)) if headers.is_empty() => {
                    return Err(fail(Problem::NoHeader));
                }
                (None, Some(headers)) => HttpAuth::Headers(
                    headers
                        .into_iter()
                        .map(|(header, name)| {
                            if http_token(&header) {
                                Ok((header, variable(name)?))
                            } else {
                                Err(fail(Problem::Header))
                            }
                        })
                        .collect::<Result<_, _>>()?,
                ),
                (None, None) => HttpAuth::None,
            };
            McpServer::Http(HttpServer { url, auth })
        }
        _ => return Err(fail(Problem::Kind)),
    };
    Ok((server, declared))
}

/// Every `[mcp.servers.<name>]` table, as a TOML parser read it, judged by [`declare`].
///
/// # Errors
///
/// The first [`DeclarationError`], in name order. A table that is not one of known keys with
/// values of the right types names its server and nothing else.
pub fn parse_servers(
    servers: BTreeMap<String, toml::Value>,
) -> Result<DeclaredServers, DeclarationError> {
    servers
        .into_iter()
        .map(|(name, value)| {
            let declaration = value
                .try_into()
                .map_err(|_| DeclarationError::malformed(&name))?;
            declare(&name, declaration)
        })
        .collect()
}

/// `https`, or `http` to `127.0.0.1`, `::1` or `localhost`, with a host and no user information.
fn url_allowed(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    let secure = scheme.eq_ignore_ascii_case("https");
    if !secure && !scheme.eq_ignore_ascii_case("http") {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return false;
    }
    let host = match authority.strip_prefix('[') {
        Some(literal) => match literal.split_once(']') {
            Some((address, port)) if port.is_empty() || port.starts_with(':') => {
                format!("[{address}]")
            }
            _ => return false,
        },
        None => authority.split(':').next().unwrap_or_default().to_owned(),
    };
    !host.is_empty()
        && (secure
            || ["127.0.0.1", "[::1]", "localhost"]
                .iter()
                .any(|loopback| host.eq_ignore_ascii_case(loopback)))
}

/// An HTTP field name: one or more token characters.
fn http_token(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// Whether the daemon's variable `have` is the declared `name`: by ASCII case on Windows, whose
/// environment names ignore it, and exactly elsewhere.
fn same_variable(have: &OsStr, name: &str) -> bool {
    if cfg!(windows) {
        have.as_encoded_bytes()
            .eq_ignore_ascii_case(name.as_bytes())
    } else {
        have == OsStr::new(name)
    }
}

/// Split the daemon's environment, once, at start: every variable a declaration names leaves the
/// environment its children start from and goes into the second vector, under its declared
/// spelling. Everything else stays in the first, in order.
///
/// This stops inheritance and is not a boundary: any process of the same user can read the
/// daemon's own environment.
#[must_use]
pub fn split_secrets(
    daemon_env: &[EnvVar],
    declared: &DeclaredServers,
) -> (Vec<EnvVar>, Vec<EnvVar>) {
    let names: BTreeSet<&str> = declared
        .values()
        .flat_map(McpServer::variables)
        .map(VariableName::as_str)
        .collect();
    let mut remainder = Vec::with_capacity(daemon_env.len());
    let mut secrets = Vec::new();
    for var in daemon_env {
        match names.iter().find(|name| same_variable(var.name(), name)) {
            Some(name) => secrets.push(EnvVar::new(*name, var.expose_value())),
            None => remainder.push(var.clone()),
        }
    }
    (remainder, secrets)
}

/// The variable an engine reads an http server's bearer token from.
#[must_use]
pub fn bearer_alias(server: &McpServerName) -> String {
    format!("DUDES_MCP_{}_BEARER", server.as_str().to_ascii_uppercase())
}

/// The variable an engine reads an http server's `position`-th header from, in header-name order.
#[must_use]
pub fn header_alias(server: &McpServerName, position: usize) -> String {
    format!(
        "DUDES_MCP_{}_HEADER_{position}",
        server.as_str().to_ascii_uppercase()
    )
}

/// Why a declared server cannot run in this daemon run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unavailable {
    /// This variable was unset or empty when the daemon started.
    Variable(VariableName),
    /// No runnable file of this bare name was on the daemon's `PATH` when it started.
    Command,
    /// The resolved program path contains NUL or engine interpolation text.
    UnsafeCommand,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    server: McpServer,
    env: Result<Vec<EnvVar>, Unavailable>,
}

/// The declared servers as this daemon run can launch them, built once at start.
///
/// Its `Debug` prints names and definitions, never a value: the values are [`EnvVar`]s.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoadedServers {
    servers: BTreeMap<McpServerName, Entry>,
}

impl LoadedServers {
    /// The loaded form of `declared`, given the `secrets` [`split_secrets`] took from the daemon's
    /// environment and `resolve`, which answers the absolute path of a runnable file for a bare
    /// program name, or `None`.
    ///
    /// An http server's secret goes under a Dudes-owned alias ([`bearer_alias`],
    /// [`header_alias`]), so the operator's own name never reaches an engine that treats some
    /// names as credentials of its own. A program server's goes under its declared name.
    #[must_use]
    pub fn new(
        declared: &DeclaredServers,
        secrets: &[EnvVar],
        resolve: impl Fn(&str) -> Option<String>,
    ) -> Self {
        let value = |name: &VariableName| {
            secrets
                .iter()
                .find(|var| var.name() == OsStr::new(name.as_str()))
                .map(EnvVar::expose_value)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| Unavailable::Variable(name.clone()))
        };
        let servers = declared
            .iter()
            .map(|(name, server)| {
                let mut server = server.clone();
                let env = match &mut server {
                    McpServer::Program(program) => program
                        .secret_env
                        .iter()
                        .map(|variable| value(variable).map(|v| EnvVar::new(variable.as_str(), v)))
                        .collect::<Result<Vec<_>, _>>()
                        .and_then(|env| {
                            if Path::new(&program.command).is_absolute() {
                                return Ok(env);
                            }
                            let resolved = resolve(&program.command).ok_or(Unavailable::Command)?;
                            if resolved.contains('\0') || resolved.contains("${") {
                                return Err(Unavailable::UnsafeCommand);
                            }
                            program.command = resolved;
                            Ok(env)
                        }),
                    McpServer::Http(http) => match &http.auth {
                        HttpAuth::None => Ok(Vec::new()),
                        HttpAuth::Bearer(variable) => {
                            value(variable).map(|v| vec![EnvVar::new(bearer_alias(name), v)])
                        }
                        HttpAuth::Headers(headers) => headers
                            .values()
                            .enumerate()
                            .map(|(position, variable)| {
                                value(variable)
                                    .map(|v| EnvVar::new(header_alias(name, position), v))
                            })
                            .collect(),
                    },
                };
                (name.clone(), Entry { server, env })
            })
            .collect();
        Self { servers }
    }

    /// Apply the hire checks to `names`, in their order, and answer what each launch composes.
    ///
    /// 1. A name this daemon does not declare, or one given twice.
    /// 2. Any server, on an engine that loads none ([`McpSupport::None`]).
    /// 3. Any server together with a per-agent isolation other than the default.
    /// 4. A program server on an engine that loads only http servers.
    /// 5. A server whose variable was unset or empty at start, or whose bare `command` was not
    ///    found.
    /// 6. A program server in a repository the operator did not trust: it starts in the
    ///    repository's directory, where a launcher reads the repository's own configuration
    ///    before the model runs.
    ///
    /// An empty list is admitted on every engine.
    ///
    /// # Errors
    ///
    /// The first [`McpRefusal`] in that order.
    pub fn admit(
        &self,
        names: &[McpServerName],
        rules: Rules,
    ) -> Result<Vec<McpLaunch>, McpRefusal> {
        let mut entries = BTreeMap::new();
        for name in names {
            let Some(entry) = self.servers.get(name) else {
                return Err(McpRefusal::Undeclared(name.clone()));
            };
            if entries.insert(name, entry).is_some() {
                return Err(McpRefusal::Repeated(name.clone()));
            }
        }
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        if rules.support == McpSupport::None {
            return Err(McpRefusal::Unsupported);
        }
        if !rules.default_isolation {
            return Err(McpRefusal::Isolation);
        }
        let program = |entry: &&Entry| matches!(entry.server, McpServer::Program(_));
        if let Some((name, _)) = entries
            .iter()
            .find(|(_, entry)| rules.support == McpSupport::Http && program(entry))
        {
            return Err(McpRefusal::ProgramUnsupported((*name).clone()));
        }
        for (name, entry) in &entries {
            if let Err(cause) = &entry.env {
                return Err(McpRefusal::Unavailable {
                    server: (*name).clone(),
                    cause: cause.clone(),
                });
            }
        }
        if let Some((name, _)) = entries
            .iter()
            .find(|(_, entry)| !rules.trusted && program(entry))
        {
            return Err(McpRefusal::Untrusted((*name).clone()));
        }
        Ok(entries
            .into_iter()
            .map(|(name, entry)| McpLaunch {
                name: name.clone(),
                server: entry.server.clone(),
                env: entry.env.clone().unwrap_or_default(),
            })
            .collect())
    }
}

/// Which declared servers an engine can load.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpSupport {
    /// None: the engine has no way to load only the declared set.
    None,
    /// http servers only: a program the engine starts would leave the turn's process group.
    Http,
    /// Program and http servers.
    Program,
}

/// What the rules of [`LoadedServers::admit`] read about the agent and its engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rules {
    /// The engine's adapter's answer.
    pub support: McpSupport,
    /// Whether the agent runs under the default isolation: no per-agent `isolation` at hire.
    pub default_isolation: bool,
    /// Whether the operator trusted the agent's repository at hire.
    pub trusted: bool,
}

/// Why declared servers were refused, at a hire or at a launch. The text names servers and
/// variables, never a value, so a launch may journal it as the reason an agent is held.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum McpRefusal {
    /// Rule 1: not declared in this daemon's `[mcp.servers]`.
    #[error("MCP server `{0}` is not declared in this daemon's [mcp.servers]")]
    Undeclared(McpServerName),
    /// Rule 1: given more than once.
    #[error("MCP server `{0}` is named more than once")]
    Repeated(McpServerName),
    /// Rule 2: the engine loads no declared server.
    #[error("this engine cannot load declared MCP servers")]
    Unsupported,
    /// Rule 3: an explicit isolation with a server.
    #[error("declared MCP servers load only under the default isolation; omit `isolation`")]
    Isolation,
    /// Rule 4: a program server on an engine that loads only http servers.
    #[error(
        "MCP server `{0}` is a program server, and this engine loads only http servers, because a \
         program it starts would outlive the turn"
    )]
    ProgramUnsupported(McpServerName),
    /// Rule 5: what the server needs was missing when the daemon started.
    #[error("MCP server `{server}` {}", describe_unavailable(.cause))]
    Unavailable {
        /// The server.
        server: McpServerName,
        /// What was missing.
        cause: Unavailable,
    },
    /// Rule 6: a program server in an untrusted repository.
    #[error(
        "MCP server `{0}`: a declared program server starts in this repository's directory, where \
         launchers such as npx read configuration; trust the repository or declare an http server"
    )]
    Untrusted(McpServerName),
}

fn describe_unavailable(cause: &Unavailable) -> String {
    match cause {
        Unavailable::Variable(name) => format!(
            "needs `{name}`, which was unset or empty in the daemon's environment when it started; \
             set it and restart the daemon"
        ),
        Unavailable::Command => "has a `command` the daemon did not find on its PATH when it \
                                 started; install it and restart the daemon"
            .to_owned(),
        Unavailable::UnsafeCommand => "has a resolved `command` containing NUL or `${`; \
                                      use a PATH without interpolation and restart the daemon"
            .to_owned(),
    }
}

impl McpRefusal {
    /// Whether no daemon state could make the request acceptable: an `InvalidParams` refusal at a
    /// hire, where the rest are states the operator can change.
    #[must_use]
    pub const fn is_invalid_request(&self) -> bool {
        !matches!(self, Self::Unavailable { .. } | Self::Untrusted(_))
    }
}

/// One admitted server of one launch: its definition with an absolute `command`, and the pairs
/// its launch adds to the engine's environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpLaunch {
    /// The server.
    pub name: McpServerName,
    /// Its definition.
    pub server: McpServer,
    /// Its secrets, under the names the engine reads them from.
    pub env: Vec<EnvVar>,
}

/// `env` with every pair of `servers` set, replacing a variable of the same name the daemon
/// passed through, sorted by name.
#[must_use]
pub fn with_secrets(mut env: Vec<EnvVar>, servers: &[McpLaunch]) -> Vec<EnvVar> {
    let added: Vec<&EnvVar> = servers.iter().flat_map(|server| &server.env).collect();
    env.retain(|var| {
        !added
            .iter()
            .any(|pair| same_variable(var.name(), &pair.name().to_string_lossy()))
    });
    env.extend(added.into_iter().cloned());
    env.sort();
    env
}

/// The declarations the adapters' tests share, read through the same parser and loading a daemon
/// uses: an http server with a header, a program server with a secret, and an http server with a
/// bearer token, each variable planted.
#[cfg(test)]
pub(crate) mod fixture {
    use super::*;

    pub(crate) const FILE: &str = r#"
[memory]
url = "https://memory.example.com/mcp"
header_env = { "Authorization" = "MEMORY_AUTH" }

[notes]
command = "notes-mcp"
args = ["--stdio"]
secret_env = ["NOTES_API_KEY"]

[search]
url = "http://127.0.0.1:8931/mcp"
bearer_env = "SEARCH_KEY"
"#;

    pub(crate) const MEMORY_AUTH: &str = "planted memory header mcp";
    pub(crate) const NOTES_API_KEY: &str = "planted-notes-secret-mcp";
    pub(crate) const SEARCH_KEY: &str = "planted-search-bearer-mcp";

    pub(crate) fn planted() -> Vec<EnvVar> {
        vec![
            EnvVar::new("MEMORY_AUTH", MEMORY_AUTH),
            EnvVar::new("NOTES_API_KEY", NOTES_API_KEY),
            EnvVar::new("SEARCH_KEY", SEARCH_KEY),
        ]
    }

    pub(crate) fn declared(file: &str) -> DeclaredServers {
        parse_servers(toml::from_str(file).expect("a TOML table")).expect("a valid declaration")
    }

    pub(crate) fn names(names: &[&str]) -> Vec<McpServerName> {
        names
            .iter()
            .map(|name| McpServerName::parse(name).expect("a server name"))
            .collect()
    }

    /// The fixture's servers as an untrusted-or-trusted Claude launch admits them, a bare
    /// `command` resolving under `/opt/homebrew/bin`.
    pub(crate) fn servers(chosen: &[&str]) -> Vec<McpLaunch> {
        LoadedServers::new(&declared(FILE), &planted(), |command| {
            Some(format!("/opt/homebrew/bin/{command}"))
        })
        .admit(
            &names(chosen),
            Rules {
                support: McpSupport::Program,
                default_isolation: true,
                trusted: true,
            },
        )
        .expect("the fixture admits")
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{FILE, declared, names, planted};
    use super::*;

    fn refusal(file: &str) -> String {
        let table: BTreeMap<String, toml::Value> = toml::from_str(file).expect("a TOML table");
        parse_servers(table)
            .expect_err("the declaration is refused")
            .to_string()
    }

    const NAME: &str = "an [mcp.servers] table has a name that is not 1 to 32 lowercase letters \
                        and digits in runs joined by single underscores, starting with a letter";
    const MALFORMED: &str = "[mcp.servers.x] has an unknown key or a value of the wrong type";
    const KIND: &str = "[mcp.servers.x] needs exactly one of `command` and `url`";
    const MIXED: &str = "[mcp.servers.x] mixes a program server's keys (`command`, `args`, \
                         `secret_env`) with an http server's (`url`, `bearer_env`, `header_env`)";
    const EXPANDED: &str =
        "[mcp.servers.x] holds a NUL or `${`, which an engine would expand from its environment";
    const COMMAND: &str =
        "[mcp.servers.x] has a `command` that is not a bare name or an absolute path";
    const URL: &str = "[mcp.servers.x] has a `url` that is not https, or http to 127.0.0.1, ::1 \
                       or localhost, with no user information";
    const HEADER: &str = "[mcp.servers.x] has a `header_env` header name that is not an HTTP token";
    const NO_HEADER: &str = "[mcp.servers.x] has an empty `header_env`";
    const PLACEHOLDER: &str = "[mcp.servers.x] has an `args` entry that holds `{agent_dir}` \
                               beside other text; the token is valid only as a whole entry";
    const BOTH: &str = "[mcp.servers.x] sets both `bearer_env` and `header_env`";
    const SHAPE: &str = "[mcp.servers.x] names a variable that is not uppercase letters, digits \
                         and underscores, not starting with a digit";

    /// The declaration checks, one literal message each. Mutant: drop any one check in [`declare`]
    /// or [`url_allowed`], and its row parses instead of failing.
    #[test]
    fn each_declaration_rule_refuses_with_its_literal_message() {
        let url = "url = \"https://h.example/mcp\"";
        for (file, message) in [
            (format!("[Mem]\n{url}"), NAME),
            (format!("[a__b]\n{url}"), NAME),
            (format!("[x]\n{url}\ncolour = \"red\""), MALFORMED),
            ("[x]\nargs = [\"a\"]".to_owned(), KIND),
            (format!("[x]\ncommand = \"a\"\n{url}"), KIND),
            (format!("[x]\n{url}\nsecret_env = [\"KEY\"]"), MIXED),
            (
                "[x]\ncommand = \"a\"\nargs = [\"${HOME}/x\"]".to_owned(),
                EXPANDED,
            ),
            (
                "[x]\ncommand = \"a\"\nargs = [\"a\\u0000b\"]".to_owned(),
                EXPANDED,
            ),
            (
                "[x]\nurl = \"https://h.example/${PATH}\"".to_owned(),
                EXPANDED,
            ),
            (
                "[x]\ncommand = \"a\"\nargs = [\"--root={agent_dir}\"]".to_owned(),
                PLACEHOLDER,
            ),
            ("[x]\ncommand = \"bin/notes\"".to_owned(), COMMAND),
            ("[x]\nurl = \"http://example.com/mcp\"".to_owned(), URL),
            (
                "[x]\nurl = \"https://user:pw@example.com/mcp\"".to_owned(),
                URL,
            ),
            ("[x]\nurl = \"ftp://example.com/mcp\"".to_owned(), URL),
            (
                format!("[x]\n{url}\nheader_env = {{ \"Bad Header\" = \"KEY\" }}"),
                HEADER,
            ),
            (format!("[x]\n{url}\nheader_env = {{}}"), NO_HEADER),
            (
                format!("[x]\n{url}\nbearer_env = \"A\"\nheader_env = {{ \"X\" = \"B\" }}"),
                BOTH,
            ),
            (
                "[x]\ncommand = \"a\"\nsecret_env = [\"notes_key\"]".to_owned(),
                SHAPE,
            ),
            (
                "[x]\ncommand = \"a\"\nsecret_env = [\"1KEY\"]".to_owned(),
                SHAPE,
            ),
        ] {
            assert_eq!(refusal(&file), message, "{file}");
        }
    }

    /// A declared variable leaves every other child's environment, so a name a child or an engine
    /// needs is refused, named. Mutant: drop a prefix or a list from [`reserved_variable`].
    #[test]
    fn a_reserved_variable_is_refused_by_name() {
        for name in [
            "DUDES_EXTRA",
            "CLAUDECODE",
            "CLAUDE_X",
            "ANTHROPIC_BASE_URL",
            "CODEX_HOME",
            "OPENAI_ORG",
            "PI_DIR",
            "AWS_BEARER_TOKEN_BEDROCK",
            "GEMINI_API_KEY",
            "META_API_KEY",
            "TYPESAFE_API_KEY",
            "PATH",
            "HOME",
            "TMPDIR",
            "LANG",
            "TEMP",
            "USERPROFILE",
            "USER",
        ] {
            assert_eq!(
                refusal(&format!("[x]\ncommand = \"a\"\nsecret_env = [\"{name}\"]")),
                format!(
                    "[mcp.servers.x] names `{name}`, which Dudes, an engine or the daemon's own \
                     children need for themselves"
                ),
            );
        }
        assert!(!reserved_variable("NOTES_API_KEY"));
    }

    /// The control for the table above: every accepted shape parses, so the refusals are the
    /// rules' and not a parser that refuses everything.
    #[test]
    fn the_accepted_shapes_parse_to_their_kinds() {
        let servers = declared(FILE);
        assert_eq!(
            servers
                .keys()
                .map(McpServerName::as_str)
                .collect::<Vec<_>>(),
            ["memory", "notes", "search"]
        );
        assert_eq!(
            servers[&names(&["notes"])[0]],
            McpServer::Program(ProgramServer {
                command: "notes-mcp".to_owned(),
                args: vec!["--stdio".to_owned()],
                secret_env: vec![VariableName("NOTES_API_KEY".to_owned())],
            })
        );
        for url in [
            "https://h.example",
            "HTTPS://h.example:8443/mcp?x=1",
            "http://127.0.0.1:9/mcp",
            "http://[::1]:9/mcp",
            "http://LOCALHOST/mcp",
        ] {
            assert!(url_allowed(url), "{url}");
        }
        for url in [
            "http://127.0.0.2/mcp",
            "https://",
            "https:///x",
            "http://[::1",
        ] {
            assert!(!url_allowed(url), "{url}");
        }
        let command = toml::Value::String(
            std::env::temp_dir()
                .join("mcp-example")
                .display()
                .to_string(),
        );
        let program = declared(&format!(
            "[a]\ncommand = {command}\n[b]\nurl = \"https://h.example\""
        ));
        assert!(matches!(program[&names(&["a"])[0]], McpServer::Program(_)));
        assert!(matches!(
            program[&names(&["b"])[0]],
            McpServer::Http(HttpServer {
                auth: HttpAuth::None,
                ..
            })
        ));
    }

    /// A value pasted where a variable name belongs, or a malformed server name, never reaches
    /// the message. Mutant: echo the offending text, as an ordinary parser error would.
    #[test]
    fn no_refusal_repeats_text_from_the_file() {
        for (file, planted) in [
            (
                "[x]\ncommand = \"a\"\nsecret_env = [\"sk-live-planted-value\"]",
                "sk-live-planted-value",
            ),
            (
                "[x]\nurl = \"https://h.example\"\nbearer_env = \"planted bearer\"",
                "planted bearer",
            ),
            (
                "[\"Planted Name\"]\nurl = \"https://h.example\"",
                "Planted Name",
            ),
            ("[x]\nurl = \"https://planted@h.example\"", "planted"),
        ] {
            assert!(!refusal(file).contains(planted), "{file}");
        }
    }

    /// The boot split. Mutant: keep a declared variable in the remainder, and every child of the
    /// daemon inherits the secret.
    #[test]
    fn the_split_moves_exactly_the_declared_variables() {
        let daemon_env = [
            EnvVar::new("PATH", "/usr/bin"),
            EnvVar::new("NOTES_API_KEY", "n"),
            EnvVar::new("UNDECLARED", "u"),
            EnvVar::new("MEMORY_AUTH", "m"),
        ];
        let (remainder, secrets) = split_secrets(&daemon_env, &declared(FILE));
        assert_eq!(
            remainder,
            [
                EnvVar::new("PATH", "/usr/bin"),
                EnvVar::new("UNDECLARED", "u")
            ]
        );
        assert_eq!(
            secrets,
            [
                EnvVar::new("NOTES_API_KEY", "n"),
                EnvVar::new("MEMORY_AUTH", "m")
            ]
        );

        let (remainder, secrets) =
            split_secrets(&[EnvVar::new("Notes_Api_Key", "n")], &declared(FILE));
        if cfg!(windows) {
            assert_eq!(
                (remainder, secrets),
                (Vec::new(), vec![EnvVar::new("NOTES_API_KEY", "n")])
            );
        } else {
            assert_eq!(
                (remainder, secrets),
                (vec![EnvVar::new("Notes_Api_Key", "n")], Vec::new())
            );
        }
    }

    fn rules(support: McpSupport, trusted: bool) -> Rules {
        Rules {
            support,
            default_isolation: true,
            trusted,
        }
    }

    /// Each refusal of [`LoadedServers::admit`], and that an earlier rule wins over a later one.
    /// Mutant: reorder two rules, or drop one.
    #[test]
    fn admission_applies_the_rules_in_their_order() {
        let loaded = LoadedServers::new(&declared(FILE), &planted(), |_| None);
        let admit = |chosen: &[&str], rules: Rules| loaded.admit(&names(chosen), rules);
        let name = |n: &str| names(&[n]).remove(0);
        let claude = rules(McpSupport::Program, true);
        assert_eq!(
            admit(&["nope"], claude),
            Err(McpRefusal::Undeclared(name("nope")))
        );
        assert_eq!(
            admit(&["memory", "memory"], claude),
            Err(McpRefusal::Repeated(name("memory")))
        );
        assert_eq!(
            admit(&["notes", "nope"], rules(McpSupport::None, false)),
            Err(McpRefusal::Undeclared(name("nope")))
        );
        assert_eq!(
            admit(&["memory"], rules(McpSupport::None, false)),
            Err(McpRefusal::Unsupported)
        );
        assert_eq!(admit(&[], rules(McpSupport::None, false)), Ok(Vec::new()));
        assert_eq!(
            admit(
                &["memory"],
                Rules {
                    default_isolation: false,
                    ..claude
                }
            ),
            Err(McpRefusal::Isolation)
        );
        assert_eq!(
            admit(&["memory", "notes"], rules(McpSupport::Http, false)),
            Err(McpRefusal::ProgramUnsupported(name("notes")))
        );
        // `notes` is a bare command and the resolver above finds nothing.
        assert_eq!(
            admit(&["notes"], rules(McpSupport::Program, false)),
            Err(McpRefusal::Unavailable {
                server: name("notes"),
                cause: Unavailable::Command
            })
        );
        let resolved =
            LoadedServers::new(&declared(FILE), &planted(), |c| Some(format!("/bin/{c}")));
        assert_eq!(
            resolved.admit(&names(&["notes"]), rules(McpSupport::Program, false)),
            Err(McpRefusal::Untrusted(name("notes")))
        );
        let unset = LoadedServers::new(&declared(FILE), &[EnvVar::new("MEMORY_AUTH", "")], |c| {
            Some(format!("/bin/{c}"))
        });
        assert_eq!(
            unset.admit(&names(&["search", "memory"]), claude),
            Err(McpRefusal::Unavailable {
                server: name("memory"),
                cause: Unavailable::Variable(VariableName("MEMORY_AUTH".to_owned()))
            })
        );
    }

    /// What an admitted launch carries: the resolved absolute command, and each secret under the
    /// name the engine reads. Mutant: hand the engine the operator's own variable name for an
    /// http secret, or leave a bare command unresolved.
    #[test]
    fn an_admitted_launch_carries_aliases_and_the_resolved_command() {
        let launches = super::fixture::servers(&["search", "notes", "memory"]);
        let summary: Vec<(&str, Vec<EnvVar>)> = launches
            .iter()
            .map(|launch| (launch.name.as_str(), launch.env.clone()))
            .collect();
        assert_eq!(
            summary,
            [
                (
                    "memory",
                    vec![EnvVar::new(
                        "DUDES_MCP_MEMORY_HEADER_0",
                        super::fixture::MEMORY_AUTH
                    )]
                ),
                (
                    "notes",
                    vec![EnvVar::new("NOTES_API_KEY", super::fixture::NOTES_API_KEY)]
                ),
                (
                    "search",
                    vec![EnvVar::new(
                        "DUDES_MCP_SEARCH_BEARER",
                        super::fixture::SEARCH_KEY
                    )]
                ),
            ]
        );
        let McpServer::Program(program) = &launches[1].server else {
            panic!("notes is a program server");
        };
        assert_eq!(program.command, "/opt/homebrew/bin/notes-mcp");
    }

    #[test]
    fn a_resolved_program_command_refuses_interpolation_and_nul_without_reflecting_the_path() {
        for command in ["/private/${HOME}/notes-mcp", "/private/\0notes-mcp"] {
            let loaded = LoadedServers::new(&declared(FILE), &planted(), |_| Some(command.into()));
            let refusal = loaded
                .admit(&names(&["notes"]), rules(McpSupport::Program, true))
                .expect_err("the resolved command is unsafe for the engine");
            assert_eq!(
                refusal.to_string(),
                "MCP server `notes` has a resolved `command` containing NUL or `${`; \
                 use a PATH without interpolation and restart the daemon"
            );
            assert_eq!(
                loaded
                    .admit(&names(&["memory"]), rules(McpSupport::Program, true))
                    .expect("the unsafe unselected server does not block another server")[0]
                    .name,
                names(&["memory"])[0]
            );
        }
        let loaded = LoadedServers::new(&declared(FILE), &planted(), |_| {
            Some("/private/bin/notes-mcp".into())
        });
        let admitted = loaded
            .admit(&names(&["notes"]), rules(McpSupport::Program, true))
            .expect("a safe resolved command");
        let McpServer::Program(program) = &admitted[0].server else {
            panic!("a program server");
        };
        assert_eq!(program.command, "/private/bin/notes-mcp");
    }

    /// A variable the daemon passed through under an alias's name is replaced, not doubled.
    #[test]
    fn launch_secrets_replace_a_passed_through_name() {
        let launches = super::fixture::servers(&["search"]);
        let env = with_secrets(
            vec![
                EnvVar::new("PATH", "/usr/bin"),
                EnvVar::new("DUDES_MCP_SEARCH_BEARER", "ambient"),
            ],
            &launches,
        );
        assert_eq!(
            env,
            [
                EnvVar::new("DUDES_MCP_SEARCH_BEARER", super::fixture::SEARCH_KEY),
                EnvVar::new("PATH", "/usr/bin"),
            ]
        );
    }

    /// Refusal text names servers and variables and holds no value, because a launch journals it.
    #[test]
    fn a_refusal_holds_no_value() {
        let loaded =
            LoadedServers::new(&declared(FILE), &[EnvVar::new("NOTES_API_KEY", "")], |_| {
                None
            });
        let refused = loaded
            .admit(&names(&["memory"]), rules(McpSupport::Program, true))
            .expect_err("MEMORY_AUTH is unset")
            .to_string();
        assert_eq!(
            refused,
            "MCP server `memory` needs `MEMORY_AUTH`, which was unset or empty in the daemon's \
             environment when it started; set it and restart the daemon"
        );
        for value in [
            super::fixture::MEMORY_AUTH,
            super::fixture::NOTES_API_KEY,
            super::fixture::SEARCH_KEY,
        ] {
            assert!(!refused.contains(value));
        }
    }
}
