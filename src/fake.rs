//! A Redis that keeps its streams in memory and answers the commands this tool sends as Redis
//! would, with one consumer per group.

use std::collections::{BTreeMap, HashMap};

use crate::db::{Db, Reply, Stop};
use crate::message::{now_ms, order};

type Entry = (String, Vec<String>);

#[derive(Default)]
pub struct Fake {
    streams: BTreeMap<String, Vec<Entry>>,
    groups: HashMap<(String, String), Group>,
    /// What every command fails with, when set.
    failure: Option<Stop>,
}

#[derive(Default)]
struct Group {
    last: String,
    pending: Vec<String>,
}

impl Fake {
    /// A database whose every command fails with `stop`.
    pub fn failing(stop: Stop) -> Self {
        Self {
            failure: Some(stop),
            ..Self::default()
        }
    }

    /// Add an entry with a chosen ID, keeping the stream in order.
    pub fn plant(&mut self, key: &str, id: &str, fields: &[&str]) {
        let stream = self.streams.entry(key.to_string()).or_default();
        stream.push((
            id.to_string(),
            fields.iter().map(|f| f.to_string()).collect(),
        ));
        stream.sort_by_key(|(id, _)| order(id));
    }

    /// The IDs a group was given and has not acked.
    pub fn pending(&self, key: &str, group: &str) -> Vec<String> {
        self.groups[&(key.to_string(), group.to_string())]
            .pending
            .clone()
    }

    fn entry((id, fields): &Entry) -> Reply {
        Reply::List(vec![
            Reply::Text(id.clone()),
            Reply::List(fields.iter().cloned().map(Reply::Text).collect()),
        ])
    }

    /// Whether `id` is at or past a start bound, and past it alone when the bound begins with (.
    fn after(id: &str, bound: &str) -> bool {
        match bound {
            "-" => true,
            "+" => false,
            _ => match bound.strip_prefix('(') {
                Some(b) => order(id) > order(b),
                None => order(id) >= order(bound),
            },
        }
    }
}

/// The word after `name` in `args`.
fn option<'a>(args: &[&'a str], name: &str) -> Option<&'a str> {
    let at = args.iter().position(|a| *a == name)?;
    args.get(at + 1).copied()
}

impl Db for Fake {
    fn call(&mut self, args: &[String]) -> Result<Reply, Stop> {
        if let Some(stop) = &self.failure {
            return Err(stop.clone());
        }
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        let count: usize = option(&a, "COUNT").map_or(usize::MAX, |c| c.parse().unwrap());
        match a[0] {
            "SCAN" => {
                let prefix = option(&a, "MATCH").unwrap_or("*").trim_end_matches('*');
                let keys = self.streams.keys().filter(|k| k.starts_with(prefix));
                Ok(Reply::List(vec![
                    Reply::Text("0".into()),
                    Reply::List(keys.cloned().map(Reply::Text).collect()),
                ]))
            }
            "XADD" => {
                let stream = self.streams.entry(a[1].to_string()).or_default();
                let (mut ms, mut seq) = (now_ms(), 0);
                if let Some((last, _)) = stream.last()
                    && order(last).0 >= ms
                {
                    (ms, seq) = (order(last).0, order(last).1 + 1);
                }
                let star = a.iter().position(|w| *w == "*").unwrap();
                let id = format!("{ms}-{seq}");
                stream.push((
                    id.clone(),
                    a[star + 1..].iter().map(|f| f.to_string()).collect(),
                ));
                Ok(Reply::Text(id))
            }
            "XGROUP" => {
                let key = (a[2].to_string(), a[3].to_string());
                if self.groups.contains_key(&key) {
                    return Err(Stop::Refused(
                        "Redis refused XGROUP: BUSYGROUP Consumer Group name already exists".into(),
                    ));
                }
                self.streams.entry(a[2].to_string()).or_default();
                self.groups.insert(
                    key,
                    Group {
                        last: a[4].to_string(),
                        pending: Vec::new(),
                    },
                );
                Ok(Reply::Text("OK".into()))
            }
            "XACK" => {
                let group = self
                    .groups
                    .get_mut(&(a[1].to_string(), a[2].to_string()))
                    .unwrap();
                let before = group.pending.len();
                group.pending.retain(|id| !a[3..].contains(&id.as_str()));
                Ok(Reply::Int((before - group.pending.len()) as i64))
            }
            "XREAD" | "XREADGROUP" => {
                let at = a.iter().position(|w| *w == "STREAMS").unwrap();
                let (key, mut cursor) = (a[at + 1], a[at + 2].to_string());
                let stream = self.streams.get(key).cloned().unwrap_or_default();
                let mut group = None;
                if a[0] == "XREADGROUP" {
                    let g = self
                        .groups
                        .get_mut(&(key.to_string(), a[2].to_string()))
                        .ok_or_else(|| Stop::Refused("Redis refused XREADGROUP: NOGROUP".into()))?;
                    if cursor != ">" {
                        let pending = stream.iter().filter(|(id, _)| g.pending.contains(id));
                        return Ok(Reply::List(vec![Reply::List(vec![
                            Reply::Text(key.to_string()),
                            Reply::List(pending.map(Self::entry).collect()),
                        ])]));
                    }
                    cursor = g.last.clone();
                    group = Some(g);
                }
                let out: Vec<&Entry> = stream
                    .iter()
                    .filter(|(id, _)| order(id) > order(&cursor))
                    .take(count)
                    .collect();
                if out.is_empty() {
                    return Ok(Reply::Nil);
                }
                if let Some(g) = group {
                    g.last = out.last().unwrap().0.clone();
                    g.pending.extend(out.iter().map(|(id, _)| id.clone()));
                }
                Ok(Reply::List(vec![Reply::List(vec![
                    Reply::Text(key.to_string()),
                    Reply::List(out.into_iter().map(Self::entry).collect()),
                ])]))
            }
            "XRANGE" | "XREVRANGE" => {
                let (first, last) = if a[0] == "XRANGE" {
                    (a[2], a[3])
                } else {
                    (a[3], a[2])
                };
                let stream = self.streams.get(a[1]).cloned().unwrap_or_default();
                let mut out: Vec<&Entry> = stream
                    .iter()
                    .filter(|(id, _)| {
                        Self::after(id, first)
                            && (last == "+" || !Self::after(id, &format!("({last}")))
                    })
                    .collect();
                if a[0] == "XREVRANGE" {
                    out.reverse();
                }
                Ok(Reply::List(
                    out.into_iter().take(count).map(Self::entry).collect(),
                ))
            }
            other => Err(Stop::Refused(format!("Fake has no {other}"))),
        }
    }
}
