use std::fmt;
use std::io::{self, Write};

use chrono::{DateTime, NaiveDateTime, Utc};
use serde::de::{DeserializeSeed, EnumAccess, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

/// Hash the exact compact JSON bytes without retaining a serialized copy.
pub fn json_sha256(value: &(impl Serialize + ?Sized)) -> Result<String, serde_json::Error> {
    struct HashWriter(Sha256);
    impl Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = HashWriter(Sha256::new());
    serde_json::to_writer(&mut writer, value)?;
    Ok(hex::encode(writer.0.finalize()))
}

// Normalize the same legacy timestamp fields as the old Value-based importer,
// including nested raw_payload values, while constructing only the typed graph.
pub(super) fn deserialize_export<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<archive_domain::ClientExportV1, D::Error> {
    archive_domain::ClientExportV1::deserialize(Normalized {
        inner: deserializer,
        timestamp: false,
    })
}

struct Normalized<D> {
    inner: D,
    timestamp: bool,
}

impl<'de, D: Deserializer<'de>> Deserializer<'de> for Normalized<D> {
    type Error = D::Error;
    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, D::Error> {
        self.inner.deserialize_any(NormalizingVisitor {
            visitor,
            timestamp: self.timestamp,
        })
    }
    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, D::Error> {
        self.inner.deserialize_option(NormalizingVisitor {
            visitor,
            timestamp: self.timestamp,
        })
    }
    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, D::Error> {
        self.inner.deserialize_newtype_struct(
            name,
            NormalizingVisitor {
                visitor,
                timestamp: self.timestamp,
            },
        )
    }
    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, D::Error> {
        self.inner.deserialize_enum(name, variants, visitor)
    }
    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, D::Error> {
        // Reading views can duplicate all messages. Skip them directly in serde_json.
        self.inner.deserialize_ignored_any(visitor)
    }
    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf unit unit_struct seq tuple tuple_struct map struct identifier
    }
}

struct NormalizingVisitor<V> {
    visitor: V,
    timestamp: bool,
}

impl<V> NormalizingVisitor<V> {
    fn normalized(&self, text: &str) -> Option<String> {
        self.timestamp
            .then(|| NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").ok())
            .flatten()
            .map(|parsed| DateTime::<Utc>::from_naive_utc_and_offset(parsed, Utc).to_rfc3339())
    }
}

impl<'de, V: Visitor<'de>> Visitor<'de> for NormalizingVisitor<V> {
    type Value = V::Value;
    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        self.visitor.expecting(formatter)
    }
    fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<V::Value, E> {
        self.visitor.visit_bool(value)
    }
    fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<V::Value, E> {
        self.visitor.visit_i64(value)
    }
    fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<V::Value, E> {
        self.visitor.visit_u64(value)
    }
    fn visit_i128<E: serde::de::Error>(self, value: i128) -> Result<V::Value, E> {
        self.visitor.visit_i128(value)
    }
    fn visit_u128<E: serde::de::Error>(self, value: u128) -> Result<V::Value, E> {
        self.visitor.visit_u128(value)
    }
    fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<V::Value, E> {
        self.visitor.visit_f64(value)
    }
    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<V::Value, E> {
        if let Some(normalized) = self.normalized(value) {
            self.visitor.visit_string(normalized)
        } else {
            self.visitor.visit_str(value)
        }
    }
    fn visit_borrowed_str<E: serde::de::Error>(self, value: &'de str) -> Result<V::Value, E> {
        if let Some(normalized) = self.normalized(value) {
            self.visitor.visit_string(normalized)
        } else {
            self.visitor.visit_borrowed_str(value)
        }
    }
    fn visit_string<E: serde::de::Error>(self, value: String) -> Result<V::Value, E> {
        if let Some(normalized) = self.normalized(&value) {
            self.visitor.visit_string(normalized)
        } else {
            self.visitor.visit_string(value)
        }
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<V::Value, E> {
        self.visitor.visit_unit()
    }
    fn visit_none<E: serde::de::Error>(self) -> Result<V::Value, E> {
        self.visitor.visit_none()
    }
    fn visit_some<D: Deserializer<'de>>(self, inner: D) -> Result<V::Value, D::Error> {
        self.visitor.visit_some(Normalized {
            inner,
            timestamp: self.timestamp,
        })
    }
    fn visit_newtype_struct<D: Deserializer<'de>>(self, inner: D) -> Result<V::Value, D::Error> {
        self.visitor.visit_newtype_struct(Normalized {
            inner,
            timestamp: self.timestamp,
        })
    }
    fn visit_seq<A: SeqAccess<'de>>(self, inner: A) -> Result<V::Value, A::Error> {
        self.visitor.visit_seq(NormalizedSeq(inner))
    }
    fn visit_map<A: MapAccess<'de>>(self, inner: A) -> Result<V::Value, A::Error> {
        self.visitor.visit_map(NormalizedMap {
            inner,
            timestamp: false,
        })
    }
    fn visit_enum<A: EnumAccess<'de>>(self, inner: A) -> Result<V::Value, A::Error> {
        self.visitor.visit_enum(inner)
    }
}

struct NormalizedSeed<T> {
    inner: T,
    timestamp: bool,
}
impl<'de, T: DeserializeSeed<'de>> DeserializeSeed<'de> for NormalizedSeed<T> {
    type Value = T::Value;
    fn deserialize<D: Deserializer<'de>>(self, inner: D) -> Result<T::Value, D::Error> {
        self.inner.deserialize(Normalized {
            inner,
            timestamp: self.timestamp,
        })
    }
}
struct NormalizedSeq<A>(A);
impl<'de, A: SeqAccess<'de>> SeqAccess<'de> for NormalizedSeq<A> {
    type Error = A::Error;
    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        inner: T,
    ) -> Result<Option<T::Value>, A::Error> {
        self.0.next_element_seed(NormalizedSeed {
            inner,
            timestamp: false,
        })
    }
    fn size_hint(&self) -> Option<usize> {
        self.0.size_hint()
    }
}
struct NormalizedMap<A> {
    inner: A,
    timestamp: bool,
}
impl<'de, A: MapAccess<'de>> MapAccess<'de> for NormalizedMap<A> {
    type Error = A::Error;
    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, A::Error> {
        let Some(key) = self.inner.next_key::<String>()? else {
            return Ok(None);
        };
        self.timestamp = super::EXPORT_TIMESTAMP_FIELDS.contains(&key.as_str());
        seed.deserialize(serde::de::value::StringDeserializer::<A::Error>::new(key))
            .map(Some)
    }
    fn next_value_seed<T: DeserializeSeed<'de>>(&mut self, inner: T) -> Result<T::Value, A::Error> {
        self.inner.next_value_seed(NormalizedSeed {
            inner,
            timestamp: self.timestamp,
        })
    }
    fn size_hint(&self) -> Option<usize> {
        self.inner.size_hint()
    }
}
