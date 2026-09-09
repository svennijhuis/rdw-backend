//! Business logic for the RDW fuel CSV export: merge-join, row widening,
//! CSV/ZIP assembly, fixed-window rate limiting, and column metadata
//! caching. No HTTP framework or Socrata HTTP details live here.

pub mod abort;
pub mod csv_writer;
pub mod failure;
pub mod merge;
pub mod metadata;
pub mod ranges;
pub mod rate_limit;
pub mod widen;

pub use abort::GlobalAbort;
pub use csv_writer::{assemble, cleanup, Assembled, Assembler, EXCEL_MAX_DATA_ROWS};
pub use failure::{FailedRange, FailureConfig, FuelFailureSummary};
pub use merge::{merge_join, ExportStatus, MergeJoinError, WidenedRow, MAX_FUEL_ENTRIES};
pub use metadata::{
    fallback_fuel_columns, fallback_vehicle_columns, load_column_metadata, Column, ColumnMetadata,
};
pub use ranges::{fixed_two_char_bands, KentekenRange};
pub use rate_limit::{RateLimitOutcome, RateLimiter, DAY_LIMIT, WEEK_LIMIT};
pub use widen::{RowWidener, BRANDSTOF_HEADER, EXPORT_STATUS_HEADER};
