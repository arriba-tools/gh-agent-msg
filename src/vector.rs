//! The vector index that `remember` stores notes in and `recall` searches, one namespace per
//! channel.

use serde_json::{Value, json};
use std::time::Duration;

use crate::cli::{Cli, Command, RecallArgs, RememberArgs};
use crate::db::{self, Io, Stop, say};
use crate::message::{self, now_ms};

/// The environment variable, and the Actions variable, that holds the index's REST URL.
pub const VECTOR_VAR: &str = "AGENT_VECTOR_URL";

/// Sends one request to the index and returns its result.
pub trait Index {
    fn post(&mut self, path: &str, body: &Value) -> Result<Value, Stop>;
}

/// An Upstash Vector index with a hosted embedding model, reached over its REST API.
pub struct Upstash {
    base: String,
    token: String,
    agent: ureq::Agent,
}

impl Upstash {
    pub fn open(value: &str) -> Result<Self, Stop> {
        let (base, token) = rest_url(value)?;
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            // An error status still carries the index's reason in its body.
            .http_status_as_error(false)
            .build()
            .into();
        Ok(Self { base, token, agent })
    }
}

impl Index for Upstash {
    fn post(&mut self, path: &str, body: &Value) -> Result<Value, Stop> {
        let mut response = self
            .agent
            .post(format!("{}/{path}", self.base))
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Type", "application/json")
            .send(body.to_string())
            .map_err(|e| failure(path, e))?;
        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|e| failure(path, e))?;
        answer(path, status, &text)
    }
}

/// A failed request as unreachable when the index could not be reached, and as failed otherwise.
fn failure(path: &str, e: ureq::Error) -> Stop {
    use ureq::Error as E;
    let message = format!("Upstash Vector failed {path}: {e}");
    match e {
        E::Io(_) | E::Timeout(_) | E::HostNotFound | E::ConnectionFailed => {
            Stop::Unreachable(message)
        }
        _ => Stop::Failed(message),
    }
}

/// The result of an answer from the index, or the reason it gives for refusing.
fn answer(path: &str, status: u16, text: &str) -> Result<Value, Stop> {
    let refused = |why: &str| Stop::Failed(format!("Upstash Vector refused {path}: {why}"));
    let value: Value = serde_json::from_str(text)
        .map_err(|_| refused(&format!("HTTP {status} {}", text.trim())))?;
    match value.get("error").and_then(Value::as_str) {
        Some(why) => Err(refused(why)),
        None if status >= 400 => Err(refused(&format!("HTTP {status}"))),
        None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
    }
}

/// The base URL and the token of `value`, an index's REST URL that carries its token as _token.
fn rest_url(value: &str) -> Result<(String, String), Stop> {
    let host = value
        .strip_prefix("https://")
        .and_then(|rest| rest.split(['/', '?']).next())
        .filter(|h| !h.is_empty());
    match (host, db::token(value)) {
        (Some(host), Some(token)) => Ok((format!("https://{host}"), token.to_string())),
        _ => Err(Stop::Failed(format!(
            "{VECTOR_VAR} is not an https:// Upstash Vector REST URL with a _token"
        ))),
    }
}

/// The index for this run, from the setting AGENT_VECTOR_URL.
pub fn connect(repo: Option<&str>) -> Result<Upstash, Stop> {
    let (value, _) = db::setting(VECTOR_VAR, repo)?;
    Upstash::open(value.trim())
}

/// Run a command that reads or writes the index.
pub fn run(index: &mut dyn Index, cli: &Cli, io: &mut Io) -> Result<i32, Stop> {
    match &cli.command {
        Command::Remember(a) => remember(index, a, io),
        Command::Recall(a) => recall(index, a, io),
        _ => unreachable!("the other commands run against Redis"),
    }
}

/// Store a note in the channel's namespace, and print its ID.
pub fn remember(index: &mut dyn Index, a: &RememberArgs, io: &mut Io) -> Result<i32, Stop> {
    let note = message::text(a.file.as_deref(), a.text.as_deref())?;
    if note.trim().is_empty() {
        return Err(Stop::Failed("The note is empty".into()));
    }
    let at = now_ms();
    // The process ID keeps apart the notes one name stores in the same millisecond.
    let id = format!("{at}-{}-{}", a.name, std::process::id());
    let body = json!({ "id": id, "data": note, "metadata": { "from": a.name, "at": at } });
    index.post(&format!("upsert-data/{}", db::encode(&a.channel)), &body)?;
    say!(io.out, "{id}");
    Ok(0)
}

/// Print the channel's notes that best match the query, each as a header, a blank line, then its
/// text.
pub fn recall(index: &mut dyn Index, a: &RecallArgs, io: &mut Io) -> Result<i32, Stop> {
    let body = json!({
        "data": a.query,
        "topK": a.count,
        "includeMetadata": true,
        "includeData": true,
        // BM25 weighs each word by its rarity, so a figure outweighs words such as "at" and "D".
        "weightingStrategy": "IDF",
    });
    let result = index.post(&format!("query-data/{}", db::encode(&a.channel)), &body)?;
    let hits = result.as_array().map(Vec::as_slice).unwrap_or_default();
    if hits.is_empty() {
        say!(io.err, "No note in {} matches", a.channel);
    }
    for hit in hits {
        let text = |key: &str| hit.get(key).and_then(Value::as_str).unwrap_or_default();
        let from = hit
            .pointer("/metadata/from")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let score = hit.get("score").and_then(Value::as_f64).unwrap_or_default();
        say!(
            io.out,
            "--- {}\nfrom: {from}\nscore: {score:.4}\n\n{}\n",
            text("id"),
            text("data").trim_end()
        );
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::invoke_index;

    /// An index that records each request and answers every one with `reply`.
    struct Recorder {
        requests: Vec<(String, Value)>,
        reply: Result<Value, Stop>,
    }

    impl Recorder {
        fn answering(reply: Result<Value, Stop>) -> Self {
            Self {
                requests: Vec::new(),
                reply,
            }
        }
    }

    impl Index for Recorder {
        fn post(&mut self, path: &str, body: &Value) -> Result<Value, Stop> {
            self.requests.push((path.to_string(), body.clone()));
            self.reply.clone()
        }
    }

    #[test]
    fn remember_stores_the_note_in_the_channel_namespace_and_prints_its_id() {
        let mut index = Recorder::answering(Ok(json!("Success")));
        let args = ["remember", "win2", "a/b", "--text", "fits cross at D 236.6"];
        let (code, out, _) = invoke_index(&mut index, &args);
        let (path, body) = &index.requests[0];
        assert_eq!(code, 0);
        // A channel is one path segment, however it is spelled.
        assert_eq!(path, "upsert-data/a%2Fb");
        assert_eq!(body["data"], "fits cross at D 236.6");
        assert_eq!(body["metadata"]["from"], "win2");
        assert_eq!(out.trim(), body["id"]);
        assert!(out.contains("-win2-"), "{out}");
    }

    #[test]
    fn remember_needs_a_note() {
        let mut index = Recorder::answering(Ok(json!("Success")));
        assert_eq!(invoke_index(&mut index, &["remember", "a", "c"]).0, 2);
        let (code, _, err) = invoke_index(&mut index, &["remember", "a", "c", "--text", " "]);
        assert_eq!((code, err.trim()), (1, "The note is empty"));
        assert!(index.requests.is_empty());
    }

    #[test]
    fn recall_prints_the_best_matches_first() {
        let hits = json!([
            { "id": "2-win2", "score": 0.0328, "metadata": { "from": "win2" },
              "data": "fits cross at D 236.6\n" },
            { "id": "1-mac", "score": 0.0161, "metadata": { "from": "mac" }, "data": "d6 at D 241.13" },
        ]);
        let mut index = Recorder::answering(Ok(hits));
        let (code, out, _) = invoke_index(&mut index, &["recall", "c", "D 236.6", "--count", "2"]);
        let (path, body) = &index.requests[0];
        assert_eq!(code, 0);
        assert_eq!(path, "query-data/c");
        assert_eq!(
            (body["topK"].as_u64(), body["includeData"].as_bool()),
            (Some(2), Some(true))
        );
        assert_eq!(body["weightingStrategy"], "IDF");
        assert_eq!(
            out,
            "--- 2-win2\nfrom: win2\nscore: 0.0328\n\nfits cross at D 236.6\n\n\
             --- 1-mac\nfrom: mac\nscore: 0.0161\n\nd6 at D 241.13\n\n"
        );
    }

    #[test]
    fn recall_takes_a_query_that_starts_with_a_dash() {
        let mut index = Recorder::answering(Ok(json!([])));
        let (code, _, _) = invoke_index(&mut index, &["recall", "c", "--a_grow 1.25"]);
        assert_eq!(code, 0);
        assert_eq!(index.requests[0].1["data"], "--a_grow 1.25");
    }

    #[test]
    fn recall_says_when_nothing_matches() {
        let mut index = Recorder::answering(Ok(json!([])));
        let (code, out, err) = invoke_index(&mut index, &["recall", "c", "anything"]);
        assert_eq!((code, out.as_str()), (0, ""));
        assert_eq!(err.trim(), "No note in c matches");
    }

    #[test]
    fn a_refusal_stops_the_run_with_the_index_reason() {
        let refused = Stop::Failed("Upstash Vector refused query-data/c: no model".into());
        let mut index = Recorder::answering(Err(refused));
        let (code, _, err) = invoke_index(&mut index, &["recall", "c", "q"]);
        assert_eq!(
            (code, err.trim()),
            (1, "Upstash Vector refused query-data/c: no model")
        );
    }

    #[test]
    fn answer_reads_the_result_or_the_reason_for_refusing() {
        assert_eq!(
            answer("p", 200, r#"{"result":"Success"}"#),
            Ok(json!("Success"))
        );
        let refused = |status, text| match answer("p", status, text) {
            Err(Stop::Failed(m)) => m,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            refused(
                400,
                r#"{"error":"Embedding data is not allowed","status":400}"#
            ),
            "Upstash Vector refused p: Embedding data is not allowed"
        );
        assert_eq!(refused(401, "{}"), "Upstash Vector refused p: HTTP 401");
        assert_eq!(
            refused(502, "Bad Gateway\n"),
            "Upstash Vector refused p: HTTP 502 Bad Gateway"
        );
    }

    #[test]
    fn rest_url_takes_an_https_url_with_a_token() {
        assert_eq!(
            rest_url("https://x-vector.upstash.io/query?_token=Ab+c/d=&y=1").unwrap(),
            (
                "https://x-vector.upstash.io".to_string(),
                "Ab+c/d=".to_string()
            )
        );
        for bad in [
            "",
            "https://x-vector.upstash.io",
            "http://x?_token=t",
            "https://?_token=t",
        ] {
            assert!(matches!(rest_url(bad), Err(Stop::Failed(_))), "{bad}");
        }
    }
}
