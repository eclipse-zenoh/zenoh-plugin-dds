//
// Copyright (c) 2022 ZettaScale Technology
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ZettaScale Zenoh Team, <zenoh@zettascale.tech>
//
use std::{env, fmt, time::Duration};

use regex::Regex;
use serde::{de, de::Visitor, Deserialize, Deserializer};
use zenoh::key_expr::OwnedKeyExpr;

pub const DEFAULT_DOMAIN: u32 = 0;
pub const DEFAULT_FORWARD_DISCOVERY: bool = false;
pub const DEFAULT_RELIABLE_ROUTES_BLOCKING: bool = true;
pub const DEFAULT_QUERIES_TIMEOUT: f32 = 5.0;
pub const DEFAULT_DDS_LOCALHOST_ONLY: bool = false;

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub scope: Option<OwnedKeyExpr>,
    #[serde(default = "default_domain")]
    pub domain: u32,
    #[serde(default, deserialize_with = "deserialize_regex")]
    pub allow: Option<Regex>,
    #[serde(default, deserialize_with = "deserialize_regex")]
    pub deny: Option<Regex>,
    #[serde(default, deserialize_with = "deserialize_max_frequencies")]
    pub max_frequencies: Vec<(Regex, f32)>,
    #[serde(default)]
    pub generalise_subs: Vec<OwnedKeyExpr>,
    #[serde(default)]
    pub generalise_pubs: Vec<OwnedKeyExpr>,
    #[serde(default = "default_forward_discovery")]
    pub forward_discovery: bool,
    #[serde(default = "default_reliable_routes_blocking")]
    pub reliable_routes_blocking: bool,
    #[serde(default = "default_localhost_only")]
    pub localhost_only: bool,
    #[serde(default)]
    #[cfg(feature = "dds_shm")]
    pub shm_enabled: bool,
    #[serde(
        default = "default_queries_timeout",
        deserialize_with = "deserialize_duration"
    )]
    pub queries_timeout: Duration,
    __required__: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_path")]
    __path__: Option<Vec<String>>,
}

fn default_domain() -> u32 {
    if let Ok(s) = env::var("ROS_DOMAIN_ID") {
        s.parse::<u32>().unwrap_or(DEFAULT_DOMAIN)
    } else {
        DEFAULT_DOMAIN
    }
}

fn deserialize_path<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_option(OptPathVisitor)
}

struct OptPathVisitor;

impl<'de> serde::de::Visitor<'de> for OptPathVisitor {
    type Value = Option<Vec<String>>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(formatter, "none or a string or an array of strings")
    }

    fn visit_none<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(None)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(PathVisitor).map(Some)
    }
}

struct PathVisitor;

impl<'de> serde::de::Visitor<'de> for PathVisitor {
    type Value = Vec<String>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(formatter, "a string or an array of strings")
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(vec![v.into()])
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: de::SeqAccess<'de>,
    {
        let mut v = if let Some(l) = seq.size_hint() {
            Vec::with_capacity(l)
        } else {
            Vec::new()
        };
        while let Some(s) = seq.next_element()? {
            v.push(s);
        }
        Ok(v)
    }
}

fn deserialize_regex<'de, D>(deserializer: D) -> Result<Option<Regex>, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_any(RegexVisitor)
}

fn deserialize_max_frequencies<'de, D>(deserializer: D) -> Result<Vec<(Regex, f32)>, D::Error>
where
    D: Deserializer<'de>,
{
    let strs: Vec<String> = Deserialize::deserialize(deserializer)?;
    let mut result: Vec<(Regex, f32)> = Vec::with_capacity(strs.len());
    for s in strs {
        let i = s
            .find('=')
            .ok_or_else(|| de::Error::custom(format!("Invalid 'max_frequency': {s}")))?;
        let regex = Regex::new(&s[0..i]).map_err(|e| {
            de::Error::custom(format!("Invalid regex for 'max_frequency': '{s}': {e}"))
        })?;
        let frequency: f32 = s[i + 1..].parse().map_err(|e| {
            de::Error::custom(format!(
                "Invalid float value for 'max_frequency': '{s}': {e}"
            ))
        })?;
        result.push((regex, frequency));
    }
    Ok(result)
}

fn default_queries_timeout() -> Duration {
    Duration::from_secs_f32(DEFAULT_QUERIES_TIMEOUT)
}

fn deserialize_duration<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: Deserializer<'de>,
{
    let seconds: f32 = Deserialize::deserialize(deserializer)?;
    Ok(Duration::from_secs_f32(seconds))
}

fn default_forward_discovery() -> bool {
    DEFAULT_FORWARD_DISCOVERY
}

fn default_reliable_routes_blocking() -> bool {
    DEFAULT_RELIABLE_ROUTES_BLOCKING
}

fn default_localhost_only() -> bool {
    env::var("ROS_LOCALHOST_ONLY").as_deref() == Ok("1")
}

// Serde Visitor for Regex deserialization.
// It accepts either a String, either a list of Strings (that are concatenated with `|`)
struct RegexVisitor;

impl<'de> Visitor<'de> for RegexVisitor {
    type Value = Option<Regex>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str(r#"either a string or a list of strings"#)
    }

    // for `null` value
    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(None)
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Regex::new(value)
            .map(Some)
            .map_err(|e| de::Error::custom(format!("Invalid regex '{value}': {e}")))
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: de::SeqAccess<'de>,
    {
        let mut vec: Vec<String> = Vec::new();
        while let Some(s) = seq.next_element()? {
            vec.push(s);
        }
        let s: String = vec.join("|");
        Regex::new(&s)
            .map(Some)
            .map_err(|e| de::Error::custom(format!("Invalid regex '{s}': {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn test_path_field() {
        // See: https://github.com/eclipse-zenoh/zenoh-plugin-webserver/issues/19
        let config = serde_json::from_str::<Config>(r#"{"__path__": "/example/path"}"#);

        assert!(config.is_ok());
        let Config {
            __required__,
            __path__,
            ..
        } = config.unwrap();

        assert_eq!(__path__, Some(vec![String::from("/example/path")]));
        assert_eq!(__required__, None);
    }

    #[test]
    fn test_required_field() {
        // See: https://github.com/eclipse-zenoh/zenoh-plugin-webserver/issues/19
        let config = serde_json::from_str::<Config>(r#"{"__required__": true}"#);
        assert!(config.is_ok());
        let Config {
            __required__,
            __path__,
            ..
        } = config.unwrap();

        assert_eq!(__path__, None);
        assert_eq!(__required__, Some(true));
    }

    #[test]
    fn test_path_field_and_required_field() {
        // See: https://github.com/eclipse-zenoh/zenoh-plugin-webserver/issues/19
        let config = serde_json::from_str::<Config>(
            r#"{"__path__": "/example/path", "__required__": true}"#,
        );

        assert!(config.is_ok());
        let Config {
            __required__,
            __path__,
            ..
        } = config.unwrap();

        assert_eq!(__path__, Some(vec![String::from("/example/path")]));
        assert_eq!(__required__, Some(true));
    }

    #[test]
    fn test_no_path_field_and_no_required_field() {
        // See: https://github.com/eclipse-zenoh/zenoh-plugin-webserver/issues/19
        let config = serde_json::from_str::<Config>("{}");

        assert!(config.is_ok());
        let Config {
            __required__,
            __path__,
            ..
        } = config.unwrap();

        assert_eq!(__path__, None);
        assert_eq!(__required__, None);
    }
}
