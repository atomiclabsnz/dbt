//! Util methods for creating query context.

use crate::errors::AdapterResult;

use dbt_adapter_core::DBT_EXECUTION_PHASES;
use dbt_adbc::QueryCtx;
use dbt_schemas::schemas::{
    DbtModel, DbtSeed, DbtSnapshot, DbtTest, DbtUnitTest, manifest::DbtOperation,
};
use minijinja::{State, constants::CURRENT_EXECUTION_PHASE};

pub fn query_ctx_from_state(state: &State) -> AdapterResult<QueryCtx> {
    // TODO: The following should really be an error, but
    // our tests (functional tests in particular) do not
    // set anything about model in the state.
    //
    // TODO: The following should be an error but there
    // are tests that do not include model.
    //return Err(AdapterError::new(
    //AdapterErrorKind::Configuration,
    //"Missing model in the state",
    //));
    let mut query = QueryCtx::default();
    // TODO: use node_metadata_from_state
    if let Some(node_id) = node_id_from_state(state) {
        query = query.with_node_id(node_id);
    }
    if let Some(phase) = execution_phase_from_state(state) {
        query = query.with_phase(phase);
    }
    Ok(query)
}

pub fn node_id_from_state(state: &State) -> Option<String> {
    let node = state.lookup("model", &[]).as_ref()?.clone();
    // all deserialization must go through yaml value
    // should this be a .ok?
    let yaml_node = dbt_yaml::to_value(&node)
        .map_err(|e| {
            minijinja::Error::new(minijinja::ErrorKind::SerdeDeserializeError, e.to_string())
        })
        .ok()?;

    if let Ok(model) = yaml_node.to_owned_typed::<DbtModel>() {
        Some(model.__common_attr__.unique_id)
    } else if let Ok(test) = yaml_node.to_owned_typed::<DbtTest>() {
        Some(test.__common_attr__.unique_id)
    } else if let Ok(snapshot) = yaml_node.to_owned_typed::<DbtSnapshot>() {
        Some(snapshot.__common_attr__.unique_id)
    } else if let Ok(seed) = yaml_node.to_owned_typed::<DbtSeed>() {
        Some(seed.__common_attr__.unique_id)
    } else if let Ok(unit_test) = yaml_node.to_owned_typed::<DbtUnitTest>() {
        Some(unit_test.__common_attr__.unique_id)
    } else if let Ok(unit_test) = yaml_node.to_owned_typed::<DbtOperation>() {
        Some(unit_test.__common_attr__.unique_id)
    } else {
        None
    }
}

pub fn execution_phase_from_state(state: &State) -> Option<&'static str> {
    let value = state.lookup(CURRENT_EXECUTION_PHASE, &[])?;
    let s = value.as_str()?;
    DBT_EXECUTION_PHASES
        .iter()
        .position(|&p| p == s)
        .map(|idx| DBT_EXECUTION_PHASES[idx])
}
