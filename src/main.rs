mod cli;
mod db;
#[cfg(test)]
mod fake;
mod message;
mod open;
mod watch;

use clap::Parser;

use cli::{Cli, Command};
use db::{Db, Io, Stop};

fn main() {
    // Exit quietly when stdout closes early, as when piped into head, instead of panicking.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let cli = Cli::parse();
    let (stdout, stderr) = (std::io::stdout(), std::io::stderr());
    let (mut out, mut err) = (stdout.lock(), stderr.lock());
    let mut io = Io {
        out: &mut out,
        err: &mut err,
    };
    let code = match db::connect(cli.repo.as_deref()) {
        Ok((mut db, repo)) => finish(run(&mut db, &cli, repo.as_deref(), &mut io), &mut io),
        Err(stop) => finish(Err(stop), &mut io),
    };
    let _ = io.out.flush();
    std::process::exit(code);
}

/// Run the command against `db`, returning the exit status it ends with. `repo` is the
/// repository whose variable named the database, which a printed restart command names.
pub fn run(db: &mut dyn Db, cli: &Cli, repo: Option<&str>, io: &mut Io) -> Result<i32, Stop> {
    match &cli.command {
        Command::Send(a) => message::send(db, a, io),
        Command::Watch(a) => watch::watch(db, a, repo, io),
        Command::Open(a) => open::open(db, a, io),
        Command::Log(a) => message::log(db, a, io),
    }
}

/// The exit status of a run, saying on stderr why one stopped.
fn finish(result: Result<i32, Stop>, io: &mut Io) -> i32 {
    match result {
        Ok(code) => code,
        Err(stop) => {
            let _ = writeln!(io.err, "{stop}");
            stop.code()
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Run a command line against `db`, returning its exit status, output and errors.
    pub fn invoke(db: &mut dyn Db, args: &[&str]) -> (i32, String, String) {
        let cli = match Cli::try_parse_from(
            std::iter::once("gh-agent-msg").chain(args.iter().copied()),
        ) {
            Ok(cli) => cli,
            Err(e) => return (e.exit_code(), String::new(), e.to_string()),
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = {
            let mut io = Io {
                out: &mut out,
                err: &mut err,
            };
            let result = run(db, &cli, cli.repo.as_deref(), &mut io);
            finish(result, &mut io)
        };
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }
}
