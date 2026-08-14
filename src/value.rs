use std::cmp::Ordering;
use std::fmt;
use std::hash::Hash;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Text(String),
    Null,
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(value) => Some(*value),
            Value::Float(value) => Some(*value as i64),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(value) => Some(*value as f64),
            Value::Float(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Value::Text(value) => Some(value.as_str()),
            _ => None,
        }
    }

    pub fn to_display_string(&self) -> String {
        match self {
            Value::Int(value) => value.to_string(),
            Value::Float(value) => format_float(*value),
            Value::Bool(value) => {
                if *value {
                    "true".to_string()
                } else {
                    "false".to_string()
                }
            }
            Value::Text(value) => value.clone(),
            Value::Null => "NULL".to_string(),
        }
    }

    pub fn truthy(&self) -> bool {
        match self {
            Value::Bool(value) => *value,
            Value::Int(value) => *value != 0,
            Value::Float(value) => *value != 0.0,
            Value::Text(value) => !value.is_empty(),
            Value::Null => false,
        }
    }

    pub fn type_rank(&self) -> u8 {
        match self {
            Value::Null => 0,
            Value::Bool(_) => 1,
            Value::Int(_) => 2,
            Value::Float(_) => 3,
            Value::Text(_) => 4,
        }
    }
}

pub fn format_float(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{:.0}", value)
    } else {
        format!("{}", value)
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_display_string())
    }
}

impl Eq for Value {}

impl Hash for Value {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self {
            Value::Int(value) => {
                2u8.hash(state);
                value.hash(state);
            }
            Value::Float(value) => {
                3u8.hash(state);
                value.to_bits().hash(state);
            }
            Value::Bool(value) => {
                1u8.hash(state);
                value.hash(state);
            }
            Value::Text(value) => {
                4u8.hash(state);
                value.hash(state);
            }
            Value::Null => 0u8.hash(state),
        }
    }
}

pub fn values_partial_cmp(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    if a.is_null() || b.is_null() {
        return Some(match (a.is_null(), b.is_null()) {
            (true, true) => std::cmp::Ordering::Equal,
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => unreachable!(),
        });
    }

    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Some(x.cmp(y)),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y),
        (Value::Int(x), Value::Float(y)) => cmp_int_float(*x, *y),
        (Value::Float(x), Value::Int(y)) => cmp_int_float(*y, *x).map(Ordering::reverse),
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        (Value::Text(x), Value::Text(y)) => Some(x.cmp(y)),
        _ => Some(a.type_rank().cmp(&b.type_rank())),
    }
}

pub fn values_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x == y,
        (Value::Int(x), Value::Float(y)) => eq_int_float(*x, *y),
        (Value::Float(x), Value::Int(y)) => eq_int_float(*y, *x),
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Text(x), Value::Text(y)) => x == y,
        (Value::Null, Value::Null) => true,
        _ => false,
    }
}

fn cmp_int_float(int: i64, float: f64) -> Option<Ordering> {
    if float.is_nan() {
        return None;
    }
    if float.fract() != 0.0 {
        return (int as f64).partial_cmp(&float);
    }
    let limit = 2f64.powi(63);
    if float >= limit {
        return Some(Ordering::Less);
    }
    if float <= -limit {
        return Some(Ordering::Greater);
    }
    Some(int.cmp(&(float as i64)))
}

fn eq_int_float(int: i64, float: f64) -> bool {
    if float.fract() != 0.0 {
        return false;
    }
    let limit = 2f64.powi(63);
    if float >= limit || float <= -limit {
        return false;
    }
    int == float as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    #[test]
    fn display_string() {
        assert_eq!(Value::Int(5).to_display_string(), "5");
        assert_eq!(Value::Float(3.0).to_display_string(), "3");
        assert_eq!(Value::Float(3.5).to_display_string(), "3.5");
        assert_eq!(Value::Bool(true).to_display_string(), "true");
        assert_eq!(Value::Bool(false).to_display_string(), "false");
        assert_eq!(Value::Text("hi".to_string()).to_display_string(), "hi");
        assert_eq!(Value::Null.to_display_string(), "NULL");
    }

    #[test]
    fn float_formatting() {
        assert_eq!(format_float(4.0), "4");
        assert_eq!(format_float(4.2), "4.2");
        assert_eq!(format_float(0.0), "0");
    }

    #[test]
    fn null_ordering() {
        assert_eq!(
            values_partial_cmp(&Value::Null, &Value::Int(1)),
            Some(Ordering::Less)
        );
        assert_eq!(
            values_partial_cmp(&Value::Null, &Value::Null),
            Some(Ordering::Equal)
        );
        assert_eq!(
            values_partial_cmp(&Value::Int(1), &Value::Null),
            Some(Ordering::Greater)
        );
    }

    #[test]
    fn cross_numeric_comparison() {
        assert_eq!(
            values_partial_cmp(&Value::Int(2), &Value::Float(2.5)),
            Some(Ordering::Less)
        );
        assert!(values_eq(&Value::Int(3), &Value::Float(3.0)));
        assert!(!values_eq(&Value::Int(3), &Value::Float(3.1)));
    }

    #[test]
    fn large_int_float_comparison_is_exact() {
        let big = 9_007_199_254_740_993i64; // 2^53 + 1, not representable as f64
        let two_pow_53 = 9_007_199_254_740_992.0;
        assert_eq!(
            values_partial_cmp(&Value::Int(big), &Value::Float(two_pow_53)),
            Some(Ordering::Greater)
        );
        assert!(!values_eq(&Value::Int(big), &Value::Float(two_pow_53)));
        assert!(values_eq(&Value::Int(3), &Value::Float(3.0)));
    }

    #[test]
    fn text_and_bool_comparison() {
        assert_eq!(
            values_partial_cmp(&Value::Text("a".to_string()), &Value::Text("b".to_string())),
            Some(Ordering::Less)
        );
        assert!(values_eq(&Value::Bool(true), &Value::Bool(true)));
        assert!(!values_eq(&Value::Bool(true), &Value::Bool(false)));
    }

    #[test]
    fn truthiness() {
        assert!(Value::Int(1).truthy());
        assert!(!Value::Int(0).truthy());
        assert!(Value::Float(0.5).truthy());
        assert!(!Value::Float(0.0).truthy());
        assert!(Value::Bool(true).truthy());
        assert!(!Value::Bool(false).truthy());
        assert!(Value::Text("x".to_string()).truthy());
        assert!(!Value::Text(String::new()).truthy());
        assert!(!Value::Null.truthy());
    }
}