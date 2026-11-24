//! Spike X2 against the real `claude` CLI. Each test also (re)records its sanitized replay
//! fixture under tests/fixtures/claude/. Uses a little quota (Haiku), so it is opt-in:
//!
//!     cargo test -p gantry-providers --test claude_live -- --ignored --test-threads 2

mod common;

use common::{Harness, scenarios};

macro_rules! live {
    ($($name:ident),* $(,)?) => {$(
        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "talks to the real claude CLI and rewrites fixtures"]
        async fn $name() {
            scenarios::$name(&Harness::live(stringify!($name))).await;
        }
    )*};
}

live!(
    multi_turn,
    approve_and_deny,
    ask_question,
    interrupt_running_tool,
    interrupt_pending_approval,
    plan_mode,
    live_config,
    resume_fork_cross_cwd,
);
