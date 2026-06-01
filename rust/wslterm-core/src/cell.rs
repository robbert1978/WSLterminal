//! One screen cell. Ports `src/WslTerminal/Vt/Cell.cs`.

bitflags_lite! {
    /// Per-cell rendition flags (SGR attributes).
    pub struct CellFlags: u16 {
        const BOLD      = 1 << 0;
        const FAINT     = 1 << 1;
        const ITALIC    = 1 << 2;
        const UNDERLINE = 1 << 3;
        const BLINK     = 1 << 4;
        const REVERSE   = 1 << 5;
        const HIDDEN    = 1 << 6;
        const STRIKE    = 1 << 7;
    }
}

/// A single grid cell. `rune == 0` renders blank. `width`: 1 normal, 2 lead of a
/// wide glyph, 0 the trailing continuation slot. `combo` holds combining marks
/// appended to the base rune (e.g. a kaomoji overlay), `None` for the common case.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Cell {
    pub rune: u32,
    pub fg: i32,
    pub bg: i32,
    pub flags: CellFlags,
    pub width: u8,
    pub combo: Option<String>,
}

impl Cell {
    pub fn blank(fg: i32, bg: i32, flags: CellFlags) -> Self {
        Cell { rune: 0, fg, bg, flags, width: 1, combo: None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_insert_remove_contains() {
        let mut f = CellFlags::default();
        assert!(f.is_empty());
        f.insert(CellFlags::UNDERLINE);
        f.insert(CellFlags::BOLD);
        assert!(f.contains(CellFlags::UNDERLINE));
        assert!(f.contains(CellFlags::BOLD));
        f.remove(CellFlags::UNDERLINE);
        assert!(!f.contains(CellFlags::UNDERLINE));
        assert!(f.contains(CellFlags::BOLD));
    }

    #[test]
    fn blank_cell_defaults() {
        let c = Cell::blank(-1, -1, CellFlags::default());
        assert_eq!(c.rune, 0);
        assert_eq!(c.width, 1);
        assert!(c.combo.is_none());
    }
}
