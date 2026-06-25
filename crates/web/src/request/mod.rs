use std::fmt;
use std::ops::Deref;

mod decode;
mod endpoint;
mod public;
mod state;

pub use decode::non_empty;
pub use public::{LinkOrigin, PageQuery, PageRequest, normalized_query};
pub use state::{PageState, SourceFilter, page_state};

pub(crate) use endpoint::{slice_query_from_uri, state_events_query_from_uri};
pub(crate) use public::{page_request_from_public_uri, public_uri};
#[cfg(test)]
pub(crate) use state::{DetailState, RefScope};

#[cfg(test)]
pub use public::page_request_from_public_url;

type ParseResult<T> = std::result::Result<T, RequestParseError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RequestParseError(String);

impl RequestParseError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for RequestParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Deref for RequestParseError {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(test)]
mod tests;
