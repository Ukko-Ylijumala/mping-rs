// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

/**
Delegate read-only method calls to something enclosed in a [parking_lot::RwLock].
The structure containing the lock must implement a method `with` that provides
read access to the inner value.

Think of it as a poor man's [std::ops::Deref] implementation for RwLock-wrapped types.
## Example
```ignore
use parking_lot::RwLock;
use ratatui::text::Line;

pub struct Wrapper<'a>(RwLock<Line<'a>>);

impl<'a> Wrapper<'a> {
    fn with<R>(&self, f: impl FnOnce(&Line<'a>) -> R) -> R {
        f(&*self.0.read())
    }

   // Define the methods to delegate (ie. pass through to the inner Line)
   delegate_read!(width -> usize);
}
```
*/
macro_rules! delegate_read {
    ($name:ident -> $ret:ty) => {
        pub fn $name(&self) -> $ret {
            self.with(|inner| inner.$name())
        }
    };
}

/**
Delegate mutating method calls to something enclosed in a [parking_lot::RwLock].
The structure containing the lock must implement a method `with_mut` that provides
write access to the inner value.

Think of it as a poor man's [std::ops::DerefMut] implementation for RwLock-wrapped types.
## Example
```ignore
use parking_lot::RwLock;
use ratatui::text::Line;

pub struct Wrapper<'a>(RwLock<Line<'a>>);

impl<'a> Wrapper<'a> {
    fn with_mut<R>(&self, f: impl FnOnce(&mut Line<'a>) -> R) -> R {
        f(&mut *self.0.write())
    }

   // Define the methods to delegate (ie. pass through to the inner Line)
   delegate_write!(bold);
   delegate_write!(style, style: Style);
   delegate_write!(push_span, <T: Into<Span<'a>>>, span: T);
}
```
*/
macro_rules! delegate_write {
    // No arguments, chainable (returns Self)
    ($name:ident) => {
        pub fn $name(self) -> Self {
            self.with_mut(|inner| {
                *inner = std::mem::take(inner).$name();
            });
            self
        }
    };

    // With arguments, chainable
    ($name:ident, $($arg:ident : $ty:ty),+) => {
        pub fn $name(self, $($arg: $ty),+) -> Self {
            self.with_mut(|inner| {
                *inner = std::mem::take(inner).$name($($arg),+);
            });
            self
        }
    };

    // With arguments, in-place modification (not consuming), not chainable
    ($name:ident, $($arg:ident : $ty:ty),+) => {
        pub fn $name(&self, $($arg: $ty),+) {
            self.with_mut(|inner| {
                inner.$name($($arg),+);
            });
            //self
        }
    };

    // With generic argument (like Into<Span>), in-place, not chainable
    ($name:ident, <$($gen:ident : $bound:path),+>, $($arg:ident : $ty:ty),+) => {
        pub fn $name<$($gen: $bound),+>(&self, $($arg: $ty),+) {
            self.with_mut(|inner| {
                inner.$name($($arg),+);
            });
            //self
        }
    };
}

/**
Stderr printing that avoids corrupting the TUI while in alt screen.

Note: this only gates *your* prints; it cannot stop other crates or
panics, so those must be handled separately.
*/
macro_rules! eprintln_nomangle {
    ($($tt:tt)*) => {
        $crate::ui::eprintln_safe(format_args!($($tt)*))
    };
}

pub(crate) use delegate_read;
pub(crate) use delegate_write;
pub(crate) use eprintln_nomangle;
