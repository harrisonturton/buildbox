//! Serializing bb's Starlark values' plain fields, for paging frozen heaps.
//!
//! Fields such as labels and paths are encoded with `skycodec`, as the
//! build graph's values are; [`serialize_encoded`] and
//! [`deserialize_encoded`] let a value's derived `StarlarkPagable` use that
//! encoding for them, with
//! `#[starlark_pagable(serialize_with = ..., deserialize_with = ...)]`.

use pagable::{PagableDeserialize, PagableSerialize};
use skycodec::{Decode, Encode};
use starlark::pagable::{StarlarkDeserializeContext, StarlarkSerializeContext};

/// Serialize a field as its `skycodec` encoding.
pub fn serialize_encoded<T: Encode>(value: &T, ctx: &mut dyn StarlarkSerializeContext) -> starlark::Result<()> {
    skycodec::to_bytes(value).pagable_serialize(ctx.pagable())?;
    Ok(())
}

/// Deserialize a field [`serialize_encoded`] serialized.
pub fn deserialize_encoded<T: Decode>(ctx: &mut dyn StarlarkDeserializeContext<'_>) -> starlark::Result<T> {
    let bytes = Vec::<u8>::pagable_deserialize(ctx.pagable())?;
    skycodec::from_bytes(&bytes).map_err(|error| starlark::Error::new_other(anyhow::anyhow!("corrupt paged value: {error}")))
}

/// Serialize a `OnceLock` as its value, if it has one.
pub fn serialize_once_lock<T: Encode>(value: &std::sync::OnceLock<T>, ctx: &mut dyn StarlarkSerializeContext) -> starlark::Result<()> {
    let mut encoder = skycodec::Encoder::new();
    match value.get() {
        Some(value) => {
            encoder.u8(1);
            value.encode(&mut encoder);
        }
        None => encoder.u8(0),
    }
    encoder.into_bytes().pagable_serialize(ctx.pagable())?;
    Ok(())
}

/// Deserialize a `OnceLock` [`serialize_once_lock`] serialized.
pub fn deserialize_once_lock<T: Decode>(ctx: &mut dyn StarlarkDeserializeContext<'_>) -> starlark::Result<std::sync::OnceLock<T>> {
    let value: Option<T> = deserialize_encoded(ctx)?;
    let lock = std::sync::OnceLock::new();
    if let Some(value) = value {
        let _ = lock.set(value);
    }
    Ok(lock)
}

/// Serialize a static string, such as a type's name.
pub fn serialize_static_str(value: &&'static str, ctx: &mut dyn StarlarkSerializeContext) -> starlark::Result<()> {
    value.to_string().pagable_serialize(ctx.pagable())?;
    Ok(())
}

/// Deserialize a string [`serialize_static_str`] serialized, interned so
/// that it's static.
pub fn deserialize_static_str(ctx: &mut dyn StarlarkDeserializeContext<'_>) -> starlark::Result<&'static str> {
    let text = String::pagable_deserialize(ctx.pagable())?;
    Ok(intern::Symbol::intern(&text).as_str())
}
