# system-builder

A small web API (Rust + Axum, Cargo workspace) that builds Gentoo systems —
one or more `systemd-nspawn` builders plus the host itself — one at a time,
with queueing, live `emerge` streaming, and build-time estimates from
`emerge.log` history.

## Layout

```text
.
├── config.example.toml      # copy to config.toml and edit
├── install.sh / uninstall.sh / Makefile
├── packaging/
│   └── system-builder.service
├── crates/
│   ├── gb-types/            # shared JSON contract (requests, jobs, events)
│   ├── gb-core/             # engine: queue, runner, history, estimates
│   ├── gb-server/           # Axum HTTP API (gb-server binary)
│   └── gb-client/           # client CLI (gb-client binary)
└── examples/
    └── client.sh            # curl walkthrough of the API
```

## Requirements

- The **server host must run systemd on Linux**: builders are managed
  through `machinectl` / `systemd-nspawn`, and `gb-server` itself ships as
  a systemd unit. The server runs as root (it enters container namespaces
  with `nsenter` and starts machines on demand).
- Builders are Gentoo `systemd-nspawn` images (named `gentoo-builder-*`
  for auto-discovery). Guests may run OpenRC or systemd — control goes
  through `nsenter`, never `machinectl shell`.
- Clients only need the `gb-client` binary; the follow-up local `emerge`
  step obviously needs Gentoo with Portage.
- A Rust toolchain to build.

## How it works

1. The client sends **system + targets** (`POST /v1/builds`).
   Targets are package atoms (`sys-apps/foo`) and/or sets (`@system`, `@world`).
2. The server resolves the plan with `emerge -p --update --newuse --deep`
   inside the target, estimates each package from history, then runs the
   real `emerge` (default args `--update --newuse --deep --verbose`),
   streaming output. Up-to-date packages are skipped, never reinstalled.
   Each builder uses its own `/etc/portage` as configured in the nspawn.
3. Only `max_concurrent_builds` (default **1**) run globally; the rest queue
   FIFO. A system never runs two builds at once.

### Builder discovery

Systems are discovered, not configured: the server runs
`machinectl list-images` and turns every `gentoo-builder-*` image into a
system named by the suffix (`gentoo-builder-laptop` → `laptop`), with files at
`/var/lib/machines/<image>` and the default emerge log/binary. The local
host (named by hostname) is registered as a non-nspawn system too.
Discovery re-runs on every lookup, so a VM added later works without a
restart. `[[systems]]` entries only add to (or override by name) the
discovered set — tune or disable discovery under `[discovery]` if ever
needed. Dry-run skips discovery and uses explicit systems only.

### No agent inside the containers

Control is plain shell-out — no helper app to install in the nspawns:

- containers: root `nsenter` into the machine leader's namespaces
  (`nsenter --target <leader> -m -u -i -n -p --wdns=/ -- env -i …
  /usr/bin/emerge …`), which streams output over plain pipes, propagates
  the exit code, and runs in its own process group so cancel kills the
  whole build tree. Machines are started with `machinectl start <name>`
  if they are not running.
- host (`buildhost`): local `emerge …` directly.

`nsenter` — not `systemd-run --machine=` or `machinectl shell` — because
the builder guests run OpenRC, and both of those require guest systemd.

Estimates read `emerge.log` straight from the host
(`/var/lib/machines/<name>/var/log/emerge.log`), so no `emlop`/`genlop` call
inside the container is needed either. The estimator is our own genlop/qlop:
`start → completed` spans keyed by `category/packagename`, median per package.

### Estimate fallback

Estimates prefer the target system's own logs. If a package was never built
there, the server uses the other system with the most samples for it and marks
the estimate `"fallback": true` with `"source_system"` set.

### Automatic conflict fixing

When `emerge -p` reports blocking packages, the server unmerges them with
`emerge --unmerge` and re-resolves, up to 3 pretend/unmerge cycles. Everything
unmerged is recorded on the job (`unmerged_blockers`) and shown in the log
stream. If blocks remain afterwards, the build fails listing them. Send
`"auto_fix_conflicts": false` (CLI: `build --no-auto-fix`) to fail on
conflicts instead of fixing them.

## Build & run

```sh
cargo build --release
cp config.example.toml config.toml
# edit config.toml (bind, systems)
./target/release/gb-server --config config.toml
```

The server must run as root: it enters container namespaces with
`nsenter`, starts machines with `machinectl start`, and reads the
machines' logs.

Dry-run mode (fake builds, no emerge/machines — for development):

```sh
SYSTEM_BUILDER_DRY_RUN=1 cargo run -p gb-server -- --config config.example.toml
```

## Config files

Each app keeps its own file under `$HOME/.config` (or `$XDG_CONFIG_HOME`),
so the two can never be confused on a host that runs both:

| App | File |
| --- | ---- |
| `gb-server` | `$HOME/.config/gb-server/config.toml` |
| `gb-client` | `$HOME/.config/gb-client/config.toml` |

On first run, when no config file exists, the app creates its file with every
option present and all defaults commented out — just uncomment and edit. The
server then exits so you can review the file before (re)starting (with
discovery, the default, it works unedited wherever builder images exist);
the client proceeds with built-in defaults. Existing files are
never overwritten. Unknown keys are rejected, so typos fail loudly.

Server lookup without `--config`: `./config.toml`, then the per-user file,
then `/etc/system-builder/config.toml` (first existing wins). The server
config needs no `[[systems]]`: builders are discovered (see above), and a
freshly scaffolded file works unedited on a machine with builder images.

Client values resolve as: flag > environment (`GB_SERVER`, `GB_TIMEOUT`,
`GB_SYSTEM`) > config file (`server`, `timeout`, `system`) > built-in
default — except the server, which is required and has no default (a
missing server fails fast instead of guessing localhost).
`--config PATH` overrides the file location for one run.

## Install as a systemd service

On the builder host, as root:

```sh
./install.sh server          # builds release gb-server, installs unit, enables+starts it
# or: make install-server    # same via the Makefile (run as your normal user)
```

This installs `/usr/local/bin/gb-server`, seeds `/etc/system-builder/config.toml`
from `config.example.toml` (never overwrites an existing one), installs the
`system-builder.service` unit, and runs `systemctl enable --now
system-builder`. Edit the config first if you want different systems:
`./install.sh server --no-enable`, edit, then enable.

Useful variants:

```sh
./install.sh client --destdir /tmp/pkg   # build gb-client, stage files under /tmp/pkg
./install.sh server --prefix /opt/sb     # non-default binary location
./uninstall.sh all --purge               # remove service, binaries, config, state
```

The service runs as root: it must enter the builders' namespaces with
`nsenter`, start them with `machinectl start`, and read their logs. Running `gb-server` by hand as a normal user
fails at the first privileged call — `machinectl start` asks for a password,
and under the server (which has no tty) that surfaces as an error telling
you to run as root. The server never invokes `sudo`, never prompts, and
never uses pty sessions (`machinectl shell` is never called; control goes
through `nsenter` over plain pipes, and every child gets stdin detached). Before each build step the target machine is checked with
`machinectl show` and started on demand, so stopped builders "just work".
On SIGTERM/SIGINT the server cancels running builds and waits (up to 10s)
for their emerge trees to die before exiting, so killing the server never
leaves orphaned builds behind. (Jobs live in memory, so anything running
is lost on restart either way.)

## Install from mw-overlay

On Gentoo, the easiest path is the [`mw` overlay](https://github.com/MarkWalters-dev/mw-overlay),
which carries `app-portage/system-builder` as a 9999 live ebuild tracking
git — no versions to follow, just re-emerge to update
(`emerge --oneshot app-portage/system-builder`):

```sh
# one-time overlay setup (as root)
eselect repository add mw git https://github.com/MarkWalters-dev/mw-overlay.git
emaint sync -r mw

# builder host: server + client, then enable the service
emerge app-portage/system-builder
systemctl enable --now system-builder

# client machines: gb-client only
echo "app-portage/system-builder -server" >>/etc/portage/package.use/system-builder
emerge app-portage/system-builder
```

The ebuild installs both binaries to `/usr/bin`, the
`system-builder.service` unit, and seeds `/etc/system-builder/config.toml`
(CONFIG_PROTECTed, so updates never clobber your edits). `USE=-server`
skips building the server and its dependencies entirely. See the
[overlay README](https://github.com/MarkWalters-dev/mw-overlay) for
setup alternatives (SSH, manual `repos.conf`).

## Client CLI

`gb-client` runs on the client machines. It submits a remote build, streams it,
and when the remote build succeeds it runs a local `emerge`:

```sh
export GB_SERVER=http://builder-host:8080

# Remote @system build (system comes from the config file), watch it,
# then `emerge --update --newuse --deep --verbose @system` locally.
gb-client build @system

# Same targets remotely, but update @world locally with sudo afterwards
# (the local defaults already include --update --newuse --deep).
gb-client build @system --local-target @world --sudo

# Submit without watching (prints the build id), or watch without local emerge.
gb-client build @system --no-watch
gb-client build @system --no-local

# Fail on blockers instead of auto-unmerging them.
gb-client build @system --no-auto-fix

# Any --emerge-arg / --local-arg replaces that side's defaults entirely,
# so re-add --update --newuse --deep yourself when customizing either.

# Targets are positional; --target is still accepted and merged with them.
# --system overrides the config file's system for one invocation.
gb-client build --system laptop @system --target sys-apps/foo

# Other commands: watch, status, list, queue, active, systems,
# system-active, cancel, log, estimate, history (all take --json too).
gb-client queue
gb-client watch <id>
gb-client estimate --system laptop --package sys-apps/iproute2
```

`watch`, `status`, and `log` default to the latest build when the id is
omitted, and all commands print full build ids (short prefixes are
rejected with a hint instead of failing server-side). Bare `cancel`
targets your last submitted build (tracked in
`~/.local/state/gb-client/last_build_id`) while it is still queued or
running, else the most recently submitted queued build, else the most
recently submitted running build. If the event stream breaks mid-watch,
the client keeps going by polling status, so a network blip doesn't lose
a long build's watch or its follow-up local emerge.

`build` pings ntfy when the local emerge finishes (success or failure)
and when the remote build fails or is cancelled — a remote success
stays silent since the local-done ping that follows implies it.
Configure it in `~/.config/gb-client/config.toml`:

```toml
notify_topic = "my-topic"
# notify_server = "https://ntfy.sh"  # only for self-hosted ntfy
```

Unset (the default) disables notifications. Failures get high priority;
a failed ping only warns on stderr and never changes the exit code.

Exit codes: `0` ok · `1` API/usage error · `2` remote build failed or
cancelled · `3` local emerge failed. Install it on clients with
`./install.sh client` (as root), `make install-client`,
`USE=-server emerge app-portage/system-builder` from
[mw-overlay](https://github.com/MarkWalters-dev/mw-overlay), or copy the
`gb-client` binary over.

## API

| Method | Path | Description |
| ------ | ---- | ----------- |
| GET | `/health` | liveness |
| POST | `/v1/builds` | enqueue: `{system, targets[], emerge_args[]?, auto_fix_conflicts?}` → `201` job |
| GET | `/v1/builds?state=&system=` | list builds (newest first) |
| GET | `/v1/builds/active` | currently running builds |
| GET | `/v1/builds/:id` | job detail incl. progress (`current_package`, `current_index`/`total_packages`, ETA) |
| DELETE | `/v1/builds/:id` | cancel queued / kill running |
| GET | `/v1/builds/:id/log?tail=N` | buffered emerge output |
| GET | `/v1/builds/:id/stream` | SSE: `status`/`log`/`progress`/`done` events |
| GET | `/v1/queue` | `{max_concurrent_builds, running, queued[], active[]}` |
| GET | `/v1/systems` | systems with `idle`/`building` status |
| GET | `/v1/systems/:system/active` | active (+queued) build for one system, with x-of-y progress |
| POST | `/v1/estimates` | `{system, packages[]?, targets[]?}` → per-package + total estimates |
| GET | `/v1/history?system=&package=` | samples/avg/median/min/max with fallback source |

See [examples/client.sh](examples/client.sh) for a full curl walkthrough.

## Tests

```sh
cargo test --workspace   # unit + manager + HTTP API tests (all dry-run, no root needed)
```
