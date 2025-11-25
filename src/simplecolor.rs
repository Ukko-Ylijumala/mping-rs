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

// ANSI control codes as constants
const ANSI_BEG: &str = "\x1b[";
const ANSI_SEP: char = ';';
const ANSI_END: char = 'm';
const ANSI_CLEAR: u8 = 0;
const ANSI_BOLD: u8 = 1;
const ANSI_FAINT: u8 = 2;
const ANSI_ITALIC: u8 = 3;
const ANSI_UNDERLINE: u8 = 4;
const ANSI_BLINK: u8 = 5;
const ANSI_BLINK_FAST: u8 = 6;
const ANSI_INVERSE: u8 = 7;
const ANSI_CONCEAL: u8 = 8;
const ANSI_CROSSOUT: u8 = 9;
const ANSI_NO_BOLD: u8 = 21;
const ANSI_NORMAL: u8 = 22;
const ANSI_NO_ITALIC: u8 = 23;
const ANSI_NO_UNDERLINE: u8 = 24;
const ANSI_NO_BLINK: u8 = 25;
const ANSI_NO_INVERSE: u8 = 27;
const ANSI_NO_CONCEAL: u8 = 28;
const ANSI_NO_CROSSOUT: u8 = 29;

/// Full ANSI clear string for convenience
pub const CLR: &str = "\x1b[0m";

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

    pub fn with_code(mut self, code: u8) -> Self {
        self.codes.push(code);
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

#[inline]
pub fn bold(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_code(ANSI_BOLD)
}
#[inline]
pub fn underline(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_code(ANSI_UNDERLINE)
}
#[inline]
pub fn blink(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_code(ANSI_BLINK)
}
#[inline]
pub fn inverse(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_code(ANSI_INVERSE)
}
#[inline]
pub fn crossout(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_code(ANSI_CROSSOUT)
}

//////// Convenience functions for foreground colors ////////

#[inline]
pub fn blk(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_fg(AnsiColor::Black)
}
#[inline]
pub fn red(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_fg(AnsiColor::Red)
}
#[inline]
pub fn grn(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_fg(AnsiColor::Green)
}
#[inline]
pub fn yel(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_fg(AnsiColor::Yellow)
}
#[inline]
pub fn blu(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_fg(AnsiColor::Blue)
}
#[inline]
pub fn mag(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_fg(AnsiColor::Magenta)
}
#[inline]
pub fn cya(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_fg(AnsiColor::Cyan)
}
#[inline]
pub fn whi(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_fg(AnsiColor::White)
}
#[inline]
pub fn rdb(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_fg(AnsiColor::RedBright)
}
#[inline]
pub fn grb(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_fg(AnsiColor::GreenBright)
}
#[inline]
pub fn org(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_fg(AnsiColor::YellowBright)
}
#[inline]
pub fn lbl(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_fg(AnsiColor::BlueBright)
}
#[inline]
pub fn mgb(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_fg(AnsiColor::MagentaBright)
}
#[inline]
pub fn cyb(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_fg(AnsiColor::CyanBright)
}
#[inline]
pub fn whb(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_fg(AnsiColor::WhiteBright)
}

//////// Convenience functions for background colors ////////

#[inline]
pub fn b_blk(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_bg(AnsiColor::Black)
}
#[inline]
pub fn b_red(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_bg(AnsiColor::Red)
}
#[inline]
pub fn b_grn(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_bg(AnsiColor::Green)
}
#[inline]
pub fn b_yel(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_bg(AnsiColor::Yellow)
}
#[inline]
pub fn b_blu(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_bg(AnsiColor::Blue)
}
#[inline]
pub fn b_mag(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_bg(AnsiColor::Magenta)
}
#[inline]
pub fn b_cya(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_bg(AnsiColor::Cyan)
}
#[inline]
pub fn b_whi(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_bg(AnsiColor::White)
}
#[inline]
pub fn b_rdb(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_bg(AnsiColor::RedBright)
}
#[inline]
pub fn b_grb(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_bg(AnsiColor::GreenBright)
}
#[inline]
pub fn b_org(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_bg(AnsiColor::YellowBright)
}
#[inline]
pub fn b_lbl(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_bg(AnsiColor::BlueBright)
}
#[inline]
pub fn b_mgb(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_bg(AnsiColor::MagentaBright)
}
#[inline]
pub fn b_cyb(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_bg(AnsiColor::CyanBright)
}
#[inline]
pub fn b_whb(text: impl Into<String>) -> AnsiString {
    AnsiString::new(text).with_bg(AnsiColor::WhiteBright)
}

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
