//! Embedded skills module.

pub mod history;
pub use history::{ReadHistory, handle_read_history};

pub mod fact;
pub use fact::{
    MemoryOutcome, RememberFact, SearchFact, handle_remember_fact, handle_search_fact,
};

pub mod eval;
pub use eval::{EvalAction, handle_eval};

pub mod skill;
pub use skill::{SkillAction, handle_skill};
