// Ported from duckdb/third_party/fmt/include/fmt/format-inl.h and format.h (fmt 6.1.2 as DuckDB
// bundles it) @ rust-port-baseline

//! What `duckdb_fmt::format` prints for a finite FLOAT or DOUBLE.
//!
//! This is Grisu with fmt's quirks, kept on purpose, as these are the digits DuckDB prints:
//!
//! - Grisu walks down from the upper boundary, so of two shortest forms it can pick the one that
//!   is not closest (`8.124038696289062`, whose exact value ends in ...0625).
//! - A FLOAT uses its own boundaries in Grisu, but when Grisu gives up, `fallback_format` sees only
//!   the DOUBLE it was promoted to (`TODO: handle float` in fmt). So some FLOATs print the shortest
//!   DOUBLE instead: `63.9921875`, and not the `63.992188` that reads back as the same FLOAT.
//! - `fallback_format` starts from the exponent of the upper boundary, and scales a power of two
//!   from 2^53 up by the wrong amount (`denominator <<= 1` for a shift of 2).
//! - `Bigint` is copied whole: a remainder of zero keeps its `exp`, and so compares as non-zero, and
//!   a borrow does not move past a zero bigit.
//!
//! - With a precision, `FixedHandler` only checks the error at the last digit. The error is a `u64`
//!   that is multiplied by 10 for each digit, so past some 300 digits it has wrapped to zero, and
//!   the digits of a large number stop being exact: `format('{:.0f}', 1.7976931348623157e308)`
//!   goes wrong from the twentieth digit on.
//! - When Grisu gives up, `snprintf_digits` falls back to `snprintf`. A precision of 0 then means
//!   the default of 6 in a general format (`format('{:.0}', 2.5)` is `2.5`), and with `#` the
//!   decimal point lands in the digits and gets printed twice (`printf('%#.0f', 2.5)` is `2..`).
//!
//! Unsigned arithmetic wraps as in C++. The digits are bytes as they can run past `'9'`.

/// A number `f * 2^e`.
#[derive(Clone, Copy)]
struct Fp {
    f: u64,
    e: i32,
}

/// Bits in a DOUBLE's significand, without the implicit one.
const DOUBLE_SIGNIFICAND_SIZE: i32 = 52;
const IMPLICIT_BIT: u64 = 1 << DOUBLE_SIGNIFICAND_SIZE;
/// Bits in `Fp::f`.
const SIGNIFICAND_SIZE: i32 = 64;

/// Scales `value`, which comes from a double multiplied by `1 << shift`, so that bit 63 is set.
fn normalize(mut value: Fp, shift: i32) -> Fp {
    let shifted_implicit_bit = IMPLICIT_BIT << shift;
    // Handle subnormals.
    while value.f & shifted_implicit_bit == 0 {
        value.f <<= 1;
        value.e -= 1;
    }
    let offset = SIGNIFICAND_SIZE - DOUBLE_SIGNIFICAND_SIZE - shift - 1;
    value.f <<= offset;
    value.e -= offset;
    value
}

/// `fp::assign`: the value, and whether its predecessor is closer than its successor.
fn assign(d: f64) -> (Fp, bool) {
    let bits = d.to_bits();
    let mut f = bits & (IMPLICIT_BIT - 1);
    let mut biased_e = ((bits & (!0u64 >> 1) & !(IMPLICIT_BIT - 1)) >> DOUBLE_SIGNIFICAND_SIZE) as i32;
    // Predecessor is closer if d is a normalized power of 2 (f == 0) other than the smallest
    // normalized number (biased_e > 1).
    let is_predecessor_closer = f == 0 && biased_e > 1;
    if biased_e != 0 {
        f += IMPLICIT_BIT;
    } else {
        biased_e = 1; // Subnormals use biased exponent 1 (min exponent).
    }
    let e = biased_e - 1023 - DOUBLE_SIGNIFICAND_SIZE;
    (Fp { f, e }, is_predecessor_closer)
}

/// `fp::assign_with_boundaries`: the value, and the significands of its lower and upper boundaries.
/// Both have the exponent of the normalized upper boundary.
fn assign_with_boundaries(d: f64) -> (Fp, u64, u64) {
    let (value, is_lower_closer) = assign(d);
    let Fp { f, e } = value;
    let mut lower = if is_lower_closer {
        Fp {
            f: (f << 2) - 1,
            e: e - 2,
        }
    } else {
        Fp {
            f: (f << 1) - 1,
            e: e - 1,
        }
    };
    // 1 accounts for the exponent shift above.
    let upper = normalize(
        Fp {
            f: (f << 1) + 1,
            e: e - 1,
        },
        1,
    );
    lower.f <<= lower.e - upper.e;
    (value, lower.f, upper.f)
}

/// `fp::assign_float_with_boundaries`: the same, for a FLOAT promoted to the double `d`.
fn assign_float_with_boundaries(d: f64) -> (Fp, u64, u64) {
    let (value, _) = assign(d);
    let Fp { f, e } = value;
    const MIN_NORMAL_E: i32 = f32::MIN_EXP - f64::MANTISSA_DIGITS as i32;
    let mut half_ulp: u64 = 1 << (f64::MANTISSA_DIGITS - f32::MANTISSA_DIGITS - 1);
    if MIN_NORMAL_E > e {
        half_ulp <<= MIN_NORMAL_E - e;
    }
    let upper = normalize(Fp { f: f + half_ulp, e }, 0);
    let lower_shift = i32::from(f == IMPLICIT_BIT && e > MIN_NORMAL_E);
    let lower = (f - (half_ulp >> lower_shift)) << (e - upper.e);
    (value, lower, upper.f)
}

/// `lhs * rhs / 2^64` rounded to nearest with half-up tie breaking.
fn multiply(lhs: u64, rhs: u64) -> u64 {
    let product = u128::from(lhs) * u128::from(rhs);
    let f = (product >> 64) as u64;
    if product as u64 & (1 << 63) != 0 {
        f.wrapping_add(1)
    } else {
        f
    }
}

/// A cached power of 10 whose binary exponent is in `min_exponent..=min_exponent + 28`, and its
/// decimal exponent.
fn cached_power(min_exponent: i32) -> (Fp, i32) {
    const ONE_OVER_LOG2_10: u64 = 0x4d104d42; // round(pow(2, 32) / log2(10))
    let index = (i64::from(min_exponent + SIGNIFICAND_SIZE - 1) as u64)
        .wrapping_mul(ONE_OVER_LOG2_10)
        .wrapping_add((1 << 32) - 1) as i64
        >> 32;
    let index = index as i32;
    // Decimal exponent of the first (smallest) cached power of 10.
    const FIRST_DEC_EXP: i32 = -348;
    // Difference between 2 consecutive decimal exponents in cached powers of 10.
    const DEC_EXP_STEP: i32 = 8;
    let index = (index - FIRST_DEC_EXP - 1) / DEC_EXP_STEP + 1;
    let power = Fp {
        f: POW10_SIGNIFICANDS[index as usize],
        e: i32::from(POW10_EXPONENTS[index as usize]),
    };
    (power, FIRST_DEC_EXP + index * DEC_EXP_STEP)
}

#[derive(PartialEq)]
enum Step {
    More,
    Done,
    Error,
}

/// What `grisu_gen_digits` does with the digits it generates.
trait DigitHandler {
    fn on_start(&mut self, divisor: u64, remainder: u64, error: u64, exp: i32) -> Step;
    fn on_digit(&mut self, digit: u8, divisor: u64, remainder: u64, error: u64, exp: i32, integral: bool) -> Step;
}

/// `get_round_direction`: `None` if the error hides the direction, else whether to round up.
fn round_up(divisor: u64, remainder: u64, error: u64) -> Option<bool> {
    // Round down if (remainder + error) * 2 <= divisor.
    if remainder <= divisor.wrapping_sub(remainder)
        && error.wrapping_mul(2) <= divisor.wrapping_sub(remainder.wrapping_mul(2))
    {
        return Some(false);
    }
    // Round up if (remainder - error) * 2 >= divisor.
    if remainder >= error && remainder - error >= divisor.wrapping_sub(remainder - error) {
        return Some(true);
    }
    None
}

/// `fixed_handler`.
struct FixedHandler {
    buf: Vec<u8>,
    precision: i32,
    exp10: i32,
    fixed: bool,
}

impl DigitHandler for FixedHandler {
    fn on_start(&mut self, divisor: u64, remainder: u64, error: u64, exp: i32) -> Step {
        // Non-fixed formats require at least one digit and no precision adjustment.
        if !self.fixed {
            return Step::More;
        }
        // Adjust the fixed precision by the exponent, as it is relative to the decimal point.
        self.precision += exp + self.exp10;
        // The precision may be satisfied by leading zeros alone: 0.001 with a precision of 2.
        if self.precision > 0 {
            return Step::More;
        }
        if self.precision < 0 {
            return Step::Done;
        }
        match round_up(divisor, remainder, error) {
            None => Step::Error,
            Some(up) => {
                self.buf.push(if up { b'1' } else { b'0' });
                Step::Done
            }
        }
    }

    fn on_digit(&mut self, digit: u8, divisor: u64, remainder: u64, error: u64, _: i32, integral: bool) -> Step {
        self.buf.push(digit);
        if (self.buf.len() as i32) < self.precision {
            return Step::More;
        }
        // Check if error * 2 < divisor with overflow prevention. The error is 1 in the integral part.
        if !integral && (error >= divisor || error >= divisor - error) {
            return Step::Error;
        }
        match round_up(divisor, remainder, error) {
            None => return Step::Error,
            Some(false) => return Step::Done,
            Some(true) => {}
        }
        let buf = &mut self.buf;
        let last = buf.len() - 1;
        buf[last] += 1;
        let mut i = last;
        while i > 0 && buf[i] > b'9' {
            buf[i] = b'0';
            buf[i - 1] += 1;
            i -= 1;
        }
        if buf[0] > b'9' {
            buf[0] = b'1';
            buf.push(b'0');
        }
        Step::Done
    }
}

/// `grisu_shortest_handler`.
struct ShortestHandler {
    buf: Vec<u8>,
    /// Distance between the scaled value and the upper bound (wp_W in Grisu3).
    diff: u64,
}

impl ShortestHandler {
    /// Decrements the generated number, approaching the value from above.
    fn round(&mut self, d: u64, divisor: u64, remainder: &mut u64, error: u64) {
        while *remainder < d
            && error.wrapping_sub(*remainder) >= divisor
            && (remainder.wrapping_add(divisor) < d
                || d - *remainder >= remainder.wrapping_add(divisor).wrapping_sub(d))
        {
            let last = self.buf.last_mut().expect("a digit was just written");
            *last = last.wrapping_sub(1);
            *remainder = remainder.wrapping_add(divisor);
        }
    }
}

impl DigitHandler for ShortestHandler {
    fn on_start(&mut self, _: u64, _: u64, _: u64, _: i32) -> Step {
        Step::More
    }

    /// Grisu's round_weed.
    fn on_digit(&mut self, digit: u8, divisor: u64, mut remainder: u64, error: u64, exp: i32, integral: bool) -> Step {
        self.buf.push(digit);
        if remainder >= error {
            return Step::More;
        }
        let unit = if integral { 1 } else { 10u64.pow(exp.unsigned_abs()) };
        let up = self.diff.wrapping_sub(1).wrapping_mul(unit); // wp_Wup
        self.round(up, divisor, &mut remainder, error);
        let down = self.diff.wrapping_add(1).wrapping_mul(unit); // wp_Wdown
        if remainder < down
            && error.wrapping_sub(remainder) >= divisor
            && (remainder.wrapping_add(divisor) < down
                || down - remainder > remainder.wrapping_add(divisor).wrapping_sub(down))
        {
            return Step::Error;
        }
        if unit.wrapping_mul(2) <= remainder && remainder <= error.wrapping_sub(unit.wrapping_mul(4)) {
            Step::Done
        } else {
            Step::Error
        }
    }
}

/// `grisu_gen_digits`. `error` is the size of the region outside of which numbers definitely do
/// not round to the value (Delta in Grisu3). `exp` is kappa in Grisu.
fn grisu_gen_digits(value: Fp, mut error: u64, exp: &mut i32, handler: &mut impl DigitHandler) -> Step {
    let shift = -value.e;
    let one = 1u64 << shift;
    // The integral part of the scaled value (p1 in Grisu), which fits as value.e is at least -60
    // and at most -32.
    let mut integral = (value.f >> shift) as u32;
    // The fractional part of the scaled value (p2 in Grisu).
    let mut fractional = value.f & (one - 1);
    *exp = integral.checked_ilog10().unwrap_or(0) as i32 + 1;
    // Divide by 10 to prevent overflow.
    let step = handler.on_start(
        10u64.pow((*exp - 1) as u32) << shift,
        value.f / 10,
        error.wrapping_mul(10),
        *exp,
    );
    if step != Step::More {
        return step;
    }
    // Digits of the integral part: up to 10.
    loop {
        let divisor = 10u32.pow((*exp - 1) as u32);
        let digit = integral / divisor;
        integral %= divisor;
        *exp -= 1;
        let remainder = (u64::from(integral) << shift).wrapping_add(fractional);
        let step = handler.on_digit(
            b'0' + digit as u8,
            u64::from(divisor) << shift,
            remainder,
            error,
            *exp,
            true,
        );
        if step != Step::More {
            return step;
        }
        if *exp <= 0 {
            break;
        }
    }
    // Digits of the fractional part.
    loop {
        fractional = fractional.wrapping_mul(10);
        error = error.wrapping_mul(10);
        let digit = b'0'.wrapping_add((fractional >> shift) as u8);
        fractional &= one - 1;
        *exp -= 1;
        let step = handler.on_digit(digit, one, fractional, error, *exp, false);
        if step != Step::More {
            return step;
        }
    }
}

/// `bigint`: `bigits * 2^(32 * exp)`, least significant bigit first.
#[derive(Clone, Default)]
struct Bigint {
    bigits: Vec<u32>,
    exp: i32,
}

const BIGIT_BITS: i32 = 32;

impl Bigint {
    fn from_u64(n: u64) -> Self {
        let mut result = Self::default();
        result.assign(n);
        result
    }

    fn assign(&mut self, mut n: u64) {
        self.bigits.clear();
        loop {
            self.bigits.push(n as u32);
            n >>= BIGIT_BITS;
            if n == 0 {
                break;
            }
        }
        self.exp = 0;
    }

    fn num_bigits(&self) -> i32 {
        self.bigits.len() as i32 + self.exp
    }

    fn subtract_bigits(&mut self, index: usize, other: u32, borrow: &mut u32) {
        let result = u64::from(self.bigits[index])
            .wrapping_sub(u64::from(other))
            .wrapping_sub(u64::from(*borrow));
        self.bigits[index] = result as u32;
        *borrow = (result >> (BIGIT_BITS * 2 - 1)) as u32;
    }

    fn remove_leading_zeros(&mut self) {
        let mut num_bigits = self.bigits.len() - 1;
        while num_bigits > 0 && self.bigits[num_bigits] == 0 {
            num_bigits -= 1;
        }
        self.bigits.truncate(num_bigits + 1);
    }

    /// `*this -= other`, assuming aligned bigints and `*this >= other`.
    fn subtract_aligned(&mut self, other: &Bigint) {
        let mut borrow = 0;
        let mut i = (other.exp - self.exp) as usize;
        for &bigit in &other.bigits {
            self.subtract_bigits(i, bigit, &mut borrow);
            i += 1;
        }
        // As in fmt, a borrow stays at this bigit even if it is zero.
        while borrow > 0 {
            self.subtract_bigits(i, 0, &mut borrow);
        }
        self.remove_leading_zeros();
    }

    fn multiply_u32(&mut self, value: u32) {
        let wide_value = u64::from(value);
        let mut carry = 0u32;
        for bigit in &mut self.bigits {
            let result = u64::from(*bigit) * wide_value + u64::from(carry);
            *bigit = result as u32;
            carry = (result >> BIGIT_BITS) as u32;
        }
        if carry != 0 {
            self.bigits.push(carry);
        }
    }

    fn multiply_u64(&mut self, value: u64) {
        const MASK: u64 = u32::MAX as u64;
        let lower = value & MASK;
        let upper = value >> BIGIT_BITS;
        let mut carry = 0u64;
        for bigit in &mut self.bigits {
            let wide = u64::from(*bigit);
            let result = wide * lower + (carry & MASK);
            carry = wide * upper + (result >> BIGIT_BITS) + (carry >> BIGIT_BITS);
            *bigit = result as u32;
        }
        while carry != 0 {
            self.bigits.push(carry as u32);
            carry >>= BIGIT_BITS;
        }
    }

    fn shift_left(&mut self, shift: i32) {
        self.exp += shift / BIGIT_BITS;
        let shift = shift % BIGIT_BITS;
        if shift == 0 {
            return;
        }
        let mut carry = 0u32;
        for bigit in &mut self.bigits {
            let c = *bigit >> (BIGIT_BITS - shift);
            *bigit = (*bigit << shift).wrapping_add(carry);
            carry = c;
        }
        if carry != 0 {
            self.bigits.push(carry);
        }
    }

    /// `pow(10, exp)`.
    fn assign_pow10(&mut self, exp: i32) {
        if exp == 0 {
            return self.assign(1);
        }
        // Find the top bit.
        let mut bitmask = 1;
        while exp >= bitmask {
            bitmask <<= 1;
        }
        bitmask >>= 1;
        // pow(10, exp) = pow(5, exp) * pow(2, exp). First compute pow(5, exp) by repeated squaring
        // and multiplication.
        self.assign(5);
        bitmask >>= 1;
        while bitmask != 0 {
            self.square();
            if exp & bitmask != 0 {
                self.multiply_u32(5);
            }
            bitmask >>= 1;
        }
        self.shift_left(exp); // Multiply by pow(2, exp) by shifting.
    }

    fn square(&mut self) {
        let n = std::mem::take(&mut self.bigits);
        let num_bigits = n.len();
        let num_result_bigits = 2 * num_bigits;
        self.bigits.resize(num_result_bigits, 0);
        let mut sum = 0u128;
        for bigit_index in 0..num_bigits {
            // Compute the bigit by adding cross-product terms n[i] * n[j] such that
            // i + j == bigit_index.
            for i in 0..=bigit_index {
                sum += u128::from(u64::from(n[i]) * u64::from(n[bigit_index - i]));
            }
            self.bigits[bigit_index] = sum as u32;
            sum >>= BIGIT_BITS; // Compute the carry.
        }
        // Do the same for the top half.
        for bigit_index in num_bigits..num_result_bigits {
            let mut j = num_bigits - 1;
            let mut i = bigit_index - j;
            while i < num_bigits {
                sum += u128::from(u64::from(n[i]) * u64::from(n[j]));
                i += 1;
                j = j.wrapping_sub(1);
            }
            self.bigits[bigit_index] = sum as u32;
            sum >>= BIGIT_BITS;
        }
        self.remove_leading_zeros();
        self.exp *= 2;
    }

    /// Divides by `divisor`, keeping the remainder in `self` and returning the quotient.
    fn divmod_assign(&mut self, divisor: &Bigint) -> i32 {
        if compare(self, divisor) < 0 {
            return 0;
        }
        let exp_difference = self.exp - divisor.exp;
        if exp_difference > 0 {
            // Align bigints by adding trailing zeros to simplify subtraction.
            self.bigits
                .splice(0..0, std::iter::repeat_n(0, exp_difference as usize));
            self.exp -= exp_difference;
        }
        let mut quotient = 0;
        loop {
            self.subtract_aligned(divisor);
            quotient += 1;
            if compare(self, divisor) < 0 {
                return quotient;
            }
        }
    }
}

fn compare(lhs: &Bigint, rhs: &Bigint) -> i32 {
    let (num_lhs_bigits, num_rhs_bigits) = (lhs.num_bigits(), rhs.num_bigits());
    if num_lhs_bigits != num_rhs_bigits {
        return if num_lhs_bigits > num_rhs_bigits { 1 } else { -1 };
    }
    let mut i = lhs.bigits.len() as i32 - 1;
    let mut j = rhs.bigits.len() as i32 - 1;
    let end = (i - j).max(0);
    while i >= end {
        let (lhs_bigit, rhs_bigit) = (lhs.bigits[i as usize], rhs.bigits[j as usize]);
        if lhs_bigit != rhs_bigit {
            return if lhs_bigit > rhs_bigit { 1 } else { -1 };
        }
        i -= 1;
        j -= 1;
    }
    if i != j {
        return if i > j { 1 } else { -1 };
    }
    0
}

/// `compare(lhs1 + lhs2, rhs)`.
fn add_compare(lhs1: &Bigint, lhs2: &Bigint, rhs: &Bigint) -> i32 {
    let max_lhs_bigits = lhs1.num_bigits().max(lhs2.num_bigits());
    let num_rhs_bigits = rhs.num_bigits();
    if max_lhs_bigits + 1 < num_rhs_bigits {
        return -1;
    }
    if max_lhs_bigits > num_rhs_bigits {
        return 1;
    }
    let get_bigit = |n: &Bigint, i: i32| -> u32 {
        if i >= n.exp && i < n.num_bigits() {
            n.bigits[(i - n.exp) as usize]
        } else {
            0
        }
    };
    let mut borrow = 0u64;
    let min_exp = lhs1.exp.min(lhs2.exp).min(rhs.exp);
    let mut i = num_rhs_bigits - 1;
    while i >= min_exp {
        let sum = u64::from(get_bigit(lhs1, i)) + u64::from(get_bigit(lhs2, i));
        let rhs_bigit = u64::from(get_bigit(rhs, i));
        if sum > rhs_bigit + borrow {
            return 1;
        }
        borrow = rhs_bigit + borrow - sum;
        if borrow > 1 {
            return -1;
        }
        borrow <<= BIGIT_BITS;
        i -= 1;
    }
    if borrow != 0 { -1 } else { 0 }
}

/// `fallback_format`: fmt's variation of Steele & White's (FPP)^2. `exp10` is the estimate of the
/// decimal exponent going in, and the exponent of the last digit coming out.
fn fallback_format(d: f64, exp10: &mut i32) -> Vec<u8> {
    let mut numerator = Bigint::default(); // 2 * R in (FPP)^2.
    let mut denominator = Bigint::default(); // 2 * S in (FPP)^2.
    // lower and upper are differences between the value and the corresponding boundaries.
    let mut lower = Bigint::default(); // (M^- in (FPP)^2).
    let mut upper: Option<Bigint> = None; // (M^+ in (FPP)^2), if different from lower.
    // Shift numerator and denominator by an extra bit or two (if the lower boundary is closer) to
    // make lower and upper integers.
    let (value, is_predecessor_closer) = assign(d);
    let shift = if is_predecessor_closer { 2 } else { 1 };
    let significand = value.f << shift;
    if value.e >= 0 {
        numerator.assign(significand);
        numerator.shift_left(value.e);
        lower.assign(1);
        lower.shift_left(value.e);
        if shift != 1 {
            let mut upper_store = Bigint::from_u64(1);
            upper_store.shift_left(value.e + 1);
            upper = Some(upper_store);
        }
        denominator.assign_pow10(*exp10);
        denominator.shift_left(shift);
    } else if *exp10 < 0 {
        numerator.assign_pow10(-*exp10);
        lower = numerator.clone();
        if shift != 1 {
            let mut upper_store = numerator.clone();
            upper_store.shift_left(1);
            upper = Some(upper_store);
        }
        numerator.multiply_u64(significand);
        denominator.assign(1);
        denominator.shift_left(shift - value.e);
    } else {
        numerator.assign(significand);
        denominator.assign_pow10(*exp10);
        denominator.shift_left(shift - value.e);
        lower.assign(1);
        if shift != 1 {
            upper = Some(Bigint::from_u64(1 << 1));
        }
    }
    // Invariant: value == (numerator / denominator) * pow(10, exp10).
    let even = i32::from(value.f & 1 == 0);
    let mut digits = Vec::with_capacity(24);
    loop {
        let digit = numerator.divmod_assign(&denominator);
        let low = compare(&numerator, &lower) - even < 0; // numerator <[=] lower.
        // numerator + upper >[=] pow10:
        let high = add_compare(&numerator, upper.as_ref().unwrap_or(&lower), &denominator) + even > 0;
        digits.push(b'0'.wrapping_add(digit as u8));
        if low || high {
            let last = digits.last_mut().expect("a digit was just written");
            if !low {
                *last = last.wrapping_add(1);
            } else if high {
                let result = add_compare(&numerator, &numerator, &denominator);
                // Round half to even.
                if result > 0 || (result == 0 && digit % 2 != 0) {
                    *last = last.wrapping_add(1);
                }
            }
            *exp10 -= digits.len() as i32 - 1;
            return digits;
        }
        numerator.multiply_u32(10);
        lower.multiply_u32(10);
        if let Some(upper) = &mut upper {
            upper.multiply_u32(10);
        }
    }
}

/// `format_float` with no precision: the digits, and the exponent of the last one. `value` is
/// positive or zero.
fn format_digits(value: f64, binary32: bool) -> (Vec<u8>, i32) {
    if value <= 0.0 {
        return (vec![b'0'], 0);
    }
    const MIN_EXP: i32 = -60; // alpha in Grisu.
    let (fp_value, lower, upper) = if binary32 {
        assign_float_with_boundaries(value)
    } else {
        assign_with_boundaries(value)
    };
    let fp_value = normalize(fp_value, 0);
    // A cached power of 10 such that multiplying the value by it brings the exponent into
    // MIN_EXP..=-32. Its decimal exponent is K in Grisu.
    let (cached_pow, cached_exp10) = cached_power(MIN_EXP - (fp_value.e + SIGNIFICAND_SIZE));
    let scaled = multiply(fp_value.f, cached_pow.f);
    let scaled_e = fp_value.e + cached_pow.e + SIGNIFICAND_SIZE;
    let lower = multiply(lower, cached_pow.f).wrapping_sub(1); // \tilde{M}^- - 1 ulp -> M^-_{\downarrow}.
    let upper = multiply(upper, cached_pow.f).wrapping_add(1); // \tilde{M}^+ + 1 ulp -> M^+_{\uparrow}.
    // Numbers outside of (lower, upper) definitely do not round to the value.
    let mut handler = ShortestHandler {
        buf: Vec::with_capacity(24),
        diff: upper.wrapping_sub(scaled),
    };
    let mut exp = 0;
    let upper_fp = Fp { f: upper, e: scaled_e };
    if grisu_gen_digits(upper_fp, upper.wrapping_sub(lower), &mut exp, &mut handler) == Step::Error {
        exp += handler.buf.len() as i32 - cached_exp10 - 1;
        let digits = fallback_format(value, &mut exp);
        return (digits, exp);
    }
    (handler.buf, exp - cached_exp10)
}

/// `write_exponent`: `[+-]d{2,3}`.
fn write_exponent(exp: i32, out: &mut String) {
    out.push(if exp < 0 { '-' } else { '+' });
    out.push_str(&format!("{:02}", exp.unsigned_abs()));
}

/// How `format_with_spec` writes a number: `float_specs` once fmt has parsed a format specifier.
#[derive(Clone, Copy, Debug)]
pub struct FloatSpec {
    pub format: FloatFormat,
    /// `E` rather than `e`.
    pub upper: bool,
    pub trailing_zeros: bool,
    /// Without one, the digits are the shortest ones that round-trip.
    pub precision: Option<usize>,
    /// `0` without one.
    pub thousand_sep: u8,
    pub decimal_point: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FloatFormat {
    General,
    Exp,
    Fixed,
}

/// `float_writer::prettify`. The value is `digits * 10^exp`.
fn prettify(digits: &[u8], exp: i32, spec: FloatSpec, out: &mut String) {
    let push = |digits: &[u8], out: &mut String| out.extend(digits.iter().map(|d| *d as char));
    let zeros = |count: i32, out: &mut String| out.extend(std::iter::repeat_n('0', count.max(0) as usize));
    let num_digits = digits.len() as i32;
    let precision = spec.precision.map_or(-1, |p| p as i32);
    let point = spec.decimal_point as char;
    let separator = spec.thousand_sep as char;
    // pow(10, full_exp - 1) <= v <= pow(10, full_exp).
    let full_exp = num_digits + exp;
    let threshold = if precision > 0 { precision } else { 16 };
    let scientific = spec.format == FloatFormat::Exp
        || (spec.format == FloatFormat::General && !(-4 < full_exp && full_exp <= threshold));
    if scientific {
        // Insert a decimal point after the first digit and add an exponent.
        out.push(digits[0] as char);
        let num_zeros = precision - num_digits;
        let trailing_zeros = num_zeros > 0 && spec.trailing_zeros;
        if digits.len() > 1 || trailing_zeros {
            out.push(point);
        }
        push(&digits[1..], out);
        if trailing_zeros {
            zeros(num_zeros, out);
        }
        out.push(if spec.upper { 'E' } else { 'e' });
        write_exponent(full_exp - 1, out);
    } else if num_digits <= full_exp {
        // 1234e7 -> 12340000000[.0+]
        if spec.thousand_sep != 0 && full_exp > 3 {
            let mut digit_count = if full_exp % 3 == 0 { 3 } else { full_exp % 3 };
            let mut i = 0;
            while i < full_exp {
                if i > 0 {
                    out.push(separator);
                }
                if i < num_digits {
                    let write_count = (num_digits - i).min(digit_count);
                    push(&digits[i as usize..(i + write_count) as usize], out);
                    zeros(digit_count - write_count, out);
                } else {
                    zeros(digit_count, out);
                }
                i += digit_count;
                digit_count = 3;
            }
        } else {
            push(digits, out);
            zeros(full_exp - num_digits, out);
        }
        if spec.trailing_zeros {
            out.push(point);
            let num_zeros = precision - full_exp;
            if num_zeros > 0 {
                zeros(num_zeros, out);
            } else if spec.format != FloatFormat::Fixed {
                out.push('0');
            }
        }
    } else if full_exp > 0 {
        // 1234e-2 -> 12.34[0+]
        let (integral, fractional) = digits.split_at(full_exp as usize);
        if spec.thousand_sep != 0 && full_exp > 3 {
            let mut digit_count = if full_exp % 3 == 0 { 3 } else { full_exp % 3 };
            let mut i = 0;
            while i < full_exp {
                if i > 0 {
                    out.push(separator);
                }
                push(&digits[i as usize..(i + digit_count) as usize], out);
                i += digit_count;
                digit_count = 3;
            }
        } else {
            push(integral, out);
        }
        if spec.trailing_zeros {
            out.push(point);
            push(fractional, out);
            // Add trailing zeros.
            zeros(precision - num_digits, out);
        } else {
            // Remove trailing zeros.
            let kept = fractional.iter().rposition(|d| *d != b'0').map_or(0, |at| at + 1);
            if kept != 0 {
                out.push(point);
                push(&fractional[..kept], out);
            }
        }
    } else {
        // 1234e-6 -> 0.001234
        out.push('0');
        let num_digits = if spec.trailing_zeros {
            digits.len()
        } else {
            digits.iter().rposition(|d| *d != b'0').map_or(0, |at| at + 1)
        };
        let mut num_zeros = -full_exp;
        if digits.is_empty() && precision >= 0 && precision < num_zeros {
            num_zeros = precision;
        }
        if num_zeros != 0 || num_digits != 0 {
            out.push(point);
            zeros(num_zeros, out);
            push(&digits[..num_digits], out);
        }
    }
}

/// A double has fewer significant or fractional digits than this; all the rest are zeros.
const MAX_EXACT_DIGITS: usize = 1100;

/// `snprintf_float` without a hex format, where Rust stands in for the exactly rounded `snprintf`.
fn snprintf_digits(value: f64, spec: FloatSpec, precision: usize) -> (Vec<u8>, i32) {
    if spec.format == FloatFormat::Fixed {
        let exact = precision.min(MAX_EXACT_DIGITS);
        let text = format!("{value:.exact$}");
        if precision == 0 {
            // The buffer is kept as is, with the point that `#` asks for.
            let point = if spec.trailing_zeros { "." } else { "" };
            return (format!("{text}{point}").into_bytes(), 0);
        }
        let mut digits: Vec<u8> = text.bytes().filter(|b| *b != b'.').collect();
        digits.resize(digits.len() + precision - exact, b'0');
        return (digits, -(precision as i32));
    }
    // Without a precision, snprintf's `%e` has one of 6.
    let fraction = precision.checked_sub(1).unwrap_or(6).min(MAX_EXACT_DIGITS);
    let text = format!("{value:.fraction$e}");
    let (mantissa, exponent) = text.split_once('e').expect("LowerExp has an e");
    let mut digits: Vec<u8> = mantissa.bytes().filter(|b| *b != b'.').collect();
    // Remove trailing zeros.
    while digits.len() > 1 && digits.last() == Some(&b'0') {
        digits.pop();
    }
    let exponent: i32 = exponent.parse().expect("an exponent");
    let fraction_size = digits.len() as i32 - 1;
    (digits, exponent - fraction_size)
}

/// `format_float` with a precision.
fn precise_digits(value: f64, spec: FloatSpec, precision: usize) -> (Vec<u8>, i32) {
    let fixed = spec.format == FloatFormat::Fixed;
    if value <= 0.0 {
        if precision == 0 || !fixed {
            return (vec![b'0'], 0);
        }
        return (vec![b'0'; precision], -(precision as i32));
    }
    if precision > 17 {
        return snprintf_digits(value, spec, precision);
    }
    const MIN_EXP: i32 = -60; // alpha in Grisu.
    let normalized = normalize(assign(value).0, 0);
    let (cached_pow, cached_exp10) = cached_power(MIN_EXP - (normalized.e + SIGNIFICAND_SIZE));
    let scaled = Fp {
        f: multiply(normalized.f, cached_pow.f),
        e: normalized.e + cached_pow.e + SIGNIFICAND_SIZE,
    };
    let mut handler = FixedHandler {
        buf: Vec::with_capacity(24),
        precision: precision as i32,
        exp10: -cached_exp10,
        fixed,
    };
    let mut exp = 0;
    if grisu_gen_digits(scaled, 1, &mut exp, &mut handler) == Step::Error {
        return snprintf_digits(value, spec, precision);
    }
    let mut digits = handler.buf;
    if !fixed {
        // Remove trailing zeros.
        while digits.last() == Some(&b'0') {
            digits.pop();
            exp += 1;
        }
    }
    (digits, exp - cached_exp10)
}

/// `basic_writer::write` for a finite, non-negative `value`, before the sign and the padding.
pub fn format_with_spec(value: f64, mut spec: FloatSpec) -> String {
    let (digits, exp) = match spec.precision {
        None => format_digits(value, false),
        Some(precision) => {
            let precision = precision + usize::from(spec.format == FloatFormat::Exp);
            spec.precision = Some(precision);
            precise_digits(value, spec, precision)
        }
    };
    let mut out = String::with_capacity(digits.len() + 8);
    prettify(&digits, exp, spec, &mut out);
    out
}

/// `%a` for a finite, non-negative `value`, as `snprintf` writes it on macOS. This depends on the
/// C library: glibc writes a subnormal as `0x0.…p-1022`, and rounds a tie to even.
pub fn format_hex_float(value: f64, precision: Option<usize>, upper: bool, alt: bool) -> String {
    const FRACTION_DIGITS: usize = (DOUBLE_SIGNIFICAND_SIZE / 4) as usize;
    let (mut significand, mut exponent) = if value == 0.0 {
        (0, 0)
    } else {
        let (fp, _) = assign(value);
        (fp.f, fp.e + DOUBLE_SIGNIFICAND_SIZE)
    };
    while value != 0.0 && significand & IMPLICIT_BIT == 0 {
        significand <<= 1;
        exponent -= 1;
    }
    let mut extra_zeros = 0;
    let (lead, fraction) = match precision {
        None => {
            let fraction = format!("{:013x}", significand & (IMPLICIT_BIT - 1));
            (
                significand >> DOUBLE_SIGNIFICAND_SIZE,
                fraction.trim_end_matches('0').to_string(),
            )
        }
        Some(precision) if precision >= FRACTION_DIGITS => {
            extra_zeros = precision - FRACTION_DIGITS;
            let fraction = format!("{:013x}", significand & (IMPLICIT_BIT - 1));
            (significand >> DOUBLE_SIGNIFICAND_SIZE, fraction)
        }
        Some(precision) => {
            // `dorounding`: an 8 only rounds up if the digit after it is odd.
            let shift = (FRACTION_DIGITS - precision) * 4;
            let mut kept = significand >> shift;
            let first = (significand >> (shift - 4)) & 0xF;
            let second = if shift >= 8 {
                (significand >> (shift - 8)) & 0xF
            } else {
                0
            };
            if first > 8 || (first == 8 && second & 1 == 1) {
                kept += 1;
            }
            let fraction_bits = precision * 4;
            let fraction = if precision == 0 {
                String::new()
            } else {
                format!("{:0precision$x}", kept & ((1u64 << fraction_bits) - 1))
            };
            (kept >> fraction_bits, fraction)
        }
    };
    let point = if !fraction.is_empty() || alt { "." } else { "" };
    let sign = if exponent < 0 { '-' } else { '+' };
    let zeros = "0".repeat(extra_zeros);
    let out = format!("0x{lead}{point}{fraction}{zeros}p{sign}{}", exponent.abs());
    if upper { out.to_uppercase() } else { out }
}

/// `duckdb_fmt::format("{}", value)` for a finite `value`. A FLOAT comes promoted to a double, with
/// `binary32` set.
pub(crate) fn format_finite(value: f64, binary32: bool) -> String {
    let (digits, exp) = format_digits(value.abs(), binary32);
    let mut out = String::with_capacity(digits.len() + 8);
    if value.is_sign_negative() {
        out.push('-');
    }
    let spec = FloatSpec {
        format: FloatFormat::General,
        upper: false,
        trailing_zeros: true,
        precision: None,
        thousand_sep: 0,
        decimal_point: b'.',
    };
    prettify(&digits, exp, spec, &mut out);
    out
}

/// Significands of `10^k` for `k = -348, -340, ..., 340` (`data::pow10_significands`).
const POW10_SIGNIFICANDS: [u64; 87] = [
    0xfa8fd5a0081c0288,
    0xbaaee17fa23ebf76,
    0x8b16fb203055ac76,
    0xcf42894a5dce35ea,
    0x9a6bb0aa55653b2d,
    0xe61acf033d1a45df,
    0xab70fe17c79ac6ca,
    0xff77b1fcbebcdc4f,
    0xbe5691ef416bd60c,
    0x8dd01fad907ffc3c,
    0xd3515c2831559a83,
    0x9d71ac8fada6c9b5,
    0xea9c227723ee8bcb,
    0xaecc49914078536d,
    0x823c12795db6ce57,
    0xc21094364dfb5637,
    0x9096ea6f3848984f,
    0xd77485cb25823ac7,
    0xa086cfcd97bf97f4,
    0xef340a98172aace5,
    0xb23867fb2a35b28e,
    0x84c8d4dfd2c63f3b,
    0xc5dd44271ad3cdba,
    0x936b9fcebb25c996,
    0xdbac6c247d62a584,
    0xa3ab66580d5fdaf6,
    0xf3e2f893dec3f126,
    0xb5b5ada8aaff80b8,
    0x87625f056c7c4a8b,
    0xc9bcff6034c13053,
    0x964e858c91ba2655,
    0xdff9772470297ebd,
    0xa6dfbd9fb8e5b88f,
    0xf8a95fcf88747d94,
    0xb94470938fa89bcf,
    0x8a08f0f8bf0f156b,
    0xcdb02555653131b6,
    0x993fe2c6d07b7fac,
    0xe45c10c42a2b3b06,
    0xaa242499697392d3,
    0xfd87b5f28300ca0e,
    0xbce5086492111aeb,
    0x8cbccc096f5088cc,
    0xd1b71758e219652c,
    0x9c40000000000000,
    0xe8d4a51000000000,
    0xad78ebc5ac620000,
    0x813f3978f8940984,
    0xc097ce7bc90715b3,
    0x8f7e32ce7bea5c70,
    0xd5d238a4abe98068,
    0x9f4f2726179a2245,
    0xed63a231d4c4fb27,
    0xb0de65388cc8ada8,
    0x83c7088e1aab65db,
    0xc45d1df942711d9a,
    0x924d692ca61be758,
    0xda01ee641a708dea,
    0xa26da3999aef774a,
    0xf209787bb47d6b85,
    0xb454e4a179dd1877,
    0x865b86925b9bc5c2,
    0xc83553c5c8965d3d,
    0x952ab45cfa97a0b3,
    0xde469fbd99a05fe3,
    0xa59bc234db398c25,
    0xf6c69a72a3989f5c,
    0xb7dcbf5354e9bece,
    0x88fcf317f22241e2,
    0xcc20ce9bd35c78a5,
    0x98165af37b2153df,
    0xe2a0b5dc971f303a,
    0xa8d9d1535ce3b396,
    0xfb9b7cd9a4a7443c,
    0xbb764c4ca7a44410,
    0x8bab8eefb6409c1a,
    0xd01fef10a657842c,
    0x9b10a4e5e9913129,
    0xe7109bfba19c0c9d,
    0xac2820d9623bf429,
    0x80444b5e7aa7cf85,
    0xbf21e44003acdd2d,
    0x8e679c2f5e44ff8f,
    0xd433179d9c8cb841,
    0x9e19db92b4e31ba9,
    0xeb96bf6ebadf77d9,
    0xaf87023b9bf0ee6b,
];

/// Binary exponents of the same powers (`data::pow10_exponents`).
const POW10_EXPONENTS: [i16; 87] = [
    -1220, -1193, -1166, -1140, -1113, -1087, -1060, -1034, -1007, -980, -954, -927, -901, -874, -847, -821, -794,
    -768, -741, -715, -688, -661, -635, -608, -582, -555, -529, -502, -475, -449, -422, -396, -369, -343, -316, -289,
    -263, -236, -210, -183, -157, -130, -103, -77, -50, -24, 3, 30, 56, 83, 109, 136, 162, 189, 216, 242, 269, 295,
    322, 348, 375, 402, 428, 455, 481, 508, 534, 561, 588, 614, 641, 667, 694, 720, 747, 774, 800, 827, 853, 880, 907,
    933, 960, 986, 1013, 1039, 1066,
];
