//! Extracts Request path segments into type safe structs

use std::collections::HashMap;
use std::error::Error;
use std::fmt::{self, Display};
use std::string::ParseError;
use std::str::{FromStr, ParseBoolError};
use std::num::{ParseFloatError, ParseIntError};

use hyper::Response;
use serde::de::{self, Deserialize, DeserializeSeed, Deserializer, EnumAccess, MapAccess,
                SeqAccess, VariantAccess, Visitor};

use http::PercentDecoded;
use state::{State, StateData};

use router::response::extender::StaticResponseExtender;

/// Extracts the `Request` path into `State`. On failure is capable of extending `Response`
/// to indicate why the extraction process failed.
///
/// This functionality can be simply derived for application structs via `PathExtractor`,
/// which will attempt to populate the associated struct. Combine with the derive
/// `StaticResponseExtender` to have invalid query string data result in "400 Bad Request".
///
/// Custom responses can be created by using the `PathExtractor` derive and then
/// implementing `StaticResponseExtender` independently.
pub trait PathExtractor
    : for<'de> Deserialize<'de> + StaticResponseExtender + StateData {
}

impl<T> PathExtractor for T
where
    for<'de> T: Deserialize<'de> + StaticResponseExtender + StateData,
{
}

/// A `PathExtractor` that does not extract/store any data from the `Request` path.
///
/// Useful in purely static routes and within documentation.
pub struct NoopPathExtractor;

// This doesn't get derived correctly if we just `#[derive(Deserialize)]` above, because the
// Deserializer expects to _ignore_ a value, not just do nothing. By filling in the impl ourselves,
// we can explicitly do nothing.
impl<'de> Deserialize<'de> for NoopPathExtractor {
    fn deserialize<D>(_de: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(NoopPathExtractor)
    }
}

impl StateData for NoopPathExtractor {}

impl StaticResponseExtender for NoopPathExtractor {
    fn extend(_state: &mut State, _res: &mut Response) {}
}

/// Data which is returned from Tree traversal, mapping internal segment value to segment(s)
/// which have been matched against the `Request` path.
///
/// Data is Percent and UTF8 decoded.
#[derive(Debug)]
pub struct SegmentMapping<'a> {
    data: HashMap<&'a str, Vec<&'a PercentDecoded>>,
}

impl<'a> SegmentMapping<'a> {
    pub fn new() -> SegmentMapping<'a> {
        SegmentMapping {
            data: HashMap::new(),
        }
    }

    pub fn insert(&mut self, key: &'a str, val: Vec<&'a PercentDecoded>) {
        self.data.insert(key, val);
    }
}

/// Describes the error cases which can result from deserializing a `SegmentMapping` into a
/// `PathExtractor` provided by the application.
#[derive(Debug)]
pub enum SegmentMappingError {
    /// The `PathExtractor` type is not one which can be deserialized from a `SegmentMapping`.
    /// This deserializer requires a structured type (usually a custom struct) which can be
    /// deserialized from key / value pairs.
    UnexpectedTargetType(&'static str),

    /// An invalid state occurred wherein a "key" (i.e. the name of a route segment) was
    /// deserialized as something other than an `identifier`.
    UnexpectedKeyType,

    /// The type of a value is not one which can be deserialized from `SegmentMapping` values. The
    /// value types are typically primitives, `String`, `Option<T>`, `Vec<T>`, or something which
    /// deserializes in the same manner as one of these (e.g. a custom `enum` can be deserialized
    /// in the same manner as a `String`).
    ///
    /// Attempting to deserialize a value into a struct is one example where this error will be
    /// triggered, since a list of `0..n` values can't be converted into key/value pairs for
    /// mapping into the struct fields.
    UnexpectedValueType(&'static str),

    /// The enum variant is not able to be deserialized from the value, because the variant is not
    /// of the correct type. Only unit variants are supported - that is, enum variants with no data
    /// attached.
    ///
    /// ```rust,no_run
    /// enum MyEnum {
    ///     // This variant is supported.
    ///     UnitVariant,
    ///
    ///     // These variants are not supported, as there is no possible source for the values
    ///     // required to construct them.
    ///     NewtypeVariant(i32),
    ///     TupleVariant(i32, i32, i32),
    ///     StructVariant { i: i32 },
    /// }
    /// ```
    UnexpectedEnumVariantType(&'static str),

    /// An invalid internal state occurred where a segment mapping had no values. This should never
    /// occur because the presence of a key implies the presence of a value.
    NoValues,

    /// Multiple values were present, but the target type expected only a single value.
    // TODO: How is this triggered?
    MultipleValues,

    /// An invalid internal state occurred where the deserializer attempted to access a value but
    /// there was no current item. This should never occur because the attempt to access a value
    /// implies that the deserializer already retrieved the key from the current item.
    NoCurrentItem,

    /// An error occurred while parsing a string into a value type for one of the fields. For
    /// example, in a route for `/resource/:id`, and with `id: i32` in the `PathExtractor` struct,
    /// a request for `/resource/abc` would result in a parse error trying to convert to `i32`.
    ParseError(String),

    /// An error occurred, and a `Deserialize` impl provided a custom error message. This is used
    /// in the implementation of the `serde::de::Error` trait for external types to provide
    /// informative error messages.
    Custom(String),
}

impl Display for SegmentMappingError {
    fn fmt(&self, out: &mut fmt::Formatter) -> fmt::Result {
        out.write_fmt(format_args!("{:?}", self))
    }
}

impl Error for SegmentMappingError {
    fn description(&self) -> &str {
        unimplemented!()
    }
}

impl de::Error for SegmentMappingError {
    fn custom<T>(t: T) -> SegmentMappingError
    where
        T: Display,
    {
        SegmentMappingError::Custom(format!("{}", t))
    }
}

/// Implements one `Deserializer` function (`$trait_fn`) to parse a single value using the
/// `parse_single_value` function herein.
macro_rules! single_value_type {
    ($trait_fn:ident, $visitor_fn:ident) => {
        fn $trait_fn<V>(self, visitor: V) -> Result<V::Value, Self::Error>
        where
            V: Visitor<'de>
        {
            let v = parse_single_value(self.values, stringify!($trait_fn))?;
            visitor.$visitor_fn(v)
        }
    }
}

/// Implements one `Deserializer` function (`$trait_fn`) to return the error defined by the `$err`
/// expression. For `Deserializer` functions with different signatures, the types that follow `self`
/// can be provided as a trailing parameter list.
macro_rules! reject_deserialize_type {
    ($trait_fn:ident, $err:expr) => {
        reject_deserialize_type!($trait_fn, $err, (_visitor: V));
    };

    {$trait_fn:ident, $err:expr, ($($arg_i:ident : $arg_t:ty),+)} => {
        fn $trait_fn<V>(self, $($arg_i: $arg_t),+) -> Result<V::Value, Self::Error>
        where V: Visitor<'de>
        {
            Err($err)
        }
    };
}

/// Specializes the `reject_deserialize_type` macro to return the `UnexpectedTargetType` variant,
/// with the provided `$err` as the descriptive string.
macro_rules! reject_target_type {
    ($trait_fn:ident, $name:expr) => {
        reject_target_type!($trait_fn, $name, (_visitor: V));
    };

    ($trait_fn:ident, $name:expr, ($($arg_i:ident : $arg_t:ty),+)) => {
        reject_deserialize_type!(
            $trait_fn,
            SegmentMappingError::UnexpectedTargetType(
                concat!("unsupported target type for path extractor: ", $name)
            ),
            ($($arg_i: $arg_t),+)
        );
    };
}

/// Specializes the `reject_deserialize_type` macro to return the `UnexpectedValueType` variant,
/// with the provided `$err` as the descriptive string.
macro_rules! reject_value_type {
    ($trait_fn:ident, $name:expr) => {
        reject_value_type!($trait_fn, $name, (_visitor: V));
    };

    ($trait_fn:ident, $name:expr, ($($arg_i:ident : $arg_t:ty),+)) => {
        reject_deserialize_type!(
            $trait_fn,
            SegmentMappingError::UnexpectedValueType(
                concat!("unsupported value type for path extractor: ", $name)
            ),
            ($($arg_i: $arg_t),+)
        );
    };
}

/// Implements a `Deserializer` for the full set of extracted path segments. This is the top level
/// of the serde side of path extraction. Primarily, we're only checking that we're deserializing
/// into a supported type. In the "normal" case, `deserialize_struct` is the only thing invoked
/// here, and we use `SegmentMappingAccess` to loop through the mappings populating the struct.
impl<'de> Deserializer<'de> for SegmentMapping<'de> {
    type Error = SegmentMappingError;

    fn deserialize_map<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_map(SegmentMappingAccess {
            iter: self.data.into_iter(),
            current: None,
        })
    }

    fn deserialize_struct<V>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_map(visitor)
    }

    fn deserialize_unit<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_unit(visitor)
    }

    fn deserialize_newtype_struct<V>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_newtype_struct(self)
    }

    // Reject types that don't make sense to deserialize at the top level. Since we have a map of
    // key/value pairs, we can't serialize into anything that expects one or more _values_ but no
    // keys. That rules out most types.
    reject_target_type!(deserialize_any, "'any'");
    reject_target_type!(deserialize_bool, "bool");
    reject_target_type!(deserialize_i8, "i8");
    reject_target_type!(deserialize_i16, "i16");
    reject_target_type!(deserialize_i32, "i32");
    reject_target_type!(deserialize_i64, "i64");
    reject_target_type!(deserialize_u8, "u8");
    reject_target_type!(deserialize_u16, "u16");
    reject_target_type!(deserialize_u32, "u32");
    reject_target_type!(deserialize_u64, "u64");
    reject_target_type!(deserialize_f32, "f32");
    reject_target_type!(deserialize_f64, "f64");
    reject_target_type!(deserialize_char, "char");
    reject_target_type!(deserialize_str, "str");
    reject_target_type!(deserialize_string, "String");
    reject_target_type!(deserialize_bytes, "bytes");
    reject_target_type!(deserialize_byte_buf, "byte buffer");
    reject_target_type!(deserialize_option, "Option<T>");
    reject_target_type!(deserialize_seq, "sequence");
    reject_target_type!(deserialize_tuple, "tuple", (_len: usize, _visitor: V));
    reject_target_type!(
        deserialize_tuple_struct,
        "tuple struct",
        (_name: &'static str, _len: usize, _visitor: V)
    );
    reject_target_type!(
        deserialize_enum,
        "enum",
        (
            _name: &'static str,
            _variants: &'static [&'static str],
            _visitor: V
        )
    );
    reject_target_type!(deserialize_identifier, "identifier");
    reject_target_type!(deserialize_ignored_any, "ignored_any");
}

/// Iterates through the segment mappings, yielding each pair of (key, values).
struct SegmentMappingAccess<'a, I>
where
    I: Iterator<Item = (&'a str, Vec<&'a PercentDecoded>)>,
{
    iter: I,
    current: Option<I::Item>,
}

impl<'de, 'a: 'de, I> MapAccess<'de> for SegmentMappingAccess<'a, I>
where
    I: Iterator<Item = (&'a str, Vec<&'a PercentDecoded>)>,
{
    type Error = SegmentMappingError;

    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>, Self::Error>
    where
        K: DeserializeSeed<'de>,
    {
        self.current = self.iter.next();
        match self.current {
            Some((ref key, ref _v)) => {
                let key = seed.deserialize(DeserializeKey { key })?;
                Ok(Some(key))
            }
            None => Ok(None),
        }
    }

    fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value, Self::Error>
    where
        V: DeserializeSeed<'de>,
    {
        match self.current.take() {
            Some((_k, values)) => seed.deserialize(DeserializeValues { values }),
            None => Err(SegmentMappingError::NoCurrentItem),
        }
    }
}

/// Deserializes an identifier string into an identifier. Just serde boilerplate.
struct DeserializeKey<'de> {
    key: &'de str,
}

impl<'de> Deserializer<'de> for DeserializeKey<'de> {
    type Error = SegmentMappingError;

    fn deserialize_identifier<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_str(self.key)
    }

    fn deserialize_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        // This really should be unreachable, but we return an error here to be polite.
        Err(SegmentMappingError::UnexpectedKeyType)
    }

    forward_to_deserialize_any! {
        bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str string bytes
        byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct enum ignored_any
    }
}

/// Deserializes one or multiple values into the value type. This is (indirectly) where the actual
/// conversion from percent-decoded strings into the _actual_ values occurs.
struct DeserializeValues<'de> {
    values: Vec<&'de PercentDecoded>,
}

/// Convert the value from a single-item list of percent-decoded strings by using
/// `<T as FromStr>::parse`. Returns an error if the list didn't have exactly one item in it, or if
/// the value failed to parse.
fn parse_single_value<'de, T>(
    values: Vec<&'de PercentDecoded>,
    fn_name: &'static str,
) -> Result<T, SegmentMappingError>
where
    T: FromStr,
    T::Err: Display,
{
    extract_single_value(values).and_then(|value| match value.val().parse() {
        Ok(t) => Ok(t),
        Err(e) => Err(SegmentMappingError::ParseError(format!("{}", e))),
    })
}

fn extract_single_value<'de>(
    values: Vec<&'de PercentDecoded>,
) -> Result<&'de PercentDecoded, SegmentMappingError> {
    let mut iter = values.into_iter();
    match (iter.next(), iter.next()) {
        (Some(val), None) => Ok(val),
        (Some(_), Some(_)) => Err(SegmentMappingError::MultipleValues),
        (None, _) => Err(SegmentMappingError::NoValues),
    }
}

impl<'de> Deserializer<'de> for DeserializeValues<'de> {
    type Error = SegmentMappingError;

    // Handle all the primitive types via `parse_single_value`
    single_value_type!(deserialize_bool, visit_bool);
    single_value_type!(deserialize_i8, visit_i8);
    single_value_type!(deserialize_i16, visit_i16);
    single_value_type!(deserialize_i32, visit_i32);
    single_value_type!(deserialize_i64, visit_i64);
    single_value_type!(deserialize_u8, visit_u8);
    single_value_type!(deserialize_u16, visit_u16);
    single_value_type!(deserialize_u32, visit_u32);
    single_value_type!(deserialize_u64, visit_u64);
    single_value_type!(deserialize_f32, visit_f32);
    single_value_type!(deserialize_f64, visit_f64);
    single_value_type!(deserialize_string, visit_string);
    single_value_type!(deserialize_byte_buf, visit_string);
    single_value_type!(deserialize_char, visit_char);

    fn deserialize_ignored_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_unit()
    }

    fn deserialize_unit<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_unit()
    }

    fn deserialize_bytes<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        let val = extract_single_value(self.values)?;
        visitor.visit_borrowed_bytes(val.val().as_bytes())
    }

    fn deserialize_str<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        let val = extract_single_value(self.values)?;
        visitor.visit_borrowed_str(val.val())
    }

    fn deserialize_option<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_some(self)
    }

    fn deserialize_enum<V>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        let value = extract_single_value(self.values)?;
        visitor.visit_enum(ValueEnum { value })
    }

    fn deserialize_seq<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_seq(ValueSeq {
            values: self.values.iter().map(|&p| p),
        })
    }

    fn deserialize_newtype_struct<V>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        unimplemented!()
    }
    //reject_value_type!(deserialize_any, "'any'");

    reject_value_type!(deserialize_map, "map");
    reject_value_type!(deserialize_identifier, "identifier");
    reject_value_type!(
        deserialize_struct,
        "struct",
        (
            _name: &'static str,
            _fields: &'static [&'static str],
            _visitor: V
        )
    );
    reject_value_type!(deserialize_tuple, "tuple", (_len: usize, _visitor: V));
    reject_value_type!(
        deserialize_tuple_struct,
        "tuple struct",
        (_name: &'static str, _len: usize, _visitor: V)
    );
}

struct ValueSeq<'de, I>
where
    I: Iterator<Item = &'de PercentDecoded>,
{
    values: I,
}

impl<'de, I> SeqAccess<'de> for ValueSeq<'de, I>
where
    I: Iterator<Item = &'de PercentDecoded>,
{
    type Error = SegmentMappingError;

    fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>, Self::Error>
    where
        T: DeserializeSeed<'de>,
    {
        match self.values.next() {
            Some(val) => {
                let val = seed.deserialize(DeserializeValues { values: vec![val] })?;
                Ok(Some(val))
            }
            None => Ok(None),
        }
    }
}

struct ValueEnum<'de> {
    value: &'de PercentDecoded,
}

impl<'de> EnumAccess<'de> for ValueEnum<'de> {
    type Error = SegmentMappingError;
    type Variant = UnitVariant;

    fn variant_seed<V>(self, seed: V) -> Result<(V::Value, Self::Variant), Self::Error>
    where
        V: DeserializeSeed<'de>,
    {
        let variant_name = seed.deserialize(DeserializeKey {
            key: self.value.val(),
        })?;
        Ok((variant_name, UnitVariant))
    }
}

struct UnitVariant;

impl<'de> VariantAccess<'de> for UnitVariant {
    type Error = SegmentMappingError;

    fn unit_variant(self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn newtype_variant_seed<T>(self, seed: T) -> Result<T::Value, Self::Error>
    where
        T: DeserializeSeed<'de>,
    {
        Err(SegmentMappingError::UnexpectedEnumVariantType(
            "enum newtype variants are unsupported in path extractors",
        ))
    }

    fn tuple_variant<V>(self, len: usize, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        Err(SegmentMappingError::UnexpectedEnumVariantType(
            "enum tuple variants are unsupported in path extractors",
        ))
    }

    fn struct_variant<V>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        Err(SegmentMappingError::UnexpectedEnumVariantType(
            "enum struct variants are unsupported in path extractors",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct SimpleValues {
        bool_val: bool,
        i8_val: i8,
        i16_val: i16,
        i32_val: i32,
        i64_val: i64,
        u8_val: u8,
        u16_val: u16,
        u32_val: u32,
        u64_val: u64,
        f32_val: f32,
        f64_val: f64,
        string_val: String,
        char_val: char,
        optional_val: Option<String>,
        missing_optional_val: Option<String>,
    }

    #[test]
    fn simple_values_tests() {
        let bool_val = PercentDecoded::new("true").unwrap();
        let i8_val = PercentDecoded::new("15").unwrap();
        let i16_val = PercentDecoded::new("511").unwrap();
        let i32_val = PercentDecoded::new("90000").unwrap();
        let i64_val = PercentDecoded::new("3000000000").unwrap();
        let u8_val = PercentDecoded::new("215").unwrap();
        let u16_val = PercentDecoded::new("40511").unwrap();
        let u32_val = PercentDecoded::new("4000000000").unwrap();
        let u64_val = PercentDecoded::new("9000000000").unwrap();
        let f32_val = PercentDecoded::new("1.4").unwrap();
        let f64_val = PercentDecoded::new("2.6").unwrap();
        let string_val = PercentDecoded::new("this is an owned string").unwrap();
        let char_val = PercentDecoded::new("a").unwrap();
        let optional_val = PercentDecoded::new("this is optional").unwrap();

        let mut sm = SegmentMapping::new();
        sm.insert("bool_val", vec![&bool_val]);
        sm.insert("i8_val", vec![&i8_val]);
        sm.insert("i16_val", vec![&i16_val]);
        sm.insert("i32_val", vec![&i32_val]);
        sm.insert("i64_val", vec![&i64_val]);
        sm.insert("u8_val", vec![&u8_val]);
        sm.insert("u16_val", vec![&u16_val]);
        sm.insert("u32_val", vec![&u32_val]);
        sm.insert("u64_val", vec![&u64_val]);
        sm.insert("f32_val", vec![&f32_val]);
        sm.insert("f64_val", vec![&f64_val]);
        sm.insert("string_val", vec![&string_val]);
        sm.insert("char_val", vec![&char_val]);
        sm.insert("optional_val", vec![&optional_val]);

        let p = SimpleValues::deserialize(sm).unwrap();

        assert_eq!(p.bool_val, true);
        assert_eq!(p.i8_val, 15);
        assert_eq!(p.i16_val, 511);
        assert_eq!(p.i32_val, 90000);
        assert_eq!(p.i64_val, 3000000000);
        assert_eq!(p.u8_val, 215);
        assert_eq!(p.u16_val, 40511);
        assert_eq!(p.u32_val, 4000000000);
        assert_eq!(p.u64_val, 9000000000);
        assert_eq!(p.f32_val, 1.4);
        assert_eq!(p.f64_val, 2.6);
        assert_eq!(p.string_val, "this is an owned string");
        assert_eq!(p.char_val, 'a');
        assert_eq!(p.optional_val, Some("this is optional".to_owned()));
        assert!(p.missing_optional_val.is_none());
    }

    #[derive(Deserialize)]
    struct WithByteBuf {
        #[serde(deserialize_with = "byte_buf::deserialize")]
        bytes_val: Vec<u8>,
    }

    mod byte_buf {
        use std::fmt;
        use serde::de::*;

        pub fn deserialize<'de, D>(de: D) -> Result<Vec<u8>, D::Error>
        where
            D: Deserializer<'de>,
        {
            de.deserialize_byte_buf(ByteBufVisitor)
        }

        struct ByteBufVisitor;

        impl<'de> Visitor<'de> for ByteBufVisitor {
            type Value = Vec<u8>;

            fn expecting(&self, out: &mut fmt::Formatter) -> fmt::Result {
                out.write_str("string")
            }

            fn visit_string<E>(self, v: String) -> Result<Vec<u8>, E>
            where
                E: Error,
            {
                Ok(v.into_bytes())
            }
        }
    }

    #[test]
    fn byte_buf_values_tests() {
        let bytes_val = PercentDecoded::new("bytes").unwrap();

        let mut sm = SegmentMapping::new();
        sm.insert("bytes_val", vec![&bytes_val]);

        let p = WithByteBuf::deserialize(sm).unwrap();

        assert_eq!(&p.bytes_val[..], b"bytes");
    }

    // This is **not** a realistic use case here, as `StateData` must also be `'static`. However,
    // this proves the implementation of `deserialize_bytes` isn't doing anything that **prevents**
    // this kind of usage.
    #[derive(Deserialize)]
    struct WithBorrowedBytes<'a> {
        #[serde(deserialize_with = "borrowed_bytes::deserialize")]
        bytes_val: &'a [u8],
    }

    mod borrowed_bytes {
        use std::fmt;
        use serde::de::*;

        pub fn deserialize<'de, D>(de: D) -> Result<&'de [u8], D::Error>
        where
            D: Deserializer<'de>,
        {
            de.deserialize_bytes(BorrowedBytesVisitor)
        }

        struct BorrowedBytesVisitor;

        impl<'de> Visitor<'de> for BorrowedBytesVisitor {
            type Value = &'de [u8];

            fn expecting(&self, out: &mut fmt::Formatter) -> fmt::Result {
                out.write_str("borrowed bytes")
            }

            fn visit_borrowed_bytes<E>(self, v: &'de [u8]) -> Result<&'de [u8], E>
            where
                E: Error,
            {
                Ok(v)
            }
        }
    }

    #[test]
    fn borrowed_bytes_tests() {
        let bytes_val = PercentDecoded::new("borrowed_bytes").unwrap();

        let mut sm = SegmentMapping::new();
        sm.insert("bytes_val", vec![&bytes_val]);

        let p = WithBorrowedBytes::deserialize(sm).unwrap();

        assert_eq!(&p.bytes_val[..], b"borrowed_bytes");
    }

    // This is **not** a realistic use case here, as `StateData` must also be `'static`. However,
    // this proves the implementation of `deserialize_str` isn't doing anything that **prevents**
    // this kind of usage.
    #[derive(Deserialize)]
    struct WithBorrowedString<'a> {
        #[serde(deserialize_with = "borrowed_str::deserialize")]
        str_val: &'a str,
    }

    mod borrowed_str {
        use std::fmt;
        use serde::de::*;

        pub fn deserialize<'de, D>(de: D) -> Result<&'de str, D::Error>
        where
            D: Deserializer<'de>,
        {
            de.deserialize_str(BorrowedStrVisitor)
        }

        struct BorrowedStrVisitor;

        impl<'de> Visitor<'de> for BorrowedStrVisitor {
            type Value = &'de str;

            fn expecting(&self, out: &mut fmt::Formatter) -> fmt::Result {
                out.write_str("borrowed string")
            }

            fn visit_borrowed_str<E>(self, v: &'de str) -> Result<&'de str, E> {
                Ok(v)
            }
        }
    }

    #[test]
    fn borrowed_str_tests() {
        let str_val = PercentDecoded::new("borrowed_str").unwrap();

        let mut sm = SegmentMapping::new();
        sm.insert("str_val", vec![&str_val]);

        let p = WithBorrowedString::deserialize(sm).unwrap();

        assert_eq!(p.str_val, "borrowed_str");
    }

    #[derive(Deserialize, Eq, PartialEq, Debug)]
    #[serde(rename_all = "kebab-case")]
    enum MyEnumType {
        A,
        B,
        C,
    }

    #[derive(Deserialize)]
    struct WithEnum {
        enum_val: MyEnumType,
    }

    #[test]
    fn enum_tests() {
        let enum_val = PercentDecoded::new("b").unwrap();

        let mut sm = SegmentMapping::new();
        sm.insert("enum_val", vec![&enum_val]);

        let p = WithEnum::deserialize(sm).unwrap();

        assert_eq!(p.enum_val, MyEnumType::B);
    }

    #[derive(Deserialize)]
    struct WithSeq {
        seq_val: Vec<i32>,
    }

    #[test]
    fn seq_tests() {
        let seq_val_1 = PercentDecoded::new("15").unwrap();
        let seq_val_2 = PercentDecoded::new("16").unwrap();
        let seq_val_3 = PercentDecoded::new("17").unwrap();
        let seq_val_4 = PercentDecoded::new("18").unwrap();
        let seq_val_5 = PercentDecoded::new("19").unwrap();

        let mut sm = SegmentMapping::new();
        sm.insert(
            "seq_val",
            vec![&seq_val_1, &seq_val_2, &seq_val_3, &seq_val_4, &seq_val_5],
        );

        let p = WithSeq::deserialize(sm).unwrap();

        assert_eq!(p.seq_val, vec![15, 16, 17, 18, 19]);
    }

    #[derive(Deserialize, Eq, PartialEq, Debug)]
    struct IntWrapper(i32);

    #[derive(Deserialize)]
    struct WithNewtypeStruct {
        wrapped_int_val: IntWrapper,
    }

    #[test]
    fn newtype_struct_tests() {
        let wrapped_int_val = PercentDecoded::new("100").unwrap();

        let mut sm = SegmentMapping::new();
        sm.insert("wrapped_int_val", vec![&wrapped_int_val]);

        let p = WithNewtypeStruct::deserialize(sm).unwrap();

        assert_eq!(p.wrapped_int_val, IntWrapper(100));
    }
}
