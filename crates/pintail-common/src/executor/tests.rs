use super::*;
use crate::types::LogicalType;
use crate::value::Value;
use proptest::prelude::*;

fn ints(values: &[Option<i32>]) -> Vector {
    let mut v = Vector::new(LogicalType::INTEGER, values.len().max(1));
    for (i, x) in values.iter().enumerate() {
        match x {
            Some(x) => v.set_value(i, &Value::integer(*x)).unwrap(),
            None => v.set_null(i, true),
        }
    }
    v
}

fn read_ints(v: &Vector, count: usize) -> Vec<Option<i32>> {
    (0..count)
        .map(|i| {
            let value = v.get_value(i);
            value.as_i128().map(|x| x as i32)
        })
        .collect()
}

/// The same logical rows in a chosen representation.
#[derive(Debug, Clone)]
enum Shape {
    Flat,
    /// Rows come from a shuffled base vector through a selection.
    Dictionary,
}

fn shaped(values: &[Option<i32>], shape: &Shape) -> Vector {
    match shape {
        Shape::Flat => ints(values),
        Shape::Dictionary => {
            // Store the values reversed and select them back into order.
            let reversed: Vec<Option<i32>> = values.iter().rev().cloned().collect();
            let mut v = ints(&reversed);
            let n = values.len();
            v.slice(
                &SelectionVector::from_vec((0..n).map(|i| (n - 1 - i) as u32).collect()),
                n,
            );
            v
        }
    }
}

fn shape() -> impl Strategy<Value = Shape> {
    prop_oneof![Just(Shape::Flat), Just(Shape::Dictionary)]
}

proptest! {
    #[test]
    fn unary_matches_row_by_row(values in prop::collection::vec(prop::option::of(any::<i32>()), 1..300), s in shape()) {
        let input = shaped(&values, &s);
        let mut result = Vector::new(LogicalType::BIGINT, 1);
        unary(&input, &mut result, values.len(), |a: i32| i64::from(a) * 2);
        let expected: Vec<Option<i64>> = values.iter().map(|v| v.map(|a| i64::from(a) * 2)).collect();
        let actual: Vec<Option<i64>> = (0..values.len())
            .map(|i| result.get_value(i).as_i128().map(|x| x as i64))
            .collect();
        prop_assert_eq!(actual, expected);
    }

    #[test]
    fn binary_matches_row_by_row(
        pairs in prop::collection::vec((prop::option::of(any::<i32>()), prop::option::of(any::<i32>())), 1..300),
        ls in shape(),
        rs in shape(),
        left_constant in any::<bool>(),
    ) {
        let n = pairs.len();
        let mut lv: Vec<Option<i32>> = pairs.iter().map(|p| p.0).collect();
        let rv: Vec<Option<i32>> = pairs.iter().map(|p| p.1).collect();
        let left = if left_constant {
            lv = vec![lv[0]; n];
            match lv[0] {
                Some(x) => Vector::constant(&Value::integer(x)),
                None => Vector::constant_null(LogicalType::INTEGER),
            }
        } else {
            shaped(&lv, &ls)
        };
        let right = shaped(&rv, &rs);
        let mut result = Vector::new(LogicalType::INTEGER, 1);
        // Overflow produces NULL, exercising the nullable path.
        binary_nullable(&left, &right, &mut result, n, |a: i32, b: i32| a.checked_add(b));
        let expected: Vec<Option<i32>> =
            lv.iter().zip(&rv).map(|(a, b)| a.zip(*b).and_then(|(a, b)| a.checked_add(b))).collect();
        prop_assert_eq!(read_ints(&result, n), expected);
    }

    #[test]
    fn select_matches_filter(values in prop::collection::vec(prop::option::of(-100i32..100), 0..300), s in shape()) {
        let input = shaped(&values, &s);
        let (sel, n) = select(&input, values.len(), |a: i32| a > 0);
        let expected: Vec<usize> = values.iter().enumerate().filter(|(_, v)| v.is_some_and(|x| x > 0)).map(|(i, _)| i).collect();
        prop_assert_eq!((0..n).map(|i| sel.get(i)).collect::<Vec<_>>(), expected);
    }
}

#[test]
fn constant_inputs_give_constant_results() {
    let c = Vector::constant(&Value::integer(20));
    let mut result = Vector::new(LogicalType::INTEGER, 1);
    unary(&c, &mut result, 1000, |a: i32| a + 1);
    assert_eq!(result.vector_type(), VectorType::Constant);
    assert_eq!(result.get_value(999), Value::integer(21));

    binary(&c, &c, &mut result, 1000, |a: i32, b: i32| a * b);
    assert_eq!(result.vector_type(), VectorType::Constant);
    assert_eq!(result.get_value(0), Value::integer(400));

    let null = Vector::constant_null(LogicalType::INTEGER);
    let flat = ints(&[Some(1), Some(2)]);
    binary(&flat, &null, &mut result, 2, |a: i32, b: i32| a + b);
    assert!(result.is_constant_null());
}

#[test]
fn sequence_inputs() {
    let s = Vector::sequence(LogicalType::INTEGER, 5, 2).unwrap();
    let mut result = Vector::new(LogicalType::INTEGER, 1);
    unary(&s, &mut result, 4, |a: i32| a * 10);
    assert_eq!(read_ints(&result, 4), [Some(50), Some(70), Some(90), Some(110)]);
}

#[test]
fn fallible_stops_at_first_error() {
    let input = ints(&[Some(1), Some(0), Some(2)]);
    let mut result = Vector::new(LogicalType::INTEGER, 1);
    let err = unary_fallible(&input, &mut result, 3, |a: i32| {
        if a == 0 {
            Err(crate::Error::new(crate::ErrorKind::DivideByZero, "division by zero"))
        } else {
            Ok(Some(10 / a))
        }
    })
    .unwrap_err();
    assert_eq!(err.to_string(), "Divide by Zero Error: division by zero");
}

#[test]
fn null_rows_skip_the_function() {
    // 200 rows with NULLs spread over several 64-row validity entries, including one entry
    // that is entirely NULL.
    let values: Vec<Option<i32>> = (0..200)
        .map(|i| {
            if (64..128).contains(&i) || i % 7 == 0 {
                None
            } else {
                Some(i)
            }
        })
        .collect();
    let input = ints(&values);
    let mut calls = 0;
    let mut result = Vector::new(LogicalType::INTEGER, 1);
    unary(&input, &mut result, 200, |a: i32| {
        calls += 1;
        a
    });
    assert_eq!(calls, values.iter().filter(|v| v.is_some()).count());
    assert_eq!(read_ints(&result, 200), values);
}

#[test]
fn string_executors() {
    let long = "a long string that will not be inlined";
    let mut input = Vector::new(LogicalType::VARCHAR, 3);
    input.set_value(0, &Value::varchar("abc")).unwrap();
    input.set_null(1, true);
    input.set_value(2, &Value::varchar(long)).unwrap();

    let mut lengths = Vector::new(LogicalType::BIGINT, 1);
    unary_str(&input, &mut lengths, 3, |s| s.len() as i64);
    assert_eq!(lengths.get_value(0), Value::bigint(3));
    assert!(lengths.get_value(1).is_null());
    assert_eq!(lengths.get_value(2), Value::bigint(long.len() as i64));

    let mut upper = Vector::new(LogicalType::VARCHAR, 1);
    unary_str_to_str(&input, &mut upper, 3, |s, out| {
        out.extend(s.iter().map(u8::to_ascii_uppercase))
    });
    drop(input);
    assert_eq!(upper.get_value(2), Value::varchar(long.to_uppercase()));
    assert!(upper.get_value(1).is_null());

    let ints_in = ints(&[Some(-5), None]);
    let mut text = Vector::new(LogicalType::VARCHAR, 1);
    unary_to_str(&ints_in, &mut text, 2, |a: i32, out| out.extend(a.to_string().bytes()));
    assert_eq!(text.get_value(0), Value::varchar("-5"));
    assert!(text.get_value(1).is_null());

    let c = Vector::constant(&Value::varchar("abc"));
    let mut eq = Vector::new(LogicalType::BOOLEAN, 1);
    binary_str(&upper, &c, &mut eq, 3, |a, b| a.eq_ignore_ascii_case(b));
    assert_eq!(eq.get_value(0), Value::boolean(true));
    assert!(eq.get_value(1).is_null());
    assert_eq!(eq.get_value(2), Value::boolean(false));
}
