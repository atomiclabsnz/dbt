// The core of our telemetry system - a `tracing::Layer` impl that bridges the gap
// from tracing crate machinery to our telemetry layers.
pub mod data_layer;
pub mod jsonl_writer;
// ferrion-wasm: OTLP export (opentelemetry-otlp -> opentelemetry-http) is native only.
#[cfg(not(target_arch = "wasm32"))]
pub mod otlp;
pub mod parquet_writer;
pub mod pretty_writer;
