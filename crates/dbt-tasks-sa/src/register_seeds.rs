use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_schema::{DataType, Field, Schema, SchemaRef};
use dbt_adapter::column::ColumnStatic;
use dbt_adapter::relation::create_relation_from_node;
use dbt_adapter::sql_types::TypeOps;
use dbt_adapter_core::AdapterType;
use dbt_common::tracing::{dbt_emit::emit_warn_log_message, spawn_blocking_traced, spawn_traced};
use dbt_common::{ErrorCode, FsError, FsResult, fs_err};
use dbt_csv::{CustomCsvOptions, read_to_arrow_records};
use dbt_df_providers::seed_io::{
    InferColumnNameStrategy, TableFormat, adapt_schema, infer_seed_column_name_strategy,
    read_json_seed, read_parquet_seed_view,
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
    let target_schema = adapt_seed_schema(&seed, &result.schema, ctx.adapter_type, &result.header)?;
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
    let (source_schema, maybe_batches) = read_json_seed(&seed_path, true)?;
    let logical_fields = if let Some(provided_types) = &seed.__seed_attr__.column_types {
        Some(parse_provided_types(provided_types, type_ops.as_ref())?)
    } else {
        None
    };
    let target_schema = adapt_seed_schema(&seed, &source_schema, ctx.adapter_type, &[])?;
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
    let (source_schema, maybe_batches) = read_parquet_seed_view(&seed_path, true).await?;
    let logical_fields = if let Some(provided_types) = &seed.__seed_attr__.column_types {
        Some(parse_provided_types(provided_types, type_ops.as_ref())?)
    } else {
        None
    };
    let target_schema = adapt_seed_schema(&seed, &source_schema, ctx.adapter_type, &[])?;
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
/// An error does not abort the run. It is returned against its seed, for the run's
/// `TaskRunnerCtxInner::seed_registration_errors`, and that seed's render fails with it.
/// So the reason is the seed's own result, the other seeds and models still run, and
/// a schema an earlier run registered for the seed cannot stand in for this one.
#[allow(clippy::too_many_arguments)]
pub async fn pre_register_seeds(
    sorted_nodes: &[&String],
    seeds: &BTreeMap<String, Arc<DbtSeed>>,
    adapter_type: AdapterType,
    schema_cache: Arc<dyn SchemaStoreTrait>,
    data_store: Arc<dyn DataStoreTrait>,
    type_ops: Arc<dyn TypeOps>,
    in_dir: &Path,
) -> (Vec<RegisteredSeed>, Vec<(String, Box<FsError>)>) {
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
    let mut errors = Vec::new();
    for (unique_id, handle) in handles {
        match handle.await {
            Ok(Ok(registered)) => results.push(registered),
            // Returned against the seed, whose render fails with it — said once, there,
            // as it was raised (its location and causes with it).
            Ok(Err(e)) => errors.push((unique_id, e)),
            // A registration that panicked failed too: not left to an earlier schema.
            Err(join_err) => errors.push((
                unique_id,
                fs_err!(
                    ErrorCode::FailedToRegisterSeedTable,
                    "seed registration panicked: {join_err}"
                ),
            )),
        }
    }
    (results, errors)
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

/// The seed's columns as the adapter will name them, refused if two of them would be one
/// column. Every seed format goes through here, so none skips the check.
///
/// Two header names are one column when they are equal once adapted. That means trimmed
/// (`id`, ` id`), folded under `quote_columns: false` (`Id`, `ID`), or, on an adapter
/// that compares names without case even when they are quoted (DuckDB), equal in all but
/// case. Written on, such columns share a name, and the agate table the seed's
/// materialization reads keeps only one of them: a column is lost without a word
/// (ferrion #533, #552). Every group is named at once, so one fix of the header settles
/// them. Each name is given as the header writes it (`header`, when the reader has it).
fn adapt_seed_schema(
    seed: &DbtSeed,
    source: &SchemaRef,
    adapter_type: AdapterType,
    header: &[String],
) -> FsResult<SchemaRef> {
    let strategy = infer_seed_column_name_strategy(seed.__seed_attr__.quote_columns, adapter_type);
    let adapted = adapt_schema(source.clone(), strategy);
    let folds = !matches!(strategy, InferColumnNameStrategy::Verbatim);
    let groups = header_collisions(source, &adapted, ignores_case(adapter_type));
    if groups.is_empty() {
        return Ok(adapted);
    }
    let written = |i: usize| {
        header
            .get(i)
            .filter(|_| header.len() == source.fields().len())
    };
    Err(fs_err!(
        ErrorCode::FailedToRegisterSeedTable,
        "seed {}: {}; rename all but one of each in the header",
        seed.__common_attr__.name,
        groups
            .iter()
            .map(|group| {
                let spellings: Vec<Spelling> = group
                    .columns
                    .iter()
                    .map(|&i| Spelling {
                        read: source.field(i).name().clone(),
                        written: written(i).cloned(),
                    })
                    .collect();
                collision_message(&spellings, &group.name, folds)
            })
            .collect::<Vec<_>>()
            .join("; ")
    ))
}

/// Whether the adapter compares column names without case even when they are quoted:
/// there, `"Id"` and `"ID"` are one column, as no unquoted name could tell them apart.
fn ignores_case(adapter_type: AdapterType) -> bool {
    matches!(adapter_type, AdapterType::DuckDB)
}

/// One header name in a collision: as the reader named it, and as the header writes
/// it when that differs (the reader renames a repeat, `id,id` to `id`, `id_2`).
struct Spelling {
    read: String,
    written: Option<String>,
}

impl Spelling {
    fn display(&self) -> String {
        match &self.written {
            Some(written) if written != &self.read => {
                format!("{written:?} (a repeat, which dbt reads as {:?})", self.read)
            }
            _ => format!("{:?}", self.read),
        }
    }
}

/// What one collision is: the header's names, the column they become, and what made
/// them one (trimming, folding under `quote_columns: false`, or a case-blind adapter).
fn collision_message(spellings: &[Spelling], name: &str, folds: bool) -> String {
    let read: Vec<&str> = spellings.iter().map(|s| s.read.as_str()).collect();
    let shown: Vec<String> = spellings.iter().map(Spelling::display).collect();
    if read.iter().all(|s| *s == read[0]) {
        return format!(
            "column {} appears {} times in the header",
            shown[0],
            spellings.len()
        );
    }
    let trimmed = read.iter().any(|s| s.trim() != *s);
    let cased = read.iter().any(|s| s.trim() != read[0].trim());
    let list = match shown.as_slice() {
        [a, b] => format!("{a} and {b}"),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
        [] => String::new(),
    };
    let verb = if spellings.len() == 2 { "both" } else { "all" };
    match (trimmed, cased, folds) {
        (_, true, false) => format!(
            "columns {list} are one column {}to DuckDB, which compares names without case \
             even when they are quoted",
            if trimmed { "once trimmed, " } else { "" }
        ),
        (true, true, true) => format!(
            "columns {list} are {verb} {name:?} once their names are trimmed and folded \
             (quote_columns: false)"
        ),
        (true, false, _) => {
            format!("columns {list} are {verb} {name:?} once their names are trimmed")
        }
        _ => format!(
            "columns {list} are {verb} {name:?} once their names are folded (quote_columns: false)"
        ),
    }
}

/// A column name the adapted schema gives more than one header name.
#[derive(Debug, PartialEq)]
struct Collision {
    /// The adapted name of the group's first column.
    name: String,
    /// The group's columns, by position, in header order.
    columns: Vec<usize>,
}

/// Every name the adapted schema gives more than one header name, in header order.
/// Names are compared as the adapter compares them: without case when `ignores_case`.
fn header_collisions(source: &Schema, adapted: &Schema, ignores_case: bool) -> Vec<Collision> {
    debug_assert_eq!(source.fields().len(), adapted.fields().len());
    let key = |name: &str| {
        if ignores_case {
            name.to_lowercase()
        } else {
            name.to_string()
        }
    };
    let mut groups: Vec<(String, Collision)> = Vec::new();
    for (i, field) in adapted.fields().iter().enumerate() {
        let k = key(field.name());
        match groups.iter_mut().find(|(gk, _)| *gk == k) {
            Some((_, group)) => group.columns.push(i),
            None => groups.push((
                k,
                Collision {
                    name: field.name().clone(),
                    columns: vec![i],
                },
            )),
        }
    }
    groups
        .into_iter()
        .map(|(_, group)| group)
        .filter(|group| group.columns.len() > 1)
        .collect()
}

#[cfg(test)]
mod folded_collision_tests {
    use super::*;

    fn schema(names: &[&str]) -> Arc<Schema> {
        Arc::new(Schema::new(
            names
                .iter()
                .map(|n| Field::new(*n, DataType::Int64, true))
                .collect::<Vec<_>>(),
        ))
    }

    /// The groups, as (the header's names, the column they become).
    fn groups(
        names: &[&str],
        strategy: InferColumnNameStrategy,
        ignores_case: bool,
    ) -> Vec<(Vec<String>, String)> {
        let source = schema(names);
        let adapted = adapt_schema(source.clone(), strategy);
        header_collisions(&source, &adapted, ignores_case)
            .into_iter()
            .map(|g| {
                let names = g.columns.iter().map(|&i| names[i].to_string()).collect();
                (names, g.name)
            })
            .collect()
    }

    fn pair(a: &[&str], name: &str) -> (Vec<String>, String) {
        (a.iter().map(|s| s.to_string()).collect(), name.to_string())
    }

    #[test]
    fn a_folded_pair_is_found_with_both_spellings() {
        use InferColumnNameStrategy::*;
        assert_eq!(
            groups(&["Id", "name", "ID"], Lowercase, false),
            vec![pair(&["Id", "ID"], "id")]
        );
        // Snowflake folds up: the same pair collides there too.
        assert_eq!(
            groups(&["Id", "name", "ID"], Uppercase, false),
            vec![pair(&["Id", "ID"], "ID")]
        );
    }

    #[test]
    fn every_group_is_found_at_once() {
        use InferColumnNameStrategy::*;
        assert_eq!(
            groups(&["Id", "ID", "Name", "NAME"], Lowercase, false),
            vec![pair(&["Id", "ID"], "id"), pair(&["Name", "NAME"], "name")]
        );
        assert_eq!(
            groups(&["Id", "ID", "iD"], Lowercase, false),
            vec![pair(&["Id", "ID", "iD"], "id")]
        );
    }

    #[test]
    fn quoted_names_differing_in_case_collide_only_where_case_is_ignored() {
        use InferColumnNameStrategy::*;
        assert_eq!(groups(&["Id", "ID"], Verbatim, false), vec![]);
        assert_eq!(
            groups(&["Id", "ID"], Verbatim, true),
            vec![pair(&["Id", "ID"], "Id")]
        );
    }

    #[test]
    fn trimmed_and_case_groups_are_found_in_one_check() {
        use InferColumnNameStrategy::*;
        // #552: under the default quoting, the trimmed pair and the case pair are both
        // named, not one a run.
        assert_eq!(
            groups(&["id", " id", "Name", "NAME"], Verbatim, true),
            vec![pair(&["id", " id"], "id"), pair(&["Name", "NAME"], "Name")]
        );
        assert_eq!(
            groups(&["id", " id"], Verbatim, false),
            vec![pair(&["id", " id"], "id")]
        );
    }

    fn message(read: &[&str], written: Option<&[&str]>, name: &str, folds: bool) -> String {
        let spellings: Vec<Spelling> = read
            .iter()
            .enumerate()
            .map(|(i, r)| Spelling {
                read: r.to_string(),
                written: written.map(|w| w[i].to_string()),
            })
            .collect();
        collision_message(&spellings, name, folds)
    }

    #[test]
    fn the_message_says_what_made_the_names_one() {
        assert_eq!(
            message(&["Id", "ID"], None, "id", true),
            "columns \"Id\" and \"ID\" are both \"id\" once their names are folded (quote_columns: false)"
        );
        assert_eq!(
            message(&["Id", "ID", "iD"], None, "id", true),
            "columns \"Id\", \"ID\" and \"iD\" are all \"id\" once their names are folded (quote_columns: false)"
        );
        assert_eq!(
            message(&["id", " id"], None, "id", false),
            "columns \"id\" and \" id\" are both \"id\" once their names are trimmed"
        );
        assert_eq!(
            message(&["Id", " id"], None, "id", true),
            "columns \"Id\" and \" id\" are both \"id\" once their names are trimmed and folded (quote_columns: false)"
        );
        assert_eq!(
            message(&["Id", "ID"], None, "Id", false),
            "columns \"Id\" and \"ID\" are one column to DuckDB, which compares names without case even when they are quoted"
        );
        assert_eq!(
            message(&["Id", " ID"], None, "Id", false),
            "columns \"Id\" and \" ID\" are one column once trimmed, to DuckDB, which compares names without case even when they are quoted"
        );
        assert_eq!(
            message(&["id", "id"], None, "id", false),
            "column \"id\" appears 2 times in the header"
        );
    }

    #[test]
    fn a_repeat_the_reader_renamed_is_named_as_written() {
        // `id,id,ID_2` under quote_columns: false: the reader names the second `id`
        // `id_2`, which then folds onto `ID_2`. The header never says `id_2`.
        assert_eq!(
            message(&["id_2", "ID_2"], Some(&["id", "ID_2"]), "id_2", true),
            "columns \"id\" (a repeat, which dbt reads as \"id_2\") and \"ID_2\" are both \"id_2\" once their names are folded (quote_columns: false)"
        );
    }

    #[test]
    fn the_csv_reader_keeps_the_header_as_written() {
        let dir = std::env::temp_dir().join(format!("seed-header-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dup.csv");
        std::fs::write(&path, "id,id,ID_2\n1,2,3\n").unwrap();
        let read = read_to_arrow_records(&path, &CustomCsvOptions::default()).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let names: Vec<&str> = read
            .schema
            .fields()
            .iter()
            .map(|f| f.name().as_str())
            .collect();
        assert_eq!(names, ["id", "id_2", "ID_2"]);
        assert_eq!(read.header, ["id", "id", "ID_2"]);
    }
}
