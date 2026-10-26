use std::fmt::{Display, Formatter};

/// How far back prices go, ending now
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Period {
    Day,
    Week,
    Month,
    Quarter,
    Year,
}

impl Period {
    pub fn days(self) -> u32 {
        match self {
            Period::Day => 1,
            Period::Week => 7,
            Period::Month => 30,
            Period::Quarter => 90,
            Period::Year => 365,
        }
    }

    pub fn millis(self) -> u64 {
        self.days() as u64 * 24 * 60 * 60 * 1000
    }
}

// As in "the past 7 days"
impl Display for Period {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Period::Day => "24 hours",
            Period::Week => "7 days",
            Period::Month => "30 days",
            Period::Quarter => "90 days",
            Period::Year => "year",
        })
    }
}
