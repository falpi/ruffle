//! XMLReadOnly (read-only XML) builtin.

pub use crate::avm2::object::xml_read_only_allocator;

use std::cmp::Ordering;

use crate::avm2::e4x::{E4XNamespace, name_to_multiname};
use crate::avm2::e4x_read_only::E4XNodeReadOnly;
use crate::avm2::function::FunctionArgs;
use crate::avm2::object::{E4XOrXml, QNameObject, XmlListObject};
use crate::avm2::parameters::ParametersExt;
use crate::avm2::{Activation, ArrayObject, ArrayStorage, Error, Multiname, Value};
use crate::avm2_stub_method;
use crate::string::AvmString;

/// The single node backing `this`, if any.
fn node_of<'gc>(this: Value<'gc>) -> Option<E4XNodeReadOnly<'gc>> {
    this.as_object()?.as_xml_object_read_only()?.node()
}

/// Build an `XMLList` of read-only elements over the shared arena.
fn ro_list<'gc>(
    activation: &mut Activation<'_, 'gc>,
    nodes: Vec<E4XNodeReadOnly<'gc>>,
) -> Value<'gc> {
    let children = nodes.into_iter().map(E4XOrXml::ReadOnly).collect();
    XmlListObject::new_with_children(activation, children, None, None).into()
}

/// Native constructor body: parse the source string into the read-only arena.
pub fn init<'gc>(
    activation: &mut Activation<'_, 'gc>,
    this: Value<'gc>,
    args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    let this = this.as_object().unwrap().as_xml_object_read_only().unwrap();

    let source = args.get_value(0).coerce_to_string(activation)?;
    this.parse_and_set(activation, source);

    Ok(Value::Undefined)
}

pub fn to_string<'gc>(
    activation: &mut Activation<'_, 'gc>,
    this: Value<'gc>,
    _args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    let this = this.as_object().unwrap().as_xml_object_read_only().unwrap();

    Ok(AvmString::new_utf8(activation.gc(), this.to_string_value()).into())
}

pub fn to_xml_string<'gc>(
    activation: &mut Activation<'_, 'gc>,
    this: Value<'gc>,
    _args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    let mut out = String::new();
    if let Some(node) = node_of(this) {
        node.write_xml_string(&mut out);
    }
    Ok(AvmString::new_utf8(activation.gc(), out).into())
}

pub fn length<'gc>(
    _activation: &mut Activation<'_, 'gc>,
    _this: Value<'gc>,
    _args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    // A single XML object always has length 1 (mirrors XML.length()).
    Ok(Value::Integer(1))
}

pub fn local_name<'gc>(
    activation: &mut Activation<'_, 'gc>,
    this: Value<'gc>,
    _args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    Ok(node_of(this)
        .and_then(|n| n.local_name(activation.gc()))
        .map_or(Value::Null, Value::String))
}

pub fn name<'gc>(
    activation: &mut Activation<'_, 'gc>,
    this: Value<'gc>,
    _args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    let Some(node) = node_of(this) else {
        return Ok(Value::Null);
    };
    let Some(local) = node.local_name(activation.gc()) else {
        return Ok(Value::Null);
    };

    // Resolve the node's namespace (mirrors XML.name / [[GetNamespace]]).
    let mc = activation.gc();
    let in_scope = node.in_scope_namespaces(mc);
    let e4x_ns = match node.namespace(mc) {
        None => E4XNamespace::default_namespace(activation.strings()),
        Some(n) => in_scope
            .iter()
            .find(|s| s.uri == n.uri)
            .copied()
            .unwrap_or_else(|| E4XNamespace::new_uri(n.uri)),
    };
    let namespace = e4x_ns.as_namespace_object(activation)?.namespace();
    let mut multiname = Multiname::new(namespace, local);
    multiname.set_is_attribute(node.is_attribute());
    Ok(QNameObject::from_name(activation, multiname).into())
}

/// Backs `XMLReadOnly.namespace([prefix])`. `namespace` is an AS3 keyword, so
/// the public method is an AS3-side wrapper that forwards to this native impl
/// (same shape as `XML.namespace_internal_impl`): the node's namespace, or the
/// in-scope namespace bound to `prefix` when one was supplied.
pub fn namespace_internal_impl<'gc>(
    activation: &mut Activation<'_, 'gc>,
    this: Value<'gc>,
    args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    let Some(node) = node_of(this) else {
        return Ok(Value::Null);
    };
    let mc = activation.gc();
    let in_scope = node.in_scope_namespaces(mc);

    let has_prefix = args.get_bool(0);
    if !has_prefix {
        // text/comment/processing-instruction nodes have no namespace.
        if node.is_text() || node.is_comment() || node.is_processing_instruction() {
            return Ok(Value::Null);
        }
        let ns = match node.namespace(mc) {
            None => E4XNamespace::default_namespace(activation.strings()),
            Some(n) => in_scope
                .iter()
                .find(|s| s.uri == n.uri)
                .copied()
                .unwrap_or_else(|| E4XNamespace::new_uri(n.uri)),
        };
        Ok(ns.as_namespace_object(activation)?.into())
    } else {
        let prefix = args.get_string(activation, 1);
        match in_scope.iter().find(|ns| ns.prefix == Some(prefix)) {
            Some(ns) => Ok(ns.as_namespace_object(activation)?.into()),
            None => Ok(Value::Undefined),
        }
    }
}

/// `XMLReadOnly.inScopeNamespaces()` — all namespaces in scope at this node.
pub fn in_scope_namespaces<'gc>(
    activation: &mut Activation<'_, 'gc>,
    this: Value<'gc>,
    _args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    let mc = activation.gc();
    let mut out: Vec<Value<'gc>> = Vec::new();
    if let Some(node) = node_of(this) {
        for ns in node.in_scope_namespaces(mc) {
            out.push(ns.as_namespace_object(activation)?.into());
        }
    }
    // Non-standard avmplus behavior: never return an empty array.
    if out.is_empty() {
        out.push(
            E4XNamespace::default_namespace(activation.strings())
                .as_namespace_object(activation)?
                .into(),
        );
    }
    Ok(ArrayObject::from_storage(activation.context, ArrayStorage::from_iter(out)).into())
}

/// `XMLReadOnly.namespaceDeclarations()` — namespaces declared *on* this node
/// (i.e. not already in scope via an ancestor).
pub fn namespace_declarations<'gc>(
    activation: &mut Activation<'_, 'gc>,
    this: Value<'gc>,
    _args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    let Some(node) = node_of(this) else {
        return Ok(ArrayObject::empty(activation.context).into());
    };
    if !node.is_element() {
        return Ok(ArrayObject::empty(activation.context).into());
    }
    let mc = activation.gc();
    let ancestor = node
        .parent()
        .map(|p| p.in_scope_namespaces(mc))
        .unwrap_or_default();
    let in_scope = node.in_scope_namespaces(mc);

    let mut out: Vec<Value<'gc>> = Vec::new();
    for ns in in_scope {
        if !ancestor.contains(&ns) {
            out.push(ns.as_namespace_object(activation)?.into());
        }
    }
    Ok(ArrayObject::from_storage(activation.context, ArrayStorage::from_iter(out)).into())
}

/// `XMLReadOnly.comments()` — comment-node children.
pub fn comments<'gc>(
    activation: &mut Activation<'_, 'gc>,
    this: Value<'gc>,
    _args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    let mut out = Vec::new();
    if let Some(node) = node_of(this) {
        node.comment_children(&mut out);
    }
    Ok(ro_list(activation, out))
}

/// `XMLReadOnly.processingInstructions([name])` — PI children, optionally
/// filtered by target name.
pub fn processing_instructions<'gc>(
    activation: &mut Activation<'_, 'gc>,
    this: Value<'gc>,
    args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    let want = match args.get_value(0) {
        Value::Undefined => None,
        v => Some(v.coerce_to_string(activation)?),
    };
    let want = want.map(|s| s.to_utf8_lossy().into_owned());
    let mut out = Vec::new();
    if let Some(node) = node_of(this) {
        node.pi_children(want.as_deref(), &mut out);
    }
    Ok(ro_list(activation, out))
}

pub fn node_kind<'gc>(
    activation: &mut Activation<'_, 'gc>,
    this: Value<'gc>,
    _args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    let kind = node_of(this).map_or("text", |n| n.node_kind_str());
    Ok(AvmString::new_utf8(activation.gc(), kind).into())
}

pub fn has_simple_content<'gc>(
    _activation: &mut Activation<'_, 'gc>,
    this: Value<'gc>,
    _args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    Ok(node_of(this).is_none_or(|n| n.has_simple_content()).into())
}

pub fn has_complex_content<'gc>(
    _activation: &mut Activation<'_, 'gc>,
    this: Value<'gc>,
    _args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    Ok(node_of(this)
        .is_some_and(|n| n.has_complex_content())
        .into())
}

pub fn parent<'gc>(
    activation: &mut Activation<'_, 'gc>,
    this: Value<'gc>,
    _args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    use crate::avm2::object::XmlObjectReadOnly;
    Ok(node_of(this)
        .and_then(|n| n.parent())
        .map_or(Value::Undefined, |p| {
            XmlObjectReadOnly::new(activation, p).into()
        }))
}

pub fn child<'gc>(
    activation: &mut Activation<'_, 'gc>,
    this: Value<'gc>,
    args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    let multiname = name_to_multiname(activation, args.get_value(0), false)?;
    let mut out = Vec::new();
    if let Some(node) = node_of(this) {
        node.children_matching(&multiname, &mut out);
    }
    Ok(ro_list(activation, out))
}

pub fn children<'gc>(
    activation: &mut Activation<'_, 'gc>,
    this: Value<'gc>,
    _args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    let mut out = Vec::new();
    if let Some(node) = node_of(this) {
        node.all_children(&mut out);
    }
    Ok(ro_list(activation, out))
}

pub fn elements<'gc>(
    activation: &mut Activation<'_, 'gc>,
    this: Value<'gc>,
    args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    let multiname = name_to_multiname(activation, args.get_value(0), false)?;
    let mut out = Vec::new();
    if let Some(node) = node_of(this) {
        node.children_matching(&multiname, &mut out);
    }
    Ok(ro_list(activation, out))
}

pub fn attribute<'gc>(
    activation: &mut Activation<'_, 'gc>,
    this: Value<'gc>,
    args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    let multiname = name_to_multiname(activation, args.get_value(0), true)?;
    let mut out = Vec::new();
    if let Some(node) = node_of(this) {
        node.attributes_matching(&multiname, &mut out);
    }
    Ok(ro_list(activation, out))
}

pub fn attributes<'gc>(
    activation: &mut Activation<'_, 'gc>,
    this: Value<'gc>,
    _args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    let mut out = Vec::new();
    if let Some(node) = node_of(this) {
        node.all_attributes(&mut out);
    }
    Ok(ro_list(activation, out))
}

pub fn descendants<'gc>(
    activation: &mut Activation<'_, 'gc>,
    this: Value<'gc>,
    args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    let multiname = name_to_multiname(activation, args.get_value(0), false)?;
    let mut out = Vec::new();
    if let Some(node) = node_of(this) {
        node.descendants_matching(&multiname, &mut out);
    }
    Ok(ro_list(activation, out))
}

pub fn text<'gc>(
    activation: &mut Activation<'_, 'gc>,
    this: Value<'gc>,
    _args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    let mut out = Vec::new();
    if let Some(node) = node_of(this) {
        node.text_children(&mut out);
    }
    Ok(ro_list(activation, out))
}

pub fn copy<'gc>(
    _activation: &mut Activation<'_, 'gc>,
    this: Value<'gc>,
    _args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    // Read-only and immutable: a copy is indistinguishable from the original.
    Ok(this)
}

/// A pre-extracted sort key. Strings and numbers compare in O(1) with no AS3
/// callback and no per-cell XMLList allocation.
enum SortKey {
    Str(String),
    Num(f64),
}

/// Column kinds, kept in sync with the `KIND_*` constants in
/// `com.terna.collections.KeyedSort`.
const KIND_NUMERIC: i32 = 1;
const KIND_LOWERCASE: i32 = 2;

impl SortKey {
    fn from_text(text: &str, kind: i32) -> Self {
        match kind {
            KIND_NUMERIC => SortKey::Num(text.trim().parse::<f64>().unwrap_or(f64::NAN)),
            KIND_LOWERCASE => SortKey::Str(text.to_lowercase()),
            _ => SortKey::Str(text.to_owned()),
        }
    }

    fn cmp(&self, other: &SortKey) -> Ordering {
        match (self, other) {
            (SortKey::Str(a), SortKey::Str(b)) => a.cmp(b),
            // Total order with NaN last (consistent across passes).
            (SortKey::Num(a), SortKey::Num(b)) => match (a.is_nan(), b.is_nan()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                _ => a.partial_cmp(b).unwrap(),
            },
            // Mixed kinds never occur (kind is per-column), but stay total.
            (SortKey::Str(_), SortKey::Num(_)) => Ordering::Less,
            (SortKey::Num(_), SortKey::Str(_)) => Ordering::Greater,
        }
    }
}

/// One sort column: how to read the key from a node, plus its kind and order.
struct SortColumn<'gc> {
    /// `None` => the node's own text; `Some` => a child element (or attribute)
    /// multiname.
    name: Option<Multiname<'gc>>,
    is_attr: bool,
    kind: i32,
    descending: bool,
}

/// `XMLReadOnly.sortKeyed(items, fields, kinds, descending)` — static fast path
/// for `KeyedSort` when the sorted rows are `XMLReadOnly`.
///
/// Extracts each column's key straight from the read-only arena (no XMLList
/// wrappers), sorts an index permutation with the Rust stdlib sort (a pattern-
/// defeating, adaptive sort — no quicksort blow-up on few-distinct columns, and
/// near-linear on already-ordered input), and permutes `items` in place. The
/// comparison runs entirely in Rust: no per-pair AS3 callback.
///
/// `fields[j]` selects the key: `""` = the node itself, `"@n"` = attribute `n`,
/// `"n"` = child element `n`. `kinds[j]`: 0 string, 1 numeric, 2 lowercased
/// string. Ties break on the original index (stable order).
///
/// Returns `true` once sorted; `false` if the rows aren't all `XMLReadOnly`, so
/// the caller can fall back to its ActionScript path.
pub fn sort_keyed<'gc>(
    activation: &mut Activation<'_, 'gc>,
    _this: Value<'gc>,
    args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    let items = args.get_object(activation, 0, "items")?;
    let fields = args.get_object(activation, 1, "fields")?;
    let kinds = args.get_object(activation, 2, "kinds")?;
    let descending = args.get_object(activation, 3, "descending")?;

    let Some(n) = items.as_array_storage().map(|s| s.length()) else {
        return Ok(false.into());
    };
    if n <= 1 {
        return Ok(true.into());
    }

    // Resolve the per-column specs once.
    let k = match (
        fields.as_array_storage().map(|s| s.length()),
        kinds.as_array_storage().map(|s| s.length()),
        descending.as_array_storage().map(|s| s.length()),
    ) {
        (Some(f), Some(ki), Some(d)) if f == ki && ki == d && f > 0 => f,
        _ => return Ok(false.into()),
    };

    let mut columns: Vec<SortColumn<'gc>> = Vec::with_capacity(k);
    for j in 0..k {
        let field_val = fields
            .as_array_storage()
            .unwrap()
            .get(j)
            .unwrap_or(Value::Undefined);
        let field_str = field_val.coerce_to_string(activation)?;
        let kind = kinds
            .as_array_storage()
            .unwrap()
            .get(j)
            .unwrap_or(Value::Undefined)
            .coerce_to_i32(activation)?;
        let desc = descending
            .as_array_storage()
            .unwrap()
            .get(j)
            .unwrap_or(Value::Undefined)
            .coerce_to_boolean();

        let utf8 = field_str.to_utf8_lossy();
        let (name, is_attr) = if utf8.is_empty() {
            (None, false)
        } else if let Some(attr) = utf8.strip_prefix('@') {
            let attr = AvmString::new_utf8(activation.gc(), attr);
            (
                Some(name_to_multiname(activation, attr.into(), true)?),
                true,
            )
        } else {
            (
                Some(name_to_multiname(activation, field_val, false)?),
                false,
            )
        };

        columns.push(SortColumn {
            name,
            is_attr,
            kind,
            descending: desc,
        });
    }

    // Snapshot the nodes and bail (false) if any row isn't read-only XML.
    let mut nodes: Vec<E4XNodeReadOnly<'gc>> = Vec::with_capacity(n);
    let mut values: Vec<Value<'gc>> = Vec::with_capacity(n);
    {
        let storage = items.as_array_storage().unwrap();
        for i in 0..n {
            let v = storage.get(i).unwrap_or(Value::Undefined);
            match v
                .as_object()
                .and_then(|o| o.as_xml_object_read_only())
                .and_then(|ro| ro.node())
            {
                Some(node) => {
                    nodes.push(node);
                    values.push(v);
                }
                None => return Ok(false.into()),
            }
        }
    }

    // Extract a dense key matrix: keys[i * k + j].
    let mut keys: Vec<SortKey> = Vec::with_capacity(n * k);
    let mut buf = String::new();
    for node in &nodes {
        for col in &columns {
            buf.clear();
            match &col.name {
                None => node.append_text(&mut buf),
                Some(name) if col.is_attr => node.append_attrs_text(name, &mut buf),
                Some(name) => node.append_children_text(name, &mut buf),
            }
            keys.push(SortKey::from_text(&buf, col.kind));
        }
    }

    // Sort an index permutation; the final tie-break on the original index keeps
    // the sort stable and keeps every composite key distinct.
    let mut order: Vec<u32> = (0..n as u32).collect();
    order.sort_unstable_by(|&a, &b| {
        let (a, b) = (a as usize, b as usize);
        for (j, col) in columns.iter().enumerate() {
            let ord = keys[a * k + j].cmp(&keys[b * k + j]);
            if ord != Ordering::Equal {
                return if col.descending { ord.reverse() } else { ord };
            }
        }
        a.cmp(&b)
    });

    // Permute the array in place.
    let mut storage = items.as_array_storage_mut(activation.gc()).unwrap();
    for (new_index, &old) in order.iter().enumerate() {
        storage.set(new_index, values[old as usize]);
    }

    Ok(true.into())
}

/// `XMLReadOnly.contains(value)` — deep equality against `value` (E4X 13.4.4.8).
pub fn contains<'gc>(
    _activation: &mut Activation<'_, 'gc>,
    this: Value<'gc>,
    args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    let value = args.get_value(0);
    if let (Some(a), Some(b)) = (
        node_of(this),
        value
            .as_object()
            .and_then(|o| o.as_xml_object_read_only())
            .and_then(|ro| ro.node()),
    ) {
        return Ok(a.deep_equals(&b).into());
    }
    Ok(false.into())
}

/// `XMLReadOnly.childIndex()` — ordinal among the parent's children, or -1.
pub fn child_index<'gc>(
    _activation: &mut Activation<'_, 'gc>,
    this: Value<'gc>,
    _args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    Ok(node_of(this)
        .and_then(|n| n.child_index())
        .map_or(Value::Integer(-1), |i| Value::Integer(i as i32)))
}

// ---------------------------------------------------------------------------
// Mutation API — unsupported on a read-only document. Implemented as stubs
// (emit the standard stub warning) rather than omitted, so that SDK code which
// reaches them via the `is XML` masquerade gets a visible WARN + a benign
// no-op instead of a "method not found" error. XML-returning mutators return
// `this` unchanged; void mutators return `undefined`.
// ---------------------------------------------------------------------------

macro_rules! ro_mutation_stub {
    // XML-returning mutator: return `this` unchanged.
    ($fn_name:ident, $as_name:literal, this) => {
        pub fn $fn_name<'gc>(
            activation: &mut Activation<'_, 'gc>,
            this: Value<'gc>,
            _args: FunctionArgs<'_, 'gc>,
        ) -> Result<Value<'gc>, Error<'gc>> {
            avm2_stub_method!(activation, "XMLReadOnly", $as_name);
            Ok(this)
        }
    };
    // void mutator: return `undefined`.
    ($fn_name:ident, $as_name:literal, undefined) => {
        pub fn $fn_name<'gc>(
            activation: &mut Activation<'_, 'gc>,
            _this: Value<'gc>,
            _args: FunctionArgs<'_, 'gc>,
        ) -> Result<Value<'gc>, Error<'gc>> {
            avm2_stub_method!(activation, "XMLReadOnly", $as_name);
            Ok(Value::Undefined)
        }
    };
}

ro_mutation_stub!(add_namespace, "addNamespace", this);
ro_mutation_stub!(append_child, "appendChild", this);
ro_mutation_stub!(prepend_child, "prependChild", this);
ro_mutation_stub!(insert_child_after, "insertChildAfter", undefined);
ro_mutation_stub!(insert_child_before, "insertChildBefore", undefined);
ro_mutation_stub!(normalize, "normalize", this);
ro_mutation_stub!(remove_namespace, "removeNamespace", this);
ro_mutation_stub!(replace, "replace", this);
ro_mutation_stub!(set_children, "setChildren", this);
ro_mutation_stub!(set_local_name, "setLocalName", undefined);
ro_mutation_stub!(set_name, "setName", undefined);
ro_mutation_stub!(set_namespace, "setNamespace", undefined);

/// `XMLReadOnly.notification()` — the stored change-notification function.
pub fn notification<'gc>(
    _activation: &mut Activation<'_, 'gc>,
    this: Value<'gc>,
    _args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    let ro = this.as_object().unwrap().as_xml_object_read_only().unwrap();
    Ok(ro.notification().map_or(Value::Null, |f| f.into()))
}

/// `XMLReadOnly.setNotification(f)` — read-only XML never *fires* change
/// notifications, so this is a stub (emits the standard stub warning). We do,
/// however, store the function: Flex's `UIDUtil.getUID` stamps a stable UID onto
/// it for XML-typed items, and the wrapper object is identity-stable.
pub fn set_notification<'gc>(
    activation: &mut Activation<'_, 'gc>,
    this: Value<'gc>,
    args: FunctionArgs<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>> {
    avm2_stub_method!(activation, "XMLReadOnly", "setNotification");
    let ro = this.as_object().unwrap().as_xml_object_read_only().unwrap();
    let func = args.try_get_function(0);
    ro.set_notification(func, activation.gc());
    Ok(Value::Undefined)
}
