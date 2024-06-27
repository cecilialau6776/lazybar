use std::{collections::HashMap, ops::Sub};

use config::{Config, Value};
use csscolorparser::Color;

/// Utility data structure to display one of several strings based on a value in
/// a range, like a volume icon.
#[derive(Clone)]
pub struct Ramp {
    icons: Vec<String>,
}

impl Ramp {
    /// Given a value and a range, chooses the appropriate icon.
    pub fn choose<T>(&self, value: T, min: T, max: T) -> String
    where
        T: Sub + Copy,
        f64: From<T>,
    {
        let prop = (f64::from(value) - f64::from(min))
            / (f64::from(max) - f64::from(min));
        let idx = prop * (self.icons.len()) as f64;
        self.icons
            .get((idx.trunc() as usize).min(self.icons.len() - 1))
            .unwrap()
            .clone()
    }

    /// Parses a new instance with a given name from the global [`Config`].
    ///
    /// Ramps should be defined in a table called `[ramps]`. Each ramp should be
    /// a table with keys ranging from 0 to any number. The values should be
    /// [pango] markup strings.
    pub fn parse(name: &str, global: &Config) -> Option<Self> {
        let ramps_table = global.get_table("ramps").ok()?;
        let ramp_table = ramps_table.get(name)?.clone().into_table().ok()?;
        let mut key = 0;
        let mut icons = Vec::new();
        while let Some(icon) = ramp_table.get(&key.to_string()) {
            if let Ok(icon) = icon.clone().into_string() {
                icons.push(icon);
                key += 1;
            } else {
                break;
            }
        }
        Some(Self { icons })
    }
}

impl FromIterator<String> for Ramp {
    fn from_iter<T: IntoIterator<Item = String>>(iter: T) -> Self {
        Self {
            icons: iter.into_iter().collect(),
        }
    }
}

/// Removes a value from a given config table and returns an attempt at parsing
/// it into a string
pub fn remove_string_from_config(
    id: &str,
    table: &mut HashMap<String, Value>,
) -> Option<String> {
    if let Some(val) = table.remove(id) {
        if let Ok(val) = val.clone().into_string() {
            Some(val)
        } else {
            log::warn!(
                "Ignoring non-string value {val:?} (location attempt: {:?})",
                val.origin()
            );
            None
        }
    } else {
        None
    }
}

/// Removes a value from a given config table and returns an attempt at parsing
/// it into a uint
pub fn remove_uint_from_config(
    id: &str,
    table: &mut HashMap<String, Value>,
) -> Option<u64> {
    if let Some(val) = table.remove(id) {
        if let Ok(val) = val.clone().into_uint() {
            Some(val)
        } else {
            log::warn!(
                "Ignoring non-uint value {val:?} (location attempt: {:?})",
                val.origin()
            );
            None
        }
    } else {
        None
    }
}

/// Removes a value from a given config table and returns an attempt at parsing
/// it into a bool
pub fn remove_bool_from_config(
    id: &str,
    table: &mut HashMap<String, Value>,
) -> Option<bool> {
    if let Some(val) = table.remove(id) {
        if let Ok(val) = val.clone().into_bool() {
            Some(val)
        } else {
            log::warn!(
                "Ignoring non-boolean value {val:?} (location attempt: {:?})",
                val.origin()
            );
            None
        }
    } else {
        None
    }
}

/// Removes a value from a given config table and returns an attempt at parsing
/// it into a float
pub fn remove_float_from_config(
    id: &str,
    table: &mut HashMap<String, Value>,
) -> Option<f64> {
    if let Some(val) = table.remove(id) {
        if let Ok(val) = val.clone().into_float() {
            Some(val)
        } else {
            log::warn!(
                "Ignoring non-float value {val:?} (location attempt: {:?})",
                val.origin()
            );
            None
        }
    } else {
        None
    }
}

/// Removes a value from a given config table and returns an attempt at parsing
/// it into a color
pub fn remove_color_from_config(
    id: &str,
    table: &mut HashMap<String, Value>,
) -> Option<Color> {
    if let Some(val) = table.remove(id) {
        if let Ok(val) = val.clone().into_string() {
            if let Ok(val) = val.parse() {
                Some(val)
            } else {
                log::warn!("Invalid color {val}");
                None
            }
        } else {
            log::warn!(
                "Ignoring non-string value {val:?} (location attempt: {:?})",
                val.origin()
            );
            None
        }
    } else {
        None
    }
}
