use crate::value::Value;

/// SQL three-valued logic: `NULL` is neither `TRUE` nor `FALSE`, and any
/// comparison involving it yields `UNKNOWN`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TriBool {
    True,
    False,
    Unknown,
}

impl TriBool {
    pub(crate) fn from_value(value: &Value) -> Self {
        if value.is_null() {
            Self::Unknown
        } else if value.truthy() {
            Self::True
        } else {
            Self::False
        }
    }

    pub(crate) fn into_value(self) -> Value {
        match self {
            Self::True => Value::Bool(true),
            Self::False => Value::Bool(false),
            Self::Unknown => Value::Null,
        }
    }

    pub(crate) fn not(self) -> Self {
        match self {
            Self::True => Self::False,
            Self::False => Self::True,
            Self::Unknown => Self::Unknown,
        }
    }

    pub(crate) fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::False, _) | (_, Self::False) => Self::False,
            (Self::True, Self::True) => Self::True,
            _ => Self::Unknown,
        }
    }

    pub(crate) fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::True, _) | (_, Self::True) => Self::True,
            (Self::False, Self::False) => Self::False,
            _ => Self::Unknown,
        }
    }
}