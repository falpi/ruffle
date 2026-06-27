//! Read-only XML object (`XMLReadOnly`) — the single-node, immutable twin of
//! [`XmlObject`](super::xml_object::XmlObject).
//!
//! It wraps **one** [`E4XNodeReadOnly`] handle (`{ store, index }`) into the
//! shared arena. Navigation (`.child`, `..`, `@attr`) returns a real
//! [`XmlListObject`] whose elements are [`E4XOrXml::ReadOnly`] — so `is XMLList`
//! stays true and the result feeds straight into `XMLListCollection`.

use core::fmt;

use gc_arena::barrier::unlock;
use gc_arena::lock::{Lock, RefLock};
use gc_arena::{Collect, Gc, GcWeak};
use ruffle_common::utils::HasPrefixField;

use crate::avm2::e4x::string_to_multiname;
use crate::avm2::e4x_read_only::{E4XNodeReadOnly, E4XStoreReadOnly, RespStreamBuilder};
use crate::avm2::object::script_object::ScriptObjectData;
use crate::avm2::object::xml_list_object::E4XOrXml;
use crate::avm2::object::{ClassObject, FunctionObject, Object, TObject, XmlListObject};
use crate::avm2::value::Value;
use crate::avm2::{Activation, Error, Multiname};
use crate::avm2_stub_method;
use crate::string::AvmString;

/// A class instance allocator that allocates empty read-only XML objects.
/// The node is filled in later by the native `init`.
pub fn xml_read_only_allocator<'gc>(
    class: ClassObject<'gc>,
    activation: &mut Activation<'_, 'gc>,
) -> Result<Object<'gc>, Error<'gc>> {
    Ok(XmlObjectReadOnly(Gc::new(
        activation.gc(),
        XmlObjectReadOnlyData {
            base: ScriptObjectData::new(class),
            node: Lock::new(None),
            notification: Lock::new(None),
            build: RefLock::new(None),
        },
    ))
    .into())
}

#[derive(Clone, Collect, Copy)]
#[collect(no_drop)]
pub struct XmlObjectReadOnly<'gc>(pub Gc<'gc, XmlObjectReadOnlyData<'gc>>);

#[derive(Clone, Collect, Copy, Debug)]
#[collect(no_drop)]
pub struct XmlObjectReadOnlyWeak<'gc>(pub GcWeak<'gc, XmlObjectReadOnlyData<'gc>>);

impl fmt::Debug for XmlObjectReadOnly<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("XmlObjectReadOnly")
            .field("ptr", &Gc::as_ptr(self.0))
            .finish()
    }
}

#[derive(Collect, HasPrefixField)]
#[collect(no_drop)]
#[repr(C, align(8))]
pub struct XmlObjectReadOnlyData<'gc> {
    /// Base script object — must stay first for `HasPrefixField`.
    base: ScriptObjectData<'gc>,

    /// The single node this object points at, or `None` before `init` runs.
    node: Lock<Option<E4XNodeReadOnly<'gc>>>,

    /// The change-notification function set via `setNotification`. Read-only XML
    /// never *fires* notifications, but Flex's `UIDUtil.getUID` stamps a stable
    /// UID onto this function for XML-typed items — so we store it (the wrapper
    /// object is identity-stable, so the UID stays stable across calls).
    notification: Lock<Option<FunctionObject<'gc>>>,

    /// In-progress streaming RESP build (`fromBinaryBegin`/`Feed`/`End`), or
    /// `None` for a normal node-backed object. Boxed so an idle (non-building)
    /// object pays only one null pointer.
    build: RefLock<Option<Box<RespStreamBuilder>>>,
}

impl<'gc> XmlObjectReadOnly<'gc> {
    /// Wrap an existing node handle (e.g. an element produced by navigation).
    pub fn new(activation: &mut Activation<'_, 'gc>, node: E4XNodeReadOnly<'gc>) -> Self {
        let class = activation.avm2().classes().xml_read_only;
        XmlObjectReadOnly(Gc::new(
            activation.gc(),
            XmlObjectReadOnlyData {
                base: ScriptObjectData::new(class),
                node: Lock::new(Some(node)),
                notification: Lock::new(None),
                build: RefLock::new(None),
            },
        ))
    }

    pub fn node(self) -> Option<E4XNodeReadOnly<'gc>> {
        self.0.node.get()
    }

    pub fn notification(self) -> Option<FunctionObject<'gc>> {
        self.0.notification.get()
    }

    pub fn set_notification(self, func: Option<FunctionObject<'gc>>, mc: &gc_arena::Mutation<'gc>) {
        unlock!(Gc::write(mc, self.0), XmlObjectReadOnlyData, notification).set(func);
    }

    /// Parse `source` and point this object at the document root. Called once
    /// from the native `init` during construction.
    pub fn parse_and_set(&self, activation: &mut Activation<'_, 'gc>, source: AvmString<'gc>) {
        // Honour the same global XML settings that `new XML()` defaults to.
        let settings = &activation.avm2().xml_settings;
        let (ignore_white, ignore_comments, ignore_pi) = (
            settings.ignore_whitespace,
            settings.ignore_comments,
            settings.ignore_processing_instructions,
        );
        let store = E4XStoreReadOnly::parse(
            &source.to_utf8_lossy(),
            ignore_white,
            ignore_comments,
            ignore_pi,
        );
        let mc = activation.gc();
        let store = Gc::new(mc, store);
        let root = store
            .roots()
            .first()
            .map(|&idx| E4XNodeReadOnly::new(store, idx));

        unlock!(Gc::write(mc, self.0), XmlObjectReadOnlyData, node).set(root);
    }

    /// Create an `XMLReadOnly` in streaming-build mode: it carries an
    /// [`RespStreamBuilder`] and has no node yet. `fromBinaryFeed` appends raw
    /// response chunks; `fromBinaryEnd` freezes the arena and points the object
    /// at the reconstructed envelope root.
    pub fn new_building(activation: &mut Activation<'_, 'gc>) -> Self {
        let class = activation.avm2().classes().xml_read_only;
        XmlObjectReadOnly(Gc::new(
            activation.gc(),
            XmlObjectReadOnlyData {
                base: ScriptObjectData::new(class),
                node: Lock::new(None),
                notification: Lock::new(None),
                build: RefLock::new(Some(Box::new(RespStreamBuilder::new()))),
            },
        ))
    }

    /// Append a raw response chunk to the in-progress streaming build (codec
    /// flag + possibly deflate-compressed RESP; inflated and row-parsed in
    /// Rust). No-op once finalised or if the object isn't in build mode.
    pub fn feed_binary(self, bytes: &[u8], mc: &gc_arena::Mutation<'gc>) {
        if let Some(builder) = unlock!(Gc::write(mc, self.0), XmlObjectReadOnlyData, build)
            .borrow_mut()
            .as_mut()
        {
            builder.feed(bytes);
        }
    }

    /// Finalise the streaming build: freeze the arena and point this object at
    /// the reconstructed SOAP envelope root. No-op if not in build mode.
    pub fn finish_binary(self, activation: &mut Activation<'_, 'gc>) {
        let mc = activation.gc();
        let builder = unlock!(Gc::write(mc, self.0), XmlObjectReadOnlyData, build)
            .borrow_mut()
            .take();
        let Some(builder) = builder else {
            return;
        };
        let store = Gc::new(mc, builder.finish());
        let root = store
            .roots()
            .first()
            .map(|&idx| E4XNodeReadOnly::new(store, idx));
        unlock!(Gc::write(mc, self.0), XmlObjectReadOnlyData, node).set(root);
    }

    /// Build an `XMLList` of read-only elements over the shared arena.
    fn list_from(
        activation: &mut Activation<'_, 'gc>,
        nodes: Vec<E4XNodeReadOnly<'gc>>,
    ) -> XmlListObject<'gc> {
        let children = nodes.into_iter().map(E4XOrXml::ReadOnly).collect();
        XmlListObject::new_with_children(activation, children, None, None)
    }

    /// Concatenated simple-content text of this node.
    pub fn to_string_value(self) -> String {
        match self.node() {
            Some(node) => node.string_value(),
            None => String::new(),
        }
    }
}

impl<'gc> TObject<'gc> for XmlObjectReadOnly<'gc> {
    fn gc_base(&self) -> Gc<'gc, ScriptObjectData<'gc>> {
        HasPrefixField::as_prefix_gc(self.0)
    }

    /// Read-only XML ignores writes. The `is XML` masquerade routes Flex SDK
    /// mutators here — e.g. `DefaultDataDescriptor.setToggled`/`setEnabled` do
    /// `node.@toggled = value` in an XML branch *without* a try/catch, so
    /// throwing (Error #1056) would break the caller (the menu's
    /// `mouseUpHandler` aborts → task not launched, selection corrupted).
    /// Ignoring the write matches read-only semantics and keeps the SDK happy;
    /// the stub warning surfaces it once (the `setProperty`/`@attr` operator
    /// path, unlike the named mutator methods, has no other place to flag it).
    fn set_property_local(
        self,
        _name: &Multiname<'gc>,
        _value: Value<'gc>,
        activation: &mut Activation<'_, 'gc>,
    ) -> Result<(), Error<'gc>> {
        avm2_stub_method!(
            activation,
            "XMLReadOnly",
            "setProperty",
            "read-only: write ignored"
        );
        Ok(())
    }

    fn get_property_local(
        self,
        name: &Multiname<'gc>,
        activation: &mut Activation<'_, 'gc>,
    ) -> Result<Value<'gc>, Error<'gc>> {
        // Numeric index: index 0 is this single node, anything else is empty.
        if !name.has_explicit_namespace()
            && let Some(local_name) = name.local_name()
            && let Ok(index) = local_name.parse::<usize>()
        {
            return Ok(if index == 0 {
                self.into()
            } else {
                Value::Undefined
            });
        }

        // Re-parse string keys that carry an '@' prefix, e.g. `x["@attr"]`
        // (the form DataGrid `dataField="@attr"` and some binding watchers use).
        // `string_to_multiname` turns "@x" into an attribute multiname and "*"
        // into any-name, matching E4X bracket-access semantics. The attribute
        // *operator* (`x.@a`) already arrives with the attribute flag set, so
        // we only re-parse when it's not already an attribute multiname.
        // NOTE: must NOT run when the name already carries an explicit namespace
        // (e.g. the `ns::item` operator) — reparsing from the local name alone
        // would drop the namespace. Mirrors `e4x::handle_input_multiname`.
        let reparsed;
        let name: &Multiname<'gc> = if !name.is_attribute()
            && !name.is_any_name()
            && !name.is_any_namespace()
            && !name.has_explicit_namespace()
            && let Some(local) = name.local_name()
        {
            reparsed = string_to_multiname(activation, local);
            &reparsed
        } else {
            name
        };

        let Some(node) = self.node() else {
            return Ok(Self::list_from(activation, Vec::new()).into());
        };

        let mut out = Vec::new();
        if name.is_attribute() {
            node.attributes_matching(name, &mut out);
        } else {
            node.children_matching(name, &mut out);
        }

        // Internal dynamic properties (e.g. the `mx_internal_uid` that Flex's
        // `UIDUtil.getUID` stamps onto items): when the E4X lookup yields
        // nothing, fall back to a plain dynamic property on the base object.
        // A real `XML` doesn't need this because `getUID` routes XML through
        // its notification branch, but `XMLReadOnly` is not `is XML` and lands
        // in the generic-object branch, which reads/writes a `uid` dynamic
        // prop. Without this the read-back returns an empty XMLList and the
        // grid regenerates a fresh UID per call (breaking rollover/selection
        // highlight on AdvancedDataGrid). Guarded to non-attribute names with
        // no E4X match, so real children/attributes always take precedence.
        if out.is_empty()
            && !name.is_attribute()
            && let Some(local) = name.local_name()
            && let Some(value) = self.get_dynamic_property(local)
        {
            return Ok(value);
        }

        Ok(Self::list_from(activation, out).into())
    }

    fn xml_descendants(
        &self,
        activation: &mut Activation<'_, 'gc>,
        multiname: &Multiname<'gc>,
    ) -> Option<XmlListObject<'gc>> {
        let mut out = Vec::new();
        if let Some(node) = self.node() {
            node.descendants_matching(multiname, &mut out);
        }
        Some(Self::list_from(activation, out))
    }

    /// Needed for E4X filter predicate scope resolution (`x.(@attr == ...)`):
    /// `findproperty` calls `has_own_property` on the `with`-scope element.
    fn has_own_property(self, name: &Multiname<'gc>) -> bool {
        let Some(node) = self.node() else {
            return false;
        };

        let mut out = Vec::new();
        if name.is_attribute() {
            node.attributes_matching(name, &mut out);
        } else {
            node.children_matching(name, &mut out);
        }
        if !out.is_empty() {
            return true;
        }

        // See `get_property_local`: internal dynamic props (`mx_internal_uid`)
        // must be visible to `("uid" in item)` so `getUID` returns a stable id.
        self.base().has_own_dynamic_property(name)
    }

    fn get_next_enumerant(
        self,
        last_index: u32,
        _activation: &mut Activation<'_, 'gc>,
    ) -> Result<u32, Error<'gc>> {
        // A single XML enumerates to itself exactly once.
        Ok(if last_index == 0 && self.node().is_some() {
            1
        } else {
            0
        })
    }

    fn get_enumerant_value(
        self,
        index: u32,
        _activation: &mut Activation<'_, 'gc>,
    ) -> Result<Value<'gc>, Error<'gc>> {
        Ok(if index == 1 {
            self.into()
        } else {
            Value::Undefined
        })
    }
}
