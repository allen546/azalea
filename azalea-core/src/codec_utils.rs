//! Some functions that are useful to have when implementing
//! `Serialize`/`Deserialize`, which Azalea uses to imitate Minecraft codecs.

use azalea_buf::SerializableUuid;
use serde::{
    Serialize, Serializer,
    ser::{SerializeMap, SerializeSeq, SerializeTupleStruct},
};
use simdnbt::owned::{Nbt, NbtCompound, NbtList, NbtTag};
use uuid::Uuid;

/// Intended to be used for skipping serialization if the value is the default.
///
/// ```ignore
/// #[serde(skip_serializing_if = "is_default")]
/// ```
pub fn is_default<T: Default + PartialEq>(t: &T) -> bool {
    *t == Default::default()
}

/// Intended to be used for skipping serialization if the value is `true`.
///
/// ```ignore
/// #[serde(skip_serializing_if = "is_true")]
/// ```
pub fn is_true(t: &bool) -> bool {
    *t
}

/// If the array has a single item, don't serialize as an array
///
/// ```ignore
/// #[serde(serialize_with = "flatten_array")]
/// ```
pub fn flatten_array<S: Serializer, T: Serialize>(x: &Vec<T>, s: S) -> Result<S::Ok, S::Error> {
    if x.len() == 1 {
        x[0].serialize(s)
    } else {
        x.serialize(s)
    }
}

/// Minecraft writes UUIDs as an IntArray<4>
pub fn uuid<'a, S: Serializer>(
    uuid: impl Into<&'a Option<Uuid>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    if let Some(uuid) = uuid.into() {
        let arr: [u32; 4] = uuid.to_int_array();
        let arr: [i32; 4] = [arr[0] as i32, arr[1] as i32, arr[2] as i32, arr[3] as i32];
        IntArray(arr).serialize(serializer)
    } else {
        serializer.serialize_unit()
    }
}

/// An internal type that makes the i32 array be serialized differently.
///
/// Azalea currently only uses this when writing checksums, but Minecraft also
/// uses this internally when converting types to NBT.
pub struct IntArray<const N: usize>(pub [i32; N]);
/// An internal type that makes the i64 array be serialized differently.
///
/// Azalea currently only uses this when writing checksums, but Minecraft also
/// uses this internally when converting types to NBT.
pub struct LongArray<const N: usize>(pub [i64; N]);

impl<const N: usize> Serialize for IntArray<N> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // see checksum::serialize_tuple_struct
        let mut seq = serializer.serialize_tuple_struct("azalea:int_array", N)?;
        for &item in &self.0 {
            seq.serialize_field(&item)?;
        }
        seq.end()
    }
}
impl<const N: usize> Serialize for LongArray<N> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // see checksum::serialize_tuple_struct
        let mut seq = serializer.serialize_tuple_struct("azalea:long_array", N)?;
        for &item in &self.0 {
            seq.serialize_field(&item)?;
        }
        seq.end()
    }
}

/// Serialize NBT like vanilla's `NbtOps` conversion does: byte, int and long
/// arrays stay arrays instead of becoming lists of numbers.
///
/// ```ignore
/// #[serde(serialize_with = "serialize_nbt")]
/// ```
pub fn serialize_nbt<S: Serializer>(nbt: &Nbt, s: S) -> Result<S::Ok, S::Error> {
    serialize_nbt_compound(nbt, s)
}

/// Like [`serialize_nbt`], but for a compound.
pub fn serialize_nbt_compound<S: Serializer>(c: &NbtCompound, s: S) -> Result<S::Ok, S::Error> {
    NbtCompoundSerializable(c).serialize(s)
}

struct NbtCompoundSerializable<'a>(&'a NbtCompound);
struct NbtTagSerializable<'a>(&'a NbtTag);
struct NbtListSerializable<'a>(&'a NbtList);

impl Serialize for NbtCompoundSerializable<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // a compound with only an empty key is how Minecraft stores a
        // mixed-type list item
        if self.0.len() == 1
            && let Some(value) = self.0.get("")
        {
            return NbtTagSerializable(value).serialize(serializer);
        }
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (key, value) in self.0.iter() {
            map.serialize_entry(&key.to_str(), &NbtTagSerializable(value))?;
        }
        map.end()
    }
}

impl Serialize for NbtTagSerializable<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            NbtTag::ByteArray(v) => NbtBytes(v).serialize(serializer),
            NbtTag::IntArray(v) => NbtArray("azalea:int_array", v).serialize(serializer),
            NbtTag::LongArray(v) => NbtArray("azalea:long_array", v).serialize(serializer),
            NbtTag::List(v) => NbtListSerializable(v).serialize(serializer),
            NbtTag::Compound(v) => NbtCompoundSerializable(v).serialize(serializer),
            other => other.serialize(serializer),
        }
    }
}

impl Serialize for NbtListSerializable<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        fn seq<'a, S: Serializer, T: Serialize + 'a>(
            serializer: S,
            len: usize,
            items: impl Iterator<Item = T>,
        ) -> Result<S::Ok, S::Error> {
            let mut seq = serializer.serialize_seq(Some(len))?;
            for item in items {
                seq.serialize_element(&item)?;
            }
            seq.end()
        }

        match self.0 {
            NbtList::ByteArray(v) => seq(serializer, v.len(), v.iter().map(|x| NbtBytes(x))),
            NbtList::IntArray(v) => seq(
                serializer,
                v.len(),
                v.iter().map(|x| NbtArray("azalea:int_array", x)),
            ),
            NbtList::LongArray(v) => seq(
                serializer,
                v.len(),
                v.iter().map(|x| NbtArray("azalea:long_array", x)),
            ),
            NbtList::Compound(v) => seq(serializer, v.len(), v.iter().map(NbtCompoundSerializable)),
            NbtList::List(v) => seq(serializer, v.len(), v.iter().map(NbtListSerializable)),
            other => other.serialize(serializer),
        }
    }
}

struct NbtBytes<'a>(&'a [u8]);
impl Serialize for NbtBytes<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(self.0)
    }
}

// see checksum::serialize_tuple_struct
struct NbtArray<'a, T>(&'static str, &'a [T]);
impl<T: Serialize> Serialize for NbtArray<'_, T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_tuple_struct(self.0, self.1.len())?;
        for item in self.1 {
            seq.serialize_field(item)?;
        }
        seq.end()
    }
}
