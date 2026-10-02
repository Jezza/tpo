use crate::units::{Percent, Rate};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Per-direction link properties. `None` means "not set" (inherited when merging).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LinkProps {
    pub delay: Option<Duration>,
    pub jitter: Option<Duration>,
    pub loss: Option<Percent>,
    pub duplicate: Option<Percent>,
    pub reorder: Option<Percent>,
    pub corrupt: Option<Percent>,
    pub rate: Option<Rate>,
}

impl LinkProps {
    /// Overrides every property that `other` sets.
    pub fn merge(&mut self, other: &LinkProps) {
        macro_rules! take {
            ($($field:ident),*) => { $( if other.$field.is_some() { self.$field = other.$field; } )* };
        }
        take!(delay, jitter, loss, duplicate, reorder, corrupt, rate);
    }

    /// Checks combinations that netem would reject.
    pub fn validate(&self) -> Result<(), String> {
        let has_delay = self.delay.is_some_and(|d| !d.is_zero());
        if self.jitter.is_some_and(|j| !j.is_zero()) && !has_delay {
            return Err("`jitter` needs a non-zero `delay`".to_owned());
        }
        if self.reorder.is_some_and(|r| !r.is_zero()) && !has_delay {
            return Err("`reorder` needs a non-zero `delay`".to_owned());
        }
        Ok(())
    }

    /// True when no property has an effect.
    pub fn is_noop(&self) -> bool {
        self.delay.is_none_or(|d| d.is_zero())
            && self.jitter.is_none_or(|d| d.is_zero())
            && self.loss.is_none_or(Percent::is_zero)
            && self.duplicate.is_none_or(Percent::is_zero)
            && self.reorder.is_none_or(Percent::is_zero)
            && self.corrupt.is_none_or(Percent::is_zero)
            && self.rate.is_none_or(|r| r.0 == 0)
    }
}
