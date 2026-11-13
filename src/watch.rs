//! Waiting on a channel for the messages to a name, or for the replies to messages.

use std::collections::BTreeSet;
use std::time::Duration;

use crate::cli::WatchArgs;
use crate::db::{Db, Deadline, Io, PAGE, PREFIX, Patient, Stop, cmd, say};
use crate::message::{Message, mine, now_ms, order, show, streamed};

/// How far back a name's first watch looks, in milliseconds.
const LOOKBACK: u64 = 24 * 60 * 60 * 1000;

/// The BLOCK of the next read, or the end of the wait once the deadline has passed.
fn hold(deadline: &mut Deadline, timeout: u64) -> Result<Vec<String>, Stop> {
    deadline
        .next_read()
        .ok_or_else(|| Stop::TimedOut(format!("Nothing new in {timeout}m")))
}

pub fn watch(db: &mut dyn Db, a: &WatchArgs, repo: Option<&str>, io: &mut Io) -> Result<i32, Stop> {
    let mut deadline = Deadline::after(a.timeout);
    let mut run = Patient::new(db, Duration::from_secs(a.offline * 60), deadline.at());
    let stream = format!("{PREFIX}{}", a.channel);
    if !a.re.is_empty() {
        return replies(&mut run, &stream, a, &mut deadline, io);
    }

    // A name's group starts a day back, so a name that joins late still sees what was sent to it.
    let start = format!("{}-0", now_ms().saturating_sub(LOOKBACK));
    match run.call(
        &cmd!["XGROUP", "CREATE", stream, a.name, start, "MKSTREAM"],
        io.err,
    ) {
        Err(Stop::Refused(e)) if e.contains("BUSYGROUP") => {}
        other => {
            other?;
        }
    }
    if !a.ack.is_empty() {
        ack(&mut run, &stream, &a.name, a.ack.iter(), io)?;
    }

    // ID 0 reads what this name was given and has not acked, and > what nobody was given yet.
    let mut cursor = "0";
    loop {
        let mut read = cmd!["XREADGROUP", "GROUP", a.name, a.name, "COUNT", PAGE];
        if cursor == ">" {
            read.extend(hold(&mut deadline, a.timeout)?);
        }
        read.extend(cmd!["STREAMS", stream, cursor]);
        let got = streamed(&run.call(&read, io.err)?);
        cursor = ">";
        let (found, others): (Vec<Message>, Vec<Message>) =
            got.into_iter().partition(|m| mine(m, &a.name));
        // Messages for other names are acked at once, so only this name's stay pending.
        if !others.is_empty() {
            ack(&mut run, &stream, &a.name, others.iter().map(|m| &m.id), io)?;
        }
        if !found.is_empty() {
            for m in &found {
                say!(io.out, "{}", show(m));
            }
            let repo = repo.map(|r| format!(" -R {r}")).unwrap_or_default();
            let acks: String = found.iter().map(|m| format!(" --ack {}", m.id)).collect();
            say!(io.out, "Once these are handled, restart with:");
            say!(
                io.out,
                "  gh agent-msg watch {} {}{repo}{acks}",
                a.name,
                a.channel
            );
            return Ok(0);
        }
    }
}

fn ack<'a>(
    run: &mut Patient,
    stream: &str,
    name: &str,
    ids: impl Iterator<Item = &'a String>,
    io: &mut Io,
) -> Result<(), Stop> {
    let mut command = cmd!["XACK", stream, name];
    command.extend(ids.cloned());
    run.call(&command, io.err).map(drop)
}

/// Wait for the replies to the messages that --re names, reading on from the earliest of them,
/// since a reply comes after the message it answers.
fn replies(
    run: &mut Patient,
    stream: &str,
    a: &WatchArgs,
    deadline: &mut Deadline,
    io: &mut Io,
) -> Result<i32, Stop> {
    let want: BTreeSet<&str> = a.re.iter().map(String::as_str).collect();
    let mut last =
        a.re.iter()
            .min_by_key(|id| order(id))
            .cloned()
            .unwrap_or_default();
    loop {
        let mut read = cmd!["XREAD", "COUNT", PAGE];
        read.extend(hold(deadline, a.timeout)?);
        read.extend(cmd!["STREAMS", stream, last]);
        let got = streamed(&run.call(&read, io.err)?);
        if let Some(m) = got.last() {
            last = m.id.clone();
        }
        let found: Vec<&Message> = got
            .iter()
            .filter(|m| {
                want.contains(m.get("re")) && m.get("from").to_lowercase() != a.name.to_lowercase()
            })
            .collect();
        if !found.is_empty() {
            for m in found {
                say!(io.out, "{}", show(m));
            }
            return Ok(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::fake::Fake;
    use crate::tests::invoke;

    fn send(db: &mut Fake, from: &str, to: &str, subject: &str, extra: &[&str]) -> String {
        let mut args = vec![
            "send",
            from,
            "c",
            "--to",
            to,
            "--subject",
            subject,
            "--text",
            "x",
        ];
        args.extend(extra);
        invoke(db, &args).1.trim().to_string()
    }

    #[test]
    fn watch_prints_a_message_to_the_name_and_acks_the_rest() {
        let mut db = Fake::default();
        let first = send(&mut db, "coordinator", "mac, win2", "#1", &[]);
        send(&mut db, "coordinator", "win", "for win", &[]);
        send(&mut db, "mac", "all", "own", &[]);
        let (code, out, _) = invoke(
            &mut db,
            &["watch", "mac", "c", "--timeout", "0", "-R", "o/r"],
        );
        assert_eq!(code, 0);
        assert!(out.contains("subject: #1"), "{out}");
        assert!(!out.contains("for win") && !out.contains("own"), "{out}");
        assert!(out.contains(&format!("gh agent-msg watch mac c -R o/r --ack {first}")));
        // What passed the name by is acked, so only its own message stays pending.
        assert_eq!(db.pending("chan:c", "mac"), [first]);
    }

    #[test]
    fn a_message_comes_back_until_acked() {
        let mut db = Fake::default();
        let id = send(&mut db, "coordinator", "mac", "keep", &[]);
        invoke(&mut db, &["watch", "mac", "c", "--timeout", "0"]);
        let (code, out, _) = invoke(&mut db, &["watch", "mac", "c", "--timeout", "0"]);
        assert_eq!(code, 0);
        assert!(out.contains("subject: keep"));
        let (code, _, err) = invoke(
            &mut db,
            &["watch", "mac", "c", "--ack", &id, "--timeout", "0"],
        );
        assert_eq!(code, 3);
        assert_eq!(err.trim(), "Nothing new in 0m");
    }

    #[test]
    fn a_late_name_sees_the_last_day_and_all_is_everyone() {
        let mut db = Fake::default();
        let old = crate::message::now_ms() - 2 * super::LOOKBACK;
        db.plant(
            "chan:c",
            &format!("{old}-0"),
            &["from", "a", "to", "late", "subject", "old"],
        );
        send(&mut db, "coordinator", "late", "recent", &[]);
        send(&mut db, "coordinator", "all", "to all", &[]);
        let (code, out, _) = invoke(&mut db, &["watch", "late", "c", "--timeout", "0"]);
        assert_eq!(code, 0);
        assert!(
            out.contains("subject: recent") && out.contains("subject: to all"),
            "{out}"
        );
        assert!(!out.contains("subject: old"), "{out}");
    }

    #[test]
    fn watch_re_prints_the_replies_to_that_message_alone() {
        let mut db = Fake::default();
        let q = send(&mut db, "coordinator", "mac", "question", &[]);
        send(&mut db, "mac", "coordinator", "noise", &[]);
        send(&mut db, "mac", "coordinator", "answer", &["--re", &q]);
        let (code, out, _) = invoke(
            &mut db,
            &["watch", "coordinator", "c", "--re", &q, "--timeout", "0"],
        );
        assert_eq!(code, 0);
        assert!(
            out.contains("subject: answer") && !out.contains("noise"),
            "{out}"
        );
        let (code, _, _) = invoke(
            &mut db,
            &["watch", "mac", "c", "--re", &q, "--timeout", "0"],
        );
        // The sender's own reply does not count for it.
        assert_eq!(code, 3);
    }

    #[test]
    fn an_outage_exits_4_and_a_refusal_exits_1() {
        let mut down = Fake::failing(crate::db::Stop::Unreachable("connection refused".into()));
        let (code, _, err) = invoke(&mut down, &["watch", "mac", "c", "--offline", "0"]);
        assert_eq!(code, 4);
        assert!(err.contains("out of reach"), "{err}");
        let mut wrong = Fake::failing(crate::db::Stop::Refused(
            "Redis refused XGROUP: WRONGPASS invalid username-password pair".into(),
        ));
        let (code, _, err) = invoke(&mut wrong, &["watch", "mac", "c"]);
        assert_eq!(code, 1);
        assert!(err.contains("WRONGPASS"), "{err}");
    }
}
