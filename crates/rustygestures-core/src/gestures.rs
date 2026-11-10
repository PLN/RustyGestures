//! JSON gesture templates and recorded strokes.
//!
//! The M0 file is a list of named samples. The recognizer preprocesses them
//! when a [`crate::GestureSet`] is built.

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{GestureSet, Point};

pub const GESTURE_SCHEMA: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawPoint {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawStroke {
    pub points: Vec<RawPoint>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedGesture {
    pub name: String,
    pub samples: Vec<RawStroke>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GestureFile {
    pub schema: u32,
    pub gestures: Vec<NamedGesture>,
}

impl Default for GestureFile {
    fn default() -> Self {
        Self {
            schema: GESTURE_SCHEMA,
            gestures: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrokeLog {
    pub schema: u32,
    pub strokes: Vec<RawStroke>,
}

#[derive(Debug)]
pub enum GestureIoError {
    Io(io::Error),
    Json(serde_json::Error),
    Schema(u32),
    Name(String),
}

impl std::fmt::Display for GestureIoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "{err}"),
            Self::Json(err) => write!(f, "{err}"),
            Self::Schema(schema) => write!(f, "unsupported gesture schema {schema}"),
            Self::Name(reason) => write!(f, "{reason}"),
        }
    }
}

impl std::error::Error for GestureIoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Json(err) => Some(err),
            Self::Schema(_) | Self::Name(_) => None,
        }
    }
}

impl From<io::Error> for GestureIoError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for GestureIoError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

/// A gesture name that can be stored in the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GestureName(String);

impl GestureName {
    pub fn parse(name: &str) -> Result<Self, GestureIoError> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(GestureIoError::Name(
                "gesture name must not be empty".to_owned(),
            ));
        }
        if trimmed.chars().any(|ch| ch.is_control()) {
            return Err(GestureIoError::Name(
                "gesture name must not contain control characters".to_owned(),
            ));
        }
        Ok(Self(trimmed.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Load `path`. A missing file is an empty set, not an error.
pub fn load_gesture_file(path: &Path) -> Result<GestureFile, GestureIoError> {
    if !path.exists() {
        return Ok(GestureFile::default());
    }
    let text = fs::read_to_string(path)?;
    let file: GestureFile = serde_json::from_str(&text)?;
    if file.schema != GESTURE_SCHEMA {
        return Err(GestureIoError::Schema(file.schema));
    }
    Ok(file)
}

pub fn save_gesture_file(path: &Path, file: &GestureFile) -> Result<(), GestureIoError> {
    let bytes = serde_json::to_vec_pretty(file)?;
    write_atomic(path, &bytes)
}

pub fn save_stroke_log(path: &Path, log: &StrokeLog) -> Result<(), GestureIoError> {
    let bytes = serde_json::to_vec_pretty(log)?;
    write_atomic(path, &bytes)
}

impl GestureFile {
    #[must_use]
    pub fn to_set(&self) -> GestureSet {
        let mut set = GestureSet::new();
        for gesture in &self.gestures {
            for sample in &gesture.samples {
                let points: Vec<Point> = sample
                    .points
                    .iter()
                    .map(|point| Point {
                        x: point.x,
                        y: point.y,
                    })
                    .collect();
                set.insert_raw(&gesture.name, &points);
            }
        }
        set
    }

    pub fn add_sample(&mut self, name: &str, points: &[Point]) {
        let raw = RawStroke {
            points: points
                .iter()
                .map(|point| RawPoint {
                    x: point.x,
                    y: point.y,
                })
                .collect(),
        };
        if let Some(gesture) = self
            .gestures
            .iter_mut()
            .find(|gesture| gesture.name == name)
        {
            gesture.samples.push(raw);
        } else {
            self.gestures.push(NamedGesture {
                name: name.to_owned(),
                samples: vec![raw],
            });
        }
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), GestureIoError> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, bytes)?;
    replace_file(&tmp, path)?;
    Ok(())
}

fn replace_file(from: &Path, to: &Path) -> Result<(), GestureIoError> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide = |path: &Path| {
            path.as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect::<Vec<u16>>()
        };
        let from_w = wide(from);
        let to_w = wide(to);
        extern "system" {
            fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
        }
        // REPLACE_EXISTING swaps the name in one call. WRITE_THROUGH asks the
        // filesystem to flush the directory entry before MoveFileExW returns.
        const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
        const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
        let ok = unsafe {
            MoveFileExW(
                from_w.as_ptr(),
                to_w.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        fs::rename(from, to)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_recognize() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "rustygestures-gestures-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut file = GestureFile::default();
        let points = [
            Point { x: 0, y: 0 },
            Point { x: 40, y: 0 },
            Point { x: 80, y: 0 },
        ];
        file.add_sample("right", &points);
        save_gesture_file(&path, &file).unwrap();
        let loaded = load_gesture_file(&path).unwrap();
        assert_eq!(loaded.gestures.len(), 1);
        assert_eq!(loaded.gestures[0].samples.len(), 1);
        let set = loaded.to_set();
        let found = set.recognize(&points).unwrap();
        assert_eq!(found.name, "right");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn missing_file_is_empty() {
        let path = std::env::temp_dir().join("rustygestures-missing-gestures.json");
        let _ = fs::remove_file(&path);
        let file = load_gesture_file(&path).unwrap();
        assert!(file.gestures.is_empty());
    }

    #[test]
    fn name_rejects_empty_and_controls() {
        assert!(GestureName::parse("  ").is_err());
        assert!(GestureName::parse("a\nb").is_err());
        assert_eq!(GestureName::parse("  circle ").unwrap().as_str(), "circle");
    }
}
