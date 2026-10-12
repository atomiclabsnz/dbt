/// Seed registration happens in `pre_register_seeds` before the task graph.
/// A seed whose registration failed this run fails here with that error (the
/// only place it is said). Otherwise this verifies the seed was registered (its
/// schema is in the store), and returns `Errored` if not, to propagate failure
/// through the task graph skip mechanism.
use std::sync::Arc;

use dbt_common::FsResult;
use dbt_common::collections::DashMap;
use dbt_common::stats::NodeStatus;
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
    if let Some(e) = crate::register_seeds::registration_error(&node.common().unique_id) {
        return Err(Box::new(e));
    }
    if !ctx.schema_cache.exists(&cfqn) {
        // Not registered, and no error recorded for it (a seed not scheduled for
        // registration): failed without a second message.
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
