use std::{
    borrow::Cow,
    collections::BTreeMap,
    fmt::{self, Write as _},
    ops::Index,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Value {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Self>),
    Object(BTreeMap<String, Self>),
}

impl Value {
    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn as_array(&self) -> Option<&[Self]> {
        match self {
            Self::Array(values) => Some(values),
            _ => None,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => output.write_str("null"),
            Self::Bool(value) => output.write_str(if *value { "true" } else { "false" }),
            Self::Number(value) => output.write_str(value),
            Self::String(value) => write_string(output, value),
            Self::Array(values) => {
                output.write_char('[')?;
                for (index, value) in values.iter().enumerate() {
                    if index != 0 {
                        output.write_char(',')?;
                    }
                    write!(output, "{value}")?;
                }
                output.write_char(']')
            }
            Self::Object(fields) => {
                output.write_char('{')?;
                for (index, (key, value)) in fields.iter().enumerate() {
                    if index != 0 {
                        output.write_char(',')?;
                    }
                    write_string(output, key)?;
                    output.write_char(':')?;
                    write!(output, "{value}")?;
                }
                output.write_char('}')
            }
        }
    }
}

fn write_string(output: &mut fmt::Formatter<'_>, value: &str) -> fmt::Result {
    output.write_char('"')?;
    for character in value.chars() {
        match character {
            '"' => output.write_str("\\\"")?,
            '\\' => output.write_str("\\\\")?,
            '\u{08}' => output.write_str("\\b")?,
            '\u{0c}' => output.write_str("\\f")?,
            '\n' => output.write_str("\\n")?,
            '\r' => output.write_str("\\r")?,
            '\t' => output.write_str("\\t")?,
            character if character <= '\u{1f}' => {
                let code = u32::from(character);
                write!(output, "\\u{code:04x}")?
            }
            character => output.write_char(character)?,
        }
    }
    output.write_char('"')
}

impl Index<&str> for Value {
    type Output = Self;

    fn index(&self, key: &str) -> &Self::Output {
        static NULL: Value = Value::Null;
        match self {
            Self::Object(fields) => fields.get(key).unwrap_or(&NULL),
            _ => &NULL,
        }
    }
}

impl PartialEq<&str> for Value {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == Some(*other)
    }
}

pub(crate) trait IntoJson {
    fn into_json(self) -> Value;
}

impl IntoJson for Value {
    fn into_json(self) -> Value {
        self
    }
}

impl IntoJson for &Value {
    fn into_json(self) -> Value {
        self.clone()
    }
}

impl IntoJson for String {
    fn into_json(self) -> Value {
        Value::String(self)
    }
}

impl IntoJson for &String {
    fn into_json(self) -> Value {
        Value::String(self.clone())
    }
}

impl IntoJson for &str {
    fn into_json(self) -> Value {
        Value::String(self.to_owned())
    }
}

impl IntoJson for Cow<'_, str> {
    fn into_json(self) -> Value {
        Value::String(self.into_owned())
    }
}

impl IntoJson for bool {
    fn into_json(self) -> Value {
        Value::Bool(self)
    }
}

macro_rules! number_into_json {
    ($($type:ty),+ $(,)?) => {
        $(
            impl IntoJson for $type {
                fn into_json(self) -> Value {
                    Value::Number(self.to_string())
                }
            }
        )+
    };
}

number_into_json!(u8, u16, u32, u64, usize, i64);

impl<T: IntoJson> IntoJson for Option<T> {
    fn into_json(self) -> Value {
        self.map_or(Value::Null, IntoJson::into_json)
    }
}

impl<T: IntoJson> IntoJson for Vec<T> {
    fn into_json(self) -> Value {
        Value::Array(self.into_iter().map(IntoJson::into_json).collect())
    }
}

impl IntoJson for PathBuf {
    fn into_json(self) -> Value {
        Value::String(self.to_string_lossy().into_owned())
    }
}

impl IntoJson for &Path {
    fn into_json(self) -> Value {
        Value::String(self.to_string_lossy().into_owned())
    }
}

impl IntoJson for &[&Path] {
    fn into_json(self) -> Value {
        Value::Array(
            self.iter()
                .map(|path| Value::String(path.to_string_lossy().into_owned()))
                .collect(),
        )
    }
}

macro_rules! json {
    ({$($key:literal : $value:expr),* $(,)?}) => {{
        #[allow(unused_mut)]
        let mut fields = ::std::collections::BTreeMap::new();
        $(
            fields.insert(
                $key.to_owned(),
                $crate::json::IntoJson::into_json($value),
            );
        )*
        $crate::json::Value::Object(fields)
    }};
}

pub(crate) use json;

#[cfg(test)]
mod tests {

    #[test]
    fn serializer_is_key_sorted_and_escapes_json_strings() {
        let value = json!({"z": 1_u64, "a": "line\n\"quoted\""});
        assert_eq!(
            value.to_string(),
            "{\"a\":\"line\\n\\\"quoted\\\"\",\"z\":1}"
        );
    }
}
