//! The Redis database the channels live in, and how a run reaches it.

use redis::IntoConnectionInfo;
use std::fmt;
use std::io::Write;
use std::process::Command;
use std::time::{Duration, Instant};

/// The stream of channel `c` is `chan:c`.
pub const PREFIX: &str = "chan:";
/// The environment variable, and the Actions variable, that holds the database's URL.
pub const URL_VAR: &str = "AGENT_MSG_URL";
/// The longest a read holds its connection waiting, in seconds.
pub const HOLD: u64 = 59;
/// The most entries one read returns.
pub const PAGE: usize = 1000;

/// Build a command from its words.
macro_rules! cmd {
    ($($word:expr),* $(,)?) => { Vec::<String>::from([$($word.to_string()),*]) };
}
pub(crate) use cmd;

/// Where a run writes what it prints.
pub struct Io<'a> {
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
}

/// Print a line, ignoring a closed stream.
macro_rules! say {
    ($w:expr, $($arg:tt)*) => {{
        let _ = writeln!($w, $($arg)*);
    }};
}
pub(crate) use say;

/// Why a run stopped short.
#[derive(Clone, Debug, PartialEq)]
pub enum Stop {
    /// The database refused a command, or the run failed before sending one. Exit status 1.
    Failed(String),
    /// Nothing arrived before the timeout. Exit status 3.
    TimedOut(String),
    /// The database stayed out of reach. Exit status 4.
    Unreachable(String),
}

impl Stop {
    pub fn code(&self) -> i32 {
        match self {
            Stop::Failed(_) => 1,
            Stop::TimedOut(_) => 3,
            Stop::Unreachable(_) => 4,
        }
    }
}

impl fmt::Display for Stop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Stop::Failed(m) | Stop::TimedOut(m) | Stop::Unreachable(m) => f.write_str(m),
        }
    }
}

/// A reply, reduced to what the commands read.
#[derive(Clone, Debug, PartialEq)]
pub enum Reply {
    Nil,
    Int(i64),
    Text(String),
    List(Vec<Reply>),
}

impl Reply {
    pub fn text(&self) -> Option<&str> {
        match self {
            Reply::Text(s) => Some(s),
            _ => None,
        }
    }

    /// The items of a list, or none for any other reply.
    pub fn list(&self) -> &[Reply] {
        match self {
            Reply::List(items) => items,
            _ => &[],
        }
    }
}

/// Sends one command and returns its reply.
pub trait Db {
    fn call(&mut self, args: &[String]) -> Result<Reply, Stop>;
}

/// A Redis server, reached over its own protocol, and over TLS for a rediss:// URL.
pub struct Redis {
    client: redis::Client,
    con: Option<redis::Connection>,
}

impl Redis {
    pub fn open(url: &str) -> Result<Self, Stop> {
        let unusable = |e: redis::RedisError| {
            Stop::Failed(format!("{URL_VAR} is not a usable Redis URL: {e}"))
        };
        let info = url.into_connection_info().map_err(unusable)?;
        // Without the client name sent behind AUTH, a server that answers a wrong password and
        // hangs up leaves its answer readable, rather than resetting it away.
        let settings = info.redis_settings().clone().set_skip_set_lib_name();
        let client = redis::Client::open(info.set_redis_settings(settings)).map_err(unusable)?;
        Ok(Self { client, con: None })
    }
}

impl Db for Redis {
    fn call(&mut self, args: &[String]) -> Result<Reply, Stop> {
        if self.con.is_none() {
            let con = self
                .client
                .get_connection_with_timeout(Duration::from_secs(15))
                .map_err(connecting)?;
            // A read blocks for up to HOLD seconds, so the socket waits longer than that.
            con.set_read_timeout(Some(Duration::from_secs(HOLD + 16)))
                .map_err(connecting)?;
            self.con = Some(con);
        }
        let con = self.con.as_mut().expect("connected above");
        let mut command = redis::cmd(&args[0]);
        for arg in &args[1..] {
            command.arg(arg);
        }
        command.query::<redis::Value>(con).map(reply).map_err(|e| {
            let stop = failure(&args[0], e);
            // The next command connects afresh.
            if matches!(stop, Stop::Unreachable(_)) {
                self.con = None;
            }
            stop
        })
    }
}

/// A failed command as unreachable when the connection failed, and as refused otherwise.
fn failure(command: &str, e: redis::RedisError) -> Stop {
    if unreachable(&e) {
        Stop::Unreachable(format!("Redis failed {command}: {e}"))
    } else {
        Stop::Failed(format!("Redis refused {command}: {e}"))
    }
}

/// A failed connection, sorted as [`failure`] sorts a failed command.
fn connecting(e: redis::RedisError) -> Stop {
    if unreachable(&e) {
        Stop::Unreachable(format!("Could not connect to Redis: {e}"))
    } else {
        Stop::Failed(format!("Redis refused the connection: {e}"))
    }
}

/// Whether `e` says the server could not be reached, rather than that it refused.
fn unreachable(e: &redis::RedisError) -> bool {
    e.is_io_error() || e.is_timeout() || e.is_connection_dropped() || e.is_connection_refusal()
}

fn reply(value: redis::Value) -> Reply {
    use redis::Value as V;
    match value {
        V::Nil => Reply::Nil,
        V::Int(i) => Reply::Int(i),
        V::BulkString(bytes) => Reply::Text(String::from_utf8_lossy(&bytes).into_owned()),
        V::SimpleString(s) => Reply::Text(s),
        V::Okay => Reply::Text("OK".into()),
        V::Array(items) | V::Set(items) => Reply::List(items.into_iter().map(reply).collect()),
        V::Map(pairs) => Reply::List(
            pairs
                .into_iter()
                .flat_map(|(k, v)| [reply(k), reply(v)])
                .collect(),
        ),
        other => Reply::Text(format!("{other:?}")),
    }
}

/// The database for this run, and the repository whose variable named it.
pub fn connect(repo: Option<&str>) -> Result<(Redis, Option<String>), Stop> {
    let (value, repo) = setting(URL_VAR, repo)?;
    Ok((Redis::open(&redis_url(value.trim())?)?, repo))
}

/// The value of the setting `name`, and the repository it came from. It is the environment
/// variable `name`, or else the Actions variable `name` of `repo`, by default the repository gh
/// resolves here.
pub fn setting(name: &str, repo: Option<&str>) -> Result<(String, Option<String>), Stop> {
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => Ok((v, repo.map(str::to_string))),
        _ => {
            let repo = resolve(name, repo)?;
            Ok((variable(name, &repo)?, Some(repo)))
        }
    }
}

/// `repo`, or the repository gh resolves for the working directory.
fn resolve(name: &str, repo: Option<&str>) -> Result<String, Stop> {
    match repo {
        Some(r) => Ok(r.to_string()),
        None => gh(&[
            "repo",
            "view",
            "--json",
            "nameWithOwner",
            "--jq",
            ".nameWithOwner",
        ])
        .map_err(|e| {
            Stop::Failed(format!(
                "No repository to read {name} from, so pass -R OWNER/REPO: {e}"
            ))
        }),
    }
}

fn variable(name: &str, repo: &str) -> Result<String, Stop> {
    gh(&[
        "api",
        &format!("repos/{repo}/actions/variables/{name}"),
        "--jq",
        ".value",
    ])
    .map_err(|e| {
        Stop::Failed(format!(
            "Reading the Actions variable {name} of {repo} failed: {e}"
        ))
    })
}

fn gh(args: &[&str]) -> Result<String, String> {
    let out = Command::new("gh")
        .args(args)
        .output()
        .map_err(|e| format!("gh: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The redis:// or rediss:// URL that `value` names. An Upstash REST URL, which carries its token
/// as _token, becomes the TLS URL of the same database, which Upstash serves on port 6379 with
/// the token as the password of its default user.
pub fn redis_url(value: &str) -> Result<String, Stop> {
    if value.starts_with("redis://") || value.starts_with("rediss://") {
        return Ok(value.to_string());
    }
    let host = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
        .and_then(|rest| rest.split(['/', '?', ':']).next())
        .filter(|h| !h.is_empty());
    match (host, token(value)) {
        (Some(host), Some(token)) => Ok(format!("rediss://default:{}@{host}:6379", encode(token))),
        _ => Err(Stop::Failed(format!(
            "{URL_VAR} is neither a redis:// URL nor an Upstash REST URL with a _token"
        ))),
    }
}

/// The token an Upstash REST URL carries as its _token parameter, as written.
pub fn token(url: &str) -> Option<&str> {
    url.split_once("_token=")
        .map(|(_, t)| t.split('&').next().unwrap_or_default())
        .filter(|t| !t.is_empty())
}

/// `s` with every byte but the unreserved ones percent-encoded, as a URL's password or path
/// segment must be.
pub fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// When a wait ends, and how long each of its reads may block.
pub struct Deadline {
    at: Instant,
    polled: bool,
}

impl Deadline {
    pub fn after(minutes: u64) -> Self {
        Self {
            at: Instant::now() + Duration::from_secs(minutes * 60),
            polled: false,
        }
    }

    pub fn at(&self) -> Instant {
        self.at
    }

    /// The BLOCK of the next read, at most HOLD seconds and never past the deadline. Past it, a
    /// first read that does not block, so a zero timeout still looks once, and None after that.
    pub fn next_read(&mut self) -> Option<Vec<String>> {
        let left = self.at.saturating_duration_since(Instant::now()).as_secs();
        let first = !std::mem::replace(&mut self.polled, true);
        match left {
            0 if first => Some(Vec::new()),
            0 => None,
            _ => Some(cmd!["BLOCK", left.min(HOLD) * 1000]),
        }
    }
}

/// Sends commands, riding out an outage for `offline` with a pause that doubles from 15 seconds
/// up to 5 minutes. An outage that outlasts it, or is still on at `deadline`, stops the run as
/// unreachable, since a wait out of reach cannot say that nothing arrived.
pub struct Patient<'a> {
    db: &'a mut dyn Db,
    offline: Duration,
    deadline: Instant,
    /// Waits out a pause, on the system clock outside tests.
    sleep: Box<dyn FnMut(Duration) + 'a>,
}

impl<'a> Patient<'a> {
    pub fn new(db: &'a mut dyn Db, offline: Duration, deadline: Instant) -> Self {
        Self {
            db,
            offline,
            deadline,
            sleep: Box::new(std::thread::sleep),
        }
    }

    pub fn call(&mut self, args: &[String], err: &mut dyn Write) -> Result<Reply, Stop> {
        let mut since: Option<Instant> = None;
        let mut pause = Duration::from_secs(15);
        loop {
            match self.db.call(args) {
                Err(Stop::Unreachable(e)) => {
                    let now = Instant::now();
                    let start = *since.get_or_insert_with(|| {
                        say!(
                            err,
                            "Redis failed, retrying for up to {}m: {e}",
                            self.offline.as_secs() / 60
                        );
                        now
                    });
                    if now - start >= self.offline || now + pause > self.deadline {
                        return Err(Stop::Unreachable(format!("Redis stayed out of reach: {e}")));
                    }
                    (self.sleep)(pause);
                    pause = (pause * 2).min(Duration::from_secs(300));
                }
                other => return other,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deadline_blocks_until_it_passes_then_reads_once() {
        let mut later = Deadline::after(10);
        assert_eq!(later.next_read().unwrap(), ["BLOCK", "59000"]);
        let mut past = Deadline::after(0);
        assert_eq!(past.next_read().unwrap(), Vec::<String>::new());
        assert_eq!(past.next_read(), None);
    }

    #[test]
    fn redis_url_takes_a_redis_url_or_an_upstash_rest_url() {
        assert_eq!(
            redis_url("rediss://default:pw@db.example:6379").unwrap(),
            "rediss://default:pw@db.example:6379"
        );
        // The path of a REST command and other parameters are dropped, and the token is encoded.
        assert_eq!(
            redis_url("https://db.upstash.io/xadd/s/*/push?_token=Ab+c/d=&x=1").unwrap(),
            "rediss://default:Ab%2Bc%2Fd%3D@db.upstash.io:6379"
        );
        for bad in [
            "",
            "https://db.upstash.io/",
            "ftp://db?_token=t",
            "https://?_token=t",
        ] {
            assert!(matches!(redis_url(bad), Err(Stop::Failed(_))), "{bad}");
        }
    }

    /// A database that fails `fails` times, then answers.
    struct Flaky {
        fails: u32,
    }

    impl Db for Flaky {
        fn call(&mut self, _: &[String]) -> Result<Reply, Stop> {
            if self.fails == 0 {
                return Ok(Reply::Text("OK".into()));
            }
            self.fails -= 1;
            Err(Stop::Unreachable("connection refused".into()))
        }
    }

    #[test]
    fn an_outage_is_ridden_out_with_a_doubling_pause() {
        let mut db = Flaky { fails: 3 };
        let mut pauses = Vec::new();
        let mut err = Vec::new();
        let reply = {
            let hour = Instant::now() + Duration::from_secs(3600);
            let mut run = Patient::new(&mut db, Duration::from_secs(15 * 60), hour);
            run.sleep = Box::new(|d: Duration| pauses.push(d.as_secs()));
            run.call(&cmd!["PING"], &mut err)
        };
        assert_eq!(reply, Ok(Reply::Text("OK".into())));
        assert_eq!(pauses, [15, 30, 60]);
        // The outage is reported once.
        assert_eq!(String::from_utf8(err).unwrap().lines().count(), 1);
    }

    #[test]
    fn an_outage_past_offline_or_the_deadline_is_unreachable() {
        for (offline, deadline) in [(0, 3600), (900, 10)] {
            let mut db = Flaky { fails: 100 };
            let deadline = Instant::now() + Duration::from_secs(deadline);
            let mut run = Patient::new(&mut db, Duration::from_secs(offline), deadline);
            run.sleep = Box::new(|_| {});
            let reply = run.call(&cmd!["PING"], &mut Vec::new());
            assert!(matches!(reply, Err(Stop::Unreachable(_))));
        }
    }
}
