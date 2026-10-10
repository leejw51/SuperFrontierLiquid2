//! A whole exchange, booted from the real binaries, on ports it chose itself.
//!
//! # What a stack is
//!
//! One `cb-node` sequencer producing blocks against a freshly-applied genesis,
//! and one `cb-gateway` in front of it with the development login mounted. Both
//! are the shipped release binaries, started the way the Makefile starts them,
//! reading the shipped environment file. Nothing is stubbed: when a test here
//! places an order, a transaction is signed, submitted over Cap'n Proto,
//! sequenced into a block and executed.
//!
//! # Isolation
//!
//! Each stack takes a fresh temporary directory and three free ports, so two
//! tests — or two engineers, or a test and a developer's `make up` — never
//! collide. Nothing in the harness reads or writes a fixed path.
//!
//! # Cleanup
//!
//! [`Stack`]'s `Drop` kills both children, reaps them, and removes the data
//! directory. `Drop` runs while a panic unwinds, so a failing assertion cleans
//! up exactly as a passing one does. A leaked sequencer holds a RocksDB lock
//! and a port, and wedges whatever runs next; that is worth being careful
//! about. The one case `Drop` cannot cover is the test binary itself being
//! killed, which is what `make e2e-clean` is for.
//!
//! When a test panics, `Drop` prints the tail of both process logs first. "The
//! gateway returned 500" is not a diagnosis; the gateway's own log usually is.

use std::collections::BTreeMap;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::client::Api;
use crate::shipped;
use crate::wait::poll_until_some;

/// How long a stack has to come up before the test gives up on it.
const BOOT_TIMEOUT: Duration = Duration::from_secs(60);

/// How many stacks may run at once.
///
/// Each stack is two processes, a RocksDB, a SQLite and three sockets. Letting
/// `cargo test` start one per core exhausts file descriptors on a default
/// `ulimit -n` — the same wall `make testallslow` exists to avoid. Two is
/// conservative and still halves the wall clock; `CB_E2E_PARALLEL` raises it.
fn stack_limit() -> &'static Arc<Semaphore> {
    static LIMIT: OnceLock<Arc<Semaphore>> = OnceLock::new();
    LIMIT.get_or_init(|| {
        let permits = std::env::var("CB_E2E_PARALLEL")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(2);
        Arc::new(Semaphore::new(permits))
    })
}

/// Which flags the installed `cb-gateway` understands.
///
/// The operator role is being moved off a self-chosen handle and onto an
/// address, because a role granted by a string anyone may pick is not a role.
/// During that move two gateways exist: one that only knows `--admin-handle`
/// and one that knows `--admin-address`. Reading `--help` once and choosing is
/// what lets this suite run against either — and, more importantly, what stops
/// the suite from failing to *boot* on the day the flag changes and hiding
/// every real result behind a start-up error.
fn gateway_supports(flag: &str) -> bool {
    static HELP: OnceLock<String> = OnceLock::new();
    let help = HELP.get_or_init(|| {
        let bin = shipped::binary("cb-gateway");
        let output = Command::new(&bin)
            .arg("--help")
            .output()
            .unwrap_or_else(|err| panic!("cannot run {} --help: {err}", bin.display()));
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    help.contains(flag)
}

/// How a stack granted the operator role, and to whom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminGrant {
    /// Bound to an address, which no other account can claim.
    Address(String),
    /// Bound to a handle, which is whoever registers it first. Only used when
    /// the installed gateway has no address flag.
    Handle(String),
}

/// The account index this suite always uses for the operator.
///
/// Deliberately far away from the low indices tests hand to ordinary users.
/// The operator is an address now, so a stack that trusted index 0 would make
/// the first account most tests create an administrator — and every "an
/// ordinary account gets 403" assertion would quietly be testing an
/// administrator instead. Reserving an index costs nothing and removes a trap
/// that only shows up as a puzzling pass.
pub const ADMIN_INDEX: u32 = 99;

/// The address [`ADMIN_INDEX`] derives from the canonical test mnemonic.
///
/// Derived with the product's own BIP-44 code rather than pasted, so it cannot
/// drift from what the gateway will recover when that account signs in.
pub fn admin_address() -> &'static str {
    static ADDRESS: OnceLock<String> = OnceLock::new();
    ADDRESS.get_or_init(|| {
        let (_, address) =
            cb_gateway::auth::eth::derive_from_mnemonic(crate::flows::TEST_MNEMONIC, ADMIN_INDEX)
                .expect("the canonical mnemonic derives every index");
        cb_gateway::auth::eth::checksum_address(&address)
    })
}

/// The server secret every stack uses.
///
/// Deliberately not the value in `.env.example`. That file ships a working
/// constant, every account's trading key is `HKDF(server_secret, address)`, and
/// a deployment that copies the example verbatim therefore hands out every key
/// it will ever issue — so the gateway is being changed to refuse exactly that
/// string. A suite that booted with it would stop booting on the day the fix
/// lands, and the failure would look like the harness rather than the fix. The
/// refusal itself is asserted in `admin.rs`.
pub const E2E_SERVER_SECRET: &str =
    "cb-e2e-2b7f41c6d9a04e5388f1ac7be0d36925-7c41e0b95af26d13-not-a-shared-deployment-secret";

/// Reserves a free loopback port by binding it and letting go.
///
/// There is a window between the bind and the child's own bind in which
/// something else could take the port. In practice the kernel does not hand the
/// same ephemeral port out twice in that window, and the alternative — a fixed
/// range with a counter — collides for real as soon as two `cargo test`
/// processes run at once, which is the case this exists to survive.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("cannot bind a loopback port");
    let port = listener
        .local_addr()
        .expect("a bound listener has an address")
        .port();
    drop(listener);
    port
}

/// Everything about a stack that a test might want to vary.
///
/// The defaults are the shipped defaults with two exceptions, both of which are
/// about not making the *harness* the thing under test: the rate limits are
/// raised, because every test in the suite arrives from `127.0.0.1` and would
/// otherwise start seeing `RATE_LIMITED` from its neighbours' traffic, and the
/// faucet cooldown is left at the shipped hour so cooldown behaviour is real.
/// The rate-limit contract test lowers the limit deliberately.
#[derive(Debug, Clone)]
pub struct StackConfig {
    /// Genesis file to apply. Defaults to whatever `rust/Makefile` defaults to.
    pub genesis: PathBuf,
    /// Milliseconds between blocks.
    pub block_interval_ms: u64,
    /// `--faucet-cooldown` in seconds.
    pub faucet_cooldown_seconds: u64,
    /// `--faucet-grant`, as the CLI spells it.
    pub faucet_grant: String,
    /// `--faucet-max-claims`.
    pub faucet_max_claims: Option<u64>,
    /// `--session-ttl-hours`.
    pub session_ttl_hours: i64,
    /// `--rate-limit-per-session`.
    pub rate_limit_per_session: u32,
    /// `--rate-limit-per-ip`.
    pub rate_limit_per_ip: u32,
    /// The account granted the operator role.
    pub admin: AdminGrant,
    /// The secret every trading key is derived from.
    pub server_secret: String,
    /// `--order-confirm-ms`.
    pub order_confirm_ms: u64,
    /// `--market-refresh-seconds`. The shipped default is 30, which is fine for
    /// a running deployment and means half a minute of a test's life waiting
    /// for the derivative catalogue to appear. Lowering it changes *when* the
    /// catalogue is fetched, never what is in it.
    pub market_refresh_seconds: u64,
    /// Extra gateway flags, for a test that needs one nothing else does.
    pub extra_gateway_args: Vec<String>,
}

impl Default for StackConfig {
    fn default() -> Self {
        Self {
            genesis: shipped::default_genesis_path(),
            block_interval_ms: 100,
            faucet_cooldown_seconds: 3_600,
            faucet_grant: "USD:100000,BTC:1".to_string(),
            faucet_max_claims: None,
            session_ttl_hours: 24,
            rate_limit_per_session: 1_000_000,
            rate_limit_per_ip: 1_000_000,
            admin: if gateway_supports("--admin-address") {
                AdminGrant::Address(admin_address().to_string())
            } else {
                AdminGrant::Handle("admin".to_string())
            },
            server_secret: E2E_SERVER_SECRET.to_string(),
            order_confirm_ms: 2_000,
            market_refresh_seconds: 1,
            extra_gateway_args: Vec::new(),
        }
    }
}

/// A running node and gateway.
pub struct Stack {
    /// An HTTP client already pointed at this stack's gateway.
    pub api: Api,
    node: Option<Child>,
    gateway: Option<Child>,
    data_dir: Option<tempfile::TempDir>,
    node_rpc_port: u16,
    node_dashboard_port: u16,
    gateway_port: u16,
    node_bin: PathBuf,
    env: BTreeMap<String, String>,
    config: StackConfig,
    _permit: OwnedSemaphorePermit,
}

impl Stack {
    /// Boots a stack with the shipped defaults and waits until it is serving.
    pub async fn start() -> Self {
        Self::start_with(StackConfig::default()).await
    }

    /// Boots a stack with a tailored configuration.
    pub async fn start_with(config: StackConfig) -> Self {
        let permit = Arc::clone(stack_limit())
            .acquire_owned()
            .await
            .expect("the stack semaphore is never closed");

        let node_bin = shipped::binary("cb-node");
        let gateway_bin = shipped::binary("cb-gateway");
        let env = shipped::stack_env();

        let data_dir = tempfile::Builder::new()
            .prefix("cb-e2e-")
            .tempdir()
            .expect("cannot create a temporary data directory");

        let node_rpc_port = free_port();
        let node_dashboard_port = free_port();
        let gateway_port = free_port();

        let mut stack = Self {
            api: Api::new(format!("http://127.0.0.1:{gateway_port}")),
            node: None,
            gateway: None,
            data_dir: Some(data_dir),
            node_rpc_port,
            node_dashboard_port,
            gateway_port,
            node_bin,
            env,
            config,
            _permit: permit,
        };

        stack.apply_genesis();
        stack.node = Some(stack.spawn_node());
        stack.gateway = Some(stack.spawn_gateway(&gateway_bin));
        stack.await_ready().await;
        stack
    }

    /// The data directory. Only the harness itself should need this — a test
    /// that reads the database has stopped testing the API.
    fn data_path(&self) -> PathBuf {
        self.data_dir
            .as_ref()
            .expect("the data directory outlives every use of it")
            .path()
            .to_path_buf()
    }

    /// `http://127.0.0.1:<port>` — the gateway's root, where `/ws` lives.
    pub fn gateway_origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.gateway_port)
    }

    /// `http://127.0.0.1:<port>/api/v1` — the REST base, which is what the
    /// frontend is configured with and what it derives the socket URL from.
    pub fn api_base(&self) -> String {
        format!("{}/api/v1", self.gateway_origin())
    }

    /// The configuration this stack was booted with.
    pub fn config(&self) -> &StackConfig {
        &self.config
    }

    /// How this stack granted the operator role.
    pub fn admin_grant(&self) -> &AdminGrant {
        &self.config.admin
    }

    /// Runs `cb-node init`, failing with the node's own output.
    fn apply_genesis(&self) {
        let output = Command::new(&self.node_bin)
            .arg("--data-dir")
            .arg(self.data_path())
            .arg("init")
            .arg("--genesis")
            .arg(&self.config.genesis)
            .envs(&self.env)
            .output()
            .unwrap_or_else(|err| panic!("cannot run {}: {err}", self.node_bin.display()));
        assert!(
            output.status.success(),
            "applying {} failed ({}).\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.config.genesis.display(),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    /// Starts the sequencer. Also used by [`Stack::restart_node`].
    fn spawn_node(&self) -> Child {
        let log = self.log_file("node.log");
        Command::new(&self.node_bin)
            .arg("--data-dir")
            .arg(self.data_path())
            .arg("sequencer")
            .arg("--listen")
            .arg(format!("127.0.0.1:{}", self.node_rpc_port))
            .arg("--dashboard-listen")
            .arg(format!("127.0.0.1:{}", self.node_dashboard_port))
            .arg("--block-interval-ms")
            .arg(self.config.block_interval_ms.to_string())
            .envs(&self.env)
            .stdin(Stdio::null())
            .stdout(log.try_clone().expect("cannot clone the node log handle"))
            .stderr(log)
            .spawn()
            .unwrap_or_else(|err| panic!("cannot start {}: {err}", self.node_bin.display()))
    }

    /// Starts the gateway.
    fn spawn_gateway(&self, bin: &PathBuf) -> Child {
        let log = self.log_file("gateway.log");
        let secret = self.config.server_secret.clone();
        let treasury_key = self.required_env("TREASURY_KEY");
        let mut command = Command::new(bin);
        command
            .arg("--listen")
            .arg(format!("127.0.0.1:{}", self.gateway_port))
            .arg("--node-rpc")
            .arg(format!("127.0.0.1:{}", self.node_rpc_port))
            .arg("--store")
            .arg("sqlite")
            .arg("--sqlite-path")
            .arg(self.data_path().join("gateway.sqlite"))
            .arg("--server-secret")
            .arg(secret)
            .arg("--faucet-treasury-key")
            .arg(treasury_key)
            .arg("--cors-origin")
            .arg("*")
            .arg("--dev-login")
            .arg("--block-time-ms")
            .arg(self.config.block_interval_ms.to_string())
            .arg("--order-confirm-ms")
            .arg(self.config.order_confirm_ms.to_string())
            .arg("--faucet-cooldown-seconds")
            .arg(self.config.faucet_cooldown_seconds.to_string())
            .arg("--faucet-grant")
            .arg(&self.config.faucet_grant)
            .arg("--session-ttl-hours")
            .arg(self.config.session_ttl_hours.to_string())
            .arg("--rate-limit-per-session")
            .arg(self.config.rate_limit_per_session.to_string())
            .arg("--rate-limit-per-ip")
            .arg(self.config.rate_limit_per_ip.to_string())
            .arg("--market-refresh-seconds")
            .arg(self.config.market_refresh_seconds.to_string());
        if let Some(max) = self.config.faucet_max_claims {
            command.arg("--faucet-max-claims").arg(max.to_string());
        }
        match &self.config.admin {
            AdminGrant::Address(address) => {
                command.arg("--admin-address").arg(address);
            }
            AdminGrant::Handle(handle) => {
                command.arg("--admin-handle").arg(handle);
                // Once the role moves to an address, `--admin-handle` keeps
                // working only behind an explicit opt-in, because granting a
                // role for a self-chosen string is the defect being closed.
                for opt_in in ["--insecure-admin-by-handle", "--allow-admin-handle"] {
                    if gateway_supports(opt_in) {
                        command.arg(opt_in);
                        break;
                    }
                }
            }
        }
        command.args(&self.config.extra_gateway_args);
        command
            .envs(&self.env)
            .stdin(Stdio::null())
            .stdout(
                log.try_clone()
                    .expect("cannot clone the gateway log handle"),
            )
            .stderr(log)
            .spawn()
            .unwrap_or_else(|err| panic!("cannot start {}: {err}", bin.display()))
    }

    /// A value the shipped environment must supply, with a message that says
    /// which file is missing it.
    fn required_env(&self, key: &str) -> String {
        self.env.get(key).cloned().unwrap_or_else(|| {
            panic!(
                "{key} is not set in {} — the stack cannot be started without it",
                shipped::env_example_path().display()
            )
        })
    }

    /// A log file inside the data directory, opened for append.
    fn log_file(&self, name: &str) -> std::fs::File {
        let path = self.data_path().join(name);
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap_or_else(|err| panic!("cannot open {}: {err}", path.display()))
    }

    /// Waits until the gateway is serving a catalogue it could only have got
    /// from the node.
    ///
    /// Not "the port accepts a connection", which a gateway that has never
    /// reached the sequencer also satisfies; every test would then race the RPC
    /// connection and fail as something other than a boot problem. And not
    /// "height ≥ 1" either: this chain does not produce empty blocks, so an
    /// idle chain sits at height 0 forever and that wait would never end. The
    /// derivative markets come from the node's own registry, so their presence
    /// in `GET /markets` is proof the link is up.
    async fn await_ready(&mut self) {
        let api = &self.api;
        let outcome = poll_until_some(
            "the gateway to serve a market catalogue fetched from the sequencer",
            BOOT_TIMEOUT,
            || async {
                let reply = api.get("/markets").await.map_err(|err| format!("{err}"))?;
                if reply.status != 200 {
                    return Err(format!("status {} body {}", reply.status, reply.body));
                }
                let markets = reply.body["markets"]
                    .as_array()
                    .ok_or_else(|| format!("no markets array in {}", reply.raw))?;
                let derivatives = markets
                    .iter()
                    .filter(|m| m["kind"].as_str() != Some("spot"))
                    .count();
                if derivatives > 0 {
                    Ok(markets.len())
                } else {
                    Err(format!(
                        "{} markets listed, none of them a derivative — the gateway has \
                         not read the node's registry yet",
                        markets.len()
                    ))
                }
            },
        )
        .await;
        if let Err(err) = outcome {
            self.print_logs("stack failed to boot");
            panic!("{err}");
        }
    }

    /// Waits until the chain is at least `target` blocks high.
    pub async fn await_height(&self, target: u64) -> u64 {
        crate::wait::expect_within(
            &format!("the chain to reach height {target}"),
            Duration::from_secs(30),
            || async {
                let reply = self
                    .api
                    .get("/chain/status")
                    .await
                    .map_err(|err| format!("{err}"))?;
                let height = reply.body["height"].as_u64().unwrap_or(0);
                if height >= target {
                    Ok(height)
                } else {
                    Err(format!("height {height}"))
                }
            },
        )
        .await
    }

    /// The current chain height.
    pub async fn height(&self) -> u64 {
        self.api
            .get("/chain/status")
            .await
            .expect("chain status is public and always available")
            .body["height"]
            .as_u64()
            .unwrap_or(0)
    }

    /// SIGKILLs the sequencer and reaps it.
    ///
    /// Reaping matters: RocksDB holds a `LOCK` file, and a restart on the same
    /// data directory fails while the old process is a zombie. A test that
    /// killed without waiting would see the restart fail and blame the node.
    pub fn kill_node(&mut self) {
        if let Some(mut child) = self.node.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Restarts the sequencer on the same data directory and the same RPC port.
    ///
    /// Same port, because the gateway was told that address once and the whole
    /// point of the exercise is whether it reconnects on its own.
    pub async fn restart_node(&mut self) {
        assert!(
            self.node.is_none(),
            "restart_node expects the sequencer to have been killed first"
        );
        self.node = Some(self.spawn_node());
    }

    /// The tail of a process log, for a failure message.
    pub fn log_tail(&self, name: &str, lines: usize) -> String {
        let path = self.data_path().join(name);
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let collected: Vec<&str> = text.lines().collect();
        let start = collected.len().saturating_sub(lines);
        collected[start..].join("\n")
    }

    /// Prints both logs to stderr, prefixed with why.
    pub fn print_logs(&self, why: &str) {
        eprintln!("\n===== cb-e2e: {why} =====");
        eprintln!("--- node.log (tail) ---\n{}", self.log_tail("node.log", 40));
        eprintln!(
            "--- gateway.log (tail) ---\n{}",
            self.log_tail("gateway.log", 40)
        );
        eprintln!("===== end =====\n");
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        let panicking = std::thread::panicking();
        if panicking {
            self.print_logs("a test failed; here is what the processes said");
        }
        for child in [self.gateway.take(), self.node.take()]
            .into_iter()
            .flatten()
        {
            let mut child = child;
            let _ = child.kill();
            let _ = child.wait();
        }
        // Keeping the directory is opt-in, because the default has to be that
        // a suite leaves nothing behind.
        if std::env::var_os("CB_E2E_KEEP_DATA").is_some() {
            if let Some(dir) = self.data_dir.take() {
                let path = dir.keep();
                eprintln!("cb-e2e: kept data directory {}", path.display());
            }
        }
    }
}

/// What happened when a gateway was asked to start with a given configuration.
#[derive(Debug)]
pub enum Startup {
    /// It came up and stayed up.
    Started,
    /// It exited, with whatever it said on the way out.
    Refused {
        /// The exit status.
        status: String,
        /// Everything it wrote to stdout and stderr.
        output: String,
    },
}

impl Startup {
    /// Whether the gateway refused to run.
    pub fn refused(&self) -> bool {
        matches!(self, Startup::Refused { .. })
    }

    /// What it said, for a failure message.
    pub fn output(&self) -> &str {
        match self {
            Startup::Started => "",
            Startup::Refused { output, .. } => output,
        }
    }
}

/// Starts a gateway with one setting changed and reports whether it stayed up.
///
/// No node and no data are involved: the question is only whether the binary
/// accepts the configuration it was handed. Some settings must be refused at
/// start-up rather than at the first request — a gateway that runs happily with
/// a shared secret has already derived everyone's trading key from it by the
/// time anyone notices.
pub async fn gateway_startup_with_secret(secret: &str) -> Startup {
    let bin = shipped::binary("cb-gateway");
    let env = shipped::stack_env();
    let treasury_key = env
        .get("TREASURY_KEY")
        .cloned()
        .expect("the shipped environment carries a treasury key");
    let dir = tempfile::Builder::new()
        .prefix("cb-e2e-startup-")
        .tempdir()
        .expect("cannot create a temporary directory");
    let log_path = dir.path().join("gateway.log");
    let log = std::fs::File::create(&log_path).expect("cannot create the start-up log");

    let mut child = Command::new(&bin)
        .arg("--listen")
        .arg(format!("127.0.0.1:{}", free_port()))
        .arg("--node-rpc")
        .arg(format!("127.0.0.1:{}", free_port()))
        .arg("--store")
        .arg("memory")
        .arg("--server-secret")
        .arg(secret)
        .arg("--faucet-treasury-key")
        .arg(treasury_key)
        .envs(&env)
        .stdin(Stdio::null())
        .stdout(log.try_clone().expect("cannot clone the log handle"))
        .stderr(log)
        .spawn()
        .unwrap_or_else(|err| panic!("cannot start {}: {err}", bin.display()));

    // A configuration refusal is immediate; anything still alive after a couple
    // of seconds has accepted what it was given.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = std::fs::read_to_string(&log_path).unwrap_or_default();
                return Startup::Refused {
                    status: status.to_string(),
                    output,
                };
            }
            Ok(None) => {}
            Err(err) => panic!("cannot poll the gateway process: {err}"),
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Startup::Started;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
