use std::ops::Sub;

use crate::{parser, remove_string_from_config};

/// Utility data structure to display one of several strings based on a value in
/// a range, like a volume icon.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Ramp {
    icons: Vec<String>,
}

impl Ramp {
    /// Creates an empty instance (no icons).
    ///
    /// When [`Ramp::choose`] is called on an empty ramp, it will always return
    /// an empty string.
    #[must_use]
    pub const fn empty() -> Self {
        Self { icons: Vec::new() }
    }

    /// Given a value and a range, chooses the appropriate icon.
    pub fn choose<T>(&self, value: T, min: T, max: T) -> String
    where
        T: Sub + Copy,
        f64: From<T>,
    {
        // prevent division by zero
        if self.icons.is_empty() {
            return String::new();
        }
        let min = f64::from(min);
        let max = f64::from(max);
        let mut prop = (f64::from(value) - min) / (max - min);
        if prop < min {
            prop = min;
        }
        if prop > max {
            prop = max;
        }
        let idx = prop * (self.icons.len()) as f64;
        self.icons
            .get((idx.trunc() as usize).min(self.icons.len() - 1))
            .unwrap()
            .clone()
    }

    /// Parses a new instance with a given name from the global
    /// [`Config`][config::Config].
    ///
    /// Ramps should be defined in a table called `[ramps]`. Each ramp should be
    /// a table with keys ranging from 0 to any number. The values should be
    /// [pango] markup strings.
    #[must_use]
    pub fn parse(name: impl AsRef<str>) -> Option<Self> {
        let ramps_table = parser::RAMPS.get().unwrap();
        let mut ramp_table =
            ramps_table.get(name.as_ref())?.clone().into_table().ok()?;
        let mut key = 0;
        let mut icons = Vec::new();
        while let Some(icon) =
            remove_string_from_config(&key.to_string(), &mut ramp_table)
        {
            icons.push(icon);
            key += 1;
        }
        Some(Self { icons })
    }
}

impl From<Vec<String>> for Ramp {
    fn from(icons: Vec<String>) -> Self {
        Self { icons }
    }
}

impl FromIterator<String> for Ramp {
    fn from_iter<T: IntoIterator<Item = String>>(iter: T) -> Self {
        Self {
            icons: iter.into_iter().collect(),
        }
    }
}

impl Extend<String> for Ramp {
    fn extend<T: IntoIterator<Item = String>>(&mut self, iter: T) {
        self.icons.extend(iter);
    }
}
