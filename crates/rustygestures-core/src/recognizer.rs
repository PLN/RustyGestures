//! Orientation-sensitive Protractor matcher.
//!
//! The steps follow the published algorithms, not another implementation:
//! resample and centroid translation from $1 (Wobbrock, Wilson, and Li, UIST 2007),
//! aspect-preserving scale for a near-straight stroke from $N (Anthony and
//! Wobbrock, 2010), and the closed-form rotation from Protractor (Li, CHI 2010),
//! The score is the $1 path-distance score on a 0–1 scale (1 is a perfect
//! overlap). Protractor supplies only the closed-form angle, and that angle is
//! a gate: if the unclamped optimum is outside ±30°, the template is rejected
//! even when it is the only one. Thin shapes (shorter side under half the
//! longer side) are scored on the aspect-preserving cloud, because non-uniform
//! square scaling scores a clockwise rotation and a counter-clockwise one
//! differently. Pixel rounding can still push a nominal ±29.9° rotation a
//! fraction of a degree across the gate in only one direction.
//!
//! A template is accepted when its score is at least 0.80 and it leads the
//! next-best *other* gesture by at least 0.05.

use crate::Point;

/// Points in a resampled stroke, as in the $1 / Protractor papers.
pub const RESAMPLE_POINTS: usize = 64;

/// Square size used by $1 before the path distance is turned into a score.
const SQUARE_SIZE: f64 = 250.0;

/// Shorter side divided by the longer side. Below this, scaling keeps the
/// aspect ratio so a nearly straight stroke is not stretched into a square.
const ONE_D_RATIO: f64 = 0.30;

const ROTATION_LIMIT_RAD: f64 = 30.0 * std::f64::consts::PI / 180.0;
const ACCEPT_SCORE: f64 = 0.80;
const ACCEPT_MARGIN: f64 = 0.05;

#[derive(Clone, Copy, Debug)]
struct Vec2 {
    x: f64,
    y: f64,
}

#[derive(Clone, Debug)]
struct Sample {
    /// Square-scaled cloud used for the $1 path-distance score.
    points: [Vec2; RESAMPLE_POINTS],
    /// Aspect-preserving cloud. The ±30° gate is measured here, before the
    /// non-uniform square scale, so a rotated shape is not accepted just
    /// because that scale changed its best angle.
    oriented: [Vec2; RESAMPLE_POINTS],
}

#[derive(Clone, Debug)]
struct Gesture {
    name: String,
    samples: Vec<Sample>,
}

/// Preprocessed templates, grouped by gesture name.
#[derive(Clone, Debug, Default)]
pub struct GestureSet {
    gestures: Vec<Gesture>,
}

/// An accepted match.
#[derive(Clone, Debug, PartialEq)]
pub struct Recognition {
    pub name: String,
    pub score: f64,
    /// How far `score` sits above the best other gesture. Infinite when the
    /// set has only one gesture name.
    pub margin: f64,
}

impl GestureSet {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.gestures.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.gestures.len()
    }

    /// Add one raw sample. Returns `false` when the stroke is too short to resample.
    pub fn insert_raw(&mut self, name: &str, points: &[Point]) -> bool {
        let Some(prepared) = preprocess(points) else {
            return false;
        };
        let Some(oriented) = oriented_cloud(points) else {
            return false;
        };
        let sample = Sample {
            points: prepared,
            oriented,
        };
        if let Some(gesture) = self.gestures.iter_mut().find(|g| g.name == name) {
            gesture.samples.push(sample);
        } else {
            self.gestures.push(Gesture {
                name: name.to_owned(),
                samples: vec![sample],
            });
        }
        true
    }

    /// Best accepted gesture for `points`, if the score and the margin both pass.
    #[must_use]
    pub fn recognize(&self, points: &[Point]) -> Option<Recognition> {
        let candidate = preprocess(points)?;
        let oriented = oriented_cloud(points)?;
        let mut best_name: Option<&str> = None;
        let mut best = f64::NEG_INFINITY;
        let mut second = f64::NEG_INFINITY;
        for gesture in &self.gestures {
            let mut gesture_score = f64::NEG_INFINITY;
            for sample in &gesture.samples {
                let Some(score) =
                    score_pair(&sample.points, &candidate, &sample.oriented, &oriented)
                else {
                    continue;
                };
                if score > gesture_score {
                    gesture_score = score;
                }
            }
            if !gesture_score.is_finite() {
                continue;
            }
            if gesture_score > best {
                second = best;
                best = gesture_score;
                best_name = Some(&gesture.name);
            } else if gesture_score > second {
                second = gesture_score;
            }
        }
        let name = best_name?;
        if best < ACCEPT_SCORE {
            return None;
        }
        let margin = if second.is_finite() {
            best - second
        } else {
            f64::INFINITY
        };
        if second.is_finite() && margin < ACCEPT_MARGIN {
            return None;
        }
        Some(Recognition {
            name: name.to_owned(),
            score: best,
            margin,
        })
    }
}

/// Resample, centre, and scale.
fn preprocess(points: &[Point]) -> Option<[Vec2; RESAMPLE_POINTS]> {
    if points.len() < 2 {
        return None;
    }
    let raw: Vec<Vec2> = points
        .iter()
        .map(|p| Vec2 {
            x: f64::from(p.x),
            y: f64::from(p.y),
        })
        .collect();
    let mut resampled = resample(&raw, RESAMPLE_POINTS)?;
    translate_to_centroid(&mut resampled);
    scale_square(&mut resampled, SQUARE_SIZE);
    let mut out = [Vec2 { x: 0.0, y: 0.0 }; RESAMPLE_POINTS];
    if resampled.len() != RESAMPLE_POINTS {
        return None;
    }
    out.copy_from_slice(&resampled);
    Some(out)
}

fn resample(points: &[Vec2], n: usize) -> Option<Vec<Vec2>> {
    if points.len() < 2 || n < 2 {
        return None;
    }
    let total = path_length(points);
    if total < 1e-6 {
        return None;
    }
    let interval = total / (n - 1) as f64;
    let mut src = points.to_vec();
    let mut out = Vec::with_capacity(n);
    out.push(src[0]);
    let mut distance_acc = 0.0;
    let mut i = 1;
    while i < src.len() {
        let prev = src[i - 1];
        let curr = src[i];
        let segment = distance(prev, curr);
        if segment > 0.0 && distance_acc + segment >= interval {
            let t = (interval - distance_acc) / segment;
            let q = Vec2 {
                x: prev.x + t * (curr.x - prev.x),
                y: prev.y + t * (curr.y - prev.y),
            };
            out.push(q);
            // Insert q so the next step continues along the remainder of this segment.
            src.insert(i, q);
            distance_acc = 0.0;
        } else {
            distance_acc += segment;
        }
        i += 1;
        if out.len() == n {
            break;
        }
    }
    if out.len() == n - 1 {
        if let Some(last) = src.last().copied() {
            out.push(last);
        }
    }
    while out.len() < n {
        out.push(*out.last()?);
    }
    out.truncate(n);
    Some(out)
}

fn path_length(points: &[Vec2]) -> f64 {
    points
        .windows(2)
        .map(|pair| distance(pair[0], pair[1]))
        .sum()
}

fn distance(a: Vec2, b: Vec2) -> f64 {
    let dx = a.x - b.x;
    let dy = a.y - b.y;
    (dx * dx + dy * dy).sqrt()
}

fn translate_to_centroid(points: &mut [Vec2]) {
    let n = points.len() as f64;
    if n == 0.0 {
        return;
    }
    let mut cx = 0.0;
    let mut cy = 0.0;
    for point in points.iter() {
        cx += point.x;
        cy += point.y;
    }
    cx /= n;
    cy /= n;
    for point in points {
        point.x -= cx;
        point.y -= cy;
    }
}

fn scale_square(points: &mut [Vec2], size: f64) {
    let Some((min_x, min_y, max_x, max_y)) = bounds(points) else {
        return;
    };
    let width = (max_x - min_x).max(0.0);
    let height = (max_y - min_y).max(0.0);
    let long = width.max(height);
    if long < 1e-9 {
        return;
    }
    let short = width.min(height);
    // $N keeps the aspect ratio when the box is very flat, so noise on a
    // horizontal or vertical stroke is not stretched into a square. A straight
    // stroke at a shallow angle has a less extreme box, so a path whose ends
    // are almost as far apart as its length is treated as 1D too.
    //
    // Uniform scale uses path length, not the long side of the box. A diagonal
    // line's box is shorter than its path, and scaling by the box would make
    // that line longer than an axis-aligned one, so rotation could not line
    // them up.
    let flat = short / long < ONE_D_RATIO;
    let straight = is_near_straight(points);
    let (sx, sy) = if flat || straight {
        let path = path_length(points);
        let scale = if path < 1e-9 { 1.0 } else { size / path };
        (scale, scale)
    } else {
        let sx = if width < 1e-9 { 1.0 } else { size / width };
        let sy = if height < 1e-9 { 1.0 } else { size / height };
        (sx, sy)
    };
    for point in points {
        point.x *= sx;
        point.y *= sy;
    }
}

fn is_near_straight(points: &[Vec2]) -> bool {
    if points.len() < 2 {
        return true;
    }
    let path = path_length(points);
    if path < 1e-6 {
        return true;
    }
    let span = distance(points[0], points[points.len() - 1]);
    span / path >= 0.90
}

fn bounds(points: &[Vec2]) -> Option<(f64, f64, f64, f64)> {
    let first = *points.first()?;
    let mut min_x = first.x;
    let mut min_y = first.y;
    let mut max_x = first.x;
    let mut max_y = first.y;
    for point in points.iter().skip(1) {
        min_x = min_x.min(point.x);
        min_y = min_y.min(point.y);
        max_x = max_x.max(point.x);
        max_y = max_y.max(point.y);
    }
    Some((min_x, min_y, max_x, max_y))
}

/// Closed-form Protractor angle, then the $1 path-distance score.
///
/// `None` means the best rotation is outside ±30°, so the template does not
/// match this orientation.
fn oriented_cloud(points: &[Point]) -> Option<[Vec2; RESAMPLE_POINTS]> {
    if points.len() < 2 {
        return None;
    }
    let raw: Vec<Vec2> = points
        .iter()
        .map(|point| Vec2 {
            x: f64::from(point.x),
            y: f64::from(point.y),
        })
        .collect();
    let mut resampled = resample(&raw, RESAMPLE_POINTS)?;
    translate_to_centroid(&mut resampled);
    let path = path_length(&resampled);
    if path < 1e-6 {
        return None;
    }
    let scale = SQUARE_SIZE / path;
    for point in &mut resampled {
        point.x *= scale;
        point.y *= scale;
    }
    let mut out = [Vec2 { x: 0.0, y: 0.0 }; RESAMPLE_POINTS];
    out.copy_from_slice(&resampled);
    Some(out)
}

fn optimal_angle(template: &[Vec2; RESAMPLE_POINTS], candidate: &[Vec2; RESAMPLE_POINTS]) -> f64 {
    let mut a = 0.0;
    let mut b = 0.0;
    for i in 0..RESAMPLE_POINTS {
        let t = template[i];
        let c = candidate[i];
        a += t.x * c.x + t.y * c.y;
        // Dot product after rotating the candidate by θ is `a cos θ + b sin θ`.
        b += t.y * c.x - t.x * c.y;
    }
    b.atan2(a)
}

fn score_pair(
    template: &[Vec2; RESAMPLE_POINTS],
    candidate: &[Vec2; RESAMPLE_POINTS],
    template_oriented: &[Vec2; RESAMPLE_POINTS],
    candidate_oriented: &[Vec2; RESAMPLE_POINTS],
) -> Option<f64> {
    // Gate on the aspect-preserving clouds. Non-uniform square scaling can
    // shrink the apparent angle of a rotated shape below the limit.
    let theta = optimal_angle(template_oriented, candidate_oriented);
    if theta.abs() > ROTATION_LIMIT_RAD {
        return None;
    }
    // A tall L (about 200×60) is not flat enough for the 1D rule, but square
    // scaling still scores +20° and −20° differently. Score those on the
    // aspect-preserving cloud.
    let thin = is_thin(template_oriented) || is_thin(candidate_oriented);
    let template_cloud = if thin { template_oriented } else { template };
    let candidate_cloud = if thin { candidate_oriented } else { candidate };
    let (sin_t, cos_t) = theta.sin_cos();
    let mut rotated = [Vec2 { x: 0.0, y: 0.0 }; RESAMPLE_POINTS];
    for i in 0..RESAMPLE_POINTS {
        let c = candidate_cloud[i];
        rotated[i] = Vec2 {
            x: c.x * cos_t - c.y * sin_t,
            y: c.x * sin_t + c.y * cos_t,
        };
    }
    Some(path_score(template_cloud, &rotated))
}

/// Shorter side under half the longer side. The box is the aspect-preserving cloud.
fn is_thin(points: &[Vec2; RESAMPLE_POINTS]) -> bool {
    let Some((min_x, min_y, max_x, max_y)) = bounds(points) else {
        return false;
    };
    let width = (max_x - min_x).max(0.0);
    let height = (max_y - min_y).max(0.0);
    let long = width.max(height);
    if long < 1e-9 {
        return true;
    }
    width.min(height) / long < 0.5
}

fn path_score(template: &[Vec2; RESAMPLE_POINTS], candidate: &[Vec2; RESAMPLE_POINTS]) -> f64 {
    let mut sum = 0.0;
    for i in 0..RESAMPLE_POINTS {
        sum += distance(template[i], candidate[i]);
    }
    let average = sum / RESAMPLE_POINTS as f64;
    let half_diagonal = 0.5 * (SQUARE_SIZE * SQUARE_SIZE * 2.0).sqrt();
    if half_diagonal <= f64::EPSILON {
        return 0.0;
    }
    (1.0 - average / half_diagonal).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rotate_about_centroid(points: &[Point], degrees: f64) -> Vec<Point> {
        let (sin_t, cos_t) = degrees.to_radians().sin_cos();
        let n = points.len() as f64;
        let cx = points.iter().map(|point| f64::from(point.x)).sum::<f64>() / n;
        let cy = points.iter().map(|point| f64::from(point.y)).sum::<f64>() / n;
        points
            .iter()
            .map(|point| {
                let x = f64::from(point.x) - cx;
                let y = f64::from(point.y) - cy;
                Point {
                    x: (cx + x * cos_t - y * sin_t).round() as i32,
                    y: (cy + x * sin_t + y * cos_t).round() as i32,
                }
            })
            .collect()
    }

    fn line(x0: i32, y0: i32, x1: i32, y1: i32) -> Vec<Point> {
        let steps = 24;
        (0..=steps)
            .map(|i| {
                let t = f64::from(i) / f64::from(steps);
                Point {
                    x: (f64::from(x0) + f64::from(x1 - x0) * t).round() as i32,
                    y: (f64::from(y0) + f64::from(y1 - y0) * t).round() as i32,
                }
            })
            .collect()
    }

    fn ray(degrees: f64, len: f64) -> Vec<Point> {
        let angle = degrees.to_radians();
        line(
            0,
            0,
            (len * angle.cos()).round() as i32,
            (len * angle.sin()).round() as i32,
        )
    }

    fn l_down_right(scale: i32, ox: i32, oy: i32) -> Vec<Point> {
        let mut points = line(ox, oy, ox, oy + scale);
        points.extend(
            line(ox, oy + scale, ox + scale, oy + scale)
                .into_iter()
                .skip(1),
        );
        points
    }

    fn circle(cx: i32, cy: i32, radius: i32) -> Vec<Point> {
        let steps = 96;
        let mut points: Vec<Point> = (0..steps)
            .map(|i| {
                let angle = f64::from(i) / f64::from(steps) * std::f64::consts::TAU;
                Point {
                    x: cx + (f64::from(radius) * angle.cos()).round() as i32,
                    y: cy + (f64::from(radius) * angle.sin()).round() as i32,
                }
            })
            .collect();
        points.push(points[0]);
        points
    }

    fn jitter(points: &[Point], amp: i32) -> Vec<Point> {
        points
            .iter()
            .enumerate()
            .map(|(i, point)| {
                let span = (2 * amp + 1) as usize;
                let jx = ((i * 17 + 3) % span) as i32 - amp;
                let jy = ((i * 29 + 5) % span) as i32 - amp;
                Point {
                    x: point.x + jx,
                    y: point.y + jy,
                }
            })
            .collect()
    }

    fn shift_scale(points: &[Point], scale: f64, ox: i32, oy: i32) -> Vec<Point> {
        points
            .iter()
            .map(|point| Point {
                x: (f64::from(point.x) * scale).round() as i32 + ox,
                y: (f64::from(point.y) * scale).round() as i32 + oy,
            })
            .collect()
    }

    fn eight_directions() -> GestureSet {
        let mut set = GestureSet::new();
        for (name, degrees) in [
            ("e", 0.0),
            ("ne", 45.0),
            ("n", 90.0),
            ("nw", 135.0),
            ("w", 180.0),
            ("sw", 225.0),
            ("s", 270.0),
            ("se", 315.0),
        ] {
            assert!(set.insert_raw(name, &ray(degrees, 180.0)));
        }
        set
    }

    #[test]
    fn preprocess_resamples_and_centres() {
        let prepared = preprocess(&line(10, 10, 200, 10)).unwrap();
        assert_eq!(prepared.len(), RESAMPLE_POINTS);
        let mut cx = 0.0;
        let mut cy = 0.0;
        for point in &prepared {
            cx += point.x;
            cy += point.y;
        }
        cx /= RESAMPLE_POINTS as f64;
        cy /= RESAMPLE_POINTS as f64;
        assert!(cx.abs() < 1e-6, "cx {cx}");
        assert!(cy.abs() < 1e-6, "cy {cy}");
    }

    #[test]
    fn identical_stroke_scores_as_itself() {
        let mut set = GestureSet::new();
        let sample = line(0, 0, 150, 0);
        set.insert_raw("right", &sample);
        let found = set.recognize(&sample).unwrap();
        assert_eq!(found.name, "right");
        assert!(found.score > 0.98, "score {}", found.score);
    }

    #[test]
    fn lines_match_four_and_eight_directions() {
        let set = eight_directions();
        for (name, degrees) in [
            ("e", 0.0),
            ("ne", 45.0),
            ("n", 90.0),
            ("nw", 135.0),
            ("w", 180.0),
            ("sw", 225.0),
            ("s", 270.0),
            ("se", 315.0),
        ] {
            let drawn = shift_scale(&jitter(&ray(degrees, 90.0), 1), 2.5, 400, -30);
            let found = set
                .recognize(&drawn)
                .unwrap_or_else(|| panic!("{name} was rejected"));
            assert_eq!(
                found.name, name,
                "{name} -> {} ({})",
                found.name, found.score
            );
            assert!(found.score >= 0.80, "{name} score {}", found.score);
            assert!(found.margin >= 0.05, "{name} margin {}", found.margin);
        }
    }

    #[test]
    fn scale_offset_and_small_noise_still_match() {
        let mut set = GestureSet::new();
        set.insert_raw("right", &line(0, 0, 100, 0));
        let drawn = jitter(&shift_scale(&line(0, 0, 100, 0), 3.0, 500, 20), 2);
        let found = set.recognize(&drawn).unwrap();
        assert_eq!(found.name, "right");
        assert!(found.score >= 0.80, "{}", found.score);
    }

    #[test]
    fn near_straight_wobble_is_not_stretched() {
        let mut set = GestureSet::new();
        set.insert_raw("right", &line(0, 0, 200, 0));
        let mut wobbly = line(5, 5, 205, 5);
        for (i, point) in wobbly.iter_mut().enumerate() {
            point.y += if i % 2 == 0 { 1 } else { -1 };
        }
        let found = set.recognize(&wobbly).unwrap();
        assert_eq!(found.name, "right");
        assert!(found.score >= 0.90, "wobble score {}", found.score);
    }

    #[test]
    fn l_shapes_match_and_do_not_match_a_line() {
        let mut set = GestureSet::new();
        set.insert_raw("l-down-right", &l_down_right(100, 0, 0));
        set.insert_raw("right", &line(0, 0, 120, 0));
        let drawn = jitter(&l_down_right(60, 300, -40), 1);
        let found = set.recognize(&drawn).unwrap();
        assert_eq!(found.name, "l-down-right");
        assert!(found.margin >= 0.05, "margin {}", found.margin);
        let line_match = set.recognize(&line(10, 10, 400, 12)).unwrap();
        assert_eq!(line_match.name, "right");
    }

    #[test]
    fn circle_matches_scaled_shifted_circle_not_a_line() {
        let mut set = GestureSet::new();
        set.insert_raw("circle", &circle(0, 0, 40));
        set.insert_raw("right", &line(0, 0, 100, 0));
        let drawn = circle(250, 80, 90);
        let found = set.recognize(&drawn).unwrap();
        assert_eq!(
            found.name, "circle",
            "score {} margin {}",
            found.score, found.margin
        );
        assert!(found.score >= 0.80);
        assert!(found.margin >= 0.05, "margin {}", found.margin);
        assert_eq!(set.recognize(&line(0, 0, 30, 0)).unwrap().name, "right");
    }

    #[test]
    fn single_template_rejects_lines_past_thirty_degrees() {
        let mut set = GestureSet::new();
        set.insert_raw("right", &ray(0.0, 160.0));
        let near = set.recognize(&ray(20.0, 160.0)).unwrap();
        assert_eq!(near.name, "right");
        assert!(near.score >= 0.90, "20° score {}", near.score);
        assert!(set.recognize(&ray(45.0, 160.0)).is_none());
        assert!(set.recognize(&ray(60.0, 160.0)).is_none());
    }

    #[test]
    fn rotated_l_past_thirty_degrees_is_rejected() {
        let mut set = GestureSet::new();
        let shape = l_down_right(100, 0, 0);
        set.insert_raw("l", &shape);
        let turned = rotate_about_centroid(&shape, 20.0);
        let near = set.recognize(&turned).expect("20° L");
        assert_eq!(near.name, "l");
        assert!(near.score >= 0.80, "20° L score {}", near.score);
        let past = rotate_about_centroid(&shape, 31.0);
        assert!(
            set.recognize(&past).is_none(),
            "an L rotated 31° must be rejected"
        );
    }

    #[test]
    fn tall_l_scores_both_twenty_degree_rotations() {
        let mut set = GestureSet::new();
        let mut shape = line(0, 0, 0, 200);
        shape.extend(line(0, 200, 60, 200).into_iter().skip(1));
        set.insert_raw("l", &shape);
        let negative = set
            .recognize(&rotate_about_centroid(&shape, -20.0))
            .expect("−20° tall L");
        let positive = set
            .recognize(&rotate_about_centroid(&shape, 20.0))
            .expect("+20° tall L");
        assert!(negative.score >= 0.80, "−20° score {}", negative.score);
        assert!(positive.score >= 0.80, "+20° score {}", positive.score);
    }

    #[test]
    fn rotation_within_thirty_degrees_still_matches_a_line() {
        let mut set = GestureSet::new();
        set.insert_raw("right", &ray(0.0, 160.0));
        let found = set.recognize(&ray(20.0, 160.0)).unwrap();
        assert_eq!(found.name, "right");
        assert!(found.score >= 0.90, "score {}", found.score);
    }

    #[test]
    fn ambiguous_pair_is_rejected() {
        let mut set = GestureSet::new();
        let sample = line(0, 0, 140, 0);
        set.insert_raw("alpha", &sample);
        set.insert_raw("beta", &sample);
        assert!(set.recognize(&line(20, 20, 200, 22)).is_none());
    }

    #[test]
    fn halfway_between_directions_is_rejected() {
        let set = eight_directions();
        assert!(
            set.recognize(&ray(22.5, 180.0)).is_none(),
            "a stroke midway between two directions should be ambiguous"
        );
    }

    #[test]
    fn scribble_is_rejected() {
        let mut set = GestureSet::new();
        set.insert_raw("right", &line(0, 0, 100, 0));
        set.insert_raw("l-down-right", &l_down_right(80, 0, 0));
        set.insert_raw("circle", &circle(0, 0, 50));
        let mut scribble = Vec::new();
        for i in 0..40 {
            scribble.push(Point {
                x: (i * 13) % 80,
                y: (i * i * 3) % 70,
            });
        }
        assert!(set.recognize(&scribble).is_none());
    }

    #[test]
    fn empty_and_dot_are_rejected() {
        let mut set = GestureSet::new();
        set.insert_raw("right", &line(0, 0, 40, 0));
        assert!(set.recognize(&[]).is_none());
        assert!(set.recognize(&[Point { x: 1, y: 1 }]).is_none());
        assert!(!set.insert_raw("dot", &[Point { x: 1, y: 1 }, Point { x: 1, y: 1 }]));
    }
}
