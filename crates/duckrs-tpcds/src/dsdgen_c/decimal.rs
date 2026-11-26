// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/decimal.cpp @ rust-port-baseline

//! Fixed-point decimals as dsdgen keeps them. As in the C code, `precision` is the number of
//! digits after the point and `scale` the number before it.

use super::r_params::atoi;

pub(crate) const OP_PLUS: i32 = 0x00001;
pub(crate) const OP_MINUS: i32 = 0x00002;
pub(crate) const OP_MULT: i32 = 0x00004;
pub(crate) const OP_DIV: i32 = 0x00008;

/// `decimal_t`
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct Decimal {
    pub(crate) flags: i32,
    pub(crate) precision: i32,
    pub(crate) scale: i32,
    pub(crate) number: i64,
}

/// `itodec`
pub(crate) fn itodec(src: i32) -> Decimal {
    let mut scale = 1;
    let mut bound = 1i32;
    while bound.wrapping_mul(10) <= src {
        scale += 1;
        bound = bound.wrapping_mul(10);
    }
    Decimal {
        flags: 0,
        precision: 0,
        scale,
        number: i64::from(src),
    }
}

/// `strtodec`
pub(crate) fn strtodec(s: &[u8]) -> Decimal {
    let mut d = Decimal::default();
    match s.iter().position(|&c| c == b'.') {
        None => {
            d.scale = s.len() as i32;
            d.number = i64::from(atoi(s));
            d.precision = 0;
        }
        Some(point) => {
            let (int_part, fraction) = (&s[..point], &s[point + 1..]);
            d.scale = int_part.len() as i32;
            d.number = i64::from(atoi(int_part));
            d.precision = fraction.len() as i32;
            for _ in 0..d.precision {
                d.number *= 10;
            }
            d.number += i64::from(atoi(fraction));
        }
    }
    if s.first() == Some(&b'-') && d.number > 0 {
        d.number *= -1;
    }
    d
}

/// `decimal_t_op`: `dest` keeps its flags.
pub(crate) fn decimal_t_op(dest: &mut Decimal, op: i32, d1: &Decimal, d2: &Decimal) {
    let (d1, d2) = (*d1, *d2);
    dest.scale = if d1.scale > d2.scale { d1.scale } else { d2.scale };
    dest.precision = if d1.precision > d2.precision {
        d1.precision
    } else {
        d2.precision
    };
    match op {
        OP_PLUS => dest.number = d1.number.wrapping_add(d2.number),
        OP_MINUS => dest.number = d1.number.wrapping_sub(d2.number),
        OP_MULT => {
            let mut res = d1.precision + d2.precision;
            dest.number = d1.number.wrapping_mul(d2.number);
            while res > dest.precision {
                res -= 1;
                dest.number /= 10;
            }
        }
        OP_DIV => {
            // single precision, as in C
            let mut f1 = d1.number as f32;
            let mut np = d1.precision;
            while np < dest.precision {
                f1 *= 10.0;
                np += 1;
            }
            np = 0;
            while np < dest.precision {
                f1 *= 10.0;
                np += 1;
            }
            let mut f2 = d2.number as f32;
            np = d2.precision;
            while np < dest.precision {
                f2 *= 10.0;
                np += 1;
            }
            dest.number = i64::from((f1 / f2) as i32);
        }
        _ => panic!("dsdgen: decimal_t_op does not support op"),
    }
}

/// `decimal_t_op(d, op, d, d2)`: C writes `dest`'s scale and precision before it reads the rest
/// of `d1`, so when they are the same decimal the operation sees the new precision.
pub(crate) fn decimal_t_op_in_place(dest: &mut Decimal, op: i32, d2: &Decimal) {
    let d2 = *d2;
    dest.scale = if dest.scale > d2.scale { dest.scale } else { d2.scale };
    dest.precision = if dest.precision > d2.precision {
        dest.precision
    } else {
        d2.precision
    };
    match op {
        OP_PLUS => dest.number = dest.number.wrapping_add(d2.number),
        OP_MINUS => dest.number = dest.number.wrapping_sub(d2.number),
        OP_MULT => {
            let mut res = dest.precision + d2.precision;
            dest.number = dest.number.wrapping_mul(d2.number);
            while res > dest.precision {
                res -= 1;
                dest.number /= 10;
            }
        }
        OP_DIV => {
            let mut f1 = dest.number as f32;
            for _ in 0..dest.precision {
                f1 *= 10.0;
            }
            let mut f2 = d2.number as f32;
            let mut np = d2.precision;
            while np < dest.precision {
                f2 *= 10.0;
                np += 1;
            }
            dest.number = i64::from((f1 / f2) as i32);
        }
        _ => panic!("dsdgen: decimal_t_op does not support op"),
    }
}

/// `decimal_t_op` into a new decimal (with no flags).
#[cfg(test)]
fn decimal_op(op: i32, d1: &Decimal, d2: &Decimal) -> Decimal {
    let mut dest = Decimal::default();
    decimal_t_op(&mut dest, op, d1, d2);
    dest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse() {
        assert_eq!(strtodec(b"1000.00").number, 100000);
        assert_eq!(strtodec(b"-0.05").number, -5);
        assert_eq!(strtodec(b"0.09").precision, 2);
        let d = decimal_op(OP_MULT, &strtodec(b"1.50"), &strtodec(b"2.00"));
        assert_eq!((d.number, d.precision), (300, 2));
    }
}
