use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_schema::{DataType, Field, Schema, SchemaRef};
use dbt_adapter::column::ColumnStatic;
use dbt_adapter::relation::create_relation_from_node;
use dbt_adapter::sql_types::TypeOps;
use dbt_adapter_core::AdapterType;
use dbt_common::tracing::{dbt_emit::emit_warn_log_message, spawn_blocking_traced, spawn_traced};
use dbt_common::{ErrorCode, FsResult, fs_err};
use dbt_csv::{CustomCsvOptions, read_to_arrow_records};
use dbt_df_providers::seed_io::{
    TableFormat, adapt_schema, infer_seed_column_name_strategy, read_json_seed,
    read_parquet_seed_view,
};

use dbt_frontend_common::Dialect;
use dbt_schema_store::{CanonicalFqn, DataStoreTrait, SchemaStoreTrait};
use dbt_schemas::schemas::DbtSeed;
use dbt_yaml::Spanned;

/// Result of registering a single seed: the canonical FQN, adapted schema, and unique_id.
/// Used by the caller to handle local-mode schema mirroring.
pub struct RegisteredSeed {
    pub canonical_fqn: CanonicalFqn,
    pub schema: SchemaRef,
    pub unique_id: String,
}

/// Shared context for seed registration, avoiding a dependency on TaskRunnerCtx.
struct SeedRegistrationCtx {
    adapter_type: AdapterType,
    schema_cache: Arc<dyn SchemaStoreTrait>,
    data_store: Arc<dyn DataStoreTrait>,
    in_dir: PathBuf,
}

pub fn resolve_seed_path(in_dir: &Path, seed: &DbtSeed) -> PathBuf {
    seed.__seed_attr__
        .root_path
        .as_ref()
        .map(|root| root.join(&seed.__common_attr__.path))
        .unwrap_or_else(|| in_dir.join(&seed.__common_attr__.original_file_path))
}

/// Synchronously register a CSV seed.
/// Intended to run inside `spawn_blocking_traced`.
/// The seed's schema is registered in the schema cache and its data is written
/// to the data store.
fn register_seed_csv(
    seed: Arc<DbtSeed>,
    ctx: Arc<SeedRegistrationCtx>,
    type_ops: Arc<dyn TypeOps>,
) -> FsResult<RegisteredSeed> {
    let relation = create_relation_from_node(ctx.adapter_type, seed.as_ref(), None)?;
    let canonical_fqn = relation.get_canonical_fqn()?;
    let delimiter = seed
        .__seed_attr__
        .delimiter
        .as_ref()
        .map(|s| s.chars().next().unwrap_or(','))
        .unwrap_or(',');
    let seed_path = resolve_seed_path(&ctx.in_dir, &seed);
    if let Some(relation_name) = &seed.__base_attr__.relation_name {
        dbt_common::seed_path_registry::register(relation_name, seed_path.clone());
    }
    let infer_column_name_strategy =
        infer_seed_column_name_strategy(seed.__seed_attr__.quote_columns, ctx.adapter_type);

    let text_columns: Vec<String> = seed
        .__seed_attr__
        .column_types
        .as_ref()
        .map(|ct| ct.keys().map(|k| k.as_ref().clone()).collect())
        .unwrap_or_default();
    let options = CustomCsvOptions::default()
        .with_delimiter(delimiter as u8)
        .with_text_columns(text_columns);
    let result = read_to_arrow_records(&seed_path, &options)?;
    if !result.unmatched_text_columns.is_empty() {
        emit_warn_log_message(
            ErrorCode::SeedColumnTypeMismatch,
            format!(
                "Columns specified in column_types were not found in seed CSV header: {:?}",
                result.unmatched_text_columns
            ),
        );
    }

    let logical_fields = if let Some(provided_types) = &seed.__seed_attr__.column_types {
        Some(parse_provided_types(provided_types, type_ops.as_ref())?)
    } else {
        None
    };
    let target_schema = adapt_schema(result.schema.clone(), infer_column_name_strategy);
    refuse_header_collisions(&seed, &result.schema, &target_schema)?;
    let schema =
        dialect_adapted_schema(&target_schema, logical_fields.as_ref(), type_ops.as_ref())?;
    ctx.schema_cache
        .register_schema(&canonical_fqn, None, schema.clone(), true)?;
    ctx.data_store
        .persist_data(&canonical_fqn, target_schema, result.batches)?;

    Ok(RegisteredSeed {
        canonical_fqn,
        schema,
        unique_id: seed.__common_attr__.unique_id.clone(),
    })
}

/// Synchronously register a JSON seed.
/// Intended to run inside `spawn_blocking_traced`.
fn register_seed_json(
    seed: Arc<DbtSeed>,
    ctx: Arc<SeedRegistrationCtx>,
    type_ops: Arc<dyn TypeOps>,
) -> FsResult<RegisteredSeed> {
    let relation = create_relation_from_node(ctx.adapter_type, seed.as_ref(), None)?;
    let canonical_fqn = relation.get_canonical_fqn()?;
    let seed_path = resolve_seed_path(&ctx.in_dir, &seed);
    let infer_column_name_strategy =
        infer_seed_column_name_strategy(seed.__seed_attr__.quote_columns, ctx.adapter_type);

    let (source_schema, maybe_batches) = read_json_seed(&seed_path, true)?;
    let logical_fields = if let Some(provided_types) = &seed.__seed_attr__.column_types {
        Some(parse_provided_types(provided_types, type_ops.as_ref())?)
    } else {
        None
    };
    let target_schema = adapt_schema(source_schema.clone(), infer_column_name_strategy);
    refuse_header_collisions(&seed, &source_schema, &target_schema)?;
    let schema =
        dialect_adapted_schema(&target_schema, logical_fields.as_ref(), type_ops.as_ref())?;
    ctx.schema_cache
        .register_schema(&canonical_fqn, None, schema.clone(), true)?;

    if let Some(batches) = maybe_batches {
        ctx.data_store
            .persist_data(&canonical_fqn, target_schema, batches)?;
    }

    Ok(RegisteredSeed {
        canonical_fqn,
        schema,
        unique_id: seed.__common_attr__.unique_id.clone(),
    })
}

/// Register a parquet seed using async parquet I/O.
/// (Data persistence is sync, but it runs on ablocking pool.)
async fn register_seed_parquet_async(
    seed: Arc<DbtSeed>,
    ctx: Arc<SeedRegistrationCtx>,
    type_ops: Arc<dyn TypeOps>,
) -> FsResult<RegisteredSeed> {
    let relation = create_relation_from_node(ctx.adapter_type, seed.as_ref(), None)?;
    let canonical_fqn = relation.get_canonical_fqn()?;
    let seed_path = resolve_seed_path(&ctx.in_dir, &seed);
    let infer_column_name_strategy =
        infer_seed_column_name_strategy(seed.__seed_attr__.quote_columns, ctx.adapter_type);

    let (source_schema, maybe_batches) = read_parquet_seed_view(&seed_path, true).await?;
    let logical_fields = if let Some(provided_types) = &seed.__seed_attr__.column_types {
        Some(parse_provided_types(provided_types, type_ops.as_ref())?)
    } else {
        None
    };
    let target_schema = adapt_schema(source_schema.clone(), infer_column_name_strategy);
    refuse_header_collisions(&seed, &source_schema, &target_schema)?;
    let schema =
        dialect_adapted_schema(&target_schema, logical_fields.as_ref(), type_ops.as_ref())?;
    ctx.schema_cache
        .register_schema(&canonical_fqn, None, schema.clone(), true)?;

    if let Some(batches) = maybe_batches {
        let cfqn = canonical_fqn.clone();
        let ts = target_schema.clone();
        let dsc = Arc::clone(&ctx);
        match spawn_blocking_traced(move || dsc.data_store.persist_data(&cfqn, ts, batches)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return Err(e.into()),
            Err(j) => return Err(j.into()),
        }
    }

    Ok(RegisteredSeed {
        canonical_fqn,
        schema,
        unique_id: seed.__common_attr__.unique_id.clone(),
    })
}

/// Pre-register scheduled seeds before the task graph runs.
/// (CSV, JSON are on blocking workers; parquet is async)
///
/// An error does not abort the run: it is recorded against its seed
/// ([`registration_error`]), and that seed's render fails with it — so
/// the reason is the seed's own result, the other seeds and models still run, and a
/// schema an earlier run registered for the seed cannot stand in for this one.
#[allow(clippy::too_many_arguments)]
pub async fn pre_register_seeds(
    sorted_nodes: &[&String],
    seeds: &BTreeMap<String, Arc<DbtSeed>>,
    adapter_type: AdapterType,
    schema_cache: Arc<dyn SchemaStoreTrait>,
    data_store: Arc<dyn DataStoreTrait>,
    type_ops: Arc<dyn TypeOps>,
    in_dir: &Path,
) -> Vec<RegisteredSeed> {
    // This run's errors only: a seed fixed since an earlier run in the same process
    // (a browser tab runs many) must not fail on that run's error.
    registration_errors()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clear();
    let ctx = Arc::new(SeedRegistrationCtx {
        adapter_type,
        schema_cache,
        data_store,
        in_dir: in_dir.to_path_buf(),
    });

    let mut handles: Vec<(String, tokio::task::JoinHandle<FsResult<RegisteredSeed>>)> = Vec::new();

    for unique_id in sorted_nodes {
        let Some(seed) = seeds.get(*unique_id) else {
            continue;
        };
        let seed = Arc::clone(seed);
        // Extension dispatch only — the actual read uses `resolve_seed_path`
        // inside the per-format function, which honors `seed.__seed_attr__.root_path`.
        let extension = seed
            .__common_attr__
            .original_file_path
            .extension()
            .and_then(|ext| ext.to_str().map(|ext| ext.to_lowercase()));
        let format = match extension.as_deref() {
            Some("csv") => None,
            Some("parquet") => Some(TableFormat::Parquet),
            Some("json") => Some(TableFormat::Json),
            _ => {
                // Silently skip — visit_render will detect missing schema and
                // emit the error through normal task graph propagation.
                continue;
            }
        };

        let ctx = Arc::clone(&ctx);
        match format {
            None => {
                let type_ops = Arc::clone(&type_ops);
                handles.push((
                    seed.__common_attr__.unique_id.clone(),
                    spawn_blocking_traced(move || register_seed_csv(seed, ctx, type_ops)),
                ))
            }
            Some(TableFormat::Json) => {
                let type_ops = Arc::clone(&type_ops);
                handles.push((
                    seed.__common_attr__.unique_id.clone(),
                    spawn_blocking_traced(move || register_seed_json(seed, ctx, type_ops)),
                ))
            }
            Some(TableFormat::Parquet) => {
                let type_ops = Arc::clone(&type_ops);
                handles.push((
                    seed.__common_attr__.unique_id.clone(),
                    spawn_traced(register_seed_parquet_async(seed, ctx, type_ops)),
                ))
            }
            Some(TableFormat::Csv) => {
                unreachable!("extension dispatch only yields csv as None, not TableFormat::Csv")
            }
        }
    }

    let mut results = Vec::with_capacity(handles.len());
    for (unique_id, handle) in handles {
        match handle.await {
            Ok(Ok(registered)) => results.push(registered),
            // Recorded against the seed, whose render fails with it — said once, there.
            Ok(Err(e)) => {
                registration_errors()
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(unique_id, (e.code, e.context.clone()));
            }
            // A registration that panicked failed too: not left to an earlier schema.
            Err(join_err) => {
                registration_errors()
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(
                        unique_id,
                        (
                            ErrorCode::FailedToRegisterSeedTable,
                            format!("seed registration panicked: {join_err}"),
                        ),
                    );
            }
        }
    }
    results
}

type RegistrationErrors = std::sync::Mutex<std::collections::HashMap<String, (ErrorCode, String)>>;

fn registration_errors() -> &'static RegistrationErrors {
    static ERRORS: std::sync::OnceLock<RegistrationErrors> = std::sync::OnceLock::new();
    ERRORS.get_or_init(Default::default)
}

/// The error this run's registration of the seed `unique_id` failed with, if it did.
pub fn registration_error(unique_id: &str) -> Option<dbt_common::FsError> {
    registration_errors()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(unique_id)
        .map(|(code, context)| *fs_err!(*code, "{context}"))
}

fn parse_provided_types(
    column_descriptions: &BTreeMap<Spanned<String>, String>,
    type_ops: &dyn TypeOps,
) -> FsResult<LogicalTypeMap> {
    // Adapter-specific type aliases in `+column_types` (e.g. BigQuery `float`,
    // `integer`) must be normalized to their canonical names (`FLOAT64`, `INT64`)
    // before parsing, mirroring dbt-adapters' `Column.translate_type`. dbt Core
    // does not statically validate these; it hands the type to the warehouse,
    // which accepts the alias. Without this, a valid seed config fails with
    // `Invalid column description`.
    let column_static = ColumnStatic::new(type_ops.adapter_type());
    let mut logical_types: BTreeMap<String, DataType> = BTreeMap::new();
    for (field_name, descr) in column_descriptions.iter() {
        // this creates a Logical type
        let translated = column_static.translate_type(descr);
        let parsed_field = type_ops
            .parse_column_description(&translated)
            .map_err(|_| {
                fs_err!(
                    code => ErrorCode::InvalidType,
                    loc => field_name.span().clone(),
                    "Invalid column description: {descr}",
                )
            })?;
        let logical_type = parsed_field.data_type().clone();

        logical_types.insert(field_name.as_ref().clone(), logical_type);
    }
    Ok(LogicalTypeMap(logical_types))
}

pub struct LogicalTypeMap(BTreeMap<String, DataType>);

impl LogicalTypeMap {
    pub fn get(&self, k: &str) -> Option<DataType> {
        self.0.get(k).cloned()
    }
}

/// Creates a Dialect adapted table provider
///
/// The provided source should represent the physical schema
/// We abstract on top of it using the provided logical schema
pub fn dialect_adapted_schema(
    source_schema: &SchemaRef,
    logical_types: Option<&LogicalTypeMap>,
    type_ops: &dyn TypeOps,
) -> FsResult<SchemaRef> {
    let adapter_type = type_ops.adapter_type();
    let adapted_fields: Vec<Arc<Field>> = source_schema
        .fields()
        .iter()
        .map(|f| {
            // Try to get logical field type
            let logical_field_type = logical_types.and_then(|types| {
                // For Snowflake, normalize field names for matching
                let lookup_name = if matches!(adapter_type, AdapterType::Snowflake) {
                    Dialect::Snowflake
                        .parse_identifier(f.name())
                        .map(|id| id.to_value())
                        .unwrap_or_else(|_| f.name().to_string())
                } else {
                    f.name().to_string()
                };
                types.get(&lookup_name)
            });

            // Determine final field type
            let final_type = if let Some(logical_type) = logical_field_type {
                logical_type
            } else {
                // Use inferred type, with optional data platform adaptation
                type_ops
                    .adapt_seed_type(f.data_type())
                    .unwrap_or_else(|| f.data_type().clone())
            };

            // Create new field with the determined type
            Arc::new(
                Field::new(f.name().clone(), final_type, f.is_nullable())
                    .with_metadata(f.metadata().clone()),
            )
        })
        .collect();

    let adapted_schema = Arc::new(Schema::new_with_metadata(
        adapted_fields,
        source_schema.metadata().clone(),
    ));
    Ok(adapted_schema)
}

/// Refuse a seed whose header names two columns the same once adapted — folded
/// (`Id`, `ID` under `quote_columns: false`) or trimmed (`id`, ` id`): written on, the
/// two fields share a name, and the agate table the seed's materialization reads keeps
/// only one — a column lost without a word (ferrion #533). Named by both spellings, as
/// the header has them, and by what made them one.
fn refuse_header_collisions(seed: &DbtSeed, source: &Schema, folded: &Schema) -> FsResult<()> {
    // Every group of colliding names at once, so one fix of the header settles them.
    let groups = header_collisions(source, folded);
    if groups.is_empty() {
        return Ok(());
    }
    Err(fs_err!(
        ErrorCode::FailedToRegisterSeedTable,
        "seed {}: {}; rename all but one of each in the header",
        seed.__common_attr__.name,
        groups
            .iter()
            .map(|(spellings, name)| collision_message(spellings, name))
            .collect::<Vec<_>>()
            .join("; ")
    ))
}

/// What one collision is: the header's spellings, the name they share, and what made
/// them one (folding under `quote_columns: false`, trimming, or both).
fn collision_message(spellings: &[String], name: &str) -> String {
    let quoted: Vec<String> = spellings.iter().map(|s| format!("{s:?}")).collect();
    if spellings.iter().all(|s| s == &spellings[0]) {
        return format!(
            "column {} appears {} times in the header",
            quoted[0],
            spellings.len()
        );
    }
    let trimmed = spellings.iter().any(|s| s.trim() != s);
    let folded = spellings.iter().any(|s| s.trim() != spellings[0].trim());
    let how = match (trimmed, folded) {
        (true, true) => "once their names are trimmed and folded (quote_columns: false)",
        (true, false) => "once their names are trimmed",
        _ => "once their names are folded (quote_columns: false)",
    };
    let list = match quoted.as_slice() {
        [a, b] => format!("{a} and {b}"),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
        [] => String::new(),
    };
    let verb = if spellings.len() == 2 { "both" } else { "all" };
    format!("columns {list} are {verb} {name:?} {how}")
}

/// Every name the adapted schema gives more than one header name, in header order:
/// (the header's spellings, the name they share).
fn header_collisions(source: &Schema, folded: &Schema) -> Vec<(Vec<String>, String)> {
    let mut groups: Vec<(Vec<String>, String)> = Vec::new();
    for (original, field) in source.fields().iter().zip(folded.fields().iter()) {
        match groups.iter_mut().find(|(_, name)| name == field.name()) {
            Some((spellings, _)) => spellings.push(original.name().clone()),
            None => groups.push((vec![original.name().clone()], field.name().clone())),
        }
    }
    groups.retain(|(spellings, _)| spellings.len() > 1);
    groups
}

#[cfg(test)]
mod folded_collision_tests {
    use super::*;
    use dbt_df_providers::seed_io::InferColumnNameStrategy;

    fn schema(names: &[&str]) -> Arc<Schema> {
        Arc::new(Schema::new(
            names
                .iter()
                .map(|n| Field::new(*n, DataType::Int64, true))
                .collect::<Vec<_>>(),
        ))
    }

    #[test]
    fn a_folded_pair_is_found_with_both_spellings() {
        let source = schema(&["Id", "name", "ID"]);
        let folded = adapt_schema(source.clone(), InferColumnNameStrategy::Lowercase);
        assert_eq!(
            header_collisions(&source, &folded),
            vec![(vec!["Id".into(), "ID".into()], "id".into())]
        );
        // Snowflake folds up: the same pair collides there too.
        let upper = adapt_schema(source.clone(), InferColumnNameStrategy::Uppercase);
        assert_eq!(
            header_collisions(&source, &upper),
            vec![(vec!["Id".into(), "ID".into()], "ID".into())]
        );
    }

    #[test]
    fn every_pair_is_found_at_once() {
        let source = schema(&["Id", "ID", "Name", "NAME"]);
        let folded = adapt_schema(source.clone(), InferColumnNameStrategy::Lowercase);
        assert_eq!(
            header_collisions(&source, &folded),
            vec![
                (vec!["Id".into(), "ID".into()], "id".into()),
                (vec!["Name".into(), "NAME".into()], "name".into()),
            ]
        );
    }

    #[test]
    fn the_message_says_what_made_the_names_one() {
        let m = |s: &[&str], n: &str| {
            collision_message(&s.iter().map(|x| x.to_string()).collect::<Vec<_>>(), n)
        };
        assert_eq!(
            m(&["Id", "ID"], "id"),
            "columns \"Id\" and \"ID\" are both \"id\" once their names are folded (quote_columns: false)"
        );
        assert_eq!(
            m(&["Id", "ID", "iD"], "id"),
            "columns \"Id\", \"ID\" and \"iD\" are all \"id\" once their names are folded (quote_columns: false)"
        );
        assert_eq!(
            m(&["id", " id"], "id"),
            "columns \"id\" and \" id\" are both \"id\" once their names are trimmed"
        );
        assert_eq!(
            m(&["Id", " id"], "id"),
            "columns \"Id\" and \" id\" are both \"id\" once their names are trimmed and folded (quote_columns: false)"
        );
        assert_eq!(
            m(&["id", "id"], "id"),
            "column \"id\" appears 2 times in the header"
        );
    }

    #[test]
    fn three_spellings_are_one_group() {
        let source = schema(&["Id", "ID", "iD"]);
        let folded = adapt_schema(source.clone(), InferColumnNameStrategy::Lowercase);
        assert_eq!(
            header_collisions(&source, &folded),
            vec![(vec!["Id".into(), "ID".into(), "iD".into()], "id".into())]
        );
    }

    #[test]
    fn verbatim_names_differing_in_case_do_not_collide_and_trimmed_ones_do() {
        let source = schema(&["Id", "ID"]);
        let verbatim = adapt_schema(source.clone(), InferColumnNameStrategy::Verbatim);
        assert_eq!(header_collisions(&source, &verbatim), vec![]);
        let twice = schema(&["id", "id"]);
        let kept = adapt_schema(twice.clone(), InferColumnNameStrategy::Verbatim);
        assert_eq!(
            header_collisions(&twice, &kept),
            vec![(vec!["id".into(), "id".into()], "id".into())]
        );
        let padded = schema(&["id", " id"]);
        let trimmed = adapt_schema(padded.clone(), InferColumnNameStrategy::Verbatim);
        assert_eq!(
            header_collisions(&padded, &trimmed),
            vec![(vec!["id".into(), " id".into()], "id".into())]
        );
    }
}
