//! Business logic for the RDW fuel CSV export: merge-join, row widening,
//! CSV/ZIP assembly, fixed-window rate limiting, and column metadata
//! caching. No HTTP framework or Socrata HTTP details live here.

pub mod csv_writer;
pub mod merge;
pub mod metadata;
pub mod rate_limit;
pub mod widen;

pub use csv_writer::{assemble, cleanup, Assembled, Assembler, EXCEL_MAX_DATA_ROWS};
pub use merge::{merge_join, MergeJoinError, WidenedRow, MAX_FUEL_ENTRIES};
pub use metadata::{
    fallback_fuel_columns, fallback_vehicle_columns, load_column_metadata, ColumnMetadata,
};
pub use rate_limit::{RateLimitOutcome, RateLimiter, DAY_LIMIT, WEEK_LIMIT};
pub use widen::RowWidener;
