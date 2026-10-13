/// Seed registration happens in `pre_register_seeds` before the task graph.
/// A seed whose registration failed this run fails here with that error, taken from
/// the run's `seed_registration_errors` (the only place it is said). The failure is
/// recorded in the run's stats, as a model's render failure is, so the seed and its
/// reason are in `run_results.json` (ferrion #547). Otherwise this verifies the seed was registered (its
/// schema is in the store), and returns `Errored` if not, to propagate failure
/// through the task graph skip mechanism.
use std::sync::Arc;

use dbt_common::FsResult;
use dbt_common::collections::DashMap;
use dbt_common::stats::{NodeStatus, Stat};
use dbt_scheduler::instructions::SqlInstruction;
use dbt_schemas::schemas::InternalDbtNodeAttributes;
use dbt_tasks_core::context::TaskRunnerCtx;
use dbt_tasks_core::task::TaskResult;

pub fn run_seed_render(
    node: Arc<dyn InternalDbtNodeAttributes>,
    ctx: TaskRunnerCtx,
    result_sender: Option<std::sync::mpsc::SyncSender<TaskResult>>,
) -> FsResult<NodeStatus> {
    let relation =
        dbt_adapter::relation::create_relation_from_node(ctx.adapter_type(), node.as_ref(), None)?;
    let cfqn = relation.get_canonical_fqn()?;
    // Its registration failed this run: that error is this seed's result, whatever
    // schema an earlier run left (ferrion #533, #547).
    let unique_id = &node.common().unique_id;
    if let Some((_, e)) = ctx.inner.seed_registration_errors.remove(unique_id) {
        record_errored(&ctx, unique_id, e.to_string());
        return Err(e);
    }
    if !ctx.schema_cache.exists(&cfqn) {
        // Not registered, and no error recorded for it (a seed not scheduled for
        // registration): failed without a second message.
        record_errored(
            &ctx,
            unique_id,
            format!("seed {unique_id} was not registered"),
        );
        return Ok(NodeStatus::Errored);
    }

    if let Some(sender) = result_sender {
        let _ = sender.send(TaskResult {
            sql_instruction: SqlInstruction::default(),
            config_map: Arc::new(DashMap::default()),
            lp_instruction: None,
        });
    }
    Ok(NodeStatus::Succeeded)
}

/// Records the seed as errored in this run's stats, with why, as
/// `handle_render_result` records a model whose render failed.
fn record_errored(ctx: &TaskRunnerCtx, unique_id: &str, message: String) {
    ctx.inner.run_stats.insert(
        unique_id.to_string(),
        Stat::new(
            unique_id.to_string(),
            dbt_vfs::time::system_now(),
            None,
            NodeStatus::Errored,
            Some(message),
            ctx.thread_id,
        ),
    );
}
