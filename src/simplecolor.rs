// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The following functionality is implemented in this file:
//! - ANSI color codes
//! - string coloring functions
//!
//! This is a from-scratch implementation without external crates like
//! `termcolor` or `ansi_term`. The design is inspired by author's
//! Python metaclass-based approach, but adapted to Rust's capabilities.
//!
//! Note: Rust does not have direct equivalents for Python's metaclasses,
//! dataclass, or in-place f-string class usage. Instead, we use constants
//! for ANSI codes, enums for colors and controls, and funcs for wrapping strings.
//! Nesting is handled by builder-like functions or manual code combination.

#![allow(dead_code)]

use regex::Regex;
use std::fmt;

// ANSI magic string sequences as constants
const ANSI_BEG: &str = "\x1b[";
const ANSI_SEP: char = ';';
const ANSI_END: char = 'm';

/// Full ANSI clear string for convenience
pub const CLR: &str = "\x1b[0m";

/// Common ANSI control codes
#[derive(Clone, Copy, Debug)]
pub enum AnsiCC {
    Clear = 0,
    Bold = 1,
    Faint = 2,
    Italic = 3,
    Underline = 4,
    Blink = 5,
    BlinkFast = 6,
    Inverse = 7,
    Conceal = 8,
    Crossout = 9,
    NoBold = 21,
    Normal = 22,
    NoItalic = 23,
    NoUnderline = 24,
    NoBlink = 25,
    NoInverse = 27,
    NoConceal = 28,
    NoCrossout = 29,
}

/// ANSI color codes (foreground; background is +10)
#[derive(Clone, Copy, Debug)]
pub enum AnsiColor {
    Black = 30,
    Red = 31,
    Green = 32,
    Yellow = 33,
    Blue = 34,
    Magenta = 35,
    Cyan = 36,
    White = 37,
    RedBright = 91,
    GreenBright = 92,
    YellowBright = 93,
    BlueBright = 94,
    MagentaBright = 95,
    CyanBright = 96,
    WhiteBright = 97,
}

impl AnsiColor {
    pub fn bg(self) -> u8 {
        self as u8 + 10
    }
}

////////////////////////////////////////////////////////////////////////////////

#[derive(Debug, Clone, Copy)]
enum Color {
    Basic(AnsiColor),
    EightBit(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone)]
enum TextElement<'a> {
    Plain(String),
    Loan(&'a str),
    Styled(StyledElement),
    StyleClear,
}

#[derive(Debug, Clone)]
pub struct StyledElement {
    s: String,
    fg: Option<Color>,
    bg: Option<Color>,
    styles: u32,    // Bitflags for bold (1<<0), underline (1<<1), etc.
}

#[derive(Debug, Clone)]
pub struct StyledTableRow<'a> {
    items: Vec<TextElement<'a>>,
    colsep: Option<String>, // Separator between columns, for example " | "
}

#[derive(Debug, Clone)]
pub struct StyledTable<'a> {
    hdr: Option<StyledTableRow<'a>>,
    rows: Vec<StyledTableRow<'a>>,
    widths: Vec<usize>, // Column widths for padding
}

/* ---------------------------------------- */

#[derive(Debug, Clone, Copy)]
enum Canvas {
    Console,
    Curses,
    Textarea,
    Plain,
    //Html,         // these 2 are probably more trouble than they're
    //Markdown,     // worth, since we intend to be ANSI-focused
    Unformatted,
    FileLike,
}

#[derive(Debug, Clone)]
pub struct StyledString<'a> {
    elems: Vec<TextElement<'a>>,
    sep: Option<String>,    // Separator between elements, for example " " (space)
}

#[derive(Debug, Clone)]
pub struct StyledCanvas<'a> {
    kind: Canvas,
    lines: Vec<StyledString<'a>>,
}

////////////////////////////////////////////////////////////////////////////////

/// Struct for building ANSI-formatted strings (to support nesting/combining)
#[derive(Clone, Debug)]
pub struct AnsiString {
    text: String,
    codes: Vec<u8>,
}

impl AnsiString {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            codes: Vec::new(),
        }
    }

    pub fn with_code(mut self, code: AnsiCC) -> Self {
        self.codes.push(code as u8);
        self
    }

    pub fn with_fg(mut self, color: AnsiColor) -> Self {
        self.codes.push(color as u8);
        self
    }

    pub fn with_bg(mut self, color: AnsiColor) -> Self {
        self.codes.push(color.bg());
        self
    }

    pub fn build(&self) -> String {
        if self.codes.is_empty() {
            return self.text.clone();
        }
        let codes_str = self
            .codes
            .iter()
            .map(|&c| c.to_string())
            .collect::<Vec<_>>()
            .join(&ANSI_SEP.to_string());
        format!("{ANSI_BEG}{codes_str}{ANSI_END}{}{CLR}", self.text)
    }
}

impl fmt::Display for AnsiString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.build())
    }
}

//////// Convenience functions for common styles (similar to Python classes) ////////

macro_rules! style_fns {
    ($( $name:ident => $code:ident ),* $(,)?) => {
        $(
            #[inline]
            pub fn $name(text: impl Into<String>) -> AnsiString {
                AnsiString::new(text).with_code(AnsiCC::$code)
            }
        )*
    };
}

style_fns!(
    bold => Bold,
    underline => Underline,
    blink => Blink,
    inverse => Inverse,
    crossout => Crossout,
);

//////// Convenience functions for foreground colors ////////

macro_rules! foreground_fns {
    ($( $name:ident => $code:ident ),* $(,)?) => {
        $(
            #[inline]
            pub fn $name(text: impl Into<String>) -> AnsiString {
                AnsiString::new(text).with_fg(AnsiColor::$code)
            }
        )*
    };
}

foreground_fns!(
    blk => Black,
    red => Red,
    grn => Green,
    yel => Yellow,
    blu => Blue,
    mag => Magenta,
    cya => Cyan,
    whi => White,
    rdb => RedBright,
    grb => GreenBright,
    org => YellowBright,
    lbl => BlueBright,
    mgb => MagentaBright,
    cyb => CyanBright,
    whb => WhiteBright,
);

//////// Convenience functions for background colors ////////

macro_rules! background_fns {
    ($( $name:ident => $code:ident ),* $(,)?) => {
        $(
            #[inline]
            pub fn $name(text: impl Into<String>) -> AnsiString {
                AnsiString::new(text).with_bg(AnsiColor::$code)
            }
        )*
    };
}

background_fns!(
    b_blk => Black,
    b_red => Red,
    b_grn => Green,
    b_yel => Yellow,
    b_blu => Blue,
    b_mag => Magenta,
    b_cya => Cyan,
    b_whi => White,
    b_rdb => RedBright,
    b_grb => GreenBright,
    b_org => YellowBright,
    b_lbl => BlueBright,
    b_mgb => MagentaBright,
    b_cyb => CyanBright,
    b_whb => WhiteBright,
);

////////////////////////////////////////////////////////////////////////////////

/// Apply an ANSI formatting function to all items in an iterator
/// and return a Vec of formatted strings.
pub fn apply_ansi_to_all<I, T>(iter: I, f: fn(T) -> AnsiString) -> Vec<String>
where
    I: IntoIterator<Item = T>,
    T: Clone,
{
    iter.into_iter().map(|item: T| f(item).build()).collect()
}

/// Modify a Vec in-place by applying an ANSI formatting function to all items.
pub fn apply_ansi_in_place(vec: &mut Vec<String>, f: fn(String) -> AnsiString) {
    for item in vec.iter_mut() {
        *item = f(item.clone()).build();
    }
}

////////////////////////////////////////////////////////////////////////////////

/// Combine ANSI codes into a single ANSI escape sequence
pub fn combine_codes(fg: AnsiColor, bg: Option<AnsiColor>, cc: Option<Vec<u8>>) -> String {
    let mut codes = vec![fg as u8];
    if let Some(b) = bg {
        codes.push(b.bg());
    }
    if let Some(c) = cc {
        codes.extend(c);
    }
    let codes_str = codes
        .iter()
        .map(|&code| code.to_string())
        .collect::<Vec<_>>()
        .join(&ANSI_SEP.to_string());
    format!("{ANSI_BEG}{codes_str}{ANSI_END}")
}

// Remove ANSI formatting
pub fn remove_ansi_formatting(text: &str) -> (String, Option<String>) {
    let re_start: Regex = Regex::new(r"^\x1b\[([\d;]+)m").unwrap();
    let digits = re_start.captures(text).map(|caps| caps[1].to_string());

    let mut cleaned: String = if digits.is_some() {
        re_start.replace(text, "").to_string()
    } else {
        text.to_string()
    };

    let re_end: Regex = Regex::new(r"\x1b\[0m$").unwrap();
    cleaned = re_end.replace(&cleaned, "").to_string();

    (cleaned, digits)
}

/// Helper to get a single ANSI code
#[inline]
pub fn ansi_code(code: u8) -> String {
    format!("{ANSI_BEG}{code}{ANSI_END}")
}

/// Helper for 8-bit color
#[inline]
pub fn ansi_8bit(code: u8) -> String {
    format!("{ANSI_BEG}38{ANSI_SEP}5{ANSI_SEP}{code}{ANSI_END}")
}

/// Helper for RGB color
#[inline]
pub fn ansi_rgb(r: u8, g: u8, b: u8) -> String {
    format!("{ANSI_BEG}38{ANSI_SEP}2{ANSI_SEP}{r}{ANSI_SEP}{g}{ANSI_SEP}{b}{ANSI_END}")
}

/// Orange colorizing helper (using 8-bit color 208 for orange)
pub fn orange<S: AsRef<str>>(s: S) -> String {
    format!("{}{}{CLR}", ansi_8bit(208), s.as_ref())
}
