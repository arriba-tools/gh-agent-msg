//! Messages as stream entries, and the commands that send and list them.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cli::{LogArgs, SendArgs};
use crate::db::{Db, Io, PREFIX, Reply, Stop, cmd, say};

/// Entries a channel keeps, roughly.
const KEEP: u64 = 100_000;

/// One entry of a channel's stream.
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub id: String,
    /// Its fields, with names lowercased.
    pub fields: BTreeMap<String, String>,
}

impl Message {
    /// The field `key`, or empty when the entry has none.
    pub fn get(&self, key: &str) -> &str {
        self.fields.get(key).map(String::as_str).unwrap_or_default()
    }

    /// The milliseconds since 1970 at which the server gave its ID.
    pub fn ms(&self) -> u64 {
        order(&self.id).0
    }
}

/// The entries of an XRANGE or XREVRANGE reply.
pub fn entries(reply: &Reply) -> Vec<Message> {
    reply
        .list()
        .iter()
        .filter_map(|entry| {
            let [id, flat] = entry.list() else {
                return None;
            };
            let (pairs, _) = flat.list().as_chunks::<2>();
            let fields = pairs
                .iter()
                .filter_map(|[key, value]| {
                    let key = key.text()?.to_lowercase();
                    Some((key, value.text().unwrap_or_default().to_string()))
                })
                .collect();
            Some(Message {
                id: id.text()?.to_string(),
                fields,
            })
        })
        .collect()
}

/// The entries of an XREAD or XREADGROUP reply, from every stream in it.
pub fn streamed(reply: &Reply) -> Vec<Message> {
    reply
        .list()
        .iter()
        .flat_map(|stream| entries(stream.list().get(1).unwrap_or(&Reply::Nil)))
        .collect()
}

/// An entry ID as numbers, which sort as the server orders the entries.
pub fn order(id: &str) -> (u64, u64) {
    let (ms, seq) = id.split_once('-').unwrap_or((id, "0"));
    (ms.parse().unwrap_or(0), seq.parse().unwrap_or(0))
}

/// Accept only an entry ID, such as 1794536187224-0.
pub fn entry_id(value: &str) -> Result<String, String> {
    match value.split_once('-') {
        Some((ms, seq))
            if !ms.is_empty()
                && !seq.is_empty()
                && ms.bytes().all(|b| b.is_ascii_digit())
                && seq.bytes().all(|b| b.is_ascii_digit()) =>
        {
            Ok(value.to_string())
        }
        _ => Err(format!("not an entry ID: {value:?}")),
    }
}

/// A comma-separated field as a set of lowercased names.
pub fn names(value: &str) -> BTreeSet<String> {
    value
        .split(',')
        .map(|n| n.trim().to_lowercase())
        .filter(|n| !n.is_empty())
        .collect()
}

/// Whether `m` is for `name`, sent to it or to all, and not by it.
pub fn mine(m: &Message, name: &str) -> bool {
    let who = name.to_lowercase();
    let to = names(m.get("to"));
    m.get("from").to_lowercase() != who && (to.contains(&who) || to.contains("all"))
}

/// A message as a header, a blank line, then its text.
pub fn show(m: &Message) -> String {
    let mut lines = vec![
        format!("--- {}", m.id),
        format!("from: {}", m.get("from")),
        format!("to: {}", m.get("to")),
    ];
    for key in ["re", "needs", "ack-required"] {
        if !m.get(key).is_empty() {
            lines.push(format!("{key}: {}", m.get(key)));
        }
    }
    lines.extend([
        format!("subject: {}", m.get("subject")),
        String::new(),
        m.get("body").to_string(),
    ]);
    lines.join("\n") + "\n"
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// The text of a --file, or else of a --text.
pub fn text(file: Option<&Path>, text: Option<&str>) -> Result<String, Stop> {
    match file {
        Some(path) => std::fs::read_to_string(path)
            .map_err(|e| Stop::Failed(format!("Failed to read {}: {e}", path.display()))),
        None => Ok(text.unwrap_or_default().to_string()),
    }
}

pub fn send(db: &mut dyn Db, a: &SendArgs, io: &mut Io) -> Result<i32, Stop> {
    let body = text(a.file.as_deref(), a.text.as_deref())?;
    let id = db.call(&cmd![
        "XADD",
        format!("{PREFIX}{}", a.channel),
        "MAXLEN",
        "~",
        KEEP,
        "*",
        "from",
        a.name,
        "to",
        a.to,
        "re",
        a.re.as_deref().unwrap_or_default(),
        "subject",
        a.subject,
        "needs",
        a.needs.as_deref().unwrap_or_default(),
        "ack-required",
        a.ack_required.as_deref().unwrap_or_default(),
        "body",
        body,
    ])?;
    say!(io.out, "{}", id.text().unwrap_or_default());
    Ok(0)
}

pub fn log(db: &mut dyn Db, a: &LogArgs, io: &mut Io) -> Result<i32, Stop> {
    let stream = format!("{PREFIX}{}", a.channel);
    for m in entries(&db.call(&cmd!["XREVRANGE", stream, "+", "-", "COUNT", a.count])?) {
        say!(
            io.out,
            "{} {} to {}: {}",
            m.id,
            m.get("from"),
            m.get("to"),
            m.get("subject")
        );
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::Fake;
    use crate::tests::invoke;

    #[test]
    fn send_prints_the_new_id_and_keeps_utf8_text() {
        let path = std::env::temp_dir().join(format!("gh-agent-msg-{}.md", std::process::id()));
        std::fs::write(&path, "σ ≤ 1e-30\n").unwrap();
        let mut db = Fake::default();
        let (code, out, _) = invoke(
            &mut db,
            &[
                "send",
                "coordinator",
                "c",
                "--to",
                "mac, win2",
                "--subject",
                "#1",
                "--file",
                path.to_str().unwrap(),
                "--needs",
                "alex",
            ],
        );
        std::fs::remove_file(&path).unwrap();
        let id = out.trim();
        assert_eq!(code, 0);
        assert!(entry_id(id).is_ok(), "{out}");
        let m = &entries(&db.call(&cmd!["XRANGE", "chan:c", id, id]).unwrap())[0];
        assert_eq!(m.get("body"), "σ ≤ 1e-30\n");
        assert_eq!(m.get("needs"), "alex");
        // Fields that do not apply are empty, so every entry has the same ones.
        assert_eq!(m.get("re"), "");
        assert!(m.fields.contains_key("ack-required"));
    }

    #[test]
    fn send_needs_a_text_and_takes_only_an_entry_id_in_re() {
        let mut db = Fake::default();
        let args = ["send", "a", "c", "--to", "b", "--subject", "s"];
        assert_eq!(invoke(&mut db, &args).0, 2);
        let mut with_re = args.to_vec();
        with_re.extend(["--text", "x", "--re", "nonsense"]);
        assert_eq!(invoke(&mut db, &with_re).0, 2);
    }

    #[test]
    fn log_prints_the_latest_messages_newest_first() {
        let mut db = Fake::default();
        for subject in ["first", "second", "third"] {
            invoke(
                &mut db,
                &[
                    "send",
                    "a",
                    "c",
                    "--to",
                    "b",
                    "--subject",
                    subject,
                    "--text",
                    "x",
                ],
            );
        }
        let (code, out, _) = invoke(&mut db, &["log", "c", "--count", "2"]);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(code, 0);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with(" a to b: third") && lines[1].ends_with(" a to b: second"));
    }

    #[test]
    fn mine_is_to_the_name_or_all_and_not_from_it() {
        let m = |from: &str, to: &str| Message {
            id: "1-0".into(),
            fields: [("from".into(), from.into()), ("to".into(), to.into())].into(),
        };
        assert!(mine(&m("coordinator", "mac, win2"), "win2"));
        assert!(mine(&m("coordinator", "ALL"), "mac"));
        assert!(!mine(&m("mac", "all"), "mac"));
        // A name that only starts the same is another name.
        assert!(!mine(&m("coordinator", "win2"), "win"));
    }

    #[test]
    fn entry_id_takes_milliseconds_and_a_sequence() {
        assert!(entry_id("1794536187224-0").is_ok());
        for bad in ["", "1", "-1", "1-", "a-0", "1-0-0"] {
            assert!(entry_id(bad).is_err(), "{bad}");
        }
    }
}
