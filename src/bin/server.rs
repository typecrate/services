//! `typecrate-services-server` - the runnable native HTTP executable (R1).
//!
//! This is the thinnest possible process wrapper around the library: it turns
//! CLI/env configuration into an `AppState` and serves
//! `typecrate_services::server::app_for(port, state)` on one tokio listener per port.
//! The route table is NOT assembled here - `server::app_for` pulls in the build-time
//! generated registration (see `src/lib.rs`), so a test that serves `app(state)` and
//! this process' default listener serve literally the same router.
//!
//! HOW MANY LISTENERS: one for the default address (`--addr`/`--port`/env, built-in
//! `0.0.0.0:8000`), plus one per port a route file declared with the trailing `port:`
//! slot of `crate::service_routes!`. That set is DERIVED from the compiled-in route
//! table (`server::declared_ports()`), never configured, so a declared port cannot end
//! up without a listener and a listener cannot exist for a port nothing declares. Each
//! one reuses the resolved host and overrides only the port, and each serves only the
//! groups that declared it (`server::app_for`), which is what lets the same API path be
//! published on two ports without either declaration overriding the other. A build
//! whose route files declare no port binds exactly one listener, prints exactly the
//! banner it printed before route ports existed, and serves exactly what `app(state)`
//! is.
//!
//! The one policy that lives here rather than in the library is *where the process
//! listens*: the built-in bind is `0.0.0.0:8000` (every interface, LAN-reachable, the
//! same surface the TS gateway publishes), with `--addr 127.0.0.1` as the opt-in
//! loopback start. The banner therefore never prints the bind answer (`0.0.0.0`) as a
//! URL - it prints loopback URLs for this machine plus a detected LAN URL, and says
//! plainly that binding is only half of cross-device reachability (see `banner_lines`).

// ---------------------------------------------------------------------------
// Native runtime.
// ---------------------------------------------------------------------------
mod native {
    use std::ffi::OsString;
    use std::io::ErrorKind;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
    use std::path::{Path, PathBuf};

    use tokio::net::TcpListener;
    use tokio::sync::watch;
    use tokio::task::JoinSet;
    use typecrate_services::server::{
        AppState, MAX_REQUEST_BODY_BYTES, app_for, declared_ports, default_database_root,
        normalize_lexically,
    };

    /// Exit code for a clean Ctrl+C/SIGTERM shutdown.
    const EXIT_OK: i32 = 0;
    /// Exit code for anything the process could not do (bind refused, runtime down).
    const EXIT_RUNTIME: i32 = 1;
    /// Exit code for bad usage - kept distinct from a server failure because a
    /// supervisor should restart the second and fix the first.
    const EXIT_USAGE: i32 = 2;

    /// Bind address override.
    const ADDR_ENV: &str = "TYPECRATE_SERVICES_ADDR";
    /// Database root override (see `server::default_database_root`).
    const DATABASE_ROOT_ENV: &str = "TYPECRATE_SERVICES_DATABASE_ROOT";

    /// Default bind: every interface on the documented port, i.e. the same public
    /// surface the TS gateway (`0.0.0.0`) has, so an unconfigured native start is
    /// reachable from other devices on the LAN. 8000 is the port this package
    /// documents; if something already holds it the process says so and exits - it
    /// does not kill anybody. Loopback stays available as an explicit opt-in
    /// (`--addr 127.0.0.1`), which is the right choice on a public or untrusted
    /// network because this server has no auth and no CORS gate yet.
    ///
    /// This is the DEFAULT listener only. A route file may declare a port of its own,
    /// and each distinct declaration is bound as a separate listener on the same host
    /// (`serve`); the set is derived from the route table, so it is not something this
    /// constant or a flag can list. A declaration that names THIS port is a startup
    /// error - see `check_declared_port_collisions`.
    const DEFAULT_ADDR: &str = "0.0.0.0:8000";

    /// Framework builtins served by `server::app` (`src/server/mod.rs`): the
    /// interactive playground at the root, and the JSON listing of the compiled-in
    /// endpoints. Declared here so `--help` and the banner describe exactly the paths
    /// the router reserves; an endpoint file must not register any of the four. The
    /// playground's two embedded assets (`/playground.css`, `/playground.js`, named by
    /// `server::PLAYGROUND_STYLES_PATH` / `server::PLAYGROUND_SCRIPT_PATH`) are reserved
    /// the same way - spelled out in `HELP` below, but not needed by the banner, which
    /// only prints navigable pages.
    ///
    /// They are mounted on the DEFAULT listener only (`app_for(None, ..)`), so the two
    /// URLs below exist on the default port alone - an extra listener a route file
    /// declared serves just the API paths that asked for it, and 404s these four.
    const PLAYGROUND_PATH: &str = "/";
    const ROUTE_LISTING_PATH: &str = "/v1/services/routes";

    const HELP: &str = "\
typecrate-services-server - native HTTP runtime for @typecrate/services

USAGE:
    cargo run -p typecrate-services --bin typecrate-services-server -- [OPTIONS]
    typecrate-services-server [OPTIONS]

OPTIONS:
    -a, --addr <ip[:port]>   Bind address, numeric IPv4 or IPv6; an IPv6 address is
                             written in brackets ([::1] or [::1]:8000 - the port may
                             be left off, see below).
                             Default 0.0.0.0:8000 - every interface, so other
                             devices on the LAN can reach it. Pass
                             --addr 127.0.0.1 for a loopback-only start. A bare ip
                             keeps the currently selected port.
        --port <port>        Port, applied to whichever ip --addr/env selected.
        --database-root <dir>
                             Writable data root that endpoints place their folders
                             under. Default <repo-parent>/temporary/dump, the same
                             dump boundary the TS service uses. It is NOT the
                             endpoint scanner root, and no filesystem path is ever
                             returned over HTTP.
    -h, --help               Print this help and exit.

ENVIRONMENT:
    TYPECRATE_SERVICES_ADDR            same meaning as --addr
    TYPECRATE_SERVICES_DATABASE_ROOT   same meaning as --database-root

Precedence per setting: CLI flag, then environment variable, then built-in default.
Exit codes: 0 clean shutdown, 1 could not serve, 2 bad usage.

WHERE IT ANSWERS (framework builtins of the DEFAULT listener, not endpoint routes):
    GET /                     interactive playground, served from the same origin
                              with no external assets - open this on the machine
                              running the server.
    GET /playground.css       the playground stylesheet (embedded in this executable).
    GET /playground.js        the playground script (embedded in this executable).
    GET /v1/services/routes   JSON listing of the compiled-in endpoints
                              (method, path, summary), the same set the playground
                              offers.
    These four paths are reserved: an endpoint file must not register any of them, and
    one that does makes the router panic at startup instead of shadowing a builtin. A
    group that declares a port of its own is not checked against them, because those
    four are not mounted on a declared listener at all.
    On this machine use http://127.0.0.1:<port>/ ; 0.0.0.0 is a bind answer, not a
    URL. From another device use http://<this machine's LAN ip>:<port>/ - the startup
    banner prints the address it detected. Binding is only half of reachability:
    Windows Firewall, the network profile and the routing have to allow inbound
    connections on that port too, so the first start may need a permission prompt
    accepted (nothing here changes the firewall or any global network setting).

    PORTS A ROUTE FILE DECLARES:
    A route file may name a listener of its own with the trailing `port:` slot of
    `crate::service_routes!`. Every DISTINCT declared port gets its OWN listener at
    startup, bound on the ip --addr/env resolved with only the port replaced - so
    --addr 127.0.0.1 isolates the declared ports exactly like it isolates the default
    one. There is no flag for them: the set is read from the compiled-in route table,
    and each listener serves only the paths that declared it. The same API path may be
    declared on two ports, and it is then served on both, independently - neither
    declaration overrides the other. The playground, its two assets and the route
    listing belong to the DEFAULT listener and answer 404 on a declared one, and a path
    that did not declare a port answers 404 on the declared listener. A route file that
    declares the port this process binds stops the start with exit 1 and names the port
    to move, because two listeners cannot share one socket. The startup banner lists
    every listener this process bound.

    PARALLEL INSTANCES:
    A start fails with exit 1 only when THAT address and port are already bound, and
    nothing is ever killed. On Windows a bind to a specific address can still succeed
    beside a listener already holding the wildcard on the same port (e.g. this process
    on 127.0.0.1:8000 while another owns 0.0.0.0:8000), and the more specific socket
    then wins the connections for that interface, so the older server keeps the LAN and
    this one serves loopback. Run parallel instances on distinct --port values unless
    that shadowing is what you want.

SECURITY:
    Requests are unauthenticated and there is no Origin/CORS gate yet, so publish
    this server on a trusted LAN only - never straight onto the public internet.
    The endpoint set today is this package's own samples (health, echo); the
    TypeScript service endpoints are not ported yet.

Routes are generated at build time from the discovered endpoint files. The process
stops on Ctrl+C (and SIGTERM on unix) after draining in-flight requests.";

    /// Parsed command line, before the environment and defaults are applied. Every
    /// field is an `Option` so precedence stays one obvious chain per setting
    /// instead of interleaved overwrites.
    #[derive(Debug, Default)]
    struct Flags {
        addr: Option<String>,
        port: Option<u16>,
        /// Held as a `PathBuf` (lossless from `OsString`) so the resolver can hand a
        /// `&Path` to the same normaliser the library default goes through.
        database_root: Option<PathBuf>,
        help: bool,
    }

    /// The resolved configuration the process actually runs on.
    #[derive(Debug)]
    struct Config {
        addr: SocketAddr,
        database_root: PathBuf,
        help: bool,
    }

    /// Entry point. The runtime is built by hand rather than with `#[tokio::main]`
    /// so the exit code is explicit: `profile.release` sets `panic = "abort"`
    /// (Cargo.toml), where a `main -> Result` would end in an abort status instead
    /// of the promised 1 (cannot serve) / 2 (bad usage) / 0 (clean shutdown).
    pub fn run() -> i32 {
        let args: Vec<OsString> = std::env::args_os().skip(1).collect();
        let config = match resolve(&args) {
            Ok(config) => config,
            Err(message) => {
                eprintln!("typecrate-services-server: {message}\n\n{HELP}");
                return EXIT_USAGE;
            }
        };
        if config.help {
            println!("{HELP}");
            return EXIT_OK;
        }

        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(err) => {
                eprintln!("typecrate-services-server: could not start the tokio runtime: {err}");
                return EXIT_RUNTIME;
            }
        };
        runtime.block_on(serve(config))
    }

    /// Environment + CLI in one place; the two sources are then handed to the pure
    /// resolver below.
    fn resolve(args: &[OsString]) -> Result<Config, String> {
        let flags = parse_flags(args)?;
        // `var`/`var_os` give `Option` (unset), and `var` also yields `None` for
        // non-UTF-8 - which is exactly right here, because a non-UTF-8 address is
        // unusable anyway while a non-UTF-8 *path* still reaches us intact via
        // `var_os`.
        let env_addr = std::env::var(ADDR_ENV).ok();
        let env_root = std::env::var_os(DATABASE_ROOT_ENV);
        from_parts(
            &flags,
            env_addr.as_deref(),
            env_root.as_deref().map(Path::new),
        )
    }

    /// Pure precedence resolution: `env_addr`/`env_database_root` arrive as
    /// parameters instead of being read here, so the rule is unit-testable without
    /// mutating the process environment (which would race other tests in the same
    /// binary - the reason `nexus-server/src/main.rs` snapshots env the same way).
    fn from_parts(
        flags: &Flags,
        env_addr: Option<&str>,
        env_database_root: Option<&Path>,
    ) -> Result<Config, String> {
        let default_addr: SocketAddr = DEFAULT_ADDR
            .parse()
            .expect("the built-in default address parses");

        if flags.help {
            // `--help` short-circuits *resolution*: the address/root values are not
            // validated, so `--help --addr garbage` still prints help. An unknown
            // FLAG is still a usage error, because it is rejected while parsing.
            return Ok(Config {
                addr: default_addr,
                database_root: default_database_root(),
                help: true,
            });
        }

        // Built-in default, then environment, then flag - the documented order.
        let mut addr = match env_addr {
            Some(value) => parse_addr(value, default_addr.port(), ADDR_ENV)?,
            None => default_addr,
        };
        if let Some(value) = flags.addr.as_deref() {
            addr = parse_addr(value, addr.port(), "--addr")?;
        }
        if let Some(port) = flags.port {
            // Applied to whichever ip the previous two steps produced, so
            // `--addr 127.0.0.1 --port 8080` (or an env ip plus a CLI port) is not
            // silently re-pinned to the built-in default host.
            addr.set_port(port);
        }

        // A user-supplied root is normalised lexically (never canonicalised: the
        // directory may not exist yet, and creating it is not this process's job).
        let database_root = match flags.database_root.as_deref() {
            Some(value) => normalize_lexically(value),
            None => match env_database_root {
                Some(value) => normalize_lexically(value),
                None => default_database_root(),
            },
        };

        Ok(Config {
            addr,
            database_root,
            help: false,
        })
    }

    /// Flag parser. Values come from the same slice, so a missing value is a usage
    /// error rather than an index panic.
    fn parse_flags(args: &[OsString]) -> Result<Flags, String> {
        let mut addr: Option<String> = None;
        let mut port: Option<u16> = None;
        let mut database_root: Option<PathBuf> = None;
        let mut help = false;

        let mut index = 0;
        while index < args.len() {
            let flag = args[index]
                .to_str()
                .ok_or_else(|| format!("argument {} is not valid text", index + 1))?;
            index += 1;
            match flag {
                "-h" | "--help" => help = true,
                "-a" | "--addr" => addr = Some(take_text(args, &mut index, flag)?),
                "--port" => {
                    let raw = take_text(args, &mut index, flag)?;
                    port = Some(raw.parse().map_err(|_| {
                        format!("--port expects a number in 0..=65535, got `{raw}`")
                    })?);
                }
                "--database-root" => {
                    database_root = Some(take_path(args, &mut index, flag)?);
                }
                other => return Err(format!("unknown option `{other}`")),
            }
        }

        Ok(Flags {
            addr,
            port,
            database_root,
            help,
        })
    }

    /// Reads one text value out of `args`, advancing `index`; the name is only used
    /// to point the error at the right flag.
    fn take_text(args: &[OsString], index: &mut usize, name: &str) -> Result<String, String> {
        let raw = args
            .get(*index)
            .ok_or_else(|| format!("{name} needs a value"))?;
        *index += 1;
        raw.to_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("{name} value is not valid text"))
    }

    /// Reads one path value out of `args`, losslessly: a Windows path can hold bytes
    /// that are not valid UTF-8, and the data root has to survive them intact (that
    /// is why this flag never goes through `to_str`).
    fn take_path(args: &[OsString], index: &mut usize, name: &str) -> Result<PathBuf, String> {
        let raw = args
            .get(*index)
            .ok_or_else(|| format!("{name} needs a value"))?;
        *index += 1;
        Ok(PathBuf::from(raw.as_os_str()))
    }

    /// Accepts `ip:port` (also `[::1]:8000`) and a bare host - `127.0.0.1`, `0.0.0.0`
    /// or the bracketed IPv6 form `[::1]` - by re-attaching the port already selected.
    /// The bracketed form is the CLI/URL spelling of an IPv6 host, so `[::1]` is a host
    /// and not a malformed `host:port`; stripping the brackets is what lets the same
    /// "bare ip keeps the port" rule cover IPv6. Numeric only on purpose:
    /// `TcpListener::bind` takes a resolved `SocketAddr`, and inventing a DNS step here
    /// would make the address depend on a nameserver rather than on what the operator
    /// typed.
    fn parse_addr(raw: &str, port: u16, source: &str) -> Result<SocketAddr, String> {
        if let Ok(addr) = raw.parse::<SocketAddr>() {
            return Ok(addr);
        }
        // A bracketed host with no port: the brackets are the only reason the value has
        // colons at all, so they are stripped and the address is parsed on its own. An
        // unbalanced value (`[::1`) still falls through to the usage error below rather
        // than being guessed at.
        if let Some(host) = raw
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
            && let Ok(ip) = host.parse::<IpAddr>()
        {
            return Ok(SocketAddr::new(ip, port));
        }
        if raw.contains(':') {
            return Err(format!(
                "{source} `{raw}` is not a valid address (expected <ip>[:port] or [<ipv6>][:port])"
            ));
        }
        format!("{raw}:{port}").parse().map_err(|_| {
            format!(
                "{source} `{raw}` is not a valid address (expected <ip>[:port] or [<ipv6>][:port])"
            )
        })
    }

    // -----------------------------------------------------------------------
    // Address reporting: turning a *bind answer* into URLs a client can type.
    //
    // `0.0.0.0` / `::` is the set of interfaces the listener accepted on, and a
    // browser cannot navigate to it, so help and the banner translate the bind into
    // (a) a loopback URL for this machine and (b) a LAN URL for other devices, and
    // state what else has to be true for (b). Everything below is pure except the OS
    // probe, whose result is passed into `banner_lines` as a parameter.
    // -----------------------------------------------------------------------

    /// `true` for the wildcard addresses (`0.0.0.0`, `::`) - bind answers only.
    fn is_wildcard(ip: IpAddr) -> bool {
        match ip {
            IpAddr::V4(v4) => v4.is_unspecified(),
            IpAddr::V6(v6) => v6.is_unspecified(),
        }
    }

    /// `true` for the loopback ranges (`127.0.0.0/8`, `::1`).
    fn is_loopback(ip: IpAddr) -> bool {
        match ip {
            IpAddr::V4(v4) => v4.is_loopback(),
            IpAddr::V6(v6) => v6.is_loopback(),
        }
    }

    /// Whether an address is worth publishing as "use this from another device".
    /// A probe answer only helps if it is a routable interface address: wildcard,
    /// loopback, and the fallback ranges (`169.254/16`, `fe80::/10`, what a host
    /// takes when DHCP has no lease) are host- or link-scoped, so printing one would
    /// promise a URL that no other device can open.
    fn is_shareable_address(ip: IpAddr) -> bool {
        if is_wildcard(ip) || is_loopback(ip) {
            return false;
        }
        match ip {
            IpAddr::V4(v4) => !v4.is_link_local(),
            IpAddr::V6(v6) => !v6.is_unicast_link_local(),
        }
    }

    /// Which ip a client on *this* machine should use for `local`. A wildcard bind
    /// also answers on loopback, so it is reported as loopback (v4, because that is
    /// what the README shows and what browsers resolve first); an explicit bind is
    /// reported as itself, because `http://127.0.0.1/` is a refused connection when
    /// the process bound `--addr 192.168.1.20`.
    fn local_report_ip(local: SocketAddr) -> IpAddr {
        if is_wildcard(local.ip()) {
            return match local.ip() {
                IpAddr::V4(_) => Ipv4Addr::LOCALHOST.into(),
                IpAddr::V6(_) => Ipv6Addr::LOCALHOST.into(),
            };
        }
        local.ip()
    }

    /// `http://<ip>:<port><path>`. Rebuilding a `SocketAddr` rather than pasting the
    /// ip is what makes an IPv6 URL correct: `SocketAddr`'s `Display` brackets it,
    /// which `http://::1:8000/` would not be.
    fn url_for(ip: IpAddr, port: u16, path: &str) -> String {
        let address = SocketAddr::new(ip, port);
        format!("http://{address}{path}")
    }

    /// The source address this host would use to reach `peer`. UDP `connect` is a
    /// purely local call: it pins the route/interface the OS *would* pick and sends
    /// no packet, so the peer never has to answer. `None` when no route exists at all
    /// (offline host, or a stack that refuses the probe).
    fn route_source_address(peer: SocketAddr) -> Option<IpAddr> {
        // Bound on the peer's family, otherwise an IPv4 peer cannot be reached from
        // an IPv6 socket and the probe would report "no route" on a working host.
        let wildcard = SocketAddr::new(
            if peer.is_ipv4() {
                Ipv4Addr::UNSPECIFIED.into()
            } else {
                Ipv6Addr::UNSPECIFIED.into()
            },
            0,
        );
        let socket = UdpSocket::bind(wildcard).ok()?;
        socket.connect(peer).ok()?;
        socket.local_addr().map(|source| source.ip()).ok()
    }

    /// Best-effort LAN address for the banner: IPv4 first, because the LANs this
    /// project runs on are IPv4 and `--addr 0.0.0.0` is an IPv4 bind. A machine with
    /// several interfaces (VPN, dock, second NIC) gets whatever the default route
    /// selects - hence help's "the address it detected" wording, and `--addr <ip>`
    /// remaining the explicit escape hatch. Std only: no dependency, no interface
    /// enumeration, and nothing is ever sent on the wire.
    fn detected_lan_address() -> Option<IpAddr> {
        let probes = [
            SocketAddr::new(Ipv4Addr::new(8, 8, 8, 8).into(), 53),
            SocketAddr::new(
                Ipv6Addr::new(0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888).into(),
                53,
            ),
        ];
        // First probe whose answer is *routable* wins; a probe that answers with a
        // link-scoped fallback address is treated as "no answer" and falls through.
        probes
            .iter()
            .filter_map(|peer| route_source_address(*peer))
            .find(|ip| is_shareable_address(*ip))
    }

    /// One banner line: `  <label padded> <value>`, so values line up on the same
    /// column the pre-existing `  database root:    ...` lines use (index 20).
    fn banner_field(label: &str, value: impl Into<String>) -> String {
        format!("  {label:<17} {}", value.into())
    }

    /// The address-facing banner lines. Pure so the exact wording is unit-testable -
    /// `lan_ip` is the caller's probe result, never read here, and the remaining
    /// facts (`database root`, body cap) are printed by `serve`, which is where
    /// `AppState` is in scope.
    fn banner_lines(local: SocketAddr, lan_ip: Option<IpAddr>) -> Vec<String> {
        let port = local.port();
        let here = local_report_ip(local);
        let mut lines = vec![
            banner_field("playground:", url_for(here, port, PLAYGROUND_PATH)),
            banner_field("endpoint list:", url_for(here, port, ROUTE_LISTING_PATH)),
        ];

        if is_loopback(local.ip()) {
            // The opt-in loopback start: name what is *not* reachable and how to
            // change it, so nobody debugs a "server is up but the phone cannot see
            // it" problem that is really a bind choice.
            lines.push(banner_field(
                "other devices:",
                format!("none - bound to {} (loopback only)", local.ip()),
            ));
            lines.push(banner_field(
                "next:",
                format!("--addr 0.0.0.0 (the default) publishes :{port} on every interface"),
            ));
            return lines;
        }

        // Wildcard bind -> the LAN ip has to be probed (0.0.0.0 is not a URL);
        // explicit non-loopback bind -> the operator already named the interface.
        let reachable = if is_wildcard(local.ip()) {
            match lan_ip {
                Some(ip) => url_for(ip, port, PLAYGROUND_PATH),
                // Probe found nothing routable (offline, or only link-local
                // candidates): keep the placeholder instead of inventing an address.
                None => format!("http://<this machine's LAN ip>:{port}/"),
            }
        } else {
            url_for(local.ip(), port, PLAYGROUND_PATH)
        };
        lines.push(banner_field("other devices:", reachable));
        lines.push(banner_field(
            "note:",
            "unauthenticated - trusted LAN only, never the public internet; \
             Windows Firewall may prompt on first start",
        ));
        lines
    }

    /// Startup check: does a route file declare the very port this process binds?
    ///
    /// A route port and the default port are two SEPARATE listeners on two sockets, so a
    /// clash is not something the process can negotiate - the second bind would refuse and
    /// the operator would read "address already in use" as if some other program owned the
    /// port. Naming the declaration is what makes the failure fixable (move the default
    /// with `--port`, or move the route), and running this BEFORE the first bind is what
    /// keeps the process from leaving one listener up while the other one failed.
    ///
    /// Pure and it returns a `String` rather than panicking: `profile.release` sets
    /// `panic = "abort"`, so a panic here would be an abort status instead of the promised
    /// exit 1, and the caller turns the message into stderr with no backtrace.
    fn check_declared_port_collisions(default: SocketAddr, declared: &[u16]) -> Result<(), String> {
        let port = default.port();
        if declared.contains(&port) {
            return Err(format!(
                "a route file declares port {port} and this process binds {default}, so both \
                 listeners want the same socket. They are separate listeners and neither \
                 shares it: move the default with --port <other> (or change the declaration). \
                 Nothing was bound."
            ));
        }
        Ok(())
    }

    /// Binds the listener for ONE declared port: the host this process resolved is reused
    /// verbatim and only the port is overridden, so `--addr 127.0.0.1` isolates the
    /// declared ports exactly like it isolates the default one, and a LAN bind publishes
    /// them on the LAN too.
    async fn bind_declared_listener(default: SocketAddr, port: u16) -> Result<TcpListener, String> {
        let addr = SocketAddr::new(default.ip(), port);
        match TcpListener::bind(addr).await {
            Ok(listener) => Ok(listener),
            Err(err) if err.kind() == ErrorKind::AddrInUse => Err(format!(
                "cannot bind {addr} - the listener a route file declared for port {port}, and \
                 the address is already in use. Nothing was killed; free the port or change \
                 the declaration."
            )),
            Err(err) => Err(format!(
                "cannot bind {addr} - the listener a route file declared for port {port}: {err}"
            )),
        }
    }

    /// The `METHOD /path` rows the route files declared for `port`, for the banner line.
    ///
    /// Same generated table `server::declared_ports()` derives the listener set from, so
    /// the banner cannot advertise a path the listener does not serve.
    fn declared_routes_for_port(port: u16) -> Vec<String> {
        typecrate_services::route_metadata()
            .into_iter()
            .filter(|route| route.port == Some(port))
            .map(|route| format!("{} {}", route.method, route.path))
            .collect()
    }

    /// One banner line per extra listener: the URL a client can dial on it plus the API
    /// paths mounted there, in the same `  <label padded> <value>` column as the rest of
    /// the banner (see `banner_field`).
    ///
    /// It answers `[]` when there is no extra listener, and that is the mechanism keeping
    /// the zero-declared-ports banner byte-identical to the single-listener one: the
    /// caller prints what it is handed, and it is handed nothing.
    ///
    /// The label reports the port the SOCKET bound (what a client dials), the path list
    /// the port the group DECLARED; those are the same number except for a literal
    /// `port: 0` declaration, which asks the OS for a free port.
    fn extra_listener_lines(extra: &[(SocketAddr, Vec<String>)]) -> Vec<String> {
        extra
            .iter()
            .map(|(addr, routes)| {
                // Same rule as the default listener's lines: a wildcard bind is reported
                // as loopback, because `http://0.0.0.0:8010` is not something to open.
                let base = url_for(local_report_ip(*addr), addr.port(), "");
                let served = if routes.is_empty() {
                    // Unreachable today (every derived port comes from a row that names
                    // itself), but an honest line beats a dangling ` - `.
                    "no declared route".to_owned()
                } else {
                    routes.join(", ")
                };
                banner_field(
                    &format!("listener {}:", addr.port()),
                    format!("{base} - {served}"),
                )
            })
            .collect()
    }

    /// The shutdown future of EVERY listener: the shared flag, never the signal itself.
    ///
    /// `borrow_and_update` marks the current value as seen, so the leading check covers a
    /// signal that fired before this listener was spawned, and `changed()` only errors once
    /// every sender is gone (the exit path, not a request path). A `watch` rather than one
    /// shared future because each listener polls its own copy: a single future can only be
    /// awaited in one place, and `Notify::notify_waiters` wakes only the receivers that
    /// already registered.
    async fn wait_for_shutdown(mut seen: watch::Receiver<bool>) {
        while !*seen.borrow_and_update() {
            if seen.changed().await.is_err() {
                return;
            }
        }
    }

    /// Binds, serves, and stops accepting on the first shutdown signal.
    ///
    /// The listener is bound by hand so a refused bind becomes this server's own
    /// message. `local_addr` is logged rather than assumed, because `--port 0` is a
    /// valid way to ask for any free port.
    ///
    /// ZERO declared ports is exactly one bind, one `axum::serve`, one banner: the extra
    /// code below is driven by `declared_ports()`, and every loop over it runs zero times
    /// while the route files declare no port (`extra_listener_lines` answers `[]`). With
    /// declared ports the process binds one listener per port on the SAME host and serves
    /// `app_for(Some(port), state)` beside the default `app_for(None, state)`.
    async fn serve(config: Config) -> i32 {
        let state = AppState::new(config.database_root);

        // The extra listeners are DERIVED from the compiled-in route table, never
        // configured: `server::declared_ports()` reads the same table the routers are built
        // from, so a declared port cannot end up without a listener and a listener cannot
        // exist for a port nothing declares. The default listener is not part of that set -
        // its port is the operator's business, resolved just above by `from_parts`.
        let declared = declared_ports();

        // A route port that names the default port fails the start, before anything binds.
        if let Err(message) = check_declared_port_collisions(config.addr, &declared) {
            eprintln!("typecrate-services-server: {message}");
            return EXIT_RUNTIME;
        }

        let listener = match TcpListener::bind(config.addr).await {
            Ok(listener) => listener,
            Err(err) if err.kind() == ErrorKind::AddrInUse => {
                eprintln!(
                    "typecrate-services-server: cannot bind {} - address already in use. \
                     Nothing was killed; pass --addr/--port or free the port first.",
                    config.addr
                );
                return EXIT_RUNTIME;
            }
            Err(err) => {
                eprintln!(
                    "typecrate-services-server: cannot bind {}: {err}",
                    config.addr
                );
                return EXIT_RUNTIME;
            }
        };
        let local = match listener.local_addr() {
            Ok(local) => local,
            Err(err) => {
                eprintln!(
                    "typecrate-services-server: bound {} but cannot read its address: {err}",
                    config.addr
                );
                return EXIT_RUNTIME;
            }
        };

        // Every declared port binds BEFORE anything is served, so a refused extra bind
        // fails the whole start instead of leaving the default listener up with a route
        // file that has nowhere to answer. `(bound socket, bound address, declared port)`.
        let mut extra: Vec<(TcpListener, SocketAddr, u16)> = Vec::new();
        for port in declared {
            let bound = match bind_declared_listener(config.addr, port).await {
                Ok(listener) => listener,
                Err(message) => {
                    eprintln!("typecrate-services-server: {message}");
                    return EXIT_RUNTIME;
                }
            };
            let bound_addr = match bound.local_addr() {
                Ok(addr) => addr,
                Err(err) => {
                    eprintln!(
                        "typecrate-services-server: bound port {port} for a route file but \
                         cannot read its address: {err}"
                    );
                    return EXIT_RUNTIME;
                }
            };
            extra.push((bound, bound_addr, port));
        }

        // The bind answer is printed as an *address*, not as a URL: `http://0.0.0.0`
        // is exactly the mistake this banner exists to remove. The lines below are the
        // URLs a client can actually open.
        let surface = if is_wildcard(local.ip()) {
            "every interface"
        } else {
            "this interface only"
        };
        println!("typecrate-services-server listening on {local} - {surface}");
        println!("  database root:    {}", state.database_root.display());
        println!("  max request body: {MAX_REQUEST_BODY_BYTES} bytes");
        println!("  routes:           generated at build time (Ctrl+C to stop)");

        // `banner_lines` only consumes the probe for a wildcard bind (an explicit
        // `--addr <ip>` already named the interface, and a loopback bind has no LAN
        // answer at all), so the probe is skipped everywhere else. It stays
        // best-effort: with no routable candidate the banner prints an honest
        // placeholder instead of a made-up address.
        let lan_ip = if is_wildcard(local.ip()) {
            detected_lan_address()
        } else {
            None
        };
        for line in banner_lines(local, lan_ip) {
            println!("{line}");
        }
        // Then one line per extra listener - empty when nothing was declared, which is
        // what keeps the single-listener banner byte-identical.
        let banner_extra: Vec<(SocketAddr, Vec<String>)> = extra
            .iter()
            .map(|(_, addr, port)| (*addr, declared_routes_for_port(*port)))
            .collect();
        for line in extra_listener_lines(&banner_extra) {
            println!("{line}");
        }

        // Graceful shutdown, in the order the standalone entry performs it, decided ONCE
        // for the whole process and shared by every listener:
        //
        //   * one task waits for Ctrl+C / SIGTERM;
        //   * it runs `begin_shutdown_all` (`typecrate_harness::server:129-134`) BEFORE the
        //     flag flips, i.e. before any listener stops accepting - the same order the
        //     single-listener version used. It is what makes the drain FINISH: it marks
        //     every cached root's store `shuttingDown` (no further turn is claimed) and
        //     closes every live event stream with 1001, so an open SSE or WebSocket
        //     response is not left waiting on US while we wait on it. A process serving no
        //     harness root answers quietly, so one path is right for both modes.
        //   * each listener then drains on the flag, so no listener can "win" the signal
        //     and leave the others accepting forever.
        let (stop_tx, stop_rx) = watch::channel(false);
        let signal = {
            let stop_tx = stop_tx.clone();
            tokio::spawn(async move {
                shutdown_signal().await;
                typecrate_harness::server::begin_shutdown_all();
                let _ = stop_tx.send(true);
            })
        };

        // One `axum::serve` per listener. The router is chosen by the port, and it is the
        // DECLARED port that selects the mounted groups; the bound address is only what
        // the banner reports (the two differ for a literal `port: 0` declaration).
        let mut serving = JoinSet::new();
        {
            let stop = stop_rx.clone();
            let state = state.clone();
            serving.spawn(async move {
                axum::serve(listener, app_for(None, state))
                    .with_graceful_shutdown(wait_for_shutdown(stop))
                    .await
            });
        }
        for (listener, _bound, port) in extra {
            let stop = stop_rx.clone();
            let state = state.clone();
            serving.spawn(async move {
                axum::serve(listener, app_for(Some(port), state))
                    .with_graceful_shutdown(wait_for_shutdown(stop))
                    .await
            });
        }

        // Wait for EVERY listener, keeping the first failure. `serve` returning Ok is the
        // "shutdown complete" case the single-listener version printed; a listener that
        // stopped by itself is a server error and takes the exit code, but still drains its
        // siblings instead of orphaning them.
        let mut exit = EXIT_OK;
        while let Some(joined) = serving.join_next().await {
            match joined {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    eprintln!("typecrate-services-server: server error: {err}");
                    exit = EXIT_RUNTIME;
                }
                // A panic inside one accept loop used to leave through `block_on` and take
                // the process with it; as a task it surfaces here, the remaining listeners
                // are drained, and the process still exits 1.
                Err(err) => {
                    eprintln!("typecrate-services-server: listener task failed: {err}");
                    exit = EXIT_RUNTIME;
                }
            }
            if exit != EXIT_OK {
                // Same drain as the signal path (and `begin_shutdown_all` is idempotent: a
                // store already marked `shuttingDown` stays marked and the streams are
                // already closed), so an event stream on another listener cannot hold the
                // drain open forever.
                typecrate_harness::server::begin_shutdown_all();
                let _ = stop_tx.send(true);
            }
        }
        // Nothing serves any more, so the signal task's Ctrl+C wait has to be released:
        // dropping the runtime blocks on spawned tasks, and that one only ends when
        // somebody presses Ctrl+C. Aborting an already-finished task is a no-op.
        signal.abort();

        if exit == EXIT_OK {
            println!("typecrate-services-server: shutdown complete");
        }
        exit
    }

    /// Resolves when the process should stop: Ctrl+C / Ctrl+Break (windows and unix)
    /// or SIGTERM, which is what containers send and what a plain `kill <pid>`
    /// produces. A signal backend that is unavailable logs why and then waits
    /// forever, so one missing signal source cannot make the server fail to start.
    async fn shutdown_signal() {
        let ctrl_c = async {
            if let Err(err) = tokio::signal::ctrl_c().await {
                eprintln!("typecrate-services-server: Ctrl+C unavailable ({err})");
                std::future::pending::<()>().await;
            }
        };

        #[cfg(unix)]
        let terminate = async {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut signal) => signal.recv().await,
                Err(err) => {
                    eprintln!("typecrate-services-server: SIGTERM unavailable ({err})");
                    // Both match arms have to yield the same type, and the `Ok` arm yields
                    // `Option<()>`: `Signal::recv()` returns `Some(())` per delivered signal
                    // and `None` once the sender is dropped. `pending::<Option<()>>()` is that
                    // exact item type on an infinite wait, so this arm keeps doing what it
                    // always did - log why the backend is missing, then park forever, which
                    // leaves the `select!` below waiting on Ctrl+C alone. The old
                    // `pending::<()>` yielded `()` and was the E0308 mismatch; `None` vs
                    // `Some(())` is never inspected, so no control flow changes either way.
                    std::future::pending::<Option<()>>().await
                }
            }
        };
        // Windows has no SIGTERM to subscribe to; the console Ctrl+C/Ctrl+Break
        // branch above is the shutdown path there, so this one never fires.
        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();

        tokio::select! {
            () = ctrl_c => {}
            // `_` instead of `()`: on unix this future resolves to the `Option<()>` item of the
            // match above, on non-unix to `()` from the bare `pending::<()>`. The payload is
            // meaningless for a terminate signal - reaching this arm at all is the whole answer.
            _ = terminate => {}
        }
        eprintln!("typecrate-services-server: shutdown signal received, draining");
    }

    // -----------------------------------------------------------------------
    // Configuration unit tests: pure (no sockets, no `set_var`), so they stay
    // valid while the endpoint layer and its tests are still moving.
    // -----------------------------------------------------------------------
    #[cfg(test)]
    mod tests {
        use super::*;
        use std::path::PathBuf;

        /// `&[&str]` -> `Vec<OsString>`, i.e. what `std::env::args_os` would hand main.
        fn owned(args: &[&str]) -> Vec<OsString> {
            args.iter().map(|arg| OsString::from(*arg)).collect()
        }

        fn flags(args: &[&str]) -> Flags {
            parse_flags(&owned(args)).unwrap_or_else(|err| panic!("`{args:?}` parses: {err}"))
        }

        fn config(args: &[&str], env_addr: Option<&str>, env_root: Option<&Path>) -> Config {
            from_parts(&flags(args), env_addr, env_root)
                .unwrap_or_else(|err| panic!("`{args:?}` resolves: {err}"))
        }

        /// `config(..)` reduced to the string nearly every address test asserts, which
        /// keeps those assertions short enough to read as a precedence table.
        fn addr(args: &[&str], env_addr: Option<&str>) -> String {
            config(args, env_addr, None).addr.to_string()
        }

        #[test]
        fn defaults_to_every_interface_on_the_documented_port() {
            // R1/T19: the unconfigured start is LAN-reachable on purpose, like the TS
            // gateway. A test that asserts `0.0.0.0` here is what stops the default
            // from silently sliding back to loopback.
            let resolved = config(&[], None, None);
            assert_eq!(resolved.addr.to_string(), "0.0.0.0:8000");
            assert_eq!(resolved.database_root, default_database_root());
            assert!(!resolved.help);
        }

        #[test]
        fn an_explicit_loopback_bind_survives_env_and_port() {
            // A bare `--addr 127.0.0.1` is the documented opt-in out of the LAN, so it
            // has to survive both an env ip (it keeps env's port) and a later --port
            // (which must not resurrect the wildcard default host).
            assert_eq!(addr(&["--addr", "127.0.0.1"], None), "127.0.0.1:8000");
            assert_eq!(
                addr(&["-a", "127.0.0.1"], Some("0.0.0.0:8080")),
                "127.0.0.1:8080"
            );
            assert_eq!(
                addr(
                    &["--addr", "127.0.0.1", "--port", "4321"],
                    Some("0.0.0.0:8080")
                ),
                "127.0.0.1:4321"
            );
            // Loopback asked for through the environment alone is honoured too.
            assert_eq!(addr(&[], Some("127.0.0.1:8000")), "127.0.0.1:8000");
        }

        #[test]
        fn environment_beats_default_and_cli_beats_environment() {
            let env_only = config(&[], Some("0.0.0.0:8080"), None);
            assert_eq!(env_only.addr.to_string(), "0.0.0.0:8080");

            let cli_wins = config(&["--addr", "127.0.0.1:9999"], Some("0.0.0.0:8080"), None);
            assert_eq!(cli_wins.addr.to_string(), "127.0.0.1:9999");

            // Any other custom bind is preserved verbatim, both through env only and
            // through env + cli port (the two halves compose, they never clobber).
            let custom_env = config(&[], Some("192.168.0.7:1234"), None);
            assert_eq!(custom_env.addr.to_string(), "192.168.0.7:1234");
            let custom_port = config(&["--port", "8080"], Some("10.0.0.5:8000"), None);
            assert_eq!(custom_port.addr.to_string(), "10.0.0.5:8080");
        }

        #[test]
        fn port_applies_to_the_selected_host() {
            // Explicit wildcard host + explicit port.
            assert_eq!(
                addr(&["--addr", "0.0.0.0", "--port", "8080"], None),
                "0.0.0.0:8080"
            );
            // No --addr at all: the LAN default keeps the requested port, so
            // `--port 4321` alone is still reachable from another device.
            assert_eq!(addr(&["--port", "4321"], None), "0.0.0.0:4321");
            // Env host + CLI port compose instead of one clobbering the other.
            assert_eq!(
                addr(&["--port", "4321"], Some("0.0.0.0:8080")),
                "0.0.0.0:4321"
            );
            // Bracketed IPv6 with an explicit port parses exactly.
            assert_eq!(addr(&["--addr", "[::1]:7007"], None), "[::1]:7007");
        }

        #[test]
        fn a_bracketed_ipv6_host_without_a_port_keeps_the_selected_port() {
            // `[::1]` is the CLI spelling of an IPv6 HOST - the brackets are what make a
            // colon-bearing address readable - so it composes with the port rules exactly
            // like a bare IPv4: built-in default, then env, then `--port` on top. (An
            // UNBRACKETED `::1` stays a usage error on purpose: without the brackets the
            // value is ambiguous with `host:port`, and this CLI never guesses.)
            assert_eq!(addr(&["--addr", "[::1]"], None), "[::1]:8000");
            assert_eq!(
                addr(&["--addr", "[::1]", "--port", "7007"], None),
                "[::1]:7007"
            );
            assert_eq!(addr(&["--addr", "[::]"], None), "[::]:8000");
            // A bracketed IPv4 is the same shape and lands on the same answer.
            assert_eq!(addr(&["--addr", "[127.0.0.1]"], None), "127.0.0.1:8000");
            // The env value goes through the same rule, and a CLI port still composes.
            assert_eq!(addr(&[], Some("[::1]:4321")), "[::1]:4321");
            assert_eq!(addr(&["--port", "4321"], Some("[::1]")), "[::1]:4321");
            assert_eq!(
                addr(&["--addr", "[::1]"], Some("0.0.0.0:8080")),
                "[::1]:8080"
            );

            // Unusable values stay errors that name the accepted spellings, instead of
            // falling back to the default bind.
            assert_eq!(
                from_parts(&Flags::default(), Some("[not-an-ip]"), None)
                    .err()
                    .as_deref(),
                Some(
                    "TYPECRATE_SERVICES_ADDR `[not-an-ip]` is not a valid address \
                     (expected <ip>[:port] or [<ipv6>][:port])"
                )
            );
            // Unbalanced brackets carry a colon, so they are a bad `host:port`.
            assert_eq!(
                from_parts(&flags(&["--addr", "[::1"]), None, None)
                    .err()
                    .as_deref(),
                Some(
                    "--addr `[::1` is not a valid address (expected <ip>[:port] or [<ipv6>][:port])"
                )
            );
            // The whole point of the brackets: an unbracketed IPv6 plus a port is
            // ambiguous, so it is refused rather than re-split.
            assert!(from_parts(&flags(&["--addr", "::1:8000"]), None, None).is_err());
        }

        #[test]
        fn database_root_flag_beats_env_beats_default() {
            let from_env = config(&[], None, Some(Path::new("/from/env")));
            assert_eq!(from_env.database_root, PathBuf::from("/from/env"));

            let from_cli = config(
                &["--database-root", "/from/cli/./sub/.."],
                None,
                Some(Path::new("/from/env")),
            );
            assert_eq!(from_cli.database_root, PathBuf::from("/from/cli"));
        }

        #[test]
        fn help_short_circuits_value_validation_but_not_unknown_options() {
            // `--help` wins over an unusable VALUE on the same line: printing usage
            // must not depend on the address parsing.
            let help = config(&["--help", "--addr", "not an address"], None, None);
            assert!(help.help);
            assert_eq!(help.addr.to_string(), "0.0.0.0:8000");

            // An unknown OPTION is rejected while parsing, which is before help.
            assert_eq!(
                parse_flags(&owned(&["--help", "--nope"])).err().as_deref(),
                Some("unknown option `--nope`")
            );
            assert_eq!(
                parse_flags(&owned(&["--nope", "--port"])).err().as_deref(),
                Some("unknown option `--nope`")
            );
            assert_eq!(
                parse_flags(&owned(&["--addr"])).err().as_deref(),
                Some("--addr needs a value")
            );
            assert_eq!(
                parse_flags(&owned(&["--database-root"])).err().as_deref(),
                Some("--database-root needs a value")
            );
            assert!(parse_flags(&owned(&["--port", "99999"])).is_err());
            // A non-numeric address stays an error rather than silently defaulting.
            assert!(from_parts(&Flags::default(), Some("not an address"), None).is_err());
        }

        // -------------------------------------------------------------------
        // Banner + address-reporting tests. These pin the exact printed strings,
        // padding included, because the whole point of the banner is that a human
        // can copy a URL out of it.
        // -------------------------------------------------------------------

        /// The trust/firewall note every LAN-reachable start ends with, as the
        /// literal line (`concat!` only keeps the source readable - the bytes are
        /// one string, and the leading padding is part of what is asserted).
        const NOTE_LINE: &str = concat!(
            "  note:             unauthenticated - trusted LAN only, never the ",
            "public internet; Windows Firewall may prompt on first start",
        );

        #[test]
        fn help_documents_the_lan_default_the_builtins_and_the_trust_rule() {
            // `--help` is the offline documentation, so the load-bearing statements
            // are asserted rather than eyeballed: the LAN default, the loopback
            // opt-in, the firewall caveat that binding does not remove, the fact
            // that there is no auth, the endpoint scope, and the four framework
            // builtins (the two page paths are kept in sync with the constants
            // `banner_lines` prints, the two assets with `server::app`).
            assert!(HELP.contains("Default 0.0.0.0:8000"));
            assert!(HELP.contains("--addr 127.0.0.1"));
            assert!(HELP.contains("Windows Firewall"));
            assert!(HELP.contains("never straight onto the public internet"));
            assert!(HELP.contains("Requests are unauthenticated"));
            assert!(HELP.contains("TypeScript service endpoints are not ported yet"));
            // `"GET / "` (slash + space) is the playground line; `"GET /v1/..."` is
            // the listing line. Neither substring matches the other.
            assert!(HELP.contains("GET / "));
            assert!(HELP.contains(&format!("GET {ROUTE_LISTING_PATH}")));
            // The reserved-path list is complete: a route author reading `--help`
            // learns the two asset paths as well, and that a clash is a startup panic.
            assert!(HELP.contains("GET /playground.css"));
            assert!(HELP.contains("GET /playground.js"));
            assert!(HELP.contains("must not register any of them"));
            // The bind-conflict story is the honest one: only the same address is
            // refused, a Windows-specific bind can coexist with a wildcard holder, and
            // the recommendation for parallel instances is distinct ports.
            assert!(HELP.contains("PARALLEL INSTANCES:"));
            assert!(HELP.contains("only when THAT address and port are already bound"));
            assert!(HELP.contains("the more specific socket"));
            assert!(HELP.contains("distinct --port values"));
            // The IPv6 host form the parser accepts is spelled out where a human reads
            // it, and it is the same message `parse_addr` produces on a bad value.
            assert!(HELP.contains("[::1] or [::1]:8000"));
        }

        #[test]
        fn only_a_routable_interface_address_is_published_as_a_lan_url() {
            // Wildcard, loopback and the fallback link-local ranges are host- or
            // link-scoped: printing one would promise a URL no other device opens.
            for hidden in [
                IpAddr::from([0, 0, 0, 0]),
                IpAddr::from([127, 0, 0, 1]),
                IpAddr::from([169, 254, 1, 1]),
                "::".parse::<IpAddr>().unwrap(),
                "::1".parse::<IpAddr>().unwrap(),
                "fe80::1".parse::<IpAddr>().unwrap(),
            ] {
                assert!(
                    !is_shareable_address(hidden),
                    "{hidden} must stay off the banner"
                );
            }

            for published in [
                IpAddr::from([192, 168, 1, 20]),
                IpAddr::from([10, 0, 0, 5]),
                "fd00::1".parse::<IpAddr>().unwrap(),
            ] {
                assert!(is_shareable_address(published), "{published} is a LAN url");
            }
        }

        #[test]
        fn a_bind_answer_is_reported_as_the_url_that_actually_works_here() {
            // `0.0.0.0`/`::` are not navigable, and a host-specific bind does not
            // answer on loopback, so the local URL has to follow the bind.
            let wildcard_v4: SocketAddr = "0.0.0.0:8000".parse().unwrap();
            let wildcard_v6: SocketAddr = "[::]:7007".parse().unwrap();
            let host: SocketAddr = "192.168.1.20:8000".parse().unwrap();
            let loopback: SocketAddr = "127.0.0.1:4321".parse().unwrap();

            assert_eq!(local_report_ip(wildcard_v4).to_string(), "127.0.0.1");
            assert_eq!(local_report_ip(wildcard_v6).to_string(), "::1");
            assert_eq!(local_report_ip(host).to_string(), "192.168.1.20");
            assert_eq!(local_report_ip(loopback).to_string(), "127.0.0.1");
        }

        #[test]
        fn a_wildcard_bind_prints_loopback_urls_and_the_detected_lan_url() {
            let local: SocketAddr = "0.0.0.0:8000".parse().unwrap();
            let lan = IpAddr::from([192, 168, 1, 20]);
            assert_eq!(
                banner_lines(local, Some(lan)),
                vec![
                    "  playground:       http://127.0.0.1:8000/",
                    "  endpoint list:    http://127.0.0.1:8000/v1/services/routes",
                    "  other devices:    http://192.168.1.20:8000/",
                    NOTE_LINE,
                ]
            );
        }

        #[test]
        fn a_wildcard_bind_without_a_probe_answer_keeps_the_placeholder() {
            // Offline host / only link-local candidates: an honest placeholder beats
            // a made-up address, and the note line still says why it may not work.
            let local: SocketAddr = "0.0.0.0:8000".parse().unwrap();
            assert_eq!(
                banner_lines(local, None),
                vec![
                    "  playground:       http://127.0.0.1:8000/",
                    "  endpoint list:    http://127.0.0.1:8000/v1/services/routes",
                    "  other devices:    http://<this machine's LAN ip>:8000/",
                    NOTE_LINE,
                ]
            );
        }

        #[test]
        fn a_loopback_bind_says_what_is_not_reachable_and_what_to_pass() {
            let local: SocketAddr = "127.0.0.1:4321".parse().unwrap();
            assert_eq!(
                banner_lines(local, None),
                vec![
                    "  playground:       http://127.0.0.1:4321/",
                    "  endpoint list:    http://127.0.0.1:4321/v1/services/routes",
                    "  other devices:    none - bound to 127.0.0.1 (loopback only)",
                    "  next:             --addr 0.0.0.0 (the default) publishes :4321 on every interface",
                ]
            );
        }

        #[test]
        fn an_explicit_host_bind_prints_that_host_only() {
            // The operator named the interface, so the probe result is irrelevant -
            // and 127.0.0.1 must not be offered, because it is refused here.
            let local: SocketAddr = "192.168.1.20:8000".parse().unwrap();
            let other = IpAddr::from([10, 0, 0, 5]);
            assert_eq!(
                banner_lines(local, Some(other)),
                vec![
                    "  playground:       http://192.168.1.20:8000/",
                    "  endpoint list:    http://192.168.1.20:8000/v1/services/routes",
                    "  other devices:    http://192.168.1.20:8000/",
                    NOTE_LINE,
                ]
            );
        }

        #[test]
        fn an_ipv6_wildcard_bind_gets_bracketed_urls() {
            let local: SocketAddr = "[::]:7007".parse().unwrap();
            let lan = IpAddr::from([10, 0, 0, 5]);
            assert_eq!(
                banner_lines(local, Some(lan)),
                vec![
                    "  playground:       http://[::1]:7007/",
                    "  endpoint list:    http://[::1]:7007/v1/services/routes",
                    "  other devices:    http://10.0.0.5:7007/",
                    NOTE_LINE,
                ]
            );
        }

        // -------------------------------------------------------------------
        // Declared-port tests. Also pure: the listener set is DERIVED from the
        // compiled-in route table, so what is pinned here is the rules around it -
        // the collision check, the banner lines it produces, and the drain flag the
        // listeners share - never a socket.
        // -------------------------------------------------------------------

        /// The default bind of this build must not be a port a route file declares, or
        /// every unconfigured start would stop at the collision check.
        #[test]
        fn no_route_file_declares_the_built_in_default_port() {
            let default: SocketAddr = DEFAULT_ADDR.parse().unwrap();
            let declared = declared_ports();
            assert!(
                check_declared_port_collisions(default, &declared).is_ok(),
                "the route tree declares the built-in default port {default}: {declared:?}"
            );
            // The set arrives sorted and de-duplicated (it is what the banner prints and
            // what gets bound in order), and every port in it has at least one row in the
            // listing - the same table both are derived from.
            let mut expected = declared.clone();
            expected.sort_unstable();
            expected.dedup();
            assert_eq!(
                expected, declared,
                "`declared_ports()` must be sorted and unique"
            );
            for port in &declared {
                assert!(
                    !declared_routes_for_port(*port).is_empty(),
                    "port {port} is bound but declares no route"
                );
            }
        }

        /// AC5: a declared port that equals the resolved default port is a startup error
        /// that names the port, the socket and the flag that moves it - and nothing else is.
        #[test]
        fn a_route_declaring_the_default_port_is_a_clean_startup_error() {
            let default: SocketAddr = "0.0.0.0:8000".parse().unwrap();
            // No declarations at all (the single-listener case) and declarations of OTHER
            // ports are both fine; the default listener is never part of the set.
            assert_eq!(check_declared_port_collisions(default, &[]), Ok(()));
            assert_eq!(
                check_declared_port_collisions(default, &[8010, 9000]),
                Ok(())
            );

            const EXPECTED: &str = concat!(
                "a route file declares port 8000 and this process binds 0.0.0.0:8000, so both ",
                "listeners want the same socket. They are separate listeners and neither ",
                "shares it: move the default with --port <other> (or change the declaration). ",
                "Nothing was bound.",
            );
            assert_eq!(
                check_declared_port_collisions(default, &[8010, 8000])
                    .err()
                    .as_deref(),
                Some(EXPECTED)
            );

            // The rule is about the RESOLVED port, so `--port 4321` collides with a 4321
            // declaration and not with an untouched 8000 one.
            let loopback: SocketAddr = "127.0.0.1:4321".parse().unwrap();
            assert_eq!(check_declared_port_collisions(loopback, &[8000]), Ok(()));
            assert!(check_declared_port_collisions(loopback, &[4321]).is_err());
        }

        /// AC4: the single-listener banner survives byte for byte because the extra lines
        /// are a `Vec` the caller prints, and it is empty when nothing was declared.
        #[test]
        fn no_declared_port_prints_no_extra_banner_line() {
            let none: Vec<String> = Vec::new();
            assert_eq!(extra_listener_lines(&[]), none);
        }

        #[test]
        fn a_declared_listener_is_printed_as_its_own_url_and_paths() {
            // A wildcard bind is reported as loopback exactly like the other banner lines,
            // and the value is the API surface of THAT listener - not the playground, which
            // an extra listener does not serve.
            let wildcard: SocketAddr = "0.0.0.0:8010".parse().unwrap();
            assert_eq!(
                extra_listener_lines(&[(wildcard, vec!["GET /v1/services/health".to_owned()])]),
                vec!["  listener 8010:    http://127.0.0.1:8010 - GET /v1/services/health"]
            );

            // One line per listener, in the order the sorted set produced, and an explicit
            // host bind is reported as itself (127.0.0.1 would be refused there).
            let lan: SocketAddr = "192.168.1.20:9000".parse().unwrap();
            assert_eq!(
                extra_listener_lines(&[
                    (wildcard, vec!["GET /a".to_owned(), "POST /b".to_owned()]),
                    (lan, vec![]),
                ]),
                vec![
                    "  listener 8010:    http://127.0.0.1:8010 - GET /a, POST /b",
                    "  listener 9000:    http://192.168.1.20:9000 - no declared route",
                ]
            );
        }

        /// The drain flag is shared, not competed for: one decision stops every listener,
        /// a listener that arrives after the decision does not wait, and a listener does not
        /// hang once the process is on its way out (every sender gone).
        #[tokio::test]
        async fn every_listener_stops_when_the_shared_flag_flips() {
            let (tx, rx) = watch::channel(false);
            let waiting = tokio::spawn(wait_for_shutdown(rx));
            tx.send(true).expect("a listener is still waiting");
            tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
                .await
                .expect("the wait ends when the flag flips")
                .expect("the wait does not panic");
        }

        #[tokio::test]
        async fn a_late_listener_does_not_wait_on_a_flag_that_already_flipped() {
            // The `borrow_and_update` check is what covers a signal arriving between binding
            // a socket and spawning that socket's serve task.
            let (_tx, rx) = watch::channel(true);
            tokio::time::timeout(std::time::Duration::from_secs(5), wait_for_shutdown(rx))
                .await
                .expect("an already-set flag is not a wait");
        }

        #[tokio::test]
        async fn a_dropped_sender_ends_the_wait_instead_of_hanging() {
            let (tx, rx) = watch::channel(false);
            drop(tx);
            tokio::time::timeout(std::time::Duration::from_secs(5), wait_for_shutdown(rx))
                .await
                .expect("with no sender left there is no further signal");
        }

        /// The route-port story a human reads offline: that declarations exist, that each
        /// one is its own listener serving only its own paths, that two ports serving one
        /// path do not override each other, that the builtins stay on the default listener,
        /// and that a clash with the bound port stops the start.
        #[test]
        fn help_documents_the_ports_a_route_file_may_declare() {
            assert!(HELP.contains("PORTS A ROUTE FILE DECLARES"));
            assert!(HELP.contains("gets its OWN listener"));
            assert!(HELP.contains("each listener serves only the paths that declared it"));
            assert!(HELP.contains("served on both, independently - neither"));
            assert!(HELP.contains("declaration overrides the other"));
            assert!(HELP.contains("framework builtins of the DEFAULT listener"));
            assert!(HELP.contains("answer 404 on a declared one"));
            assert!(HELP.contains("two listeners cannot share one socket"));
            assert!(HELP.contains("The startup banner lists"));
        }
    }
}

fn main() {
    // `run()` already picked an exit code; forwarding it verbatim is what keeps
    // "bad usage" distinguishable from "the port was taken".
    std::process::exit(native::run());
}
