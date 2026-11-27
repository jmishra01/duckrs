// Ported from duckdb/src/include/duckdb/main/client_properties.hpp @ rust-port-baseline

//! `ClientProperties`, as far as the Arrow conversion reads them (`CClientArrowOptionsWrapper`).

use duckrs::Connection;
use duckrs_common::value::ValueData;

/// The settings the Arrow export follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrowOptions {
    /// `arrow_lossless_conversion`: types Arrow has no match for go out as extension types
    pub lossless: bool,
    /// `produce_arrow_string_view`: VARCHAR as `Utf8View` (from format 1.4 on)
    pub string_view: bool,
    /// `arrow_large_buffer_size`: 64-bit offsets (`ArrowOffsetSize::LARGE`)
    pub large_offsets: bool,
    /// `arrow_output_list_view`: LIST as `ListView` (from format 1.4 on)
    pub list_view: bool,
    /// `arrow_output_version`: the minor version of format 1.x
    pub version: u8,
    /// `TimeZone`: the time zone of TIMESTAMP WITH TIME ZONE
    pub time_zone: String,
}

impl Default for ArrowOptions {
    fn default() -> Self {
        Self {
            lossless: false,
            string_view: false,
            large_offsets: false,
            list_view: false,
            version: 0,
            time_zone: "UTC".to_string(),
        }
    }
}

impl ArrowOptions {
    /// `ClientContext::GetClientProperties`: the options of `connection`'s settings now.
    pub fn of(connection: &Connection) -> Self {
        let defaults = Self::default();
        let flag = |name: &str, default: bool| match connection.setting(name).as_ref().map(|v| v.data()) {
            Some(ValueData::Bool(b)) => *b,
            _ => default,
        };
        let text = |name: &str| {
            connection
                .setting(name)
                .filter(|v| !v.is_null())
                .and_then(|v| v.try_to_string().ok())
        };
        let version = text("arrow_output_version")
            .and_then(|v| v.strip_prefix("1.").and_then(|minor| minor.parse().ok()))
            .unwrap_or(defaults.version);
        Self {
            lossless: flag("arrow_lossless_conversion", defaults.lossless),
            string_view: flag("produce_arrow_string_view", defaults.string_view),
            large_offsets: flag("arrow_large_buffer_size", defaults.large_offsets),
            list_view: flag("arrow_output_list_view", defaults.list_view),
            version,
            time_zone: text("TimeZone").filter(|tz| !tz.is_empty()).unwrap_or(defaults.time_zone),
        }
    }

    /// `arrow_output_version >= ArrowFormatVersion::V1_4`.
    pub fn at_least_1_4(&self) -> bool {
        self.version >= 4
    }
}
