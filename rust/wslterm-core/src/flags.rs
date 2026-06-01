//! Minimal flags macro. Avoids pulling the `bitflags` crate while the core crate
//! is intentionally dependency-free (until the GUI lands). `#[macro_use]` in
//! lib.rs makes it available to sibling modules regardless of file order.

#[macro_export]
macro_rules! bitflags_lite {
    (
        $(#[$meta:meta])*
        pub struct $name:ident: $ty:ty { $(const $flag:ident = $val:expr;)* }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
        pub struct $name(pub $ty);
        impl $name {
            $(pub const $flag: $name = $name($val);)*
            #[inline] pub fn contains(self, other: $name) -> bool { (self.0 & other.0) == other.0 }
            #[inline] pub fn insert(&mut self, other: $name) { self.0 |= other.0; }
            #[inline] pub fn remove(&mut self, other: $name) { self.0 &= !other.0; }
            #[inline] pub fn is_empty(self) -> bool { self.0 == 0 }
        }
        impl core::ops::BitOr for $name {
            type Output = $name;
            #[inline] fn bitor(self, rhs: $name) -> $name { $name(self.0 | rhs.0) }
        }
    };
}
