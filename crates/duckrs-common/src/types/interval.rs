// Ported from duckdb/src/include/duckdb/common/types/interval.hpp @ rust-port-baseline

/// `interval_t`: months, days and microseconds are kept separately, as in DuckDB.
#[derive(Debug, Clone, Copy, Default)]
#[repr(C)]
pub struct Interval {
    pub months: i32,
    pub days: i32,
    pub micros: i64,
}

impl Interval {
    pub const DAYS_PER_MONTH: i64 = 30;
    pub const MICROS_PER_DAY: i64 = 86_400_000_000;
    pub const MICROS_PER_MONTH: i64 = Self::DAYS_PER_MONTH * Self::MICROS_PER_DAY;

    pub const fn new(months: i32, days: i32, micros: i64) -> Self {
        Self { months, days, micros }
    }

    /// Normalizes to `(months, days, micros)` with `0 <= days < 30` and `0 <= micros < 1 day`,
    /// carrying micros into days and days into months and borrowing so remainders are never
    /// negative (`interval_t::Normalize`). Comparisons use this form, so
    /// `INTERVAL 30 DAY = INTERVAL 1 MONTH`.
    pub fn normalize(self) -> (i64, i64, i64) {
        let mut micros = self.micros;
        let mut carry_days = micros / Self::MICROS_PER_DAY;
        micros -= carry_days * Self::MICROS_PER_DAY;
        if micros < 0 {
            carry_days -= 1;
            micros += Self::MICROS_PER_DAY;
        }
        let mut days = i64::from(self.days) + carry_days;
        let mut carry_months = days / Self::DAYS_PER_MONTH;
        days -= carry_months * Self::DAYS_PER_MONTH;
        if days < 0 {
            carry_months -= 1;
            days += Self::DAYS_PER_MONTH;
        }
        (i64::from(self.months) + carry_months, days, micros)
    }

    /// Equality after normalization (`interval_t::operator==`).
    pub fn equals(self, other: Self) -> bool {
        (self.months == other.months && self.days == other.days && self.micros == other.micros)
            || self.normalize() == other.normalize()
    }

    /// Ordering after normalization (`interval_t::operator>` and friends).
    pub fn compare(self, other: Self) -> std::cmp::Ordering {
        self.normalize().cmp(&other.normalize())
    }
}

impl PartialEq for Interval {
    fn eq(&self, other: &Self) -> bool {
        self.equals(*other)
    }
}

impl Eq for Interval {}

impl PartialOrd for Interval {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Interval {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.compare(*other)
    }
}

impl std::hash::Hash for Interval {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.normalize().hash(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_equality() {
        let month = Interval::new(1, 0, 0);
        assert!(month.equals(Interval::new(0, 30, 0)));
        assert_eq!(month, Interval::new(0, 30, 0));
        assert!(month.equals(Interval::new(0, 0, Interval::MICROS_PER_MONTH)));
        assert!(!month.equals(Interval::new(0, 29, 0)));
        assert_eq!(Interval::new(0, 31, 0).normalize(), (1, 1, 0));
        // 1 month - 4 days borrows: it is 26 days, not "1 month, -4 days".
        assert_eq!(Interval::new(1, -4, 0).normalize(), (0, 26, 0));
        assert_eq!(
            Interval::new(1, -4, 0).compare(Interval::new(0, 27, 0)),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            Interval::new(0, 0, -1).normalize(),
            (-1, 29, Interval::MICROS_PER_DAY - 1)
        );
    }
}
