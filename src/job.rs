//! Domain types shared by the store, the worker, and the HTTP API.
//!
//! A *job* is a row of `chat_messages` with `sender = 'user'` and
//! `is_agentic = 1`. Its `content` is the prompt; the answer becomes a new
//! `sender = 'ai'` row in the same chat, and the original row's `status`
//! records how far the runner got.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Primary key of a row in `chat_messages`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageId(i64);

impl MessageId {
    /// Wraps a raw database identifier.
    #[must_use]
    pub const fn new(raw: i64) -> Self {
        Self(raw)
    }

    /// Returns the raw database identifier.
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

impl fmt::Display for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Primary key of a row in `chats`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChatId(i64);

impl ChatId {
    /// Wraps a raw database identifier.
    #[must_use]
    pub const fn new(raw: i64) -> Self {
        Self(raw)
    }

    /// Returns the raw database identifier.
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

impl fmt::Display for ChatId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Lifecycle of a job as stored in the `status` column.
///
/// `New` is represented by `NULL` in the database so that rows inserted by
/// the chat application without touching `status` are picked up automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    /// Not yet claimed by any worker (`NULL` in the database).
    New,
    /// Claimed by a worker and currently running.
    Pending,
    /// Finished; the answer was stored as an `ai` message.
    Done,
    /// Finished with an error stored in the row's `payload`.
    Failed,
}

/// Error returned when the `status` column holds a value this crate does not know.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown job status {0:?}")]
pub struct UnknownStatus(pub String);

impl JobStatus {
    /// Converts the database representation (`NULL` meaning `New`).
    pub fn from_db(raw: Option<&str>) -> Result<Self, UnknownStatus> {
        match raw {
            None => Ok(Self::New),
            Some("pending") => Ok(Self::Pending),
            Some("done") => Ok(Self::Done),
            Some("failed") => Ok(Self::Failed),
            Some(other) => Err(UnknownStatus(other.to_owned())),
        }
    }

    /// Converts to the database representation (`None` meaning `NULL`).
    #[must_use]
    pub const fn to_db(self) -> Option<&'static str> {
        match self {
            Self::New => None,
            Self::Pending => Some("pending"),
            Self::Done => Some("done"),
            Self::Failed => Some("failed"),
        }
    }

    /// Whether the job has reached a final state.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed)
    }
}

impl fmt::Display for JobStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.to_db().unwrap_or("new"))
    }
}

/// Who asked for a job: the owner of the chat it was posted in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Requester {
    /// `chats.user_id`, i.e. `users.id` of the person talking.
    pub user_id: i64,
    /// `chats.company_id`, the company that user works for.
    pub company_id: i64,
    /// Whether that user is an admin, from `payload.requester_is_admin` on
    /// the message (written by the chat application). Informational only:
    /// what a job may see is decided by the API, from the user's own
    /// permissions.
    pub is_admin: bool,
}

/// An agentic user message as seen by the runner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    /// Primary key of the user message.
    pub id: MessageId,
    /// Chat the message belongs to; the answer is inserted there.
    pub chat_id: ChatId,
    /// The prompt handed to Claude Code verbatim.
    pub content: String,
    /// Current lifecycle state.
    pub status: JobStatus,
    /// Id of the `ai` message holding the answer, once the job is done.
    pub reply_id: Option<MessageId>,
    /// Human-readable failure reason, once the job failed.
    pub error: Option<String>,
    /// Owner of the chat, so the job knows who "I" and "my" refer to.
    /// `None` only if the chat row is gone. Not part of the HTTP API.
    #[serde(skip)]
    pub requester: Option<Requester>,
}

/// Final outcome of running a job, used to update the row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobOutcome {
    /// Claude finished normally and produced this answer.
    Success {
        /// The text of the final assistant message.
        result: String,
    },
    /// Something went wrong; `result` carries any partial answer we got.
    Failed {
        /// Why the job failed.
        error: String,
        /// Whatever Claude managed to answer before failing, if anything.
        result: Option<String>,
    },
}

impl JobOutcome {
    /// Status this outcome maps to.
    #[must_use]
    pub const fn status(&self) -> JobStatus {
        match self {
            Self::Success { .. } => JobStatus::Done,
            Self::Failed { .. } => JobStatus::Failed,
        }
    }

    /// Convenience constructor for failures without a partial answer.
    pub fn failed(error: impl Into<String>) -> Self {
        Self::Failed {
            error: error.into(),
            result: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_round_trips_through_db_representation() {
        for status in [
            JobStatus::New,
            JobStatus::Pending,
            JobStatus::Done,
            JobStatus::Failed,
        ] {
            assert_eq!(JobStatus::from_db(status.to_db()), Ok(status));
        }
    }

    #[test]
    fn unknown_status_is_rejected() {
        assert_eq!(
            JobStatus::from_db(Some("success")),
            Err(UnknownStatus("success".to_owned()))
        );
    }

    #[test]
    fn only_done_and_failed_are_terminal() {
        assert!(!JobStatus::New.is_terminal());
        assert!(!JobStatus::Pending.is_terminal());
        assert!(JobStatus::Done.is_terminal());
        assert!(JobStatus::Failed.is_terminal());
    }

    #[test]
    fn status_serialises_as_lowercase_string() {
        assert_eq!(serde_json::to_string(&JobStatus::New).unwrap(), "\"new\"");
        assert_eq!(
            serde_json::to_string(&JobStatus::Pending).unwrap(),
            "\"pending\""
        );
    }

    #[test]
    fn ids_are_transparent_in_json() {
        assert_eq!(serde_json::to_string(&MessageId::new(7)).unwrap(), "7");
        assert_eq!(
            serde_json::from_str::<MessageId>("7").unwrap(),
            MessageId::new(7)
        );
        assert_eq!(serde_json::to_string(&ChatId::new(3)).unwrap(), "3");
    }
}

/// An earlier question of the same conversation, for a follow-up's context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    /// What was asked.
    pub question: String,
    /// The answer, if there was one.
    pub answer: Option<String>,
    /// The question failed.
    pub failed: bool,
    /// The titles of the pages made for it.
    pub pages: Vec<String>,
}

/// Longest earlier answer quoted in a follow-up's prompt, in characters.
const MAX_QUOTED_ANSWER: usize = 3000;

fn quoted(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(max).collect();
    cut.push_str(" […]");
    cut
}

/// The prompt for `question` after the conversation `history`: the
/// question alone when there is no history.
#[must_use]
pub fn prompt_with_history(history: &[Turn], question: &str) -> String {
    if history.is_empty() {
        return question.to_owned();
    }
    let mut prompt = String::from(
        "This is a follow-up in a conversation with the same person. Earlier in it:\n\n",
    );
    for turn in history {
        prompt.push_str("<earlier_question>\n");
        prompt.push_str(turn.question.trim());
        prompt.push_str("\n</earlier_question>\n<earlier_answer>\n");
        match &turn.answer {
            Some(answer) => prompt.push_str(&quoted(answer, MAX_QUOTED_ANSWER)),
            None if turn.failed => prompt.push_str("(no answer: it failed)"),
            None => prompt.push_str("(no answer)"),
        }
        if !turn.pages.is_empty() {
            prompt.push_str("\n(Pages made for it: ");
            prompt.push_str(&turn.pages.join(", "));
            prompt.push_str(". Build them again if this question needs them.)");
        }
        prompt.push_str("\n</earlier_answer>\n\n");
    }
    prompt.push_str("Answer this new message (\"it\", \"that\", \"them\" refer to the conversation above):\n\n");
    prompt.push_str(question);
    prompt
}

#[cfg(test)]
mod history_tests {
    use super::*;

    #[test]
    fn a_first_question_is_sent_as_is() {
        assert_eq!(prompt_with_history(&[], "ok:hi"), "ok:hi");
    }

    #[test]
    fn a_follow_up_carries_the_conversation() {
        let history = [
            Turn {
                question: "Sales this month?".into(),
                answer: Some("$10,000.".into()),
                failed: false,
                pages: vec!["Sales by week".into()],
            },
            Turn {
                question: "Broken".into(),
                answer: None,
                failed: true,
                pages: vec![],
            },
        ];
        let prompt = prompt_with_history(&history, "And last month?");
        assert!(prompt.contains("<earlier_question>\nSales this month?"));
        assert!(prompt.contains("$10,000."));
        assert!(prompt.contains("Pages made for it: Sales by week"));
        assert!(prompt.contains("(no answer: it failed)"));
        assert!(prompt.ends_with("And last month?"));
        assert!(quoted(&"x".repeat(5000), 10).ends_with("[…]"));
    }
}
