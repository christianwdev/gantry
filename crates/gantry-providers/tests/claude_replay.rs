//! Spike X2 offline: the same scenarios as claude_live, replayed by `fake-claude` from the
//! recorded fixtures. Exercises the real session actor (process spine, control protocol,
//! normalizer) deterministically, without a Claude login.

mod common;

use common::{Harness, scenarios};

macro_rules! replay {
    ($($name:ident),* $(,)?) => {$(
        #[tokio::test(flavor = "multi_thread")]
        async fn $name() {
            scenarios::$name(&Harness::replay(stringify!($name))).await;
        }
    )*};
}

replay!(
    multi_turn,
    approve_and_deny,
    ask_question,
    interrupt_running_tool,
    interrupt_pending_approval,
    plan_mode,
    live_config,
    resume_fork_cross_cwd,
    unsupported_control_request,
    exit_during_control,
    late_control_reply,
    shutdown_busy_worker,
    deny_and_stop,
    dismiss_question,
);
