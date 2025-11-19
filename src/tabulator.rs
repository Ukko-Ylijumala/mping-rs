// Copyright (c) 2025 Mikko Tanner. All rights reserved.
// Licensed under the MIT License or the Apache License, Version 2.0.
// SPDX-License-Identifier: MIT OR Apache-2.0

use lazy_static::lazy_static;
use regex::Regex;
use std::fmt::Display;

lazy_static! {
    /// Regex to match ANSI escape codes
    static ref ANSI_RX: Regex = Regex::new(r"\x1b\[[0-9;]*m").unwrap();
}

/// Return the visible length of a string, ignoring ANSI escape codes
#[inline]
fn visible_len(s: &str) -> usize {
    ANSI_RX.replace_all(s, "").chars().count()
}

/// Format a single row with padding
fn format_row(row: &[String], widths: &[usize], missing: Option<&str>) -> String {
    let mut items: Vec<String> = Vec::new();

    for (i, item) in row.iter().enumerate() {
        let vis_len: usize = visible_len(item);
        let pad: String = " ".repeat(widths[i].saturating_sub(vis_len));
        items.push(format!("{item}{pad}"));
        //items.push(item.to_owned() + &pad);
    }

    // Pad with `missing` value(s) if the row has too few items
    if missing.is_some() {
        let diff: usize = widths.len().saturating_sub(items.len());
        if diff > 0 {
            let missing: &str = missing.expect("\"missing\" should be a string");
            let missing_vis_len: usize = visible_len(missing);
            for j in 0..diff {
                let pad: String =
                    " ".repeat(widths[items.len() + j].saturating_sub(missing_vis_len));
                items.push(format!("{missing}{pad}"));
            }
        }
    }
    items.join(" | ")
}

/// Core tabulation function
fn tabulate(data: Vec<Vec<String>>, hdr: bool, out: &mut Vec<String>, missing: Option<&str>) {
    // Find the maximum width needed for each column (based on visible lengths)
    let columns: usize = data
        .iter()
        .map(|row: &Vec<String>| row.len())
        .max()
        .unwrap_or(1);
    let mut widths: Vec<usize> = vec![0; columns];

    for row in &data {
        for (i, item) in row.iter().enumerate() {
            widths[i] = widths[i].max(visible_len(item));
        }
    }

    // Format each row with appropriate padding
    let start_index: usize = if hdr {
        // Format headers with a separator line
        out.push(format_row(&data[0], &widths, missing));
        let separator: String = widths
            .iter()
            .map(|w: &usize| "-".repeat(*w))
            .collect::<Vec<_>>()
            .join("-+-");
        out.push(separator);
        1
    } else {
        0
    };

    for row in &data[start_index..] {
        out.push(format_row(row, &widths, missing));
    }
}

/// Format a collection of rows as a table for printing.
///
/// ## Arguments
/// * `data` - Iterator of rows (each row is an iterator of items)
/// * `headers` - Optional slice of column headers
///
/// ## Returns
///   * Vec of Strings containing the formatted table
pub fn simple_tabulate<I, R, T>(data: I, headers: Option<&[&str]>) -> Vec<String>
where
    I: IntoIterator<Item = R>,
    R: IntoIterator<Item = T>,
    T: Display,
{
    let mut data_rows: Vec<Vec<String>> = Vec::new();
    let mut formatted: Vec<String> = Vec::new();

    // Add headers if provided
    if let Some(hdrs) = headers {
        data_rows.push(hdrs.iter().map(|h: &&str| h.to_string()).collect());
    }

    // Stringify all data row items
    for row in data {
        let stringified_items: Vec<String> = row.into_iter().map(|item| item.to_string()).collect();
        data_rows.push(stringified_items);
    }

    if data_rows.is_empty() {
        return formatted;
    }

    tabulate(data_rows, headers.is_some(), &mut formatted, None);
    formatted
}

/// Format a collection of rows as a table for printing. Handles Option<T> values,
/// replacing None with the provided `missing` string.
///
/// ## Arguments
/// * `data` - Iterator of rows (each row is an iterator of items)
/// * `headers` - Optional slice of column headers
/// * `missing` - String to replace None values with
///
/// ## Returns
///   * Vec of Strings containing the formatted table
pub fn tabulate_with_missing<I, R, T>(
    data: I,
    headers: Option<&[&str]>,
    missing: &str,
) -> Vec<String>
where
    I: IntoIterator<Item = R>,
    R: IntoIterator<Item = Option<T>>,
    T: Display,
{
    let mut data_rows: Vec<Vec<String>> = Vec::new();
    let mut formatted: Vec<String> = Vec::new();

    // Add headers if provided
    if let Some(hdrs) = headers {
        data_rows.push(hdrs.iter().map(|h: &&str| h.to_string()).collect());
    }

    // Handle Option<T> values and stringify all data row items
    for row in data {
        let stringified_items: Vec<String> = row
            .into_iter()
            .map(|item| match item {
                Some(val) => val.to_string(),
                None => missing.to_string(),
            })
            .collect();
        data_rows.push(stringified_items);
    }

    if data_rows.is_empty() {
        return formatted;
    }

    tabulate(data_rows, headers.is_some(), &mut formatted, Some(missing));
    formatted
}
