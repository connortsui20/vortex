// This file is @generated by prost-build.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Scalar {
    #[prost(message, optional, tag = "1")]
    pub dtype: ::core::option::Option<super::dtype::DType>,
    #[prost(message, optional, tag = "2")]
    pub value: ::core::option::Option<ScalarValue>,
}
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ScalarValue {
    #[prost(oneof = "scalar_value::Kind", tags = "1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 12")]
    pub kind: ::core::option::Option<scalar_value::Kind>,
}
/// Nested message and enum types in `ScalarValue`.
pub mod scalar_value {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Kind {
        #[prost(enumeration = "::prost_types::NullValue", tag = "1")]
        NullValue(i32),
        #[prost(bool, tag = "2")]
        BoolValue(bool),
        #[prost(int32, tag = "3")]
        Int32Value(i32),
        #[prost(int64, tag = "4")]
        Int64Value(i64),
        #[prost(uint32, tag = "5")]
        Uint32Value(u32),
        #[prost(uint64, tag = "6")]
        Uint64Value(u64),
        #[prost(float, tag = "7")]
        FloatValue(f32),
        #[prost(double, tag = "8")]
        DoubleValue(f64),
        #[prost(string, tag = "9")]
        StringValue(::prost::alloc::string::String),
        #[prost(bytes, tag = "10")]
        BytesValue(::prost::alloc::vec::Vec<u8>),
        #[prost(message, tag = "12")]
        ListValue(super::ListValue),
    }
}
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ListValue {
    #[prost(message, repeated, tag = "1")]
    pub values: ::prost::alloc::vec::Vec<ScalarValue>,
}
