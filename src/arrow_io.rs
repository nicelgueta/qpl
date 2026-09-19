//! `DataFrame` <-> Arrow IPC *stream* bytes.
//!
//! This is the data boundary for the browser build: a host that has no
//! filesystem (`load` is unavailable on wasm) hands tables in as Arrow IPC and
//! gets query results back the same way, instead of scraping the pretty-printed
//! text the REPL would show. Compiled only with the `wasm` feature, but it has no
//! browser dependency, so its tests run on the host
//! (`cargo test --no-default-features --features wasm`).
//!
//! It goes through `polars-arrow`'s `io_ipc` directly rather than polars'
//! `ipc` / `ipc_streaming` features: the former forces `streaming` (and with it
//! the cloud stack) and the latter enables IPC compression, which pulls in the
//! C `lz4-sys` / `zstd-sys` builds. Neither builds for wasm. So:
//!
//! * **writing** never compresses, and
//! * **reading** rejects compressed bodies (a JS `apache-arrow` writer doesn't
//!   compress by default, so this is not something a caller hits by accident).

use polars::prelude::*;
use polars_arrow::array::new_empty_array;
use polars_arrow::io::ipc::read::{StreamReader, StreamState, read_stream_metadata};
use polars_arrow::io::ipc::write::{StreamWriter, WriteOptions};
use std::io::Cursor;

use crate::errors::QplError;
use crate::vm::Vm;

fn rt(e: impl std::fmt::Display) -> QplError {
    QplError::Runtime(e.to_string())
}

/// Serialise `df` as an Arrow IPC stream.
///
/// Uses `CompatLevel::oldest()`, so text columns go out as `LargeUtf8` rather
/// than the newer `Utf8View`, which most Arrow implementations (including
/// `apache-arrow` for JS) can't read yet. Categoricals go out dictionary-encoded.
pub fn df_to_ipc(df: &DataFrame) -> Result<Vec<u8>, QplError> {
    let compat = CompatLevel::oldest();
    // `iter_chunks` requires equal chunk layout across columns.
    let mut df = df.clone();
    df.rechunk_mut();

    let schema: ArrowSchema = df.schema().to_arrow(compat);
    let mut writer = StreamWriter::new(Vec::new(), WriteOptions { compression: None });
    writer.start(&schema, None).map_err(rt)?;
    for batch in df.iter_chunks(compat, false) {
        writer.write(&batch, None).map_err(rt)?;
    }
    writer.finish().map_err(rt)?;
    Ok(writer.into_inner())
}

/// Parse an Arrow IPC stream into a `DataFrame`. All record batches are
/// concatenated; a stream with a schema but no batches gives an empty frame
/// with the right columns.
pub fn ipc_to_df(bytes: &[u8]) -> Result<DataFrame, QplError> {
    let mut cursor = Cursor::new(bytes);
    let meta = read_stream_metadata(&mut cursor).map_err(rt)?;
    let fields: Vec<_> = meta.schema.iter_values().cloned().collect();
    let mut columns: Vec<Vec<ArrayRef>> = vec![Vec::new(); fields.len()];

    for state in StreamReader::new(cursor, meta, None) {
        match state.map_err(rt)? {
            StreamState::Some(batch) => {
                for (chunks, array) in columns.iter_mut().zip(batch.into_arrays()) {
                    chunks.push(array);
                }
            }
            // A byte slice is never "waiting for more": treat it as the end.
            StreamState::Waiting => break,
        }
    }

    let mut cols = Vec::with_capacity(fields.len());
    for (field, mut chunks) in fields.iter().zip(columns) {
        if chunks.is_empty() {
            chunks.push(new_empty_array(field.dtype().clone()));
        }
        let series = Series::from_arrow_chunks(field.name().clone(), chunks).map_err(rt)?;
        cols.push(Column::from(series));
    }
    let height = cols.first().map_or(0, |c| c.len());
    DataFrame::new(height, cols).map_err(rt)
}

/// A table name a host may bind: a plain identifier. Anything with a dot would
/// land in (or collide with) a namespace, and the rest can't be typed in a query.
pub fn is_valid_table_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Bind the table encoded in `ipc` as `name`, replacing any table, lazy frame
/// or scalar already bound to it.
pub fn register_table(vm: &mut Vm, name: &str, ipc: &[u8]) -> Result<(), QplError> {
    if !is_valid_table_name(name) {
        return Err(QplError::Runtime(format!(
            "invalid table name '{name}': use letters, digits and underscores, not starting with a digit"
        )));
    }
    let df = ipc_to_df(ipc)?;
    vm.bind_table(name.to_string(), df)
}

/// Number of rows in the table (or lazy frame) bound to `name`, the way a host
/// UI needs it for paging. Not the language's `count`, which counts the non-null
/// values of a table's first column.
pub fn row_count(vm: &Vm, name: &str) -> Result<usize, QplError> {
    if let Some(df) = vm.tables.get(name) {
        return Ok(df.height());
    }
    if let Some(lf) = vm.lazy_frames.get(name) {
        let n = lf.clone().select([len().alias("n")]).collect().map_err(rt)?;
        return n.column("n").map_err(rt)?.u32().map_err(rt)?.get(0)
            .map(|n| n as usize)
            .ok_or_else(|| QplError::Runtime(format!("could not count the rows of '{name}'")));
    }
    Err(QplError::Runtime(format!("unknown table '{name}'")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repl::eval_capture_table;

    fn sample() -> DataFrame {
        df![
            "sym"   => ["AAPL", "MSFT", "AAPL"],
            "price" => [1.5f64, 2.5, 3.5],
            "size"  => [10i64, 20, 30],
            "flag"  => [true, false, true],
        ].unwrap()
    }

    #[test]
    fn round_trips_a_frame() {
        let df = sample();
        let back = ipc_to_df(&df_to_ipc(&df).unwrap()).unwrap();
        assert_eq!(back.shape(), df.shape());
        assert_eq!(back.get_column_names(), df.get_column_names());
        assert!(back.equals(&df));
    }

    #[test]
    fn round_trips_nulls_and_an_empty_frame() {
        let df = df!["a" => [Some(1i64), None, Some(3)], "b" => [Some("x"), None, Some("z")]].unwrap();
        let back = ipc_to_df(&df_to_ipc(&df).unwrap()).unwrap();
        assert!(back.equals_missing(&df));

        let empty = df.head(Some(0));
        let back = ipc_to_df(&df_to_ipc(&empty).unwrap()).unwrap();
        assert_eq!(back.shape(), (0, 2));
        assert_eq!(back.dtypes(), df.dtypes());
    }

    #[test]
    fn text_columns_are_written_as_large_utf8_not_views() {
        let ipc = df_to_ipc(&sample()).unwrap();
        let mut cursor = Cursor::new(ipc.as_slice());
        let meta = read_stream_metadata(&mut cursor).unwrap();
        let sym = meta.schema.iter_values().find(|f| f.name().as_str() == "sym").unwrap();
        assert_eq!(sym.dtype(), &ArrowDataType::LargeUtf8);
    }

    #[test]
    fn rejects_garbage() {
        assert!(ipc_to_df(b"not arrow").is_err());
    }

    #[test]
    fn table_names_must_be_plain_identifiers() {
        for ok in ["t", "_t", "trades2", "a_b"] {
            assert!(is_valid_table_name(ok), "{ok}");
        }
        for bad in ["", "2t", "a-b", "a b", ".ns.t", "t;drop"] {
            assert!(!is_valid_table_name(bad), "{bad}");
        }
    }

    #[test]
    fn registered_table_is_queryable_and_comes_back_as_ipc() {
        let mut vm = Vm::new();
        register_table(&mut vm, "trades", &df_to_ipc(&sample()).unwrap()).unwrap();

        let r = eval_capture_table("select from trades where price > 2", &mut vm);
        assert_eq!(r.error, None);
        let df = r.table.expect("a select returns a table");
        assert_eq!(df.height(), 2);
        // the table came back as data, not as pretty-printed output
        assert_eq!(r.output, "");
        assert_eq!(ipc_to_df(&df_to_ipc(&df).unwrap()).unwrap().height(), 2);
    }

    #[test]
    fn row_count_counts_rows_not_non_null_values() {
        let mut vm = Vm::new();
        let df = df!["a" => [Some("x"), None, Some("z")], "b" => [1i64, 2, 3]].unwrap();
        register_table(&mut vm, "t", &df_to_ipc(&df).unwrap()).unwrap();
        assert_eq!(row_count(&vm, "t").unwrap(), 3);
        assert!(row_count(&vm, "nope").is_err());
        // a lazy binding is counted too
        eval_capture_table("l: lazy select from t where b > 1", &mut vm);
        assert_eq!(row_count(&vm, "l").unwrap(), 2);
    }

    #[test]
    fn registering_again_replaces_the_table() {
        let mut vm = Vm::new();
        register_table(&mut vm, "t", &df_to_ipc(&sample()).unwrap()).unwrap();
        register_table(&mut vm, "t", &df_to_ipc(&sample().head(Some(1))).unwrap()).unwrap();
        let r = eval_capture_table("select from t", &mut vm);
        assert_eq!(r.table.unwrap().height(), 1);
    }

    #[test]
    fn a_bad_name_or_payload_is_an_error_and_binds_nothing() {
        let mut vm = Vm::new();
        assert!(register_table(&mut vm, "2bad", &df_to_ipc(&sample()).unwrap()).is_err());
        assert!(register_table(&mut vm, "ok", b"junk").is_err());
        assert!(vm.tables.is_empty());
    }
}
