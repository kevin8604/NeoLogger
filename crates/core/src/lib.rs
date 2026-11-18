pub mod bits;
pub mod correlate;
pub mod filter;
pub mod matcher;
pub mod merge;
pub mod persist;
pub mod plugin;
pub mod session;
pub mod source;
pub mod style;
pub mod text;
pub mod time;

pub use bits::Bitmap;
pub use correlate::{correlation_ids, line_has_id, CorrelationId};
pub use filter::{parse_query, Filter, MatchCtx, Query};
pub use persist::{MatcherSnapshot, SessionSnapshot};
pub use plugin::{PluginRegistry, Severity};
pub use session::{
    Base, BitRows, BitsSource, ReloadMode, ReloadReport, Rows, Session, ViewId, ViewNode,
};
pub use source::{DocId, LineRef, LogDoc, TextEncoding};
pub use style::{HighlightRule, Rgb, RowStyle, RuleScope, StyledSpan, TextStyle};
pub use time::format_ts;
