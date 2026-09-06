//! ExportUtils builtin: fast native export of grid-style data, returned to
//! ActionScript as a `flash.utils.ByteArray`.
//!
//! Entry points — `rows`, `fields`, `header` are positional; everything else
//! rides in the trailing `options` bag:
//!   ExportUtils.syncExport(rows, fields, header, options):ByteArray
//!   ExportUtils.asyncExportBegin(rows, fields, header, options):Object
//!     (+ asyncExportContinue / asyncExportEnd / asyncExportCancel)
//!
//! The `options.format` key (default `"xlsx"`) selects a real `.xlsx` file or a
//! CSV (UTF-8 + BOM, CRLF). Options that do not apply to the chosen format are
//! simply ignored.
//!
//! Positional parameters:
//!   rows:Array|XMLList   (required) row source; Array elements may be
//!                        XMLReadOnly / XML / plain objects, or an XMLList.
//!   fields:Array         (required) per-column selector, same spec as
//!                        XMLReadOnly.sortKeyed: ""=node, "@n"=attribute,
//!                        "n"=child element/property; "" or undefined = empty.
//!   header:XML           (xlsx; may be null) grouped/multi-row header
//!                        `<h><c t="A"/><g t="G">..</g></h>`; `<c>` leaves match
//!                        `fields` order; `textAlign` attr (left/center/right)
//!                        sets the column alignment. null (or CSV) -> a flat
//!                        header row is derived from `fields` (`@` stripped).
//!
//! options bag:
//!   format:String="xlsx"             "xlsx" | "csv".
//!
//! xlsx-only:
//!   sheetName:String="Foglio1"
//!   compression:int=6                 ZIP/deflate level 0-9.
//!   fontFamily:String="Calibri"       global font.
//!   fontSize:Number=11                global font size.
//!   headerBackgroundColor:uint        header fill (0xRRGGBB).
//!   headerForegroundColor:uint        header font colour (0xRRGGBB).
//!   rowBackgroundColors:[uint,uint]   alternating data-row fill (populated cells).
//!   detectTypes:Boolean=true          auto-detect each column's type (text/number/
//!                                     date) from a sample; false -> all text.
//!   detectDates:Boolean=true          within detection, store `YYYY-MM-DD HH:MM:SS`
//!                                     columns as real dates (serial + date format).
//!   typeSampleRows:int=1000           rows scanned to infer types (0 = all).
//!
//! csv-only:
//!   separator:String=";"             column separator.
//!   separatorHint:Boolean=false      prepend a `sep=<sep>\r\n` line so some
//!                                    spreadsheet apps adopt a non-standard sep.
//!   forceText:Boolean=false          wrap every cell as `="<value>"` so it is
//!                                    imported as text, not parsed.
//!   compress:Boolean=false           wrap the CSV in a ZIP archive (one entry,
//!                                    `<sheetName>.csv`) using `compression`
//!                                    (0-9, default 6). The CSV is streamed
//!                                    straight into the deflate encoder.
//! In CSV mode `header` is ignored; the column headings always come from
//! `fields` (the leading `@` of attribute selectors is stripped).
//!
//! The xlsx worksheet is streamed row by row straight into a hand-rolled ZIP
//! entry compressed with flate2 (raw deflate), inline strings, O(1) peak
//! memory per row. A thin black border is drawn around every cell.

use crate::avm2::bytearray::ByteArrayStorage;
use crate::avm2::e4x::{E4XNode, E4XNodeKind, name_to_multiname};
use crate::avm2::e4x_read_only::E4XNodeReadOnly;
use crate::avm2::function::FunctionArgs;
use crate::avm2::object::{ByteArrayObject, Object, ScriptObject, TObject as _};
use crate::avm2::parameters::ParametersExt;
use crate::avm2::{Activation, Error, Multiname, Value};
use crate::string::AvmString;
use flate2::write::DeflateEncoder;
use flate2::{Compress, CompressError, Compression, FlushCompress, Status};
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::OnceLock;

/// Hard limits of the xlsx grid.
const MAX_ROW_INDEX: u32 = 1_048_575;
const MAX_COL_INDEX: usize = 16_383;

// =================================================================================================
// Error plumbing
// =================================================================================================

#[derive(Debug)]
struct ExportError(&'static str);

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for ExportError {}

fn fail<'gc>(msg: &'static str) -> Error<'gc> {
    Error::rust_error(Box::new(ExportError(msg)))
}

fn map_io<'gc>(e: std::io::Error) -> Error<'gc> {
    Error::rust_error(Box::new(e))
}

// =================================================================================================
// Options bag readers
// =================================================================================================

/// Read `key` from the options bag, `None` if the bag/property is absent/null.
fn opt_value<'gc>(
    activation: &mut Activation<'_, 'gc>,
    options: Option<Object<'gc>>,
    key: &str,
) -> Result<Option<Value<'gc>>, Error<'gc>> {
    let Some(o) = options else { return Ok(None) };
    let name = AvmString::new_utf8(activation.gc(), key);
    let value = Value::Object(o).get_public_property(name, activation)?;
    Ok(if matches!(value, Value::Undefined | Value::Null) {
        None
    } else {
        Some(value)
    })
}

fn opt_color<'gc>(
    activation: &mut Activation<'_, 'gc>,
    options: Option<Object<'gc>>,
    key: &str,
) -> Result<Option<u32>, Error<'gc>> {
    Ok(match opt_value(activation, options, key)? {
        Some(v) => Some(v.coerce_to_i32(activation)? as u32),
        None => None,
    })
}

// =================================================================================================
// Column selectors and rows
// =================================================================================================

enum Selector<'gc> {
    /// `undefined`/`null` field -> always an empty cell.
    Empty,
    /// `""` -> the row's own simple content.
    Node,
    Child {
        name: Multiname<'gc>,
        prop: AvmString<'gc>,
    },
    Attr {
        name: Multiname<'gc>,
        prop: AvmString<'gc>,
    },
}

#[derive(Clone, Copy)]
enum Row<'gc> {
    ReadOnly(E4XNodeReadOnly<'gc>),
    Xml(E4XNode<'gc>),
    Object(Value<'gc>),
}

fn classify_value<'gc>(v: Value<'gc>) -> Row<'gc> {
    if let Some(o) = v.as_object() {
        if let Some(node) = o.as_xml_object_read_only().and_then(|x| x.node()) {
            return Row::ReadOnly(node);
        }
        if let Some(x) = o.as_xml_object() {
            return Row::Xml(x.node());
        }
    }
    Row::Object(v)
}

/// Number of rows in an Array (`ArrayStorage`) or XMLList; `None` if `rows_obj`
/// is neither.
fn rows_len<'gc>(rows_obj: Object<'gc>) -> Option<usize> {
    if let Some(storage) = rows_obj.as_array_storage() {
        Some(storage.length())
    } else {
        rows_obj
            .as_xml_list_object()
            .map(|list| list.children().len())
    }
}

// =================================================================================================
// Cell extraction
// =================================================================================================

fn append_children_text<'gc>(node: E4XNode<'gc>, want: &Multiname<'gc>, out: &mut String) {
    if let E4XNodeKind::Element(elem) = &*node.kind() {
        let mut first = true;
        for child in elem.children.iter() {
            let is_element = matches!(&*child.kind(), E4XNodeKind::Element(_));
            if is_element && child.matches_name(want) {
                if !first {
                    out.push('\n');
                }
                append_simple_text(*child, out);
                first = false;
            }
        }
    }
}

fn append_attrs_text<'gc>(node: E4XNode<'gc>, want: &Multiname<'gc>, out: &mut String) {
    if let E4XNodeKind::Element(elem) = &*node.kind() {
        let mut first = true;
        for attr in elem.attributes().iter() {
            if attr.matches_name(want) {
                if !first {
                    out.push('\n');
                }
                append_simple_text(*attr, out);
                first = false;
            }
        }
    }
}

fn append_simple_text<'gc>(node: E4XNode<'gc>, out: &mut String) {
    match &*node.kind() {
        E4XNodeKind::Text(s) | E4XNodeKind::CData(s) | E4XNodeKind::Attribute(s) => {
            out.push_str(&s.to_utf8_lossy());
        }
        E4XNodeKind::Comment(_) | E4XNodeKind::ProcessingInstruction(_) => {}
        E4XNodeKind::Element(elem) => {
            for child in elem.children.iter() {
                if !matches!(
                    &*child.kind(),
                    E4XNodeKind::Comment(_) | E4XNodeKind::ProcessingInstruction(_)
                ) {
                    append_simple_text(*child, out);
                }
            }
        }
    }
}

fn extract_cell<'gc>(
    activation: &mut Activation<'_, 'gc>,
    row: &Row<'gc>,
    selector: &Selector<'gc>,
    buf: &mut String,
) -> Result<(), Error<'gc>> {
    buf.clear();
    match row {
        Row::ReadOnly(node) => match selector {
            Selector::Empty => {}
            Selector::Node => node.append_text(buf),
            Selector::Attr { name, .. } => node.append_attrs_text(name, buf),
            Selector::Child { name, .. } => node.append_children_text(name, buf),
        },
        Row::Xml(node) => match selector {
            Selector::Empty => {}
            Selector::Node => append_simple_text(*node, buf),
            Selector::Attr { name, .. } => append_attrs_text(*node, name, buf),
            Selector::Child { name, .. } => append_children_text(*node, name, buf),
        },
        Row::Object(value) => {
            let cell = match selector {
                Selector::Empty => return Ok(()),
                Selector::Node => *value,
                Selector::Child { prop, .. } | Selector::Attr { prop, .. } => {
                    value.get_public_property(*prop, activation)?
                }
            };
            if !matches!(cell, Value::Undefined | Value::Null) {
                let text = cell.coerce_to_string(activation)?;
                buf.push_str(&text.to_utf8_lossy());
            }
        }
    }
    Ok(())
}

/// Column data type, inferred per column from a sample of the rows.
#[derive(Clone, Copy, PartialEq)]
enum ColType {
    Text,
    Number,
    Date,
}

/// Type-detection options (xlsx only).
#[derive(Clone, Copy)]
struct TypeOpts {
    detect_types: bool,
    detect_dates: bool,
    sample_rows: usize,
}

/// Conservative numeric check + invariant normalisation. Accepts an optional
/// sign, decimal digits, and at most ONE decimal separator (`.` or `,`, always
/// decimal — never thousands). The integer part may be omitted (`,332` / `.332`
/// == `0.332`). Rejects exponents, extra separators, leading-zero codes and
/// values over 15 significant digits. Returns the value with a `.` decimal
/// separator (borrowed when already invariant), ready for `<v>`; `None` when the
/// string is not a plain number.
fn number_repr(s: &str) -> Option<Cow<'_, str>> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let body = t.strip_prefix(['+', '-']).unwrap_or(t);
    let mut sep: Option<usize> = None;
    let mut digits = 0usize;
    for (i, ch) in body.char_indices() {
        match ch {
            '0'..='9' => digits += 1,
            '.' | ',' => {
                if sep.is_some() {
                    return None;
                }
                sep = Some(i);
            }
            _ => return None,
        }
    }
    if digits == 0 {
        return None;
    }
    let (int_part, frac_part) = match sep {
        Some(p) => (&body[..p], &body[p + 1..]),
        None => (body, ""),
    };
    // With a separator the integer part may be omitted (",332" == 0.332), but the
    // fractional part must have digits; without a separator, integer digits are
    // required.
    if sep.is_some() {
        if frac_part.is_empty() {
            return None;
        }
    } else if int_part.is_empty() {
        return None;
    }
    // Reject leading-zero codes ("007"); a lone "0" or "0.x" is fine.
    if int_part.len() > 1 && int_part.as_bytes()[0] == b'0' {
        return None;
    }
    if int_part.len() + frac_part.len() > 15 {
        return None;
    }
    // Omitted integer part -> synthesise a leading "0" (",332" -> "0.332").
    if int_part.is_empty() {
        let mut out = String::with_capacity(t.len() + 1);
        if t.starts_with('-') {
            out.push('-');
        }
        out.push_str("0.");
        out.push_str(frac_part);
        return Some(Cow::Owned(out));
    }
    // Already invariant (plain or with a leading '-') -> borrow as-is.
    if !body.contains(',') && !t.starts_with('+') {
        return Some(Cow::Borrowed(t));
    }
    let mut out = String::with_capacity(t.len());
    if t.starts_with('-') {
        out.push('-');
    }
    for ch in body.chars() {
        out.push(if ch == ',' { '.' } else { ch });
    }
    Some(Cow::Owned(out))
}

/// Days from 1970-01-01 in the proleptic Gregorian calendar
/// (Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Parse a `YYYY-MM-DD HH:MM:SS` timestamp into an Excel serial date (1900 date
/// system: whole part = days since 1899-12-30, fractional part = time of day).
/// `None` unless the exact 19-char layout and valid ranges are present.
fn parse_date_serial(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    if b.len() != 19 {
        return None;
    }
    if b[4] != b'-' || b[7] != b'-' || b[10] != b' ' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |lo: usize, hi: usize| -> Option<i64> {
        let mut n = 0i64;
        for &c in &b[lo..hi] {
            if !c.is_ascii_digit() {
                return None;
            }
            n = n * 10 + (c - b'0') as i64;
        }
        Some(n)
    };
    let year = num(0, 4)?;
    let month = num(5, 7)?;
    let day = num(8, 10)?;
    let hh = num(11, 13)?;
    let mm = num(14, 16)?;
    let ss = num(17, 19)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hh > 23 || mm > 59 || ss > 59 {
        return None;
    }
    // 25569 = Excel serial of 1970-01-01; adding it reproduces the 1900
    // leap-year bug for every date from 1900-03-01 on (all real timestamps).
    let serial_days = days_from_civil(year, month, day) + 25569;
    if serial_days < 1 {
        return None;
    }
    let frac = (hh * 3600 + mm * 60 + ss) as f64 / 86400.0;
    Some(serial_days as f64 + frac)
}

fn is_date(s: &str) -> bool {
    parse_date_serial(s).is_some()
}

/// Read the type-detection options from the bag (xlsx only).
fn read_type_opts<'gc>(
    activation: &mut Activation<'_, 'gc>,
    options: Option<Object<'gc>>,
) -> Result<TypeOpts, Error<'gc>> {
    let detect_types = match opt_value(activation, options, "detectTypes")? {
        Some(v) => v.coerce_to_boolean(),
        None => true,
    };
    let detect_dates = match opt_value(activation, options, "detectDates")? {
        Some(v) => v.coerce_to_boolean(),
        None => true,
    };
    let sample_rows = match opt_value(activation, options, "typeSampleRows")? {
        Some(v) => v.coerce_to_i32(activation)?.max(0) as usize,
        None => 1000,
    };
    Ok(TypeOpts {
        detect_types,
        detect_dates,
        sample_rows,
    })
}

/// Infer a [`ColType`] per column by scanning up to `opts.sample_rows` rows
/// (0 = all). A column is `Date`/`Number` only if *every* non-empty sampled
/// cell matches; otherwise `Text`. `get_row(i)` yields the i-th row.
fn infer_col_types<'gc>(
    activation: &mut Activation<'_, 'gc>,
    get_row: impl Fn(usize) -> Row<'gc>,
    nrows: usize,
    selectors: &[Selector<'gc>],
    ncols: usize,
    opts: TypeOpts,
) -> Result<Vec<ColType>, Error<'gc>> {
    if !opts.detect_types {
        return Ok(vec![ColType::Text; ncols]);
    }
    let n = if opts.sample_rows == 0 {
        nrows
    } else {
        opts.sample_rows.min(nrows)
    };
    let mut nonempty = vec![0usize; ncols];
    let mut ndate = vec![0usize; ncols];
    let mut nnum = vec![0usize; ncols];
    let mut buf = String::new();
    for i in 0..n {
        let row = get_row(i);
        for c in 0..ncols {
            let Some(sel) = selectors.get(c) else {
                continue;
            };
            extract_cell(activation, &row, sel, &mut buf)?;
            let t = buf.trim();
            if t.is_empty() {
                continue;
            }
            nonempty[c] += 1;
            if opts.detect_dates && is_date(t) {
                ndate[c] += 1;
            } else if number_repr(t).is_some() {
                nnum[c] += 1;
            }
        }
    }
    Ok((0..ncols)
        .map(|c| {
            if nonempty[c] == 0 {
                ColType::Text
            } else if opts.detect_dates && ndate[c] == nonempty[c] {
                ColType::Date
            } else if nnum[c] == nonempty[c] {
                ColType::Number
            } else {
                ColType::Text
            }
        })
        .collect())
}

// =================================================================================================
// xlsx (OOXML) text helpers
// =================================================================================================

fn sanitize_sheet_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .filter(|c| !matches!(c, '\\' | '/' | '*' | '?' | ':' | '[' | ']'))
        .collect();
    let trimmed = out.trim();
    if trimmed.len() != out.len() {
        out = trimmed.to_string();
    }
    if out.is_empty() {
        return "Foglio1".to_string();
    }
    if out.chars().count() > 31 {
        out = out.chars().take(31).collect();
    }
    out
}

fn xml_escape(s: &str, out: &mut String) {
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\t' | '\n' | '\r' => out.push(ch),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
}

fn xml_escape_attr(s: &str, out: &mut String) {
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\t' | '\n' | '\r' => out.push(ch),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
}

fn col_letter(col: usize) -> String {
    let mut bytes = Vec::new();
    let mut n = col + 1;
    while n > 0 {
        let rem = (n - 1) % 26;
        bytes.push(b'A' + rem as u8);
        n = (n - 1) / 26;
    }
    bytes.reverse();
    String::from_utf8(bytes).unwrap()
}

// =================================================================================================
// Grouped (multi-row) header layout
// =================================================================================================

/// `align` is the raw `textAlign` ("", "left", "center", "right").
enum HeaderNode {
    Leaf {
        label: String,
        align: String,
        width: Option<f64>,
    },
    Group {
        label: String,
        align: String,
        children: Vec<HeaderNode>,
    },
}

fn leaf_count(n: &HeaderNode) -> usize {
    match n {
        HeaderNode::Leaf { .. } => 1,
        HeaderNode::Group { children, .. } => children.iter().map(leaf_count).sum::<usize>().max(1),
    }
}

fn node_depth(n: &HeaderNode) -> u32 {
    match n {
        HeaderNode::Leaf { .. } => 1,
        HeaderNode::Group { children, .. } => {
            1 + children.iter().map(node_depth).max().unwrap_or(0)
        }
    }
}

struct HeaderCell {
    row: u32,
    col: usize,
    rowspan: u32,
    colspan: usize,
    label: String,
    align: String,
}

fn collect_cells(
    nodes: &[HeaderNode],
    depth: u32,
    total: u32,
    col0: usize,
    out: &mut Vec<HeaderCell>,
) {
    let mut col = col0;
    for n in nodes {
        let span = leaf_count(n);
        match n {
            HeaderNode::Leaf { label, align, .. } => out.push(HeaderCell {
                row: depth,
                col,
                rowspan: total - depth + 1,
                colspan: 1,
                label: label.clone(),
                align: header_align(align),
            }),
            HeaderNode::Group {
                label,
                align,
                children,
            } => {
                out.push(HeaderCell {
                    row: depth,
                    col,
                    rowspan: 1,
                    colspan: span,
                    label: label.clone(),
                    align: header_align(align),
                });
                collect_cells(children, depth + 1, total, col, out);
            }
        }
        col += span;
    }
}

/// Leaf alignment in column order (extends to the data cells of that column).
fn collect_leaf_aligns(nodes: &[HeaderNode], out: &mut Vec<String>) {
    for n in nodes {
        match n {
            // Propagate the leaf header's alignment (centred by default) to the
            // whole data column.
            HeaderNode::Leaf { align, .. } => out.push(header_align(align)),
            HeaderNode::Group { children, .. } => collect_leaf_aligns(children, out),
        }
    }
}

/// Leaf column widths (xlsx width units) in column order; `None` = default width.
fn collect_leaf_widths(nodes: &[HeaderNode], out: &mut Vec<Option<f64>>) {
    for n in nodes {
        match n {
            HeaderNode::Leaf { width, .. } => out.push(*width),
            HeaderNode::Group { children, .. } => collect_leaf_widths(children, out),
        }
    }
}

/// Grouped headers default to centred when no explicit `textAlign`.
fn header_align(raw: &str) -> String {
    if raw.is_empty() {
        "center".to_string()
    } else {
        raw.to_string()
    }
}

fn parse_header(root: E4XNode) -> Vec<HeaderNode> {
    let mut out = Vec::new();
    if let E4XNodeKind::Element(elem) = &*root.kind() {
        for child in elem.children.iter() {
            if !matches!(&*child.kind(), E4XNodeKind::Element(_)) {
                continue;
            }
            let tag = child
                .local_name()
                .map(|n| n.to_utf8_lossy().into_owned())
                .unwrap_or_default();
            let label = header_attr(*child, "t");
            let align = header_attr(*child, "textAlign");
            match tag.as_str() {
                "g" => out.push(HeaderNode::Group {
                    label,
                    align,
                    children: parse_header(*child),
                }),
                "c" => {
                    let w = header_attr(*child, "width");
                    let width = if w.is_empty() {
                        None
                    } else {
                        w.parse::<f64>().ok()
                    };
                    out.push(HeaderNode::Leaf {
                        label,
                        align,
                        width,
                    });
                }
                _ => {}
            }
        }
    }
    out
}

fn header_attr(node: E4XNode, key: &str) -> String {
    if let E4XNodeKind::Element(elem) = &*node.kind() {
        for attr in elem.attributes().iter() {
            if let E4XNodeKind::Attribute(value) = &*attr.kind()
                && attr
                    .local_name()
                    .map(|n| n.to_utf8_lossy() == key)
                    .unwrap_or(false)
            {
                return value.to_utf8_lossy().into_owned();
            }
        }
    }
    String::new()
}

// =================================================================================================
// Styles
// =================================================================================================

struct StyleOpts {
    font_family: String,
    font_size: f64,
    header_bg: Option<u32>,
    header_fg: Option<u32>,
    row_colors: Option<(u32, u32)>,
}

/// Alignment index: ""/unknown = 0 (general), left = 1, center = 2, right = 3.
fn align_index(a: &str) -> usize {
    match a {
        "left" => 1,
        "center" => 2,
        "right" => 3,
        _ => 0,
    }
}

/// cellXfs layout (built by [`build_styles`]). Each data band is 8 entries:
/// 4 alignments (general/left/center/right) x 2 row parities (even/odd).
///   0           default for cells OUTSIDE the populated area (no border, no fill)
///   1..=4       header
///   5..=12      data, General numFmt 0 (empty + text + number cells)
///   13..=20     data, DATE (numFmt 164 = yyyy-mm-dd hh:mm:ss)
///
/// Numbers use General so integers show no trailing decimal separator (a custom
/// `0.###` shows one on integers in Excel). The "number stored as text" warning
/// on numeric-looking text cells is suppressed with `<ignoredErrors>`, not a
/// cell style. Empty cells reuse `data_style("", parity)`.
fn header_style(align: &str) -> u32 {
    1 + align_index(align) as u32
}

fn data_style(align: &str, parity: usize) -> u32 {
    5 + (parity * 4) as u32 + align_index(align) as u32
}

fn date_style(align: &str, parity: usize) -> u32 {
    13 + (parity * 4) as u32 + align_index(align) as u32
}

fn solid_fill(color: u32) -> String {
    format!(
        "<fill><patternFill patternType=\"solid\"><fgColor rgb=\"FF{:06X}\"/></patternFill></fill>",
        color & 0xFF_FFFF
    )
}

fn cell_xf(num_fmt: u32, font: usize, fill: usize, align: &str) -> String {
    // Every cell uses border 1 (thin black box).
    let apply_nf = if num_fmt != 0 {
        " applyNumberFormat=\"1\""
    } else {
        ""
    };
    if align.is_empty() {
        format!(
            "<xf numFmtId=\"{num_fmt}\" fontId=\"{font}\" fillId=\"{fill}\" borderId=\"1\" xfId=\"0\"{apply_nf} applyFont=\"1\" applyFill=\"1\" applyBorder=\"1\"/>"
        )
    } else {
        format!(
            "<xf numFmtId=\"{num_fmt}\" fontId=\"{font}\" fillId=\"{fill}\" borderId=\"1\" xfId=\"0\"{apply_nf} applyFont=\"1\" applyFill=\"1\" applyBorder=\"1\" applyAlignment=\"1\"><alignment horizontal=\"{align}\" vertical=\"center\"/></xf>"
        )
    }
}

fn build_styles(opts: &StyleOpts) -> String {
    let mut family = String::new();
    xml_escape_attr(&opts.font_family, &mut family);
    let size = opts.font_size;

    // Fonts: 0 = normal, 1 = header (bold + optional colour).
    let hdr_color = opts
        .header_fg
        .map(|c| format!("<color rgb=\"FF{:06X}\"/>", c & 0xFF_FFFF))
        .unwrap_or_default();
    let fonts = format!(
        "<fonts count=\"2\"><font><sz val=\"{size}\"/><name val=\"{family}\"/></font><font><b/>{hdr_color}<sz val=\"{size}\"/><name val=\"{family}\"/></font></fonts>"
    );

    // Fills: 0 none, 1 gray125 (OOXML convention), then optional header/row fills.
    let mut fills = String::from(
        "<fill><patternFill patternType=\"none\"/></fill><fill><patternFill patternType=\"gray125\"/></fill>",
    );
    let mut nfills = 2usize;
    let header_fill = match opts.header_bg {
        Some(c) => {
            fills.push_str(&solid_fill(c));
            let i = nfills;
            nfills += 1;
            i
        }
        None => 0,
    };
    let (row0_fill, row1_fill) = match opts.row_colors {
        Some((c0, c1)) => {
            fills.push_str(&solid_fill(c0));
            let i0 = nfills;
            nfills += 1;
            fills.push_str(&solid_fill(c1));
            let i1 = nfills;
            nfills += 1;
            (i0, i1)
        }
        None => (0, 0),
    };
    let fills = format!("<fills count=\"{nfills}\">{fills}</fills>");

    // Borders: 0 = none, 1 = thin black box.
    let borders = "<borders count=\"2\"><border><left/><right/><top/><bottom/><diagonal/></border><border><left style=\"thin\"><color rgb=\"FF000000\"/></left><right style=\"thin\"><color rgb=\"FF000000\"/></right><top style=\"thin\"><color rgb=\"FF000000\"/></top><bottom style=\"thin\"><color rgb=\"FF000000\"/></bottom><diagonal/></border></borders>";

    // Custom number format: 164 = date/time. Numbers use General (numFmt 0):
    // integers show no trailing separator, decimals only when present, no
    // thousands separator.
    let numfmts = "<numFmts count=\"1\"><numFmt numFmtId=\"164\" formatCode=\"yyyy-mm-dd hh:mm:ss\"/></numFmts>";

    let aligns = ["", "left", "center", "right"];
    let mut xfs = String::new();
    // 0: default for untouched cells -> NO border (otherwise spreadsheet apps draw
    // the grid over the whole sheet). Only the explicit cell styles below carry a border.
    xfs.push_str("<xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\"/>");
    // 1..=4 header.
    for a in aligns {
        xfs.push_str(&cell_xf(0, 1, header_fill, a));
    }
    // 5..=12 data General numFmt 0 (empty + text + number): even then odd rows.
    for fill in [row0_fill, row1_fill] {
        for a in aligns {
            xfs.push_str(&cell_xf(0, 0, fill, a));
        }
    }
    // 13..=20 DATE (numFmt 164): even then odd rows.
    for fill in [row0_fill, row1_fill] {
        for a in aligns {
            xfs.push_str(&cell_xf(164, 0, fill, a));
        }
    }
    let cellxfs = format!("<cellXfs count=\"21\">{xfs}</cellXfs>");

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<styleSheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">{numfmts}{fonts}{fills}{borders}<cellStyleXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\"/></cellStyleXfs>{cellxfs}<cellStyles count=\"1\"><cellStyle name=\"Normal\" xfId=\"0\" builtinId=\"0\"/></cellStyles></styleSheet>"
    )
}

// Static OOXML parts.
const CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/></Types>"#;

const RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#;

const WB_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#;

fn workbook_xml(sheet_name: &str) -> String {
    let mut name = String::new();
    xml_escape_attr(&sanitize_sheet_name(sheet_name), &mut name);
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><sheets><sheet name=\"{name}\" sheetId=\"1\" r:id=\"rId1\"/></sheets></workbook>"
    )
}

fn sheet_open(freeze_rows: u32, cols: &str) -> String {
    let views = if freeze_rows == 0 {
        String::new()
    } else {
        format!(
            "<sheetViews><sheetView workbookViewId=\"0\"><pane ySplit=\"{freeze_rows}\" topLeftCell=\"A{top}\" activePane=\"bottomLeft\" state=\"frozen\"/><selection pane=\"bottomLeft\"/></sheetView></sheetViews>",
            top = freeze_rows + 1,
        )
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">{views}{cols}<sheetData>"
    )
}

// =================================================================================================
// Minimal hand-rolled ZIP container
// =================================================================================================

fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        let mut i = 0usize;
        while i < 256 {
            let mut c = i as u32;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
                k += 1;
            }
            t[i] = c;
            i += 1;
        }
        t
    });
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc
}

fn push_u16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}

fn push_u32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}

fn write_local_header(out: &mut Vec<u8>, name: &str, crc: u32, comp: u32, uncomp: u32) {
    push_u32(out, 0x0403_4b50);
    push_u16(out, 20);
    push_u16(out, 0);
    push_u16(out, 8);
    push_u16(out, 0);
    push_u16(out, 0x0021);
    push_u32(out, crc);
    push_u32(out, comp);
    push_u32(out, uncomp);
    push_u16(out, name.len() as u16);
    push_u16(out, 0);
    out.extend_from_slice(name.as_bytes());
}

fn write_central_entry(
    cd: &mut Vec<u8>,
    name: &str,
    crc: u32,
    comp: u32,
    uncomp: u32,
    offset: u32,
) {
    push_u32(cd, 0x0201_4b50);
    push_u16(cd, 20);
    push_u16(cd, 20);
    push_u16(cd, 0);
    push_u16(cd, 8);
    push_u16(cd, 0);
    push_u16(cd, 0x0021);
    push_u32(cd, crc);
    push_u32(cd, comp);
    push_u32(cd, uncomp);
    push_u16(cd, name.len() as u16);
    push_u16(cd, 0);
    push_u16(cd, 0);
    push_u16(cd, 0);
    push_u16(cd, 0);
    push_u32(cd, 0);
    push_u32(cd, offset);
    cd.extend_from_slice(name.as_bytes());
}

fn add_file(
    out: &mut Vec<u8>,
    cd: &mut Vec<u8>,
    count: &mut u16,
    name: &str,
    content: &[u8],
    level: u32,
) -> std::io::Result<()> {
    let crc = !crc32_update(!0u32, content);
    let mut enc = DeflateEncoder::new(Vec::new(), Compression::new(level));
    enc.write_all(content)?;
    let compressed = enc.finish()?;
    let offset = out.len() as u32;
    write_local_header(
        out,
        name,
        crc,
        compressed.len() as u32,
        content.len() as u32,
    );
    out.extend_from_slice(&compressed);
    write_central_entry(
        cd,
        name,
        crc,
        compressed.len() as u32,
        content.len() as u32,
        offset,
    );
    *count += 1;
    Ok(())
}

fn patch_u32(out: &mut [u8], pos: usize, val: u32) {
    out[pos..pos + 4].copy_from_slice(&val.to_le_bytes());
}

// =================================================================================================
// Workbook assembly (streaming)
// =================================================================================================

#[allow(clippy::too_many_arguments)]
/// Write one data cell (`<c ...>`) into `row_xml`, formatted per its column
/// type. Empty -> bordered/filled "general" style. Number/Date values that
/// fail to parse fall back to inline text, so no data is ever lost.
/// `<ignoredErrors>` block that suppresses the "number stored as text" green
/// triangle over the populated data area (text cells that look like numbers,
/// e.g. codes such as `23456E1`). Empty when there is no data.
fn ignored_errors_xml(
    col_letters: &[String],
    ncols: usize,
    data_start: u64,
    ndata: usize,
) -> String {
    if ncols == 0 || ndata == 0 {
        return String::new();
    }
    let last_row = (data_start + ndata as u64 - 1).min(MAX_ROW_INDEX as u64 + 1);
    let last_col = &col_letters[ncols - 1];
    format!(
        "<ignoredErrors><ignoredError sqref=\"A{data_start}:{last_col}{last_row}\" numberStoredAsText=\"1\"/></ignoredErrors>"
    )
}

fn write_data_cell(
    row_xml: &mut String,
    col_letter: &str,
    r_str: &str,
    buf: &str,
    col_type: ColType,
    align: &str,
    parity: usize,
) {
    row_xml.push_str("<c r=\"");
    row_xml.push_str(col_letter);
    row_xml.push_str(r_str);
    if buf.is_empty() {
        let s = data_style("", parity);
        let _ = write!(row_xml, "\" s=\"{s}\"/>");
        return;
    }
    match col_type {
        ColType::Number => {
            if let Some(n) = number_repr(buf) {
                let s = data_style(align, parity);
                let _ = write!(row_xml, "\" s=\"{s}\"><v>{n}</v></c>");
                return;
            }
        }
        ColType::Date => {
            if let Some(serial) = parse_date_serial(buf.trim()) {
                let s = date_style(align, parity);
                let _ = write!(row_xml, "\" s=\"{s}\"><v>{serial}</v></c>");
                return;
            }
        }
        ColType::Text => {}
    }
    // Text column, or a Number/Date outlier outside the sampled range. Uses the
    // General style; the "number stored as text" flag is suppressed sheet-wide
    // via <ignoredErrors>.
    let s = data_style(align, parity);
    let _ = write!(
        row_xml,
        "\" s=\"{s}\" t=\"inlineStr\"><is><t xml:space=\"preserve\">"
    );
    xml_escape(buf, row_xml);
    row_xml.push_str("</t></is></c>");
}

fn make_bytearray<'gc>(
    activation: &mut Activation<'_, 'gc>,
    bytes: Vec<u8>,
) -> ByteArrayObject<'gc> {
    let storage = ByteArrayStorage::from_vec(activation.context, bytes);
    ByteArrayObject::from_storage(activation.context, storage)
}

// =================================================================================================
// Native entry point
// =================================================================================================

/// `ExportUtils.syncExport(rows, fields, header, options):ByteArray` — dispatches
/// on `options.format` (default `"xlsx"`).
pub fn sync_export<'gc>(
    activation: &mut Activation<'_, 'gc>,
    this: Value<'gc>,
    args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    let options = args.try_get_object(3);
    let format = match opt_value(activation, options, "format")? {
        Some(v) => v.coerce_to_string(activation)?.to_utf8_lossy().into_owned(),
        None => "xlsx".to_string(),
    };
    match format.as_str() {
        "xlsx" => do_sync_xlsx(activation, this, args),
        "csv" => do_sync_csv(activation, this, args),
        _ => Err(fail(
            "ExportUtils.syncExport: options.format must be \"xlsx\" or \"csv\"",
        )),
    }
}

fn do_sync_xlsx<'gc>(
    activation: &mut Activation<'_, 'gc>,
    _this: Value<'gc>,
    args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    // rows / fields / header are positional; the trailing options bag holds the rest.
    let rows_obj = args
        .try_get_object(0)
        .ok_or_else(|| fail("ExportUtils.syncExport: rows must be an Array or XMLList"))?;
    let total_rows = rows_len(rows_obj)
        .ok_or_else(|| fail("ExportUtils.syncExport: rows must be an Array or XMLList"))?;

    let fields_obj = args
        .try_get_object(1)
        .ok_or_else(|| fail("ExportUtils.syncExport: fields must be an Array"))?;
    let selectors = parse_owned_selectors(activation, fields_obj)?;

    let header = args.get_optional(2).unwrap_or(Value::Undefined);
    let options = args.try_get_object(3);
    let type_opts = read_type_opts(activation, options)?;

    // Sync = the async pipeline run in one shot: begin + a single full-size
    // chunk + end. `begin_xlsx_state` does not register the state in the
    // thread-local map, so there is no handle to clean up.
    let mut state = begin_xlsx_state(
        activation, options, header, fields_obj, selectors, total_rows, total_rows, rows_obj,
        type_opts,
    )?;
    continue_export(activation, &mut state, rows_obj, total_rows)?;
    end_export(&mut state)?;
    Ok(make_bytearray(activation, state.output).into())
}

// =================================================================================================
// CSV export
// =================================================================================================

/// Column heading for the CSV header row, derived from a field selector.
/// "@name" -> "name", "name" -> "name", "" / undefined -> "".
fn parse_field_labels<'gc>(
    activation: &mut Activation<'_, 'gc>,
    fields: Object<'gc>,
) -> Result<Vec<String>, Error<'gc>> {
    let storage = fields
        .as_array_storage()
        .ok_or_else(|| fail("ExportUtils: fields must be an Array"))?;
    let n = storage.length();
    let mut out = Vec::with_capacity(n);
    for j in 0..n {
        let value = storage.get(j).unwrap_or(Value::Undefined);
        if matches!(value, Value::Undefined | Value::Null) {
            out.push(String::new());
            continue;
        }
        let text = value.coerce_to_string(activation)?;
        let utf8 = text.to_utf8_lossy();
        out.push(match utf8.strip_prefix('@') {
            Some(stripped) => stripped.to_string(),
            None => utf8.into_owned(),
        });
    }
    Ok(out)
}

/// Append `value` to `line` as one CSV cell, applying RFC 4180 quoting + the
/// optional `="..."` text sentinel.
fn csv_write_cell(value: &str, separator: &str, force_text: bool, line: &mut String) {
    if force_text {
        // Build the formula string ="VALUE" with every inner `"` doubled
        // (the formula-string escape), then wrap the whole thing in CSV quotes
        // and double every `"` again. The net effect: each original `"` ends
        // up as four `"` in the output.
        line.push('"');
        line.push('=');
        line.push_str("\"\"");
        for ch in value.chars() {
            if ch == '"' {
                line.push_str("\"\"\"\"");
            } else {
                line.push(ch);
            }
        }
        line.push_str("\"\"");
        line.push('"');
        return;
    }
    let needs_quote = value.contains('"')
        || value.contains('\n')
        || value.contains('\r')
        || (!separator.is_empty() && value.contains(separator));
    if !needs_quote {
        line.push_str(value);
        return;
    }
    line.push('"');
    for ch in value.chars() {
        if ch == '"' {
            line.push_str("\"\"");
        } else {
            line.push(ch);
        }
    }
    line.push('"');
}

fn do_sync_csv<'gc>(
    activation: &mut Activation<'_, 'gc>,
    _this: Value<'gc>,
    args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    // rows / fields are positional; header (args[2]) is ignored in CSV.
    let rows_obj = args
        .try_get_object(0)
        .ok_or_else(|| fail("ExportUtils.syncExport: rows must be an Array or XMLList"))?;
    let total_rows = rows_len(rows_obj)
        .ok_or_else(|| fail("ExportUtils.syncExport: rows must be an Array or XMLList"))?;

    let fields_obj = args
        .try_get_object(1)
        .ok_or_else(|| fail("ExportUtils.syncExport: fields must be an Array"))?;
    let selectors = parse_owned_selectors(activation, fields_obj)?;
    let labels = parse_field_labels(activation, fields_obj)?;

    let options = args.try_get_object(3);
    // Sync = begin + a single full-size chunk + end (see do_sync_xlsx).
    let mut state = begin_csv_state(
        activation, options, selectors, labels, total_rows, total_rows,
    )?;
    continue_export(activation, &mut state, rows_obj, total_rows)?;
    end_export(&mut state)?;
    Ok(make_bytearray(activation, state.output).into())
}

// =================================================================================================
// Asynchronous (chunked) export API
// =================================================================================================
//
// The synchronous export above can block the AS3 runtime for several seconds on
// large datasets. The `asyncExportBegin / Continue / End / Cancel` flow lets
// the caller drive the export in small chunks, yielding to the runtime between
// calls so a Flex progress bar can repaint.
//
// All `'gc`-tied data (the rows Array, the per-call Multiname objects) lives on
// the AS3 side: the handle returned by Begin is a plain ScriptObject that
// stores the rows Array as a dynamic property (kept alive by the AS3 GC) plus
// an integer id pointing into the thread-local state map below.
//
// The persistent state itself is owned (no `'gc` lifetime): selectors are
// stored as plain Strings and reconstructed into Multiname at each Continue,
// and the deflate stream uses the lower-level `flate2::Compress` instead of
// `DeflateEncoder` (the latter borrows its sink, which a long-lived state
// cannot).

/// Owned counterpart of [`Selector`], stored across native calls without a
/// `'gc` lifetime; rebuilt into a `'gc`-bound `Selector` at each Continue.
#[derive(Debug)]
enum OwnedSelector {
    Empty,
    Node,
    Child(String),
    Attr(String),
}

impl OwnedSelector {
    fn from_str(s: &str) -> Self {
        if s.is_empty() {
            OwnedSelector::Node
        } else if let Some(stripped) = s.strip_prefix('@') {
            OwnedSelector::Attr(stripped.to_string())
        } else {
            OwnedSelector::Child(s.to_string())
        }
    }
}

fn parse_owned_selectors<'gc>(
    activation: &mut Activation<'_, 'gc>,
    fields: Object<'gc>,
) -> Result<Vec<OwnedSelector>, Error<'gc>> {
    let storage = fields
        .as_array_storage()
        .ok_or_else(|| fail("ExportUtils: fields must be an Array"))?;
    let n = storage.length();
    let mut out = Vec::with_capacity(n);
    for j in 0..n {
        let value = storage.get(j).unwrap_or(Value::Undefined);
        if matches!(value, Value::Undefined | Value::Null) {
            out.push(OwnedSelector::Empty);
            continue;
        }
        let text = value.coerce_to_string(activation)?;
        out.push(OwnedSelector::from_str(&text.to_utf8_lossy()));
    }
    Ok(out)
}

fn owned_to_selector<'gc>(
    activation: &mut Activation<'_, 'gc>,
    owned: &OwnedSelector,
) -> Result<Selector<'gc>, Error<'gc>> {
    Ok(match owned {
        OwnedSelector::Empty => Selector::Empty,
        OwnedSelector::Node => Selector::Node,
        OwnedSelector::Child(name) => {
            let prop = AvmString::new_utf8(activation.gc(), name);
            Selector::Child {
                name: name_to_multiname(activation, prop.into(), false)?,
                prop,
            }
        }
        OwnedSelector::Attr(name) => {
            let prop = AvmString::new_utf8(activation.gc(), name);
            Selector::Attr {
                name: name_to_multiname(activation, prop.into(), true)?,
                prop,
            }
        }
    })
}

fn rebuild_selectors<'gc>(
    activation: &mut Activation<'_, 'gc>,
    owned: &[OwnedSelector],
) -> Result<Vec<Selector<'gc>>, Error<'gc>> {
    owned
        .iter()
        .map(|o| owned_to_selector(activation, o))
        .collect()
}

/// Cross-call format dispatch for [`ExportState`].
enum FormatState {
    Xlsx {
        col_align: Vec<String>,
        col_letters: Vec<String>,
        ncols: usize,
        data_start: u64,
        col_types: Vec<ColType>,
        merges: Vec<String>,
    },
    Csv {
        separator: String,
        force_text: bool,
    },
}

/// Owned, cross-call persistent export state. Stored in a thread-local map and
/// referenced by id from the AS3 handle.
struct ExportState {
    /// Output buffer (raw bytes for plain CSV, ZIP container otherwise).
    output: Vec<u8>,
    /// Fixed, pre-initialised buffer deflate compresses into, <=64 KiB at a
    /// time. Compressing straight into `output`'s spare capacity makes flate2
    /// zero-fill the whole uninit tail on every call (O(spare) -> quadratic
    /// once `output` is large); a small fixed buffer keeps each call O(produced).
    scratch: Vec<u8>,
    /// Raw deflate state for xlsx and csv+compress; `None` for plain csv.
    compress: Option<Compress>,
    crc: u32, // running !crc, complemented at finalize
    uncomp: u64,
    /// ZIP local-header offset for the streamed entry (xlsx worksheet or csv).
    header_offset: usize,
    /// Start of the deflate payload after the local header (for size patching).
    data_offset: usize,
    /// File name of the streamed entry inside the ZIP.
    entry_name: String,
    central_dir: Vec<u8>,
    file_count: u16,

    selectors: Vec<OwnedSelector>,
    rows_done: usize,
    total_rows: usize,
    chunk_size: usize,

    fmt: FormatState,
}

thread_local! {
    static EXPORT_STATES: RefCell<HashMap<u32, Box<ExportState>>> =
        RefCell::new(HashMap::new());
    static NEXT_EXPORT_ID: Cell<u32> = const { Cell::new(1) };
}

fn alloc_export_id() -> u32 {
    NEXT_EXPORT_ID.with(|c| {
        let id = c.get();
        c.set(id.wrapping_add(1).max(1));
        id
    })
}

fn map_compress<'gc>(e: CompressError) -> Error<'gc> {
    Error::rust_error(Box::new(e))
}

/// Write `bytes` into the export buffer, through the deflate stream when
/// active. CRC-32 + uncompressed counter are kept up to date for the ZIP
/// local-header / central-directory patch at finalize time.
fn emit_bytes<'gc>(state: &mut ExportState, bytes: &[u8]) -> Result<(), Error<'gc>> {
    let Some(compress) = state.compress.as_mut() else {
        state.output.extend_from_slice(bytes);
        return Ok(());
    };
    state.crc = crc32_update(state.crc, bytes);
    state.uncomp += bytes.len() as u64;
    let scratch = &mut state.scratch;
    let out = &mut state.output;
    let mut rest = bytes;
    loop {
        let before_in = compress.total_in();
        let before_out = compress.total_out();
        // `compress` (not `compress_vec`) writes into our already-initialised
        // scratch slice, so flate2 never zero-fills a giant buffer.
        compress
            .compress(rest, scratch.as_mut_slice(), FlushCompress::None)
            .map_err(map_compress)?;
        let consumed = (compress.total_in() - before_in) as usize;
        let produced = (compress.total_out() - before_out) as usize;
        out.extend_from_slice(&scratch[..produced]);
        if consumed >= rest.len() {
            break;
        }
        rest = &rest[consumed..];
        if consumed == 0 && produced == 0 {
            break; // safety: no forward progress
        }
    }
    Ok(())
}

/// Flush the deflate stream (no-op for plain csv).
fn finish_deflate<'gc>(state: &mut ExportState) -> Result<(), Error<'gc>> {
    let Some(compress) = state.compress.as_mut() else {
        return Ok(());
    };
    let scratch = &mut state.scratch;
    let out = &mut state.output;
    loop {
        let before_out = compress.total_out();
        let status = compress
            .compress(&[], scratch.as_mut_slice(), FlushCompress::Finish)
            .map_err(map_compress)?;
        let produced = (compress.total_out() - before_out) as usize;
        out.extend_from_slice(&scratch[..produced]);
        if matches!(status, Status::StreamEnd) {
            break;
        }
    }
    Ok(())
}

/// Patch the ZIP local header and append the central directory + EOCD.
/// No-op for plain csv (no ZIP container).
fn finalize_zip(state: &mut ExportState) {
    if state.compress.is_none() {
        return;
    }
    let crc = !state.crc;
    let comp_size = (state.output.len() - state.data_offset) as u32;
    patch_u32(&mut state.output, state.header_offset + 14, crc);
    patch_u32(&mut state.output, state.header_offset + 18, comp_size);
    patch_u32(
        &mut state.output,
        state.header_offset + 22,
        state.uncomp as u32,
    );
    write_central_entry(
        &mut state.central_dir,
        &state.entry_name,
        crc,
        comp_size,
        state.uncomp as u32,
        state.header_offset as u32,
    );
    state.file_count += 1;
    let cd_offset = state.output.len() as u32;
    state.output.extend_from_slice(&state.central_dir);
    let cd_size = state.output.len() as u32 - cd_offset;
    push_u32(&mut state.output, 0x0605_4b50);
    push_u16(&mut state.output, 0);
    push_u16(&mut state.output, 0);
    push_u16(&mut state.output, state.file_count);
    push_u16(&mut state.output, state.file_count);
    push_u32(&mut state.output, cd_size);
    push_u32(&mut state.output, cd_offset);
    push_u16(&mut state.output, 0);
}

// =================================================================================================
// Begin: parse options, emit static parts + preamble, return the persistent state.
// =================================================================================================

#[allow(clippy::too_many_arguments)]
fn begin_xlsx_state<'gc>(
    activation: &mut Activation<'_, 'gc>,
    options: Option<Object<'gc>>,
    header: Value<'gc>,
    fields_obj: Object<'gc>,
    selectors_owned: Vec<OwnedSelector>,
    total_rows: usize,
    chunk_size: usize,
    rows_obj: Object<'gc>,
    type_opts: TypeOpts,
) -> Result<Box<ExportState>, Error<'gc>> {
    let sheet_name = match opt_value(activation, options, "sheetName")? {
        Some(v) => v.coerce_to_string(activation)?.to_utf8_lossy().into_owned(),
        None => "Foglio1".to_string(),
    };
    let compression = match opt_value(activation, options, "compression")? {
        Some(v) => v.coerce_to_i32(activation)?.clamp(0, 9) as u32,
        None => 6,
    };

    // Grouped header from the `header` param: XML -> grouped; null/undefined ->
    // a flat header row derived from `fields` (leading `@` stripped), exactly
    // like CSV. Any other type is a caller mistake, flagged like `rows`/`fields`.
    let header_tree = match header {
        Value::Null | Value::Undefined => None,
        other => {
            let xml = other
                .as_object()
                .and_then(|o| o.as_xml_object())
                .ok_or_else(|| fail("ExportUtils: header must be an XML or null"))?;
            Some(parse_header(xml.node()))
        }
    };
    let flat_headers = if header_tree.is_none() {
        parse_field_labels(activation, fields_obj)?
    } else {
        Vec::new()
    };

    let font_family = match opt_value(activation, options, "fontFamily")? {
        Some(v) => v.coerce_to_string(activation)?.to_utf8_lossy().into_owned(),
        None => "Calibri".to_string(),
    };
    let font_size = match opt_value(activation, options, "fontSize")? {
        Some(v) => v.coerce_to_number(activation)?,
        None => 11.0,
    };
    let row_colors = match opt_value(activation, options, "rowBackgroundColors")? {
        Some(v) => {
            let obj = v.as_object();
            let pair = obj
                .as_ref()
                .and_then(|o| o.as_array_storage())
                .filter(|arr| arr.length() >= 2)
                .map(|arr| {
                    (
                        arr.get(0).unwrap_or(Value::Undefined),
                        arr.get(1).unwrap_or(Value::Undefined),
                    )
                });
            match pair {
                Some((v0, v1)) => Some((
                    v0.coerce_to_i32(activation)? as u32,
                    v1.coerce_to_i32(activation)? as u32,
                )),
                None => None,
            }
        }
        None => None,
    };
    let style_opts = StyleOpts {
        font_family,
        font_size,
        header_bg: opt_color(activation, options, "headerBackgroundColor")?,
        header_fg: opt_color(activation, options, "headerForegroundColor")?,
        row_colors,
    };

    // Header layout (cells + per-column alignment + depth).
    let (header_cells, header_depth, header_cols, col_align, col_widths) =
        match header_tree.as_deref() {
            Some(tree) => {
                let depth = tree.iter().map(node_depth).max().unwrap_or(1);
                let cols: usize = tree.iter().map(leaf_count).sum();
                let mut cells = Vec::new();
                collect_cells(tree, 1, depth, 0, &mut cells);
                cells.sort_by_key(|c| (c.row, c.col));
                let mut aligns = Vec::new();
                collect_leaf_aligns(tree, &mut aligns);
                let mut widths = Vec::new();
                collect_leaf_widths(tree, &mut widths);
                (cells, depth, cols, aligns, widths)
            }
            None if flat_headers.is_empty() => (Vec::new(), 0u32, 0usize, Vec::new(), Vec::new()),
            None => {
                let cells: Vec<HeaderCell> = flat_headers
                    .iter()
                    .enumerate()
                    .map(|(i, h)| HeaderCell {
                        row: 1,
                        col: i,
                        rowspan: 1,
                        colspan: 1,
                        label: h.clone(),
                        align: String::new(),
                    })
                    .collect();
                (
                    cells,
                    1u32,
                    flat_headers.len(),
                    vec![String::new(); flat_headers.len()],
                    vec![None; flat_headers.len()],
                )
            }
        };
    let data_start = header_depth as u64 + 1;
    let ncols = (selectors_owned.len().max(header_cols)).min(MAX_COL_INDEX + 1);
    let col_letters: Vec<String> = (0..ncols).map(col_letter).collect();

    // Per-column type inference from a sample of the rows. `detectTypes:false`
    // (handled inside infer_col_types) makes every column Text; forceText is
    // CSV-only and has no effect on xlsx.
    let col_types = {
        let scan_selectors = rebuild_selectors(activation, &selectors_owned)?;
        infer_col_types(
            activation,
            |i| read_row(rows_obj, i),
            total_rows,
            &scan_selectors,
            ncols,
            type_opts,
        )?
    };

    // <cols> block (from leaf widths).
    let mut cols_xml = String::new();
    if col_widths.iter().any(|w| w.is_some()) {
        cols_xml.push_str("<cols>");
        for (c, w) in col_widths.iter().enumerate() {
            if let Some(width) = w {
                let _ = write!(
                    cols_xml,
                    "<col min=\"{n}\" max=\"{n}\" width=\"{width}\" customWidth=\"1\"/>",
                    n = c + 1
                );
            }
        }
        cols_xml.push_str("</cols>");
    }

    // Merge ranges (for grouped headers and spanning leaves).
    let mut merges: Vec<String> = Vec::new();
    for cell in &header_cells {
        if (cell.rowspan > 1 || cell.colspan > 1) && cell.col < ncols {
            let c2 = (cell.col + cell.colspan - 1).min(ncols - 1);
            let r2 = cell.row + cell.rowspan - 1;
            merges.push(format!(
                "{}{}:{}{}",
                col_letters[cell.col], cell.row, col_letters[c2], r2
            ));
        }
    }

    // Pre-render the header rows (tile the rectangle so merged cells keep
    // their border).
    let depth = header_depth as usize;
    let mut grid: Vec<(Option<&str>, &str)> = vec![(None, ""); depth * ncols];
    for cell in &header_cells {
        if cell.col >= ncols {
            continue;
        }
        let c_end = (cell.col + cell.colspan).min(ncols);
        let r_end = (cell.row + cell.rowspan) as usize;
        for r in (cell.row as usize)..r_end.min(depth + 1) {
            for c in cell.col..c_end {
                let top_left = r == cell.row as usize && c == cell.col;
                grid[(r - 1) * ncols + c] = (
                    if top_left {
                        Some(cell.label.as_str())
                    } else {
                        None
                    },
                    cell.align.as_str(),
                );
            }
        }
    }
    let mut header_rows_xml: Vec<String> = Vec::with_capacity(depth);
    for r in 1..=header_depth {
        let mut row_xml = String::new();
        let _ = write!(row_xml, "<row r=\"{r}\">");
        for c in 0..ncols {
            let (label, align) = grid[(r as usize - 1) * ncols + c];
            row_xml.push_str("<c r=\"");
            row_xml.push_str(&col_letters[c]);
            let _ = write!(row_xml, "{r}\" s=\"{}\"", header_style(align));
            match label {
                Some(text) => {
                    row_xml.push_str(" t=\"inlineStr\"><is><t xml:space=\"preserve\">");
                    xml_escape(text, &mut row_xml);
                    row_xml.push_str("</t></is></c>");
                }
                None => row_xml.push_str("/>"),
            }
        }
        row_xml.push_str("</row>");
        header_rows_xml.push(row_xml);
    }
    let opening = sheet_open(header_depth, &cols_xml);

    // Build the ZIP container with the static OOXML parts (each compressed
    // with its own short-lived DeflateEncoder).
    let styles = build_styles(&style_opts);
    let mut output: Vec<u8> = Vec::new();
    let mut central_dir: Vec<u8> = Vec::new();
    let mut file_count: u16 = 0;
    add_file(
        &mut output,
        &mut central_dir,
        &mut file_count,
        "[Content_Types].xml",
        CONTENT_TYPES.as_bytes(),
        compression,
    )
    .map_err(map_io)?;
    add_file(
        &mut output,
        &mut central_dir,
        &mut file_count,
        "_rels/.rels",
        RELS.as_bytes(),
        compression,
    )
    .map_err(map_io)?;
    let wb = workbook_xml(&sheet_name);
    add_file(
        &mut output,
        &mut central_dir,
        &mut file_count,
        "xl/workbook.xml",
        wb.as_bytes(),
        compression,
    )
    .map_err(map_io)?;
    add_file(
        &mut output,
        &mut central_dir,
        &mut file_count,
        "xl/_rels/workbook.xml.rels",
        WB_RELS.as_bytes(),
        compression,
    )
    .map_err(map_io)?;
    add_file(
        &mut output,
        &mut central_dir,
        &mut file_count,
        "xl/styles.xml",
        styles.as_bytes(),
        compression,
    )
    .map_err(map_io)?;

    let entry_name = "xl/worksheets/sheet1.xml".to_string();
    let header_offset = output.len();
    write_local_header(&mut output, &entry_name, 0, 0, 0);
    let data_offset = output.len();

    // Pre-size the output once, up front, so it never reallocates during the
    // chunked phase. Growing it mid-export is pathologically slow: the wasm
    // allocator, fragmented by Ruffle's event loop between chunks, falls back
    // to `memory.grow`, which copies the whole linear memory.
    let est = total_rows
        .saturating_mul(ncols.max(1))
        .saturating_mul(8)
        .clamp(1 << 20, 256 << 20);
    output.reserve(est);

    let mut state = Box::new(ExportState {
        output,
        scratch: vec![0u8; 64 * 1024],
        compress: Some(Compress::new(Compression::new(compression), false)),
        crc: !0u32,
        uncomp: 0,
        header_offset,
        data_offset,
        entry_name,
        central_dir,
        file_count,
        selectors: selectors_owned,
        rows_done: 0,
        total_rows,
        chunk_size,
        fmt: FormatState::Xlsx {
            col_align,
            col_letters,
            ncols,
            data_start,
            col_types,
            merges,
        },
    });

    emit_bytes(&mut state, opening.as_bytes())?;
    for hr in &header_rows_xml {
        emit_bytes(&mut state, hr.as_bytes())?;
    }
    Ok(state)
}

#[allow(clippy::too_many_arguments)]
fn begin_csv_state<'gc>(
    activation: &mut Activation<'_, 'gc>,
    options: Option<Object<'gc>>,
    selectors_owned: Vec<OwnedSelector>,
    labels: Vec<String>,
    total_rows: usize,
    chunk_size: usize,
) -> Result<Box<ExportState>, Error<'gc>> {
    let separator = match opt_value(activation, options, "separator")? {
        Some(v) => v.coerce_to_string(activation)?.to_utf8_lossy().into_owned(),
        None => ";".to_string(),
    };
    let force_text = match opt_value(activation, options, "forceText")? {
        Some(v) => v.coerce_to_boolean(),
        None => false,
    };
    let separator_hint = match opt_value(activation, options, "separatorHint")? {
        Some(v) => v.coerce_to_boolean(),
        None => false,
    };
    let compress = match opt_value(activation, options, "compress")? {
        Some(v) => v.coerce_to_boolean(),
        None => false,
    };
    let compression = match opt_value(activation, options, "compression")? {
        Some(v) => v.coerce_to_i32(activation)?.clamp(0, 9) as u32,
        None => 6,
    };
    let sheet_name = match opt_value(activation, options, "sheetName")? {
        Some(v) => v.coerce_to_string(activation)?.to_utf8_lossy().into_owned(),
        None => "Foglio1".to_string(),
    };

    let mut output: Vec<u8> = Vec::new();
    let central_dir: Vec<u8> = Vec::new();
    let mut header_offset = 0usize;
    let mut data_offset = 0usize;
    let mut entry_name = String::new();
    let compress_state = if compress {
        entry_name = format!("{}.csv", sanitize_sheet_name(&sheet_name));
        header_offset = output.len();
        write_local_header(&mut output, &entry_name, 0, 0, 0);
        data_offset = output.len();
        Some(Compress::new(Compression::new(compression), false))
    } else {
        None
    };

    // Pre-size the output once so it never reallocates during the chunked phase
    // (see the note in begin_xlsx_state). CSV cells are wider than compressed
    // xlsx, so estimate more bytes per cell when not compressing.
    let ncols = selectors_owned.len().max(1);
    let per_row = if compress { ncols * 8 } else { ncols * 14 };
    let est = total_rows.saturating_mul(per_row).clamp(1 << 20, 256 << 20);
    output.reserve(est);

    // Pre-render the whole preamble (BOM + sep hint + header row) into a
    // single Vec<u8>, then push it through emit_bytes once the state is
    // built (avoids reborrowing `state.fmt` while calling emit_bytes).
    let mut preamble: Vec<u8> = Vec::new();
    preamble.extend_from_slice(b"\xEF\xBB\xBF");
    if separator_hint {
        preamble.extend_from_slice(b"sep=");
        preamble.extend_from_slice(separator.as_bytes());
        preamble.extend_from_slice(b"\r\n");
    }
    {
        let mut line = String::new();
        for (i, label) in labels.iter().enumerate() {
            if i > 0 {
                line.push_str(&separator);
            }
            csv_write_cell(label, &separator, false, &mut line);
        }
        line.push_str("\r\n");
        preamble.extend_from_slice(line.as_bytes());
    }

    let mut state = Box::new(ExportState {
        output,
        scratch: vec![0u8; 64 * 1024],
        compress: compress_state,
        crc: !0u32,
        uncomp: 0,
        header_offset,
        data_offset,
        entry_name,
        central_dir,
        file_count: 0,
        selectors: selectors_owned,
        rows_done: 0,
        total_rows,
        chunk_size,
        fmt: FormatState::Csv {
            separator,
            force_text,
        },
    });
    emit_bytes(&mut state, &preamble)?;
    Ok(state)
}

// =================================================================================================
// Continue: emit the next data-row chunk.
// =================================================================================================

/// Read the i-th row from `rows_obj` (Array or XMLList) and classify it.
fn read_row<'gc>(rows_obj: Object<'gc>, i: usize) -> Row<'gc> {
    if let Some(storage) = rows_obj.as_array_storage() {
        classify_value(storage.get(i).unwrap_or(Value::Undefined))
    } else if let Some(list) = rows_obj.as_xml_list_object() {
        let children = list.children();
        match children.get(i) {
            Some(entry) => match entry.read_only_node() {
                Some(node) => Row::ReadOnly(node),
                None => Row::Xml(entry.node()),
            },
            None => Row::Object(Value::Undefined),
        }
    } else {
        Row::Object(Value::Undefined)
    }
}

/// Render one xlsx data row into the reusable `row_xml` buffer. Pure helper:
/// it does not touch `ExportState`, so the caller can hold an immutable borrow
/// of `state.fmt` (col_align/col_letters by reference, zero per-row cloning)
/// while building, then emit afterwards.
#[allow(clippy::too_many_arguments)]
fn build_xlsx_row<'gc>(
    activation: &mut Activation<'_, 'gc>,
    row_xml: &mut String,
    cell_buf: &mut String,
    r_str: &mut String,
    selectors: &[Selector<'gc>],
    col_align: &[String],
    col_letters: &[String],
    ncols: usize,
    col_types: &[ColType],
    row: &Row<'gc>,
    r: u64,
    parity: usize,
) -> Result<(), Error<'gc>> {
    row_xml.clear();
    r_str.clear();
    let _ = write!(r_str, "{r}");
    row_xml.push_str("<row r=\"");
    row_xml.push_str(r_str);
    row_xml.push_str("\">");
    for (c, col_letter) in col_letters.iter().enumerate().take(ncols) {
        cell_buf.clear();
        if let Some(selector) = selectors.get(c) {
            extract_cell(activation, row, selector, cell_buf)?;
        }
        let align = col_align.get(c).map(String::as_str).unwrap_or("");
        let col_type = col_types.get(c).copied().unwrap_or(ColType::Text);
        write_data_cell(
            row_xml, col_letter, r_str, cell_buf, col_type, align, parity,
        );
    }
    row_xml.push_str("</row>");
    Ok(())
}

/// Render one CSV data row into the reusable `line` buffer.
fn build_csv_row<'gc>(
    activation: &mut Activation<'_, 'gc>,
    line: &mut String,
    cell_buf: &mut String,
    selectors: &[Selector<'gc>],
    separator: &str,
    force_text: bool,
    row: &Row<'gc>,
) -> Result<(), Error<'gc>> {
    line.clear();
    for (i, selector) in selectors.iter().enumerate() {
        if i > 0 {
            line.push_str(separator);
        }
        cell_buf.clear();
        extract_cell(activation, row, selector, cell_buf)?;
        csv_write_cell(cell_buf, separator, force_text, line);
    }
    line.push_str("\r\n");
    Ok(())
}

fn continue_export<'gc>(
    activation: &mut Activation<'_, 'gc>,
    state: &mut ExportState,
    rows_obj: Object<'gc>,
    chunk: usize,
) -> Result<(), Error<'gc>> {
    let to = (state.rows_done + chunk).min(state.total_rows);
    if to == state.rows_done {
        return Ok(());
    }
    let selectors = rebuild_selectors(activation, &state.selectors)?;

    // Buffers allocated once and reused for every row of the chunk — no
    // per-row heap allocation (this is what keeps throughput flat; the old
    // per-row String/Vec churn degraded badly under the wasm allocator).
    let mut row_xml = String::new();
    let mut cell_buf = String::new();
    let mut r_str = String::new();

    let mut i = state.rows_done;
    while i < to {
        let row = read_row(rows_obj, i);
        // Build into row_xml reading fmt fields by reference; the &state.fmt
        // borrow ends with the match, so emit_bytes(&mut state) is free after.
        match &state.fmt {
            FormatState::Xlsx {
                col_align,
                col_letters,
                ncols,
                data_start,
                col_types,
                ..
            } => {
                let r = i as u64 + *data_start;
                if r > MAX_ROW_INDEX as u64 + 1 {
                    break;
                }
                build_xlsx_row(
                    activation,
                    &mut row_xml,
                    &mut cell_buf,
                    &mut r_str,
                    &selectors,
                    col_align,
                    col_letters,
                    *ncols,
                    col_types,
                    &row,
                    r,
                    i % 2,
                )?;
            }
            FormatState::Csv {
                separator,
                force_text,
            } => {
                build_csv_row(
                    activation,
                    &mut row_xml,
                    &mut cell_buf,
                    &selectors,
                    separator,
                    *force_text,
                    &row,
                )?;
            }
        }
        emit_bytes(state, row_xml.as_bytes())?;
        i += 1;
    }
    state.rows_done = i;
    Ok(())
}

// =================================================================================================
// End: emit closing markers + ZIP central directory, return the bytes.
// =================================================================================================

fn finalize_xlsx<'gc>(state: &mut ExportState) -> Result<(), Error<'gc>> {
    emit_bytes(state, b"</sheetData>")?;
    // Build the <mergeCells> block from a local clone of the merge ranges so
    // we can call emit_bytes without aliasing state.fmt.
    let merges_xml: Option<Vec<u8>> = match &state.fmt {
        FormatState::Xlsx { merges, .. } if !merges.is_empty() => {
            let mut buf = String::new();
            let _ = write!(buf, "<mergeCells count=\"{}\">", merges.len());
            for m in merges {
                buf.push_str("<mergeCell ref=\"");
                buf.push_str(m);
                buf.push_str("\"/>");
            }
            buf.push_str("</mergeCells>");
            Some(buf.into_bytes())
        }
        _ => None,
    };
    if let Some(bytes) = &merges_xml {
        emit_bytes(state, bytes)?;
    }
    let total = state.total_rows;
    let ie_xml: Option<Vec<u8>> = match &state.fmt {
        FormatState::Xlsx {
            col_letters,
            ncols,
            data_start,
            ..
        } => {
            let s = ignored_errors_xml(col_letters, *ncols, *data_start, total);
            if s.is_empty() {
                None
            } else {
                Some(s.into_bytes())
            }
        }
        _ => None,
    };
    if let Some(bytes) = &ie_xml {
        emit_bytes(state, bytes)?;
    }
    emit_bytes(state, b"</worksheet>")?;
    finish_deflate(state)?;
    finalize_zip(state);
    Ok(())
}

fn finalize_csv<'gc>(state: &mut ExportState) -> Result<(), Error<'gc>> {
    finish_deflate(state)?;
    finalize_zip(state);
    Ok(())
}

fn end_export<'gc>(state: &mut ExportState) -> Result<(), Error<'gc>> {
    match &state.fmt {
        FormatState::Xlsx { .. } => finalize_xlsx(state),
        FormatState::Csv { .. } => finalize_csv(state),
    }
}

// =================================================================================================
// Handle plumbing (AS3 ScriptObject <-> thread-local state id)
// =================================================================================================

const HANDLE_ID_PROP: &str = "__exportId";
const HANDLE_ROWS_PROP: &str = "__rows";
const HANDLE_TOTAL_PROP: &str = "__total";
const HANDLE_DONE_PROP: &str = "__done";

fn make_handle<'gc>(
    activation: &mut Activation<'_, 'gc>,
    id: u32,
    rows: Object<'gc>,
    total: usize,
) -> Object<'gc> {
    let object = ScriptObject::new_object(activation.context);
    let mc = activation.gc();
    let id_name = AvmString::new_utf8(mc, HANDLE_ID_PROP);
    let rows_name = AvmString::new_utf8(mc, HANDLE_ROWS_PROP);
    let total_name = AvmString::new_utf8(mc, HANDLE_TOTAL_PROP);
    let done_name = AvmString::new_utf8(mc, HANDLE_DONE_PROP);
    object.set_dynamic_property(id_name, Value::from(id as i32), mc);
    object.set_dynamic_property(rows_name, Value::Object(rows), mc);
    object.set_dynamic_property(total_name, Value::from(total as i32), mc);
    object.set_dynamic_property(done_name, Value::from(0), mc);
    object
}

fn read_handle<'gc>(
    activation: &mut Activation<'_, 'gc>,
    handle: Object<'gc>,
) -> Result<(u32, Object<'gc>), Error<'gc>> {
    let mc = activation.gc();
    let id_name = AvmString::new_utf8(mc, HANDLE_ID_PROP);
    let rows_name = AvmString::new_utf8(mc, HANDLE_ROWS_PROP);
    let id_val = Value::Object(handle).get_public_property(id_name, activation)?;
    let rows_val = Value::Object(handle).get_public_property(rows_name, activation)?;
    let id = id_val.coerce_to_i32(activation)? as u32;
    let rows = rows_val
        .as_object()
        .ok_or_else(|| fail("ExportUtils: handle is missing or invalid"))?;
    Ok((id, rows))
}

fn update_handle_done<'gc>(activation: &mut Activation<'_, 'gc>, handle: Object<'gc>, done: usize) {
    let mc = activation.gc();
    let done_name = AvmString::new_utf8(mc, HANDLE_DONE_PROP);
    handle.set_dynamic_property(done_name, Value::from(done as i32), mc);
}

/// `ExportUtils.asyncExportBegin(rows, fields, header, options):Object`.
pub fn async_export_begin<'gc>(
    activation: &mut Activation<'_, 'gc>,
    _this: Value<'gc>,
    args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    // rows / fields / header are positional; the trailing options bag holds the rest.
    let rows_obj = args
        .try_get_object(0)
        .ok_or_else(|| fail("ExportUtils.asyncExportBegin: rows must be an Array or XMLList"))?;
    let total_rows = rows_len(rows_obj)
        .ok_or_else(|| fail("ExportUtils.asyncExportBegin: rows must be an Array or XMLList"))?;

    let fields_obj = args
        .try_get_object(1)
        .ok_or_else(|| fail("ExportUtils.asyncExportBegin: fields must be an Array"))?;
    let selectors_owned = parse_owned_selectors(activation, fields_obj)?;

    let header = args.get_optional(2).unwrap_or(Value::Undefined);
    let options = args.try_get_object(3);
    let format = match opt_value(activation, options, "format")? {
        Some(v) => v.coerce_to_string(activation)?.to_utf8_lossy().into_owned(),
        None => "xlsx".to_string(),
    };

    let min_chunk = match opt_value(activation, options, "minChunkSize")? {
        Some(v) => v.coerce_to_i32(activation)?.max(1) as usize,
        None => 100,
    };
    // 1% of total rows, floored at `minChunkSize` (default 100).
    let one_percent = total_rows.div_ceil(100).max(1);
    let chunk_size = one_percent.max(min_chunk);

    let state = match format.as_str() {
        "xlsx" => {
            let type_opts = read_type_opts(activation, options)?;
            begin_xlsx_state(
                activation,
                options,
                header,
                fields_obj,
                selectors_owned,
                total_rows,
                chunk_size,
                rows_obj,
                type_opts,
            )?
        }
        "csv" => {
            let labels = parse_field_labels(activation, fields_obj)?;
            begin_csv_state(
                activation,
                options,
                selectors_owned,
                labels,
                total_rows,
                chunk_size,
            )?
        }
        _ => {
            return Err(fail(
                "ExportUtils.asyncExportBegin: options.format must be \"xlsx\" or \"csv\"",
            ));
        }
    };

    let id = alloc_export_id();
    EXPORT_STATES.with(|map| map.borrow_mut().insert(id, state));
    Ok(Value::Object(make_handle(
        activation, id, rows_obj, total_rows,
    )))
}

/// `ExportUtils.asyncExportContinue(handle):int` — returns the cumulative
/// number of data rows processed so far.
pub fn async_export_continue<'gc>(
    activation: &mut Activation<'_, 'gc>,
    _this: Value<'gc>,
    args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    let handle = args
        .try_get_object(0)
        .ok_or_else(|| fail("ExportUtils.asyncExportContinue: handle is required"))?;
    let (id, rows_obj) = read_handle(activation, handle)?;

    // Take the state out of the map for the duration of the chunk (avoids
    // re-entrancy issues if AS3 calls back into the API).
    let mut state = EXPORT_STATES
        .with(|map| map.borrow_mut().remove(&id))
        .ok_or_else(|| fail("ExportUtils.asyncExportContinue: handle is no longer valid"))?;

    let chunk = state.chunk_size;
    let result = continue_export(activation, &mut state, rows_obj, chunk);
    let done = state.rows_done;

    // Put the state back regardless of result, so a later End/Cancel still
    // sees and cleans it up.
    EXPORT_STATES.with(|map| map.borrow_mut().insert(id, state));
    result?;
    update_handle_done(activation, handle, done);
    Ok(Value::from(done as i32))
}

/// `ExportUtils.asyncExportEnd(handle):ByteArray` — flushes any remaining
/// rows, finalises the output and consumes the handle.
pub fn async_export_end<'gc>(
    activation: &mut Activation<'_, 'gc>,
    _this: Value<'gc>,
    args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    let handle = args
        .try_get_object(0)
        .ok_or_else(|| fail("ExportUtils.asyncExportEnd: handle is required"))?;
    let (id, rows_obj) = read_handle(activation, handle)?;
    let mut state = EXPORT_STATES
        .with(|map| map.borrow_mut().remove(&id))
        .ok_or_else(|| fail("ExportUtils.asyncExportEnd: handle is no longer valid"))?;

    // Flush any rows the caller did not push through Continue.
    if state.rows_done < state.total_rows {
        let remaining = state.total_rows - state.rows_done;
        continue_export(activation, &mut state, rows_obj, remaining)?;
    }
    end_export(&mut state)?;
    let bytes = state.output;
    Ok(make_bytearray(activation, bytes).into())
}

/// `ExportUtils.asyncExportCancel(handle):void` — drops the state without
/// emitting anything; the handle is consumed.
pub fn async_export_cancel<'gc>(
    activation: &mut Activation<'_, 'gc>,
    _this: Value<'gc>,
    args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    let handle = args
        .try_get_object(0)
        .ok_or_else(|| fail("ExportUtils.asyncExportCancel: handle is required"))?;
    let (id, _rows) = read_handle(activation, handle)?;
    EXPORT_STATES.with(|map| {
        map.borrow_mut().remove(&id);
    });
    Ok(Value::Undefined)
}
