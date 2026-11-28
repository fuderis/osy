//! Helper methods module.

pub mod text;
pub use text::*;

pub mod info;
pub use info::*;

pub mod context;
pub use context::*;

pub mod prompt;
pub use prompt::*;

pub mod cli;
pub use cli::*;

pub mod input;
pub use input::*;

pub mod response;
pub use response::*;

pub mod args;
pub use args::*;

use crate::prelude::*;
use chrono::FixedOffset;

/// Returns session local date time
pub fn now_local(timezone_m: i16) -> DateTime<FixedOffset> {
    let offset_seconds = (timezone_m as i32) * 60;
    let tz =
        FixedOffset::east_opt(offset_seconds).unwrap_or_else(|| FixedOffset::east_opt(0).unwrap());

    let utc_now = Utc::now();
    utc_now.with_timezone(&tz)
}

/// Pause before retrying a failed LLM request (0.4s, 0.8s, 1.6s... up to 6.4s).
///
/// Used only for transport errors (rate limits, 5xx, broken streams):
/// retrying instantly would burn all attempts within a second.
pub async fn backoff(attempt: usize) {
    let shift = attempt.saturating_sub(1).min(4) as u32;
    tokio::time::sleep(std::time::Duration::from_millis(400u64 << shift)).await;
}
