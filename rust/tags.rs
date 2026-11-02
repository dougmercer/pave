use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;

pub const HIGHWAYS: [&str; 15] = [
    "motorway",
    "trunk",
    "primary",
    "secondary",
    "tertiary",
    "unclassified",
    "residential",
    "service",
    "track",
    "road",
    "motorway_link",
    "trunk_link",
    "primary_link",
    "secondary_link",
    "tertiary_link",
];
pub const STRINGS: [&str; 5] = ["access", "motor_vehicle", "hgv", "surface", "name"];
pub type Tags = HashMap<String, String>;

pub fn highway_code(value: &str) -> Option<u8> {
    HIGHWAYS
        .iter()
        .position(|&name| name == value)
        .map(|i| i as u8)
}

pub fn get<'a>(tags: &'a Tags, key: &str) -> Option<&'a str> {
    tags.get(key).map(String::as_str)
}

static SCALAR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([+]?(?:\d+(?:\.\d*)?|\.\d+)(?:[eE][+-]?\d+)?)\s*([a-z/]*)$").unwrap()
});
static FEET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^([+]?(?:\d+(?:\.\d*)?|\.\d+))\s*'\s*(?:([+]?(?:\d+(?:\.\d*)?|\.\d+))\s*")?$"#)
        .unwrap()
});

/// Parse explicit nonnegative quantities; symbolic, compound and conditional values stay null.
pub fn scalar(value: Option<&str>, kind: &str) -> f64 {
    let Some(value) = value else { return f64::NAN };
    let value = value.trim().to_lowercase();
    if kind == "length"
        && let Some(parts) = FEET.captures(&value)
    {
        let feet = parts[1].parse::<f64>().unwrap_or(f64::NAN);
        let inches = parts
            .get(2)
            .map_or(0.0, |v| v.as_str().parse::<f64>().unwrap_or(f64::NAN));
        let result = feet * 0.3048 + inches * 0.0254;
        return if result.is_finite() { result } else { f64::NAN };
    }
    let Some(parts) = SCALAR.captures(&value) else {
        return f64::NAN;
    };
    let factor = match (kind, &parts[2]) {
        (_, "") => 1.0,
        ("length", "m" | "meter" | "meters" | "metre" | "metres") => 1.0,
        ("length", "cm") => 0.01,
        ("length", "ft" | "feet") => 0.3048,
        ("length", "in") => 0.0254,
        ("speed", "km/h" | "kmh" | "kph") => 1.0,
        ("speed", "mph") => 1.609344,
        ("weight", "t" | "tonne" | "tonnes") => 1.0,
        ("weight", "kg") => 0.001,
        ("weight", "lb" | "lbs") => 0.00045359237,
        ("weight", "st") => 0.90718474,
        ("weight", "lt") => 1.0160469088,
        _ => return f64::NAN,
    };
    let result = parts[1].parse::<f64>().unwrap_or(f64::NAN) * factor;
    if result.is_finite() { result } else { f64::NAN }
}

pub fn limit(value: Option<&str>, kind: &str) -> f32 {
    let value = scalar(value, kind) as f32;
    if value.is_finite() { value } else { f32::NAN }
}

fn default_width(highway: &str) -> f64 {
    match highway {
        "motorway" => 11.0,
        "trunk" => 9.0,
        "primary" => 7.5,
        "secondary" => 7.0,
        "tertiary" => 6.5,
        "unclassified" | "residential" | "road" => 5.5,
        "service" => 4.0,
        "track" => 3.5,
        _ => 4.0,
    }
}

pub fn width(tags: &Tags) -> f64 {
    let width = scalar(get(tags, "width"), "length");
    if width > 0.0 {
        return width;
    }
    let lanes = scalar(get(tags, "lanes"), "lanes") * 3.5;
    if lanes > 0.0 && lanes.is_finite() {
        return lanes;
    }
    let highway = get(tags, "highway").unwrap_or("");
    if highway == "service" {
        match get(tags, "service") {
            Some("driveway") => return 3.0,
            Some("parking_aisle") => return 5.5,
            Some("alley") => return 3.5,
            _ => (),
        }
    }
    match highway.strip_suffix("_link") {
        Some(base) => default_width(base) * 0.6,
        None => default_width(highway),
    }
}

pub fn direction(tags: &Tags) -> i8 {
    match get(tags, "oneway")
        .unwrap_or("")
        .trim()
        .to_lowercase()
        .as_str()
    {
        "yes" | "true" | "1" => return 1,
        "-1" => return -1,
        // An explicit tag beats the implied rules below; these roads carry both directions.
        "no" | "false" | "0" | "reversible" | "alternating" => return 0,
        _ => (),
    }
    if matches!(get(tags, "junction"), Some("roundabout" | "circular"))
        || matches!(get(tags, "highway"), Some("motorway" | "motorway_link"))
    {
        1
    } else {
        0
    }
}

pub fn flags(tags: &Tags) -> u8 {
    let mut result = 0;
    for (name, bit, present) in [("bridge", 1, 4), ("tunnel", 2, 8)] {
        if let Some(value) = get(tags, name) {
            result |= present;
            if value != "no" {
                result |= bit;
            }
        }
    }
    result
}
