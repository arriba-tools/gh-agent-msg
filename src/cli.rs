use clap::{ArgGroup, Args, Parser, Subcommand};
use std::path::PathBuf;

use crate::message::entry_id;

#[derive(Parser)]
#[command(
    name = "gh-agent-msg",
    bin_name = "gh agent-msg",
    about = "Messages between coding agents on several machines, over Redis streams",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Repository whose Actions variables AGENT_MSG_URL and AGENT_VECTOR_URL name the database and
    /// the index (default: current repo)
    #[arg(short = 'R', long, global = true, value_name = "OWNER/REPO")]
    pub repo: Option<String>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Send a message to a channel and print its ID
    Send(SendArgs),
    /// Wait for messages to a name, or with --re for the replies to messages
    ///
    /// Exits 0 after printing the messages, 3 when nothing arrived before the timeout, 4 when the
    /// database stayed out of reach for --offline minutes, and 1 on any other failure.
    ///
    /// Printing a message does not acknowledge it. Restart with the --ack the command printed once
    /// the messages are handled, so a caller that stops first sees them again.
    Watch(WatchArgs),
    /// List the asks that wait on someone and the messages with missing replies
    ///
    /// Exits 0 when nothing is open and 1 when something is. With --wait, exits 0 once every name
    /// in the message's ack-required has replied, and 3 at the timeout.
    Open(OpenArgs),
    /// Print a channel's latest messages, newest first
    Log(LogArgs),
    /// Store a note in a channel's index, where recall finds it by meaning or by its words
    ///
    /// The index is the Upstash Vector index that the variable AGENT_VECTOR_URL names, and the
    /// channel is its namespace.
    Remember(RememberArgs),
    /// Search a channel's notes by meaning and by keyword together, best match first
    Recall(RecallArgs),
}

#[derive(Args)]
#[command(group(ArgGroup::new("text_source").required(true).args(["file", "text"])))]
pub struct SendArgs {
    /// Your name
    pub name: String,

    /// Channel to send on
    pub channel: String,

    /// Recipients, separated by commas, or all
    #[arg(long, value_name = "NAMES")]
    pub to: String,

    /// One line saying what the message is about
    #[arg(long)]
    pub subject: String,

    /// UTF-8 file holding the text
    #[arg(long, value_name = "PATH")]
    pub file: Option<PathBuf>,

    /// The text itself
    #[arg(long)]
    pub text: Option<String>,

    /// ID of the message this answers
    #[arg(long, value_name = "ID", value_parser = entry_id)]
    pub re: Option<String>,

    /// The person the message waits on. `open` lists it until you, its sender, reply to it with
    /// --re once they have answered
    #[arg(long, value_name = "NAME")]
    pub needs: Option<String>,

    /// Recipients who must each reply with --re, separated by commas. `open` lists it until all of
    /// them have
    #[arg(long, value_name = "NAMES")]
    pub ack_required: Option<String>,
}

#[derive(Args)]
pub struct WatchArgs {
    /// Your name
    pub name: String,

    /// Channel to watch
    pub channel: String,

    /// Acknowledge this handled message before waiting, repeated for several
    #[arg(long, value_name = "ID", value_parser = entry_id)]
    pub ack: Vec<String>,

    /// Wait for the replies to this message instead, repeated to wait on several
    #[arg(long = "re", value_name = "ID", value_parser = entry_id)]
    pub re: Vec<String>,

    /// Minutes to wait before giving up
    #[arg(long, value_name = "MINS", default_value_t = 110)]
    pub timeout: u64,

    /// Minutes the database may stay out of reach before the watch gives up, retried meanwhile
    /// with a pause that doubles from 15 seconds up to 5 minutes
    #[arg(long, value_name = "MINS", default_value_t = 15)]
    pub offline: u64,
}

#[derive(Args)]
pub struct OpenArgs {
    /// Only this channel, repeated for several (default: every channel)
    #[arg(long, value_name = "CHANNEL")]
    pub channel: Vec<String>,

    /// Days back to read
    #[arg(long, value_name = "N", default_value_t = 7)]
    pub days: u64,

    /// Only the asks that wait on this name
    #[arg(long, value_name = "NAME")]
    pub needs: Option<String>,

    /// Block until every name in this message's ack-required has replied
    #[arg(long, value_name = "CHANNEL/ID")]
    pub wait: Option<String>,

    /// Minutes --wait waits before giving up
    #[arg(long, value_name = "MINS", default_value_t = 110)]
    pub timeout: u64,
}

#[derive(Args)]
#[command(group(ArgGroup::new("note_source").required(true).args(["file", "text"])))]
pub struct RememberArgs {
    /// Your name
    pub name: String,

    /// Channel whose index keeps the note
    pub channel: String,

    /// UTF-8 file holding the note
    #[arg(long, value_name = "PATH")]
    pub file: Option<PathBuf>,

    /// The note itself
    #[arg(long)]
    pub text: Option<String>,
}

#[derive(Args)]
pub struct RecallArgs {
    /// Channel whose notes to search
    pub channel: String,

    /// What to look for, in words
    pub query: String,

    /// How many notes
    #[arg(long, value_name = "N", default_value_t = 5,
          value_parser = clap::value_parser!(u64).range(1..=1000))]
    pub count: u64,
}

#[derive(Args)]
pub struct LogArgs {
    /// Channel to print
    pub channel: String,

    /// How many messages
    #[arg(long, value_name = "N", default_value_t = 20)]
    pub count: u64,
}
