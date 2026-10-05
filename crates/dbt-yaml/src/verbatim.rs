//! This module defines the `Verbatim` type, which is a wrapper type that can be
//! used to in `#[derive(Deserialize)]` structs to protect fields from the
//! `field_transfomer` when deserialized by the `Value::into_typed` method.

use std::{
    fmt::{self, Debug},
    hash::Hash,
    hash::Hasher,
    ops::{Deref, DerefMut},
};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

////////////////////////////////////////////////////////////////////////

/// A wrapper type that protects the inner value from being transformed by the
/// `field_transformer` when deserialized by the `Value::into_typed` method.
///
/// Generic type parameters `T` is the type of the inner value. Optional type
/// parameter `Sch` is used to specify a type for JsonSchema generation, and
/// defaults to the same type as `T`. This is useful for cases where e.g. you
/// want to capture the value of a field as a `Value` during deserialization,
/// but still treat the field *as though* it were some primitive type like
/// `bool` in the Json schema -- in which case you would use `Verbatim<Value,
/// bool>`.
pub struct Verbatim<T, Sch = T>(pub T, pub std::marker::PhantomData<Sch>);

impl<T, Sch> Deref for Verbatim<T, Sch> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T, Sch> DerefMut for Verbatim<T, Sch> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T, Sch> AsRef<T> for Verbatim<T, Sch> {
    fn as_ref(&self) -> &T {
        &self.0
    }
}

impl<T, Sch> AsMut<T> for Verbatim<T, Sch> {
    fn as_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

impl<T, Sch> Clone for Verbatim<T, Sch>
where
    T: Clone,
{
    fn clone(&self) -> Self {
        Verbatim(self.0.clone(), std::marker::PhantomData::<Sch>)
    }
}

impl<T, Sch> Copy for Verbatim<T, Sch> where T: Copy {}

impl<T, Sch> Debug for Verbatim<T, Sch>
where
    T: Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<T, Sch> PartialEq for Verbatim<T, Sch>
where
    T: PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<T, Sch> Eq for Verbatim<T, Sch> where T: Eq {}

impl<T, Sch> PartialOrd for Verbatim<T, Sch>
where
    T: PartialOrd,
{
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.0.partial_cmp(&other.0)
    }
}

impl<T, Sch> Ord for Verbatim<T, Sch>
where
    T: Ord,
{
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

impl<T, Sch> Hash for Verbatim<T, Sch>
where
    T: Hash,
{
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state)
    }
}

impl<T, Sch> Default for Verbatim<T, Sch>
where
    T: Default,
{
    fn default() -> Self {
        Verbatim(T::default(), std::marker::PhantomData::<Sch>)
    }
}

impl<T, Sch> From<T> for Verbatim<T, Sch> {
    fn from(value: T) -> Self {
        Verbatim(value, std::marker::PhantomData::<Sch>)
    }
}

impl<T, Sch> Serialize for Verbatim<T, Sch>
where
    T: Serialize,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de, T, Sch> Deserialize<'de> for Verbatim<T, Sch>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let _g = with_should_not_transform_any();
        T::deserialize(deserializer).map(|value| Verbatim(value, std::marker::PhantomData::<Sch>))
    }
}

#[cfg(feature = "schemars")]
impl<T, Sch> schemars::JsonSchema for Verbatim<T, Sch>
where
    Sch: schemars::JsonSchema,
{
    fn schema_name() -> String {
        Sch::schema_name()
    }

    fn json_schema(generator: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        Sch::json_schema(generator)
    }

    fn is_referenceable() -> bool {
        Sch::is_referenceable()
    }

    fn schema_id() -> std::borrow::Cow<'static, str> {
        Sch::schema_id()
    }

    #[doc(hidden)]
    fn _schemars_private_non_optional_json_schema(
        generator: &mut schemars::gen::SchemaGenerator,
    ) -> schemars::schema::Schema {
        Sch::_schemars_private_non_optional_json_schema(generator)
    }

    #[doc(hidden)]
    fn _schemars_private_is_option() -> bool {
        Sch::_schemars_private_is_option()
    }
}

#[cfg(feature = "schemars")]
/// Module containing `MaybeTransformable` wrapper type for `DbtSchema` derive.
pub mod maybe_transformable {
    use std::collections::BTreeMap;

    const TRANSFORMABLE_STRING_PATTERN: &str = "^\\{.*\\}$";

    /// Transforms the schema of a field to allow it to be either the original
    /// type or a string matching the `TRANSFORMABLE_STRING_PATTERN`.
    fn make_stringable_field(schema: schemars::schema::Schema) -> schemars::schema::Schema {
        let schemars::schema::Schema::Object(mut schema_object) = schema else {
            return schema;
        };

        if let Some(obj) = &mut schema_object.object {
            if let Some(add) = obj.additional_properties.take() {
                obj.additional_properties = Some(Box::new(make_stringable_field(*add)));
            }
        }
        if let Some(arr) = &mut schema_object.array {
            if let Some(items) = arr.items.take() {
                match items {
                    schemars::schema::SingleOrVec::Single(sch) => {
                        arr.items = Some(schemars::schema::SingleOrVec::Single(Box::new(
                            make_stringable_field(*sch),
                        )));
                    }
                    schemars::schema::SingleOrVec::Vec(schs) => {
                        arr.items = Some(schemars::schema::SingleOrVec::Vec(
                            schs.into_iter().map(make_stringable_field).collect(),
                        ));
                    }
                }
            }
        }

        let is_primitive = match schema_object.instance_type {
            Some(schemars::schema::SingleOrVec::Single(ref mut ty)) => match **ty {
                schemars::schema::InstanceType::Boolean
                | schemars::schema::InstanceType::Integer
                | schemars::schema::InstanceType::Number => true,
                _ => false,
            },
            Some(schemars::schema::SingleOrVec::Vec(ref mut tys)) => {
                tys.iter().any(|ty| match *ty {
                    schemars::schema::InstanceType::Boolean
                    | schemars::schema::InstanceType::Integer
                    | schemars::schema::InstanceType::Number => true,
                    _ => false,
                })
            }
            _ => false,
        };

        if is_primitive {
            let subschema = schemars::schema::SubschemaValidation {
                any_of: Some(vec![
                    schemars::schema::Schema::Object(schema_object),
                    schemars::schema::Schema::Object(schemars::schema::SchemaObject {
                        instance_type: Some(schemars::schema::SingleOrVec::Single(Box::new(
                            schemars::schema::InstanceType::String,
                        ))),
                        string: Some(Box::new(schemars::schema::StringValidation {
                            pattern: Some(TRANSFORMABLE_STRING_PATTERN.to_string()),
                            ..Default::default()
                        })),
                        ..Default::default()
                    }),
                ]),
                ..Default::default()
            };
            schemars::schema::Schema::Object(schemars::schema::SchemaObject {
                subschemas: Some(Box::new(subschema)),
                ..Default::default()
            })
        } else {
            schemars::schema::Schema::Object(schema_object)
        }
    }

    /// Transforms the given schema to allow transformable fields to be strings.
    pub fn maybe_transformable(schema: schemars::schema::Schema) -> schemars::schema::Schema {
        if !SHOULD_GENERATE_PRE_TRANSFORMATION_SCHEMA.with(|flag| flag.get()) {
            return schema;
        }
        let schemars::schema::Schema::Object(mut schema_object) = schema else {
            return schema;
        };

        match &schema_object.instance_type {
            Some(schemars::schema::SingleOrVec::Single(ty)) => match **ty {
                schemars::schema::InstanceType::Object => {
                    // This is a struct -- apply the transformation to all
                    // fields in the struct
                    if let Some(obj) = schema_object.object.take() {
                        let properties = obj
                            .properties
                            .into_iter()
                            .map(|(key, value)| (key, make_stringable_field(value)))
                            .collect::<BTreeMap<_, _>>();
                        let additional_properties = obj
                            .additional_properties
                            .map(|add| Box::new(make_stringable_field(*add)));
                        schema_object.object = Some(Box::new(schemars::schema::ObjectValidation {
                            properties,
                            additional_properties,
                            ..*obj
                        }));
                    }
                }
                schemars::schema::InstanceType::Array => {
                    // This is a Vec -- apply the transformation to the items
                    if let Some(arr) = &mut schema_object.array {
                        if let Some(items) = arr.items.take() {
                            match items {
                                schemars::schema::SingleOrVec::Single(sch) => {
                                    arr.items = Some(schemars::schema::SingleOrVec::Single(
                                        Box::new(make_stringable_field(*sch)),
                                    ));
                                }
                                schemars::schema::SingleOrVec::Vec(schs) => {
                                    arr.items = Some(schemars::schema::SingleOrVec::Vec(
                                        schs.into_iter().map(make_stringable_field).collect(),
                                    ));
                                }
                            }
                        }
                    }
                }
                _ => {}
            },
            None => {
                if let Some(subschemas) = &mut schema_object.subschemas {
                    // This is an enum
                    if let Some(any_of) = &mut subschemas.any_of {
                        // This is an untagged enum -- add a stringable alternative
                        any_of.push(schemars::schema::Schema::Object(
                            schemars::schema::SchemaObject {
                                instance_type: Some(schemars::schema::SingleOrVec::Single(
                                    Box::new(schemars::schema::InstanceType::String),
                                )),
                                string: Some(Box::new(schemars::schema::StringValidation {
                                    pattern: Some(TRANSFORMABLE_STRING_PATTERN.to_string()),
                                    ..Default::default()
                                })),
                                ..Default::default()
                            },
                        ));
                    }
                    if let Some(one_of) = &mut subschemas.one_of {
                        // This is a tagged enum -- apply the transformation to each variant
                        *one_of = one_of.drain(..).map(make_stringable_field).collect();
                    }
                }
            }
            _ => {}
        }

        schemars::schema::Schema::Object(schema_object)
    }

    /// Whether `schemar_for!` should generate a schema that allows
    /// transformable fields to be strings. Call this before generating any
    /// schemas to set the behavior for all subsequent schema generation.
    pub fn set_generate_pre_transformation_schema(value: bool) {
        SHOULD_GENERATE_PRE_TRANSFORMATION_SCHEMA.with(|flag| flag.set(value));
    }

    thread_local! {
        static SHOULD_GENERATE_PRE_TRANSFORMATION_SCHEMA: std::cell::Cell<bool> = const {
            std::cell::Cell::new(false)
        };
    }
}

pub(crate) fn should_transform_any() -> bool {
    SHOULD_TRANSFORM_ANY.with(|flag| flag.get())
}

pub(crate) struct ShouldTransformAnyGuard(bool);

impl Drop for ShouldTransformAnyGuard {
    fn drop(&mut self) {
        SHOULD_TRANSFORM_ANY.with(|flag| flag.set(self.0));
    }
}

pub(crate) fn with_should_not_transform_any() -> ShouldTransformAnyGuard {
    let current = SHOULD_TRANSFORM_ANY.with(|flag| flag.get());
    SHOULD_TRANSFORM_ANY.with(|flag| flag.set(false));
    ShouldTransformAnyGuard(current)
}

thread_local! {
    static SHOULD_TRANSFORM_ANY: std::cell::Cell<bool>  = const {
        std::cell::Cell::new(true)
    };
}
