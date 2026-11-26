//! Points and rectangles in world space (y up), shared by every module.
//! `Rect` is the JS wall's `{ minX, minY, maxX, maxY }`.

/// A point in world units.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const fn new(x: f64, y: f64) -> Point {
        Point { x, y }
    }
}

/// An axis-aligned rectangle by its corners, y up: `min_y` is its bottom.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Rect {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl Rect {
    pub const fn new(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Rect {
        Rect { min_x, min_y, max_x, max_y }
    }

    /// From a top-left corner and a size (y up: the rect hangs below `top`).
    pub fn from_top_left(left: f64, top: f64, w: f64, h: f64) -> Rect {
        Rect { min_x: left, min_y: top - h, max_x: left + w, max_y: top }
    }

    pub fn width(&self) -> f64 {
        self.max_x - self.min_x
    }

    pub fn height(&self) -> f64 {
        self.max_y - self.min_y
    }

    pub fn centre(&self) -> Point {
        Point { x: (self.min_x + self.max_x) * 0.5, y: (self.min_y + self.max_y) * 0.5 }
    }

    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.min_x && p.x <= self.max_x && p.y >= self.min_y && p.y <= self.max_y
    }

    /// Whether two rects share area (touching edges do not count), as the
    /// JS `overlaps`.
    pub fn overlaps(&self, o: &Rect) -> bool {
        self.min_x < o.max_x && self.max_x > o.min_x && self.min_y < o.max_y && self.max_y > o.min_y
    }
}
