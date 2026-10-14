//! [`Adapters`]: the one adapter registry, built once from the operator's `[providers]` table.
//!
//! # Not a `'static` registry
//!
//! A `fn for_provider(Provider) -> &'static dyn Adapter` cannot exist:
//! [`ClaudeAdapter::new`](crate::claude::ClaudeAdapter::new) takes a
//! [`ClaudeConfig`], so an adapter is a value built from the operator's file at daemon start, not
//! a zero-config singleton with the program's lifetime. What replaces it is a value: built once,
//! borrowed for as long as the daemon runs, and asked [`Adapters::get`] per hire and per launch.
//!
//! # One registry, two readers, and why that is the point
//!
//! The supervisor holds this value and reads it when it composes a launch; the daemon's
//! `agent.hire` reads the *same* value to decide whether to admit the hire at all. A second map
//! built beside it would let the two drift, and the failure would be silent in the worst
//! direction: a hire admitted for a provider no launch can start, leaving an agent that is
//! journaled, visible on the floor and permanently unable to take a turn. Because both readers
//! ask this one value, "hire refuses exactly what launch cannot start" is a property of the type
//! rather than of two call sites agreeing.
//!
//! # No environment read, no clock, no global sink
//!
//! Everything arrives as an argument, including the daemon's environment: the `[providers]` table
//! is parsed by the daemon's config loader and handed in. `tests/purity.rs` walks this file and
//! enforces that — measured, a read of the process environment planted in
//! [`Adapters::from_config`] reddens it, naming this file and the line.
//!
//! **That gate covers reads of the environment and print/log sinks, and nothing wider.** It has
//! no token for the filesystem or for spawning, so "this module opens no file" is true of the
//! source as written and is *not* held by a check: a `read_to_string` planted in the same place
//! is measured green. Read that half as a description of today's code rather than as a
//! guarantee, and do not lean on it the way the first half can be leaned on. (The scan matches
//! substrings in comments too, which is why this paragraph names no token literally.)

use std::collections::BTreeMap;
use std::sync::Arc;

use dudes_types::Provider;

use crate::adapter::Adapter;
use crate::claude::ClaudeAdapter;
use crate::claude::config::{ClaudeConfig, ConfigError};
use crate::launch::EnvVar;

/// The Claude entry accepted by [`Adapters::from_config`].
///
/// **A provider is absent until its table is written.** `claude: None` is not "the default
/// Claude adapter", it is *no Claude adapter*, and a hire on it is refused. That is the
/// conservative direction: an operator who has not said which `command` to run, which
/// `permission_mode` to launch under and whether to isolate the turn has not asked for an agent
/// to be launched under guesses about all three.
///
/// This compatibility constructor input stays Claude-shaped. Production registry assembly uses
/// [`Adapters::new`], so adding another adapter does not add another named field here.
///
/// This compatibility input has no serialization contract. `duded` parses provider tables
/// in its configuration loader and assembles the production registry with [`Adapters::new`].
///
/// It also settles a question that is not this type's to settle. [`ClaudeConfig`] is
/// `deny_unknown_fields` because an operator's misspelled *key* silently widens what a turn runs.
/// Applying the same attribute to the table *of providers* means something else: once a second
/// adapter exists, a `[providers.codex]` written for a newer daemon makes an **older** one refuse
/// the whole file at startup rather than ignore one provider it cannot serve. Whether that
/// trade is right belongs with the config loader that has to live with it, decided against a
/// round-trip test in **TOML**, which is the spelling the table actually has.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProvidersConfig {
    /// `[providers.claude]`. `None` means no Claude adapter, not a defaulted one.
    pub claude: Option<ClaudeConfig>,
}

/// The adapters one daemon run has, keyed by provider.
///
/// Built by [`Adapters::new`] in production; [`Adapters::from_config`] remains the checked
/// constructor for the existing Claude configuration surface.
/// There is no way to add one after construction: a registry that could grow would be a registry
/// two readers could see differently depending on when they asked.
#[derive(Default)]
pub struct Adapters {
    by_provider: BTreeMap<Provider, Arc<dyn Adapter>>,
}

impl Adapters {
    /// Build one immutable registry from configured adapter instances.
    ///
    /// Each adapter supplies its own [`Provider`] key. A caller therefore cannot register a value
    /// under a provider it does not serve, and adding a provider does not change this registry's
    /// type or its daemon wiring.
    #[must_use]
    pub fn new(adapters: impl IntoIterator<Item = Box<dyn Adapter>>) -> Self {
        Self {
            by_provider: adapters
                .into_iter()
                .map(|adapter| (adapter.provider(), Arc::from(adapter)))
                .collect(),
        }
    }

    /// The registry the `[providers]` table describes.
    ///
    /// `daemon_env` is the environment a managed child will start from, **before** any scrub, and
    /// it is an argument rather than something read here — the same discipline
    /// [`ClaudeConfig::check`] keeps, for the same reason.
    ///
    /// # Why the environment is a second parameter
    ///
    /// The check a table has to pass is not a property of the table alone.
    /// `isolation = "bare"` is honourable only where the adapter can see an API-key path, because
    /// bare mode reads no OAuth credential and no keychain — so refusing it
    /// needs the environment. Without that parameter this function could not fail, and a `Result`
    /// that is always `Ok` would tell a caller nothing.
    ///
    /// This compatibility constructor validates only the supplied Claude entry. Production
    /// startup validates its configuration and assembles all configured adapters with
    /// [`Self::new`].
    ///
    /// # Errors
    ///
    /// Whatever [`ClaudeConfig::check`] refuses the `[providers.claude]` table for.
    pub fn from_config(
        config: &ProvidersConfig,
        daemon_env: &[EnvVar],
    ) -> Result<Self, ConfigError> {
        let mut adapters: Vec<Box<dyn Adapter>> = Vec::new();
        if let Some(claude) = &config.claude {
            claude.check(daemon_env)?;
            adapters.push(Box::new(ClaudeAdapter::new(claude.clone())));
        }
        Ok(Self::new(adapters))
    }

    /// The adapter for `provider`, or `None` where this daemon has none.
    ///
    /// `None` has one meaning and it is not "not supported yet": it is *this run cannot launch a
    /// turn on that provider*, which is why `agent.hire` refuses on it rather than journaling an
    /// agent it could never start.
    #[must_use]
    pub fn get(&self, provider: Provider) -> Option<&dyn Adapter> {
        self.by_provider.get(&provider).map(AsRef::as_ref)
    }

    /// Share the same immutable configured adapter with a bounded observation worker.
    #[must_use]
    pub fn shared(&self, provider: Provider) -> Option<Arc<dyn Adapter>> {
        self.by_provider.get(&provider).map(Arc::clone)
    }
}

/// Collect adapters into a registry, keyed by what each one says it is.
///
/// The seam a test uses to put a recorder, a stub or a single real adapter behind the machine
/// without going through a config table. Keyed on [`Adapter::provider`] rather than on a key the
/// caller supplies, so a registry cannot file an adapter under a provider it does not serve — the
/// same reason `from_config` derives the key rather than taking one.
impl FromIterator<Box<dyn Adapter>> for Adapters {
    fn from_iter<I: IntoIterator<Item = Box<dyn Adapter>>>(adapters: I) -> Self {
        Self::new(adapters)
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::Path;

    use dudes_types::{AgentId, ModelSelection, SessionId, Tier};

    use super::*;
    use crate::adapter::{
        AgentCtx, Isolation, ManagedInstall, SessionAcquisition, TurnSession, UsageSource,
    };
    use crate::claude::SESSION_ACQUISITION;
    use crate::claude::stream::TurnStream;
    use crate::launch::{LaunchError, LaunchSpec};
    use crate::stream::TurnReader;
    use dudes_types::IsolationMode;

    use crate::claude::config::PermissionMode;

    /// A table with `[providers.claude]` in it.
    fn with_claude(config: ClaudeConfig) -> ProvidersConfig {
        ProvidersConfig {
            claude: Some(config),
        }
    }

    /// A config with only `[providers.claude]` yields `Some` for [`Provider::Claude`]
    /// and `None` for [`Provider::Codex`].
    ///
    /// Three arms, because the first two alone are green on registries that are plainly wrong.
    ///
    /// - The `Some` arm alone passes on a registry that answers `Some` for **everything**, so the
    ///   `None` arm sweeps all nine other [`Provider::ALL`] values rather than only the `Codex`
    ///   the specific case.
    /// - Both of those together still pass on a registry with a **hardcoded Claude arm** that
    ///   never reads the table — which is the likelier mistake, since Claude is the only adapter
    ///   there is. The third arm is the control for that: the same call against an *empty* table
    ///   must answer `None` for `Claude` too.
    #[test]
    fn only_a_configured_table_gets_an_adapter() {
        let configured = Adapters::from_config(&with_claude(ClaudeConfig::default()), &[])
            .expect("the default table is honourable");
        assert!(
            configured.get(Provider::Claude).is_some(),
            "`[providers.claude]` is what puts the Claude adapter in the registry"
        );
        assert!(
            configured.get(Provider::Codex).is_none(),
            "no `[providers.codex]` adapter exists, so the registry must not answer for it"
        );
        for provider in Provider::ALL.into_iter().filter(|p| *p != Provider::Claude) {
            assert!(
                configured.get(provider).is_none(),
                "{provider} has no adapter and must not be answered for"
            );
        }

        let empty = Adapters::from_config(&ProvidersConfig::default(), &[])
            .expect("an empty table is honourable");
        assert!(
            empty.get(Provider::Claude).is_none(),
            "the control: with no `[providers.claude]` table there is no Claude adapter either, \
             so `get` reads the config rather than the provider's name"
        );
        assert_eq!(empty.by_provider.len(), 0);
    }

    /// The returned adapter's `managed()` reflects the config's `permission_mode`.
    ///
    /// The flag is the observable: `argv` pushes `--dangerously-skip-permissions` exactly under
    /// [`PermissionMode::Bypass`]. Asserting only its *absence* under `Default` is green on a
    /// registry that ignored the table entirely and built one from `ClaudeConfig::default()` —
    /// no, worse than green: that default **is** `Bypass`, so the absence arm alone would be red
    /// for the right reason by luck. Both polarities are built through `from_config` here, so
    /// what is checked is that the value travelled, not that one particular value did.
    #[test]
    fn the_returned_adapter_carries_the_configured_permission_mode() {
        let flag = OsString::from("--dangerously-skip-permissions");
        let argv_under = |mode: PermissionMode| {
            let registry = Adapters::from_config(
                &with_claude(ClaudeConfig {
                    permission_mode: mode,
                    ..ClaudeConfig::default()
                }),
                &[],
            )
            .expect("both permission modes are honourable");
            let adapter = registry
                .get(Provider::Claude)
                .expect("the table was configured");
            let ctx = AgentCtx {
                agent: AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("a well-formed ULID"),
                cwd: Path::new("/repo"),
                agent_dir: Path::new("/var/lib/dudes/agents/a"),
                daemon_env: &[],
                sandbox: None,
                trusted: false,
                max_turns: None,
                max_budget: None,
                mcp_servers: &[],
            };
            adapter
                .managed(
                    &ctx,
                    "ship it",
                    &TurnSession::First(SessionId::from("s1")),
                    &ModelSelection::provider_default(),
                )
                .expect("a non-empty task without NUL is a launch")
                .argv
        };

        assert!(
            !argv_under(PermissionMode::Default).contains(&flag),
            "`permission_mode = \"default\"` must not compose the bypass flag"
        );
        assert!(
            argv_under(PermissionMode::Bypass).contains(&flag),
            "the control: the flag is composed under `bypass`, so the assertion above is about \
             the configured value and not about a flag this argv never carries"
        );
    }

    /// A `[providers.claude]` table the adapter cannot honour is refused **at build**, so the
    /// `Result` is not vacuous.
    ///
    /// `isolation = "bare"` reads no OAuth credential and no keychain, so it needs an API-key path
    /// in the daemon's environment. Without the second arm this test would pass
    /// on a `from_config` that never called [`ClaudeConfig::check`] at all — and without the first
    /// it would pass on one that refused every table.
    #[test]
    fn a_table_the_adapter_cannot_honour_is_refused_at_build() {
        let bare = with_claude(ClaudeConfig {
            isolation: IsolationMode::Bare,
            ..ClaudeConfig::default()
        });
        assert_eq!(
            Adapters::from_config(&bare, &[EnvVar::new("PATH", "/usr/bin")]).err(),
            Some(ConfigError::BareWithoutApiKey),
            "a table the adapter cannot honour must not reach the registry"
        );
        assert!(
            Adapters::from_config(
                &bare,
                &[EnvVar::new(
                    "ANTHROPIC_API_KEY",
                    "fixture-value-not-a-credential"
                )]
            )
            .is_ok(),
            "the control: the same table builds once the adapter can see a key, so the refusal \
             above is the check firing and not `from_config` refusing everything"
        );
    }

    struct TestAdapter(Provider);

    impl Adapter for TestAdapter {
        fn provider(&self) -> Provider {
            self.0
        }

        fn program(&self) -> &std::ffi::OsStr {
            std::ffi::OsStr::new("claude")
        }

        fn tier(&self) -> Tier {
            Tier::ObservedOnly
        }

        fn isolation(&self) -> Isolation {
            Isolation::None
        }

        fn session_acquisition(&self) -> SessionAcquisition {
            SESSION_ACQUISITION
        }

        fn turn_reader(&self) -> Box<dyn TurnReader> {
            Box::new(TurnStream::new())
        }

        fn model_selection(&self) -> ModelSelection {
            ModelSelection::provider_default()
        }

        fn managed(
            &self,
            ctx: &AgentCtx<'_>,
            _task: &str,
            _session: &TurnSession,
            _selection: &ModelSelection,
        ) -> Result<LaunchSpec, LaunchError> {
            Ok(LaunchSpec {
                argv: vec![self.0.as_str().into()],
                env: Vec::new(),
                cwd: ctx.cwd.to_path_buf(),
                stdin: crate::launch::LaunchInput::Null,
            })
        }

        fn managed_install(
            &self,
            _ctx: &AgentCtx<'_>,
            _selection: &ModelSelection,
        ) -> ManagedInstall {
            ManagedInstall::default()
        }

        fn usage_source(&self) -> UsageSource {
            UsageSource::None
        }

        fn sandbox_flag(&self) -> Option<&'static [&'static str]> {
            None
        }

        fn billing_env_vars(&self) -> &'static [&'static str] {
            &[]
        }
    }

    /// two provider keys resolve to their own configured adapter, and the immutable
    /// registry hands hire and launch the same stored object rather than constructing two views.
    #[test]
    fn two_providers_share_each_registered_adapter_between_hire_and_launch() {
        let registry = Adapters::new(
            [Provider::Claude, Provider::Codex]
                .map(|provider| Box::new(TestAdapter(provider)) as Box<dyn Adapter>),
        );

        for provider in [Provider::Claude, Provider::Codex] {
            let hire = registry.get(provider).expect("hire finds the adapter");
            let launch = registry.get(provider).expect("launch finds the adapter");

            assert_eq!(hire.provider(), provider, "the key selects its adapter");
            assert!(
                std::ptr::eq(hire, launch),
                "hire and launch borrow the same {provider} adapter instance"
            );
        }
    }

    /// The test seam keys an adapter on what it says it is, so a registry collected from
    /// adapters answers the same way one built from a table does.
    #[test]
    fn collecting_adapters_keys_them_on_the_provider_they_serve() {
        let registry: Adapters =
            [Box::new(ClaudeAdapter::new(ClaudeConfig::default())) as Box<dyn Adapter>]
                .into_iter()
                .collect();
        assert!(registry.get(Provider::Claude).is_some());
        assert!(registry.get(Provider::Codex).is_none());
    }
}
