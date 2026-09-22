use crate::error::{Error, Result};
use byteorder::{LittleEndian, ReadBytesExt};
use serde::de::IntoDeserializer;

/// How deep a value may nest. Every struct, enum, sequence, map and
/// newtype costs a level, so a real message sits well under a hundred;
/// the limit is there because recursion is on the reader thread's stack
/// and the wire could otherwise ask for a million levels.
const MAX_DEPTH: usize = 128;
const MAX_DECODED_BYTES: usize = 256 * 1024 * 1024;

pub struct Deserializer<'a> {
    reader: &'a mut std::io::Read,
    depth: usize,
    remaining_bytes: usize,
}

impl<'a> Deserializer<'a> {
    pub fn new(reader: &'a mut std::io::Read) -> Self {
        Self { reader, depth: 0, remaining_bytes: MAX_DECODED_BYTES }
    }

    fn charge(&mut self, bytes: usize) -> Result<()> {
        self.remaining_bytes = self.remaining_bytes.checked_sub(bytes)
            .ok_or_else(|| Error::Message("decoded value exceeds allocation budget".into()))?;
        Ok(())
    }

    fn enter(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(Error::Message(format!(
                "value nests deeper than {MAX_DEPTH} levels"
            )));
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    fn read_signed(&mut self) -> Result<i64> {
        leb128::read::signed(&mut self.reader).map_err(Into::into)
    }

    fn read_unsigned(&mut self) -> Result<u64> {
        leb128::read::unsigned(&mut self.reader).map_err(Into::into)
    }

    fn read_vec(&mut self) -> Result<Vec<u8>> {
        let len: usize = serde::Deserialize::deserialize(&mut *self)?;
        self.charge(len)?;
        // Grow as the bytes arrive rather than trusting the declared
        // length: the stream, not the header, bounds the allocation.
        const STEP: usize = 64 * 1024;
        let mut result = Vec::with_capacity(len.min(STEP));
        let mut remaining = len;
        while remaining > 0 {
            let chunk = remaining.min(STEP);
            let start = result.len();
            result.resize(start + chunk, 0);
            self.reader.read_exact(&mut result[start..])?;
            remaining -= chunk;
        }
        Ok(result)
    }

    fn read_string(&mut self) -> Result<String> {
        let vec = self.read_vec()?;
        String::from_utf8(vec).map_err(|e| Error::InvalidUtf8Encoding(e.utf8_error()))
    }
}

macro_rules! impl_uint {
    ($ty:ty, $dser_method:ident, $visitor_method:ident, $reader_method:ident) => {
        #[inline]
        fn $dser_method<V>(self, visitor: V) -> Result<V::Value>
            where V: serde::de::Visitor<'de>,
        {
            let value = self.$reader_method()?;
            if value > <$ty>::max_value() as u64 {
                Err(Error::NumberOutOfRange)
            } else {
                visitor.$visitor_method(value as $ty)
            }
        }
    }
}

macro_rules! impl_int {
    ($ty:ty, $dser_method:ident, $visitor_method:ident, $reader_method:ident) => {
        #[inline]
        fn $dser_method<V>(self, visitor: V) -> Result<V::Value>
            where V: serde::de::Visitor<'de>,
        {
            let value = self.$reader_method()?;
            if value < <$ty>::min_value() as i64 || value > <$ty>::max_value() as i64 {
                Err(Error::NumberOutOfRange)
            } else {
                visitor.$visitor_method(value as $ty)
            }
        }
    }
}
macro_rules! impl_float {
    ($dser_method:ident, $visitor_method:ident, $reader_method:ident) => {
        #[inline]
        fn $dser_method<V>(self, visitor: V) -> Result<V::Value>
            where V: serde::de::Visitor<'de>,
        {
            let value = self.reader.$reader_method::<LittleEndian>()?;
            visitor.$visitor_method(value)
        }
    }
}

impl<'de, 'a, 'b> serde::Deserializer<'de> for &'a mut Deserializer<'b> {
    type Error = Error;

    #[inline]
    fn deserialize_u8<V>(self, visitor: V) -> Result<V::Value>
    where
        V: serde::de::Visitor<'de>,
    {
        visitor.visit_u8(self.reader.read_u8()?)
    }

    #[inline]
    fn deserialize_i8<V>(self, visitor: V) -> Result<V::Value>
    where
        V: serde::de::Visitor<'de>,
    {
        visitor.visit_i8(self.reader.read_i8()?)
    }

    impl_uint!(u16, deserialize_u16, visit_u16, read_unsigned);
    impl_uint!(u32, deserialize_u32, visit_u32, read_unsigned);
    impl_uint!(u64, deserialize_u64, visit_u64, read_unsigned);

    impl_int!(i16, deserialize_i16, visit_i16, read_signed);
    impl_int!(i32, deserialize_i32, visit_i32, read_signed);
    impl_int!(i64, deserialize_i64, visit_i64, read_signed);

    impl_float!(deserialize_f32, visit_f32, read_f32);
    impl_float!(deserialize_f64, visit_f64, read_f64);

    #[inline]
    fn deserialize_any<V>(self, _visitor: V) -> Result<V::Value>
    where
        V: serde::de::Visitor<'de>,
    {
        Err(Error::DeserializeAnyNotSupported)
    }

    fn deserialize_bool<V>(self, visitor: V) -> Result<V::Value>
    where
        V: serde::de::Visitor<'de>,
    {
        let value: u8 = serde::Deserialize::deserialize(self)?;
        match value {
            1 => visitor.visit_bool(true),
            0 => visitor.visit_bool(false),
            value => Err(Error::InvalidBoolEncoding(value).into()),
        }
    }

    fn deserialize_unit<V>(self, visitor: V) -> Result<V::Value>
    where
        V: serde::de::Visitor<'de>,
    {
        visitor.visit_unit()
    }

    fn deserialize_char<V>(self, visitor: V) -> Result<V::Value>
    where
        V: serde::de::Visitor<'de>,
    {
        let value: u32 = serde::Deserialize::deserialize(self)?;
        match std::char::from_u32(value) {
            Some(c) => visitor.visit_char(c),
            None => Err(Error::InvalidCharEncoding(value)),
        }
    }

    fn deserialize_str<V>(self, visitor: V) -> Result<V::Value>
    where
        V: serde::de::Visitor<'de>,
    {
        visitor.visit_string(self.read_string()?)
    }

    fn deserialize_string<V>(self, visitor: V) -> Result<V::Value>
    where
        V: serde::de::Visitor<'de>,
    {
        visitor.visit_string(self.read_string()?)
    }

    fn deserialize_bytes<V>(self, visitor: V) -> Result<V::Value>
    where
        V: serde::de::Visitor<'de>,
    {
        visitor.visit_byte_buf(self.read_vec()?)
    }

    fn deserialize_byte_buf<V>(self, visitor: V) -> Result<V::Value>
    where
        V: serde::de::Visitor<'de>,
    {
        visitor.visit_byte_buf(self.read_vec()?)
    }

    fn deserialize_enum<V>(
        self,
        _enum: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value>
    where
        V: serde::de::Visitor<'de>,
    {
        self.enter()?;
        let value = visitor.visit_enum(&mut *self);
        self.leave();
        value
    }

    fn deserialize_tuple<V>(self, len: usize, visitor: V) -> Result<V::Value>
    where
        V: serde::de::Visitor<'de>,
    {
        self.enter()?;
        let value = visitor.visit_seq(Access {
            deserializer: &mut *self,
            len,
        });
        self.leave();
        value
    }

    fn deserialize_option<V>(self, visitor: V) -> Result<V::Value>
    where
        V: serde::de::Visitor<'de>,
    {
        let value: u8 = serde::de::Deserialize::deserialize(&mut *self)?;
        match value {
            0 => visitor.visit_none(),
            1 => {
                self.enter()?;
                let result = visitor.visit_some(&mut *self);
                self.leave();
                result
            }
            v => Err(Error::InvalidTagEncoding(v as usize)),
        }
    }

    fn deserialize_seq<V>(self, visitor: V) -> Result<V::Value>
    where
        V: serde::de::Visitor<'de>,
    {
        let len = serde::Deserialize::deserialize(&mut *self)?;

        self.deserialize_tuple(len, visitor)
    }

    fn deserialize_map<V>(self, visitor: V) -> Result<V::Value>
    where
        V: serde::de::Visitor<'de>,
    {
        let len = serde::Deserialize::deserialize(&mut *self)?;

        self.enter()?;
        let value = visitor.visit_map(Access {
            deserializer: &mut *self,
            len,
        });
        self.leave();
        value
    }

    fn deserialize_struct<V>(
        self,
        _name: &str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value>
    where
        V: serde::de::Visitor<'de>,
    {
        self.deserialize_tuple(fields.len(), visitor)
    }

    fn deserialize_identifier<V>(self, _visitor: V) -> Result<V::Value>
    where
        V: serde::de::Visitor<'de>,
    {
        Err(Error::DeserializeIdentifierNotSupported)
    }

    fn deserialize_newtype_struct<V>(self, _name: &str, visitor: V) -> Result<V::Value>
    where
        V: serde::de::Visitor<'de>,
    {
        self.enter()?;
        let value = visitor.visit_newtype_struct(&mut *self);
        self.leave();
        value
    }

    fn deserialize_unit_struct<V>(self, _name: &'static str, visitor: V) -> Result<V::Value>
    where
        V: serde::de::Visitor<'de>,
    {
        visitor.visit_unit()
    }

    fn deserialize_tuple_struct<V>(
        self,
        _name: &'static str,
        len: usize,
        visitor: V,
    ) -> Result<V::Value>
    where
        V: serde::de::Visitor<'de>,
    {
        self.deserialize_tuple(len, visitor)
    }

    fn deserialize_ignored_any<V>(self, _visitor: V) -> Result<V::Value>
    where
        V: serde::de::Visitor<'de>,
    {
        Err(Error::DeserializeIgnoredAnyNotSupported)
    }

    fn is_human_readable(&self) -> bool {
        false
    }
}

struct Access<'a, 'b> {
    deserializer: &'a mut Deserializer<'b>,
    len: usize,
}

impl<'de, 'a, 'b> serde::de::SeqAccess<'de> for Access<'a, 'b> {
    type Error = Error;

    fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>>
    where
        T: serde::de::DeserializeSeed<'de>,
    {
        if self.len > 0 {
            self.len -= 1;
            self.deserializer.charge(std::mem::size_of::<T::Value>().max(1))?;
            let value = serde::de::DeserializeSeed::deserialize(seed, &mut *self.deserializer)?;
            Ok(Some(value))
        } else {
            Ok(None)
        }
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.len.min(4096))
    }
}

impl<'de, 'a, 'b> serde::de::MapAccess<'de> for Access<'a, 'b> {
    type Error = Error;

    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>>
    where
        K: serde::de::DeserializeSeed<'de>,
    {
        if self.len > 0 {
            self.len -= 1;
            self.deserializer.charge(std::mem::size_of::<K::Value>().max(1))?;
            let key = serde::de::DeserializeSeed::deserialize(seed, &mut *self.deserializer)?;
            Ok(Some(key))
        } else {
            Ok(None)
        }
    }

    fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value>
    where
        V: serde::de::DeserializeSeed<'de>,
    {
        self.deserializer.charge(std::mem::size_of::<V::Value>().max(1))?;
        let value = serde::de::DeserializeSeed::deserialize(seed, &mut *self.deserializer)?;
        Ok(value)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.len.min(4096))
    }
}

impl<'de, 'a, 'b> serde::de::EnumAccess<'de> for &'a mut Deserializer<'b> {
    type Error = Error;
    type Variant = Self;

    fn variant_seed<V>(self, seed: V) -> Result<(V::Value, Self::Variant)>
    where
        V: serde::de::DeserializeSeed<'de>,
    {
        let idx: u32 = serde::de::Deserialize::deserialize(&mut *self)?;
        let val: Result<_> = seed.deserialize(idx.into_deserializer());
        Ok((val?, self))
    }
}

impl<'de, 'a, 'b> serde::de::VariantAccess<'de> for &'a mut Deserializer<'b> {
    type Error = Error;

    fn unit_variant(self) -> Result<()> {
        Ok(())
    }

    fn newtype_variant_seed<T>(self, seed: T) -> Result<T::Value>
    where
        T: serde::de::DeserializeSeed<'de>,
    {
        serde::de::DeserializeSeed::deserialize(seed, self)
    }

    fn tuple_variant<V>(self, len: usize, visitor: V) -> Result<V::Value>
    where
        V: serde::de::Visitor<'de>,
    {
        serde::de::Deserializer::deserialize_tuple(self, len, visitor)
    }

    fn struct_variant<V>(self, fields: &'static [&'static str], visitor: V) -> Result<V::Value>
    where
        V: serde::de::Visitor<'de>,
    {
        serde::de::Deserializer::deserialize_tuple(self, fields.len(), visitor)
    }
}

#[cfg(test)]
mod security_tests {
    use super::*;

    fn limited<T: serde::de::DeserializeOwned>(data: &[u8], limit: usize) -> Result<T> {
        let mut input = data;
        let mut decoder = Deserializer::new(&mut input);
        decoder.remaining_bytes = limit;
        T::deserialize(&mut decoder)
    }

    #[test]
    fn flat_and_nested_collections_share_the_allocation_budget() {
        let flat = crate::serialize(&vec![String::new(); 100]).unwrap();
        assert!(limited::<Vec<String>>(&flat, 128).is_err());
        let nested = crate::serialize(&vec![vec![String::new(); 4]; 10]).unwrap();
        assert!(limited::<Vec<Vec<String>>>(&nested, 256).is_err());
        let normal = vec!["one".to_string(), "two".to_string()];
        let bytes = crate::serialize(&normal).unwrap();
        assert_eq!(limited::<Vec<String>>(&bytes, 256).unwrap(), normal);
        let map: std::collections::BTreeMap<u32, String> = (0..100).map(|n| (n, String::new())).collect();
        assert!(limited::<std::collections::BTreeMap<u32, String>>(&crate::serialize(&map).unwrap(), 128).is_err());
    }

    #[test]
    fn recursive_option_values_are_bounded_on_a_reader_sized_stack() {
        #[derive(serde_derive::Deserialize)]
        struct Chain(#[allow(dead_code)] Option<Box<Chain>>);
        // Every byte is Some, avoiding an enormous in-memory source fixture.
        let bytes = vec![1u8; 10_000];
        std::thread::Builder::new().stack_size(2 * 1024 * 1024).spawn(move || {
            assert!(crate::deserialize::<Chain, _>(&bytes[..]).is_err());
        }).unwrap().join().unwrap();
    }
}
