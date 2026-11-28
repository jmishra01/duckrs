use super::*;
use crate::types::LogicalTypeId;

fn ints(values: &[Option<i32>]) -> Vector {
    let mut v = Vector::new(LogicalType::INTEGER, values.len());
    for (i, x) in values.iter().enumerate() {
        match x {
            Some(x) => v.set_value(i, &Value::integer(*x)).unwrap(),
            None => v.set_null(i, true),
        }
    }
    v
}

fn collect(v: &Vector, count: usize) -> Vec<Value> {
    (0..count).map(|i| v.get_value(i)).collect()
}

#[test]
fn flat_values_and_nulls() {
    let v = ints(&[Some(1), None, Some(3)]);
    assert_eq!(v.vector_type(), VectorType::Flat);
    assert_eq!(v.data::<i32>()[..3], [1, 0, 3]);
    assert!(!v.is_valid(1));
    assert_eq!(
        collect(&v, 3),
        [Value::integer(1), Value::null(LogicalType::INTEGER), Value::integer(3)]
    );
}

#[test]
fn clones_are_copy_on_write() {
    let a = ints(&[Some(1), Some(2)]);
    let mut b = a.clone();
    b.data_mut::<i32>()[0] = 10;
    b.set_null(1, true);
    assert_eq!(a.data::<i32>()[..2], [1, 2]);
    assert!(a.is_valid(1));
    assert_eq!(b.data::<i32>()[0], 10);
}

#[test]
fn constant_vectors() {
    let c = Vector::constant(&Value::bigint(7));
    assert_eq!(c.vector_type(), VectorType::Constant);
    assert_eq!(c.get_value(1000), Value::bigint(7));
    let n = Vector::constant_null(LogicalType::VARCHAR);
    assert!(n.is_constant_null());
    let mut flat = c.clone();
    flat.flatten(4);
    assert_eq!(flat.vector_type(), VectorType::Flat);
    assert_eq!(flat.data::<i64>()[..4], [7, 7, 7, 7]);
    let mut null_flat = n;
    null_flat.flatten(3);
    assert_eq!(null_flat.validity().count_valid(3), 0);
}

#[test]
fn sequence_vectors() {
    let s = Vector::sequence(LogicalType::BIGINT, 10, 5).unwrap();
    assert_eq!(s.get_value(3), Value::bigint(25));
    let unified = s.to_unified(4);
    assert_eq!(unified.data::<i64>()[unified.sel.get(2)], 20);
    assert!(Vector::sequence(LogicalType::VARCHAR, 0, 1).is_err());
}

#[test]
fn dictionary_slicing_and_flatten() {
    let mut v = ints(&[Some(10), None, Some(30), Some(40)]);
    v.slice(&SelectionVector::from_vec(vec![3, 1, 0]), 3);
    assert_eq!(v.vector_type(), VectorType::Dictionary);
    assert_eq!(
        collect(&v, 3),
        [
            Value::integer(40),
            Value::null(LogicalType::INTEGER),
            Value::integer(10)
        ]
    );
    // Slicing a dictionary composes selections instead of nesting.
    v.slice(&SelectionVector::from_vec(vec![2, 2]), 2);
    assert_eq!(collect(&v, 2), [Value::integer(10), Value::integer(10)]);
    let unified = v.to_unified(2);
    assert_eq!(unified.data::<i32>()[unified.sel.get(1)], 10);
    v.flatten(2);
    assert_eq!(v.vector_type(), VectorType::Flat);
    assert_eq!(v.data::<i32>()[..2], [10, 10]);
}

#[test]
fn strings_survive_slicing_and_flatten() {
    let long = "a string that is definitely not inlined";
    let mut v = Vector::new(LogicalType::VARCHAR, 3);
    v.set_value(0, &Value::varchar("short")).unwrap();
    v.set_value(1, &Value::varchar(long)).unwrap();
    v.set_null(2, true);
    let mut d = v.clone();
    drop(v);
    d.slice(&SelectionVector::from_vec(vec![1, 2, 0]), 3);
    d.flatten(3);
    assert_eq!(d.str_at(0), Some(long.as_bytes()));
    assert_eq!(d.str_at(1), None);
    assert_eq!(d.str_at(2), Some(&b"short"[..]));
    assert_eq!(d.get_value(0), Value::varchar(long));
    let unified = d.to_unified(3);
    drop(d);
    assert_eq!(unified.str_at(0), long.as_bytes());
}

#[test]
#[should_panic(expected = "use set_str")]
fn raw_string_writes_are_rejected() {
    let mut v = Vector::new(LogicalType::VARCHAR, 1);
    let _ = v.data_mut::<StringT>();
}

#[test]
fn keep_strings_alive_across_vectors() {
    let long = "another string that does not fit inline";
    let mut source = Vector::new(LogicalType::VARCHAR, 1);
    source.set_str(0, long.as_bytes());
    let s = source.data::<StringT>()[0];
    let mut target = Vector::new(LogicalType::VARCHAR, 1);
    target.keep_strings_alive(&source);
    // SAFETY: `source`'s buffer is registered with `target`.
    unsafe { target.string_data_mut()[0] = s };
    drop(source);
    assert_eq!(target.str_at(0), Some(long.as_bytes()));
}

#[test]
fn nested_values_round_trip() {
    let list_ty = LogicalType::list(LogicalType::INTEGER);
    let list = Value::list(
        LogicalType::INTEGER,
        vec![Value::integer(1), Value::null(LogicalType::INTEGER)],
    )
    .unwrap();
    let empty = Value::list(LogicalType::INTEGER, vec![]).unwrap();
    let mut v = Vector::new(list_ty.clone(), 3);
    v.set_value(0, &list).unwrap();
    v.set_value(1, &Value::null(list_ty)).unwrap();
    v.set_value(2, &empty).unwrap();
    assert_eq!(v.list_size(), 2);
    assert_eq!(v.get_value(0), list);
    assert!(v.get_value(1).is_null());
    assert_eq!(v.get_value(2), empty);

    let s = Value::struct_value(vec![("a".into(), Value::integer(1)), ("b".into(), Value::varchar("x"))]);
    let mut sv = Vector::new(s.logical_type().clone(), 2);
    sv.set_value(1, &s).unwrap();
    sv.set_value(0, &Value::null(s.logical_type().clone())).unwrap();
    assert_eq!(sv.get_value(1), s);
    assert!(sv.get_value(0).is_null());
    assert!(!sv.struct_children()[0].is_valid(0), "NULL structs null their children");

    let arr_ty = LogicalType::array(LogicalType::DOUBLE, 2);
    let arr = Value::new(
        arr_ty.clone(),
        ValueData::List(vec![Value::double(1.5), Value::double(2.5)]),
    )
    .unwrap();
    let mut av = Vector::new(arr_ty, 2);
    av.set_value(1, &arr).unwrap();
    assert_eq!(av.get_value(1), arr);
}

#[test]
fn nested_dictionary_flatten() {
    let s = Value::struct_value(vec![("a".into(), Value::integer(1))]);
    let s2 = Value::struct_value(vec![("a".into(), Value::integer(2))]);
    let mut sv = Vector::new(s.logical_type().clone(), 2);
    sv.set_value(0, &s).unwrap();
    sv.set_value(1, &s2).unwrap();
    sv.slice(&SelectionVector::from_vec(vec![1, 1, 0]), 3);
    sv.flatten(3);
    assert_eq!(collect(&sv, 3), [s2.clone(), s2, s.clone()]);

    let mut c = Vector::constant(&s);
    c.flatten(2);
    assert_eq!(collect(&c, 2), [s.clone(), s]);

    let arr_ty = LogicalType::array(LogicalType::INTEGER, 2);
    let a = Value::new(
        arr_ty.clone(),
        ValueData::List(vec![Value::integer(1), Value::integer(2)]),
    )
    .unwrap();
    let b = Value::new(
        arr_ty.clone(),
        ValueData::List(vec![Value::integer(3), Value::integer(4)]),
    )
    .unwrap();
    let mut av = Vector::new(arr_ty, 2);
    av.set_value(0, &a).unwrap();
    av.set_value(1, &b).unwrap();
    av.slice(&SelectionVector::from_vec(vec![1, 0]), 2);
    av.flatten(2);
    assert_eq!(collect(&av, 2), [b, a]);
}

#[test]
fn set_value_checks_types() {
    let mut v = Vector::new(LogicalType::INTEGER, 1);
    assert!(v.set_value(0, &Value::bigint(1)).is_err());
    let mut d = Vector::new(LogicalType::decimal(10, 2), 1);
    d.set_value(0, &Value::decimal(12345, 10, 2).unwrap()).unwrap();
    assert_eq!(d.get_value(0).try_to_string().unwrap(), "123.45");
    assert_eq!(d.logical_type().id(), LogicalTypeId::Decimal);
}

#[test]
fn data_chunk_rows() {
    let mut chunk = DataChunk::with_capacity(&[LogicalType::INTEGER, LogicalType::VARCHAR], 4);
    chunk.append_row(&[Value::integer(1), Value::varchar("a")]).unwrap();
    chunk
        .append_row(&[Value::null(LogicalType::INTEGER), Value::varchar("b")])
        .unwrap();
    assert_eq!(chunk.len(), 2);
    assert_eq!(chunk.to_string(), "Chunk - [2 Columns, 2 Rows]\n1\ta\nNULL\tb\n");
    chunk.slice(&SelectionVector::from_vec(vec![1]), 1);
    assert_eq!(chunk.get_value(1, 0), Value::varchar("b"));
    chunk.flatten();
    assert_eq!(chunk.column(1).vector_type(), VectorType::Flat);
    chunk.reset();
    assert!(chunk.is_empty());
}

#[test]
fn prepare_flat_reuses_unshared_buffers() {
    let mut v = ints(&[Some(1), None]);
    v.prepare_flat(2);
    assert!(v.validity().all_valid());
    let shared = v.clone();
    v.prepare_flat(2);
    v.data_mut::<i32>()[0] = 99;
    assert_eq!(shared.data::<i32>()[0], 1);
}

mod copy_props {
    use super::*;
    use proptest::prelude::*;

    fn strings(values: &[Option<String>]) -> Vector {
        let mut v = Vector::new(LogicalType::VARCHAR, values.len().max(1));
        for (i, x) in values.iter().enumerate() {
            match x {
                Some(s) => v.set_value(i, &Value::varchar(s.clone())).unwrap(),
                None => v.set_null(i, true),
            }
        }
        v
    }

    proptest! {
        #[test]
        fn copy_from_matches_row_by_row(
            values in prop::collection::vec(prop::option::of("[a-z]{0,20}"), 1..200),
            picks in prop::collection::vec(any::<prop::sample::Index>(), 0..200),
            as_dictionary in any::<bool>(),
            deep in any::<bool>(),
            target_offset in 0usize..50,
        ) {
            let mut source = strings(&values);
            if as_dictionary {
                let n = values.len();
                source.slice(&SelectionVector::from_vec((0..n as u32).rev().collect()), n);
            }
            let logical = |i: usize| if as_dictionary { values.len() - 1 - i } else { i };
            let sel: Vec<u32> = picks.iter().map(|p| p.index(values.len()) as u32).collect();
            let mut target = Vector::new(LogicalType::VARCHAR, 1);
            let mode = if deep { StringCopy::Deep } else { StringCopy::Reference };
            target.copy_from(&source, &SelectionVector::from_vec(sel.clone()), sel.len(), target_offset, mode);
            drop(source);
            for (i, &s) in sel.iter().enumerate() {
                let expected = values[logical(s as usize)].clone();
                let actual = target.get_value(target_offset + i);
                prop_assert_eq!(actual, expected.map_or(Value::null(LogicalType::VARCHAR), Value::varchar));
            }
        }
    }

    #[test]
    fn copies_lists_by_appending_children() {
        let ty = LogicalType::list(LogicalType::INTEGER);
        let a = Value::list(LogicalType::INTEGER, vec![Value::integer(1), Value::integer(2)]).unwrap();
        let b = Value::list(LogicalType::INTEGER, vec![Value::integer(3)]).unwrap();
        let mut source = Vector::new(ty.clone(), 2);
        source.set_value(0, &a).unwrap();
        source.set_value(1, &b).unwrap();
        let mut target = Vector::new(ty, 1);
        target.copy_from(
            &source,
            &SelectionVector::from_vec(vec![1, 0, 1]),
            3,
            0,
            StringCopy::Deep,
        );
        target.copy_from(&source, &SelectionVector::Identity, 2, 3, StringCopy::Deep);
        assert_eq!(collect(&target, 5), [b.clone(), a.clone(), b.clone(), a, b]);
        assert_eq!(target.list_size(), 1 + 2 + 1 + 2 + 1);
    }
}
