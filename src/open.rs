//! The asks that wait on someone (`needs`), and the messages whose named recipients have not all
//! replied (`ack-required`).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crate::cli::OpenArgs;
use crate::db::{Db, Deadline, Io, PAGE, PREFIX, Patient, Stop, cmd, say};
use crate::message::{Message, entries, names, now_ms, streamed};

/// Every channel that has a stream.
fn channels(db: &mut dyn Db) -> Result<Vec<String>, Stop> {
    let mut found = BTreeSet::new();
    let mut cursor = "0".to_string();
    loop {
        let reply = db.call(&cmd![
            "SCAN",
            cursor,
            "MATCH",
            format!("{PREFIX}*"),
            "COUNT",
            PAGE
        ])?;
        let [next, keys] = reply.list() else {
            return Err(Stop::Refused("Unexpected reply to SCAN".into()));
        };
        found.extend(
            keys.list()
                .iter()
                .filter_map(|k| k.text()?.strip_prefix(PREFIX).map(str::to_string)),
        );
        cursor = next.text().unwrap_or("0").to_string();
        if cursor == "0" {
            return Ok(found.into_iter().collect());
        }
    }
}

/// A message, keyed by channel/ID.
struct Item {
    key: String,
    message: Message,
}

/// Every message since `since_ms` in `chans`, oldest first, read `page` entries at a time.
fn read(db: &mut dyn Db, chans: &[String], since_ms: u64, page: usize) -> Result<Vec<Item>, Stop> {
    let mut items = Vec::new();
    for chan in chans {
        let mut first = format!("{since_ms}-0");
        loop {
            let batch = entries(&db.call(&cmd![
                "XRANGE",
                format!("{PREFIX}{chan}"),
                first,
                "+",
                "COUNT",
                page
            ])?);
            let full = batch.len() >= page;
            if let Some(m) = batch.last() {
                first = format!("({}", m.id);
            }
            items.extend(batch.into_iter().map(|message| Item {
                key: format!("{chan}/{}", message.id),
                message,
            }));
            if !full {
                break;
            }
        }
    }
    items.sort_by_key(|i| i.message.ms());
    Ok(items)
}

/// For each channel/ID, the senders that replied to it.
fn answered(items: &[Item]) -> BTreeMap<String, BTreeSet<String>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for i in items {
        let re = i.message.get("re");
        if !re.is_empty() {
            let chan = i.key.split('/').next().unwrap_or_default();
            out.entry(format!("{chan}/{re}"))
                .or_default()
                .insert(i.message.get("from").to_lowercase());
        }
    }
    out
}

/// The open asks, each with who it waits on, and the messages with the names that have not
/// replied, oldest first.
#[allow(clippy::type_complexity)]
fn open_items<'a>(
    items: &'a [Item],
    needs: Option<&str>,
) -> (Vec<(&'a Item, Vec<String>)>, Vec<(&'a Item, Vec<String>)>) {
    let replied = answered(items);
    let none = BTreeSet::new();
    let (mut asks, mut acks) = (Vec::new(), Vec::new());
    for i in items {
        let got = replied.get(&i.key).unwrap_or(&none);
        let who = names(i.message.get("needs"));
        if !who.is_empty()
            && needs.is_none_or(|n| who.contains(&n.to_lowercase()))
            && !got.contains(&i.message.get("from").to_lowercase())
        {
            asks.push((i, who.into_iter().collect()));
        }
        let missing: Vec<String> = names(i.message.get("ack-required"))
            .difference(got)
            .cloned()
            .collect();
        if !missing.is_empty() {
            acks.push((i, missing));
        }
    }
    (asks, acks)
}

fn age(i: &Item, now: u64) -> String {
    let mins = now.saturating_sub(i.message.ms()) / 60_000;
    if mins >= 60 {
        format!("{}h{:02}m", mins / 60, mins % 60)
    } else {
        format!("{mins}m")
    }
}

fn subject(i: &Item) -> &str {
    match i.message.get("subject") {
        "" => "(no subject)",
        s => s,
    }
}

pub fn open(db: &mut dyn Db, a: &OpenArgs, io: &mut Io) -> Result<i32, Stop> {
    if let Some(key) = &a.wait {
        let mut deadline = Deadline::after(a.timeout);
        let mut run = Patient::new(db, Duration::from_secs(15 * 60), deadline.at());
        return wait(&mut run, key, &mut deadline, a.timeout, io);
    }
    let chans = if a.channel.is_empty() {
        channels(db)?
    } else {
        a.channel.clone()
    };
    let now = now_ms();
    let items = read(
        db,
        &chans,
        now.saturating_sub(a.days * 24 * 60 * 60 * 1000),
        PAGE,
    )?;
    let (asks, acks) = open_items(&items, a.needs.as_deref());
    for (i, who) in &asks {
        say!(
            io.out,
            "waits on {} for {}: {}  from {}: {}",
            who.join(", "),
            age(i, now),
            i.key,
            i.message.get("from"),
            subject(i)
        );
    }
    for (i, missing) in &acks {
        say!(
            io.out,
            "no reply from {} after {}: {}  {}",
            missing.join(", "),
            age(i, now),
            i.key,
            subject(i)
        );
    }
    if asks.is_empty() && acks.is_empty() {
        say!(io.out, "nothing open");
        return Ok(0);
    }
    Ok(1)
}

/// Block until everyone in the ack-required of the message `key`, a channel/ID, has replied.
fn wait(
    run: &mut Patient,
    key: &str,
    deadline: &mut Deadline,
    timeout: u64,
    io: &mut Io,
) -> Result<i32, Stop> {
    let Some((chan, id)) = key.split_once('/') else {
        return Err(Stop::Refused(format!(
            "--wait takes CHANNEL/ID, not {key:?}"
        )));
    };
    let stream = format!("{PREFIX}{chan}");
    let found = entries(&run.call(&cmd!["XRANGE", stream, id, id], io.err)?);
    let Some(message) = found.first() else {
        return Err(Stop::Refused(format!("No message {key}")));
    };
    let need = names(message.get("ack-required"));
    let mut replied = BTreeSet::new();
    let mut last = id.to_string();
    let mut shown: Option<Vec<String>> = None;
    loop {
        let Some(block) = deadline.next_read() else {
            let missing: Vec<String> = need.difference(&replied).cloned().collect();
            say!(
                io.out,
                "no reply from {} within {timeout} minutes",
                missing.join(", ")
            );
            return Ok(3);
        };
        let mut read = cmd!["XREAD", "COUNT", PAGE];
        read.extend(block);
        read.extend(cmd!["STREAMS", stream, last]);
        let got = streamed(&run.call(&read, io.err)?);
        for m in &got {
            last = m.id.clone();
            if m.get("re") == id {
                replied.insert(m.get("from").to_lowercase());
            }
        }
        let missing: Vec<String> = need.difference(&replied).cloned().collect();
        if missing.is_empty() {
            say!(io.out, "every name in ack-required has replied to {key}");
            return Ok(0);
        }
        if shown.as_ref() != Some(&missing) {
            say!(io.out, "waiting on {}", missing.join(", "));
            shown = Some(missing);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::Fake;
    use crate::tests::invoke;

    /// The milliseconds of a time of day on 2026-11-10.
    fn at(hhmm: u64) -> u64 {
        1_794_268_800_000 + (hhmm / 100 * 60 + hhmm % 100) * 60_000
    }

    fn planted() -> Fake {
        let mut db = Fake::default();
        let id = |hhmm| format!("{}-0", at(hhmm));
        let (push, key, pause) = (id(1000), id(1100), id(1200));
        let chan = [
            (
                1000,
                vec![
                    "from",
                    "linux",
                    "to",
                    "coordinator",
                    "subject",
                    "push?",
                    "needs",
                    "alex",
                ],
            ),
            (
                1005,
                vec![
                    "from",
                    "coordinator",
                    "to",
                    "linux",
                    "re",
                    &push,
                    "subject",
                    "I asked him",
                ],
            ),
            (
                1100,
                vec![
                    "from",
                    "win",
                    "to",
                    "coordinator",
                    "subject",
                    "key?",
                    "needs",
                    "Alex",
                ],
            ),
            (
                1105,
                vec![
                    "from",
                    "win",
                    "to",
                    "coordinator",
                    "re",
                    &key,
                    "subject",
                    "he said yes",
                ],
            ),
            (
                1200,
                vec![
                    "from",
                    "coordinator",
                    "to",
                    "win, win2, mac",
                    "subject",
                    "pause",
                    "ack-required",
                    "win, win2, mac",
                ],
            ),
            (1201, vec!["from", "win", "re", &pause]),
            // A capitalized name still counts, and a field name is read in any case.
            (1202, vec!["FROM", "Win2", "re", &pause]),
        ];
        for (hhmm, fields) in chan {
            db.plant("chan:chan", &id(hhmm), &fields);
        }
        // A reply in another channel to the same ID does not count.
        db.plant("chan:other", &id(1203), &["from", "mac", "re", &id(1200)]);
        db
    }

    #[test]
    fn open_lists_what_waits_and_what_is_missing() {
        let mut db = planted();
        let chans = channels(&mut db).unwrap();
        let items = read(&mut db, &chans, at(0), PAGE).unwrap();
        assert_eq!(items.len(), 8);
        let (asks, acks) = open_items(&items, None);
        // An ask answered only by someone else stays open, and one its sender answered closes.
        let asks: Vec<(&str, Vec<String>)> = asks
            .iter()
            .map(|(i, w)| (i.key.as_str(), w.clone()))
            .collect();
        assert_eq!(
            asks,
            [(
                format!("chan/{}-0", at(1000)).as_str(),
                vec!["alex".to_string()]
            )]
        );
        // Only the name that has not replied is missing.
        let acks: Vec<(&str, Vec<String>)> = acks
            .iter()
            .map(|(i, m)| (i.key.as_str(), m.clone()))
            .collect();
        assert_eq!(
            acks,
            [(
                format!("chan/{}-0", at(1200)).as_str(),
                vec!["mac".to_string()]
            )]
        );
        // --needs filters the asks by name.
        assert!(open_items(&items, Some("win2")).0.is_empty());
    }

    #[test]
    fn reading_in_pages_finds_the_same_messages() {
        let mut db = planted();
        let chans = channels(&mut db).unwrap();
        assert_eq!(chans, ["chan", "other"]);
        assert_eq!(read(&mut db, &chans, at(0), 2).unwrap().len(), 8);
        assert!(read(&mut db, &chans, at(2359), 2).unwrap().is_empty());
    }

    #[test]
    fn open_wait_exits_0_once_all_replied_and_3_at_its_timeout() {
        let mut db = planted();
        let all = format!("chan/{}-0", at(1201));
        assert_eq!(
            invoke(&mut db, &["open", "--wait", &all, "--timeout", "0"]).0,
            0
        );
        let pause = format!("chan/{}-0", at(1200));
        let (code, out, _) = invoke(&mut db, &["open", "--wait", &pause, "--timeout", "0"]);
        assert_eq!(code, 3);
        assert!(out.contains("no reply from mac"), "{out}");
    }

    #[test]
    fn open_exits_1_when_something_is_open() {
        let mut db = Fake::default();
        assert_eq!(
            invoke(&mut db, &["open"]),
            (0, "nothing open\n".into(), String::new())
        );
        invoke(
            &mut db,
            &[
                "send",
                "a",
                "c",
                "--to",
                "b",
                "--subject",
                "s",
                "--text",
                "x",
                "--needs",
                "n",
            ],
        );
        let (code, out, _) = invoke(&mut db, &["open", "--needs", "n"]);
        assert_eq!(code, 1);
        assert!(out.starts_with("waits on n for 0m: c/"), "{out}");
    }
}
