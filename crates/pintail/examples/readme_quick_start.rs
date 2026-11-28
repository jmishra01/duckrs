//! The Rust quick start of the README.

fn main() -> pintail::Result<()> {
    let db = pintail::Database::open_in_memory()?;
    let mut connection = db.connect()?;
    let result = connection.query("SELECT 42 AS answer")?;
    assert_eq!(result.value(0, 0), pintail::Value::integer(42));
    println!("{:?}", result.value(0, 0));
    Ok(())
}
