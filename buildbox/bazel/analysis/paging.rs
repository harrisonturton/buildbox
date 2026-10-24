//! Serializing analyzed targets' frozen heaps, so analysis can be paged
//! out of memory and kept across restarts.

use pagable::{PagableDeserialize, PagableSerialize};
use starlark::values::OwnedFrozenValue;
use std::panic::AssertUnwindSafe;

/// Serialize `value`, with every heap it refers to, and read it back,
/// returning how many bytes it took. Fails with what couldn't be
/// serialized.
pub fn round_trip(value: &OwnedFrozenValue) -> Result<usize, String> {
    let attempt = std::panic::catch_unwind(AssertUnwindSafe(|| -> anyhow::Result<usize> {
        let mut serializer = pagable::testing::TestingSerializer::new();
        value.pagable_serialize(&mut serializer)?;
        let bytes = serializer.finish();
        let mut deserializer = pagable::testing::TestingDeserializer::new(&bytes);
        OwnedFrozenValue::pagable_deserialize(&mut deserializer)?;
        Ok(bytes.len())
    }));
    match attempt {
        Ok(Ok(len)) => Ok(len),
        Ok(Err(error)) => Err(format!("{error:#}")),
        Err(panic) => Err(panic_message(&*panic)),
    }
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = panic.downcast_ref::<String>() {
        return message.clone();
    }
    if let Some(message) = panic.downcast_ref::<&str>() {
        return message.to_string();
    }
    "panicked".to_string()
}
