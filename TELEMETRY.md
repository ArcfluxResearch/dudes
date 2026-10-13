# What Dudes and its engines send

The `duded`, `dudes`, and `dudes-hook` binaries use local connections by default.
Usage and error information can pass through those local connections. A custom
Windows `DUDES_SOCK` path can select a remote named-pipe server, as described below.
Separate programs launched by Dudes can make their own network connections.

## Dudes connections and endpoint overrides

By default, the CLI and hook communicate with the daemon through a Unix socket or
a Windows named pipe. The daemon's Windows pipe refuses remote clients. The daemon
serves the Floor on `127.0.0.1`. The Floor loads its assets from that server and
uses the page's origin for HTTP requests and its WebSocket connection. It loads no remote fonts,
scripts, or analytics. Opening a setup documentation link visits an external site
in your browser.

On Windows, the CLI and hook use `DUDES_SOCK` as the pipe path when you set it.
They do not require a local pipe name before connecting. Windows allows a pipe
path to name another computer, so a custom value can cause a connection attempt
to that computer. See [Windows pipe names](https://learn.microsoft.com/en-us/windows/win32/ipc/pipe-names).
For ordinary CLI use, leave `DUDES_SOCK` unset to use Dudes' default local endpoint.
Dudes sets the hook's endpoint when it launches an engine.

Dudes gives engines their task input and reads the output and protocol messages
they expose locally. Dudes does not intercept or proxy the engines' network
connections. Engine requests can contain prompts, repository content, and tool
results. Your engine, model provider, account, and settings determine where that
data goes and how the provider retains or uses it.

Separate programs can make their own connections. These include engines and their
login commands, configured tools and MCP servers, completion checks, and your
selected editor. Git checkout can also run repository-configured filters that
connect to a service. Dudes does not add a network sandbox around these programs.
See [what agents can do on your machine](docs/what-agents-can-do.md).

The installer is separate from the installed binaries. It uses `curl` to download
release archives and `SHA256SUMS` from GitHub. See [release verification](SECURITY.md).

## Engine traffic and controls

The controls below come from the vendors' documentation, checked on 13 October
2026. Check the documentation for your installed engine version. Turning off
optional telemetry does not stop the model requests needed to run an agent.

### Claude Code

Claude Code sends prompts and model context to its configured model provider using
your credentials. Anthropic documents usage metrics and error reports separately
from model requests in [Claude Code data usage](https://code.claude.com/docs/en/data-usage).
That page also links the privacy, retention, and training policies for each account
type and provider.

Set `DISABLE_TELEMETRY=1` to disable usage metrics and
`DISABLE_ERROR_REPORTING=1` to disable error reports. Set
`CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` to disable the nonessential traffic
covered by that setting. The vendor documents exceptions and effects on other
features on the same page.

For Dudes-launched Claude Code turns, export these variables before starting the
daemon. If it is running, stop it first, then start it from that environment.
The Claude Code adapter passes these environment variables to the engine. Dudes'
default Managed launch excludes Claude Code's user settings. Trusted repositories
still load project settings, and an explicit settings-source override can change
that selection. Check the settings your launch admits if they also set these
environment variables.

### Codex

Codex sends model requests to its configured provider using your sign-in or API
credentials. Read [OpenAI's API data controls](https://developers.openai.com/api/docs/guides/your-data)
for API-key use and the [Codex plan and data commitments](https://learn.chatgpt.com/docs/pricing)
for ChatGPT sign-in. The account's policies govern model data separately from
client analytics.

For a Codex session that reads your `config.toml`, set `analytics.enabled = false`.
The [Codex configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference)
also documents `feedback.enabled` and the OpenTelemetry exporters. These are
separate controls. See [Codex telemetry guidance](https://learn.chatgpt.com/docs/agent-approvals-security#monitoring-and-telemetry)
before you enable an exporter or export prompts.

Dudes' legacy Managed Codex launch uses `--ignore-user-config`. Its streaming
launch uses a separate provider home that refuses a `config.toml`. Your ordinary
Codex configuration therefore does not control analytics for those launches.
Dudes currently supplies no analytics override for Managed Codex turns.

### Pi

Pi sends model context to the provider or compatible endpoint you select, using
your subscription or API credentials. A compatible endpoint can be local.
Read [Pi's model and connection documentation](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/models.md)
and [provider setup](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/providers.md).
The selected provider's privacy and retention policies apply to those requests.

Pi's current [settings documentation](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/settings.md#updates-telemetry-and-warnings)
lists `enableInstallTelemetry` for anonymous install/update reports and selected
provider attribution headers, and `enableAnalytics` for optional analytics.
Set both to `false` in Pi's `settings.json` if your installed version supports
them. Install telemetry and update checks are separate. See [Pi configuration](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/configuration.md)
for the user settings file location. The Pi adapter passes the daemon's environment.
Dudes' Managed Pi launch excludes project settings and automatically discovered
extensions. Set these controls in Pi's user settings.

## Check the boundary

The source is available for inspection. CI's `cargo xtask layering --strict`
checks dependency layering, rejects known HTTP and TLS client dependencies for
the shipped binaries, and scans their reachable Rust source for known outbound
socket and DNS APIs. It permits IPC and does not validate custom Windows pipe
destinations. These static checks do not isolate engines or other child programs.
They are not a runtime traffic recording or proof against every possible
implementation.
