//! Read-only E4X node model — the compact, immutable counterpart to [`e4x`](super::e4x).
//!
//! Where the mutable side scatters one `Gc<E4XNodeData>` per element/attribute/
//! text (to support mutation and Flex `COLLECTION_CHANGE` notification), the
//! read-only side parses a whole document **once** into a single
//! [`E4XStoreReadOnly`], held behind **one** `Gc`. Because a read-only tree can
//! never contain cycles, the store owns plain Rust data (no `Gc` pointers
//! inside) and is a GC leaf (`#[collect(require_static)]`) — traced in O(1)
//! instead of O(nodes).
//!
//! ## Chunked arenas (never copied on growth)
//!
//! Both the fixed-size node records and the variable-length string bytes live in
//! **segmented arenas** — a `Vec` of fixed-size `Box<[_]>` extents:
//!
//! * **Nodes**: `Vec<Box<[E4XNodeDataReadOnly]>>`, [`NODE_CHUNK_LEN`] records per
//!   extent. A node is addressed by a global `u32` index → `(idx >> SHIFT, idx &
//!   MASK)`. Records are `Copy`, so reads return a value (no borrow).
//! * **Strings**: `Vec<Box<[u8]>>`, [`STR_CHUNK_SIZE`]-byte extents holding
//!   length-prefixed `[len: u32][utf8]` entries packed back-to-back. A string is
//!   addressed by a `u32` reference packed as `(chunk << STR_CHUNK_SHIFT) |
//!   local_offset`, and never straddles an extent.
//!
//! Growing an arena only ever **appends a new extent** — the existing extents
//! are never reallocated or moved, so there is no copy-on-grow, no transient
//! "old + new buffer" peak, and no single giant contiguous allocation to
//! fragment the heap. The outer `Vec` of `Box` pointers does reallocate, but it
//! only carries (tiny) fat pointers, not the data.
//!
//! Strings are interned, so repeats (element/attribute names especially) are
//! stored once. This — together with the packed length-prefix layout —
//! eliminates the per-string heap allocation, the 24-byte `String` header, and
//! the slab-class rounding that a `Vec<String>` would pay for every one of
//! (potentially millions of) strings.
//!
//! A "node" is just a lightweight, `Copy` handle [`E4XNodeReadOnly`] = `{ store,
//! index }` pointing into the arena. It is the read-only twin of [`E4XNode`].
//!
//! ## Single-child-text collapse
//!
//! An element whose **only** child is a text node (the overwhelmingly common
//! `<tag>value</tag>` leaf) stores that text inline in its own `value` field
//! instead of as a separate Text node — halving the node count on
//! text-leaf-heavy documents. The text node is reconstituted on demand as a
//! *virtual* node ([`VIRTUAL_TEXT`]) so `children()`/`text()`/serialization see
//! it unchanged. The collapse is purely opportunistic: any element with mixed
//! or multiple content keeps real Text nodes, so it never costs more than the
//! un-collapsed layout.
//!
//! Parsing mirrors the mutable [`E4XNode::parse`](super::e4x::E4XNode::parse)
//! semantics for parity with `new XML()`: namespace resolution (prefix→URI),
//! entity unescaping, CDATA, comments, processing-instructions, and the
//! `ignoreWhitespace`/`ignoreComments`/`ignoreProcessingInstructions` settings.
//!
//! [`E4XNode`]: super::e4x::E4XNode

use std::collections::HashMap;

use flate2::{Decompress, FlushDecompress, Status};
use gc_arena::{Collect, Gc, Mutation};
use quick_xml::NsReader;
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;

use crate::avm2::Multiname;
use crate::avm2::e4x::E4XNamespace;
use crate::string::AvmString;

use ruffle_common::xml::avm2_unescape;

/// Sentinel for "no node"/"no string" in the flat arena (links and string refs).
pub const NO_NODE: u32 = u32::MAX;

/// Flag bit (bit 31) marking a *virtual* node index: the synthetic text child of
/// a **collapsed** element — one whose sole text child was folded into the
/// element's `value` to avoid storing a separate Text node. The low 31 bits hold
/// the owning element's index. Real indices are always `< 2^31`, so the bit is
/// free; [`E4XStoreReadOnly::node`] materialises the text record on demand.
const VIRTUAL_TEXT: u32 = 0x8000_0000;

/// The XML namespace reserved for `xmlns:` declarations.
const XMLNS_NS: &[u8] = b"http://www.w3.org/2000/xmlns/";

/// Node records per extent (power of two). 64Ki × 28 B = 1.75 MiB per extent.
const NODE_CHUNK_SHIFT: u32 = 16;
const NODE_CHUNK_LEN: usize = 1 << NODE_CHUNK_SHIFT;
const NODE_CHUNK_MASK: u32 = (1 << NODE_CHUNK_SHIFT) - 1;

/// String extent size (power of two). A string ref is `(chunk << SHIFT) |
/// local`, so `local` must fit in `SHIFT` bits → extents are this many bytes,
/// and the chunk index gets the remaining `32 - SHIFT` bits.
const STR_CHUNK_SHIFT: u32 = 20;
const STR_CHUNK_SIZE: usize = 1 << STR_CHUNK_SHIFT; // 1 MiB
const STR_CHUNK_MASK: u32 = (1 << STR_CHUNK_SHIFT) - 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum E4XNodeKindReadOnly {
    Element,
    Text,
    Cdata,
    Comment,
    ProcessingInstruction,
    Attribute,
}

/// Bit width of the parent-index field inside [`E4XNodeDataReadOnly::kind_parent`]
/// — the remaining 3 high bits hold the node kind. `2^29` (536 M) is far beyond
/// any reachable node count, so the top bits are always free to borrow.
const PARENT_BITS: u32 = 29;
const PARENT_MASK: u32 = (1 << PARENT_BITS) - 1;
/// "No parent" sentinel within the 29-bit parent field (real indices are always
/// `< PARENT_MASK`). Distinct from `NO_NODE`, which doesn't fit in 29 bits.
const PARENT_NONE: u32 = PARENT_MASK;

const fn kind_to_bits(kind: E4XNodeKindReadOnly) -> u32 {
    match kind {
        E4XNodeKindReadOnly::Element => 0,
        E4XNodeKindReadOnly::Text => 1,
        E4XNodeKindReadOnly::Cdata => 2,
        E4XNodeKindReadOnly::Comment => 3,
        E4XNodeKindReadOnly::ProcessingInstruction => 4,
        E4XNodeKindReadOnly::Attribute => 5,
    }
}

/// Pack a kind + parent index into the `kind_parent` word.
const fn pack_kind_parent(kind: E4XNodeKindReadOnly, parent: u32) -> u32 {
    let p = if parent == NO_NODE {
        PARENT_NONE
    } else {
        parent
    };
    (kind_to_bits(kind) << PARENT_BITS) | p
}

/// Marks the `value_or_child` slot as holding a *first-child node index* rather
/// than a value string ref. Value refs are always `< 2^25` (`chunk << 20 |
/// local`), so bit 31 is free as the discriminator. (Same bit value as
/// [`VIRTUAL_TEXT`], but a different namespace: this flags the union *slot*,
/// that flags a node *index*.)
const CHILD_FLAG: u32 = 0x8000_0000;

/// A single node record in the arena. Fixed size (7×`u32` = 28 bytes), no heap
/// pointers — links are `u32` node indices and string fields are `u32` string
/// refs into the string arena. `NO_NODE` is the "none" sentinel for all.
///
/// Two fields are bit-packed to keep the record compact:
/// * `kind` is folded into the high 3 bits of the parent slot (`kind_parent`) —
///   a standalone 1-byte enum would cost 4 bytes after alignment padding.
/// * `value` and `first_child` share one slot (`value_or_child`): they are
///   mutually exclusive by construction — an element collapses its sole text
///   child into a value *only* when it has no children, and non-elements never
///   have children. A leaf's value and a branch's first child therefore never
///   coexist, so one word holds whichever applies.
///
/// Always go through the accessors ([`kind`](Self::kind) /
/// [`parent_idx`](Self::parent_idx) / [`value_ref`](Self::value_ref) /
/// [`first_child_idx`](Self::first_child_idx)); never read the packed words.
#[derive(Clone, Copy)]
pub struct E4XNodeDataReadOnly {
    /// High 3 bits: node kind. Low 29 bits: parent index (or [`PARENT_NONE`]).
    kind_parent: u32,
    /// String ref for the *local* name (`NO_NODE` for text/cdata/comment nodes;
    /// for PIs it is the target).
    pub name: u32,
    /// Union slot: `NO_NODE` → neither; bit 31 (`CHILD_FLAG`) set → first-child
    /// node index in the low bits; otherwise → value string ref (text/attribute/
    /// comment/PI content, or an element's collapsed inline text).
    value_or_child: u32,
    /// String ref for the namespace URI of this element/attribute (`NO_NODE`
    /// when the node is in no namespace / the empty namespace).
    pub ns: u32,
    /// String ref for the namespace *prefix* of this element/attribute
    /// (`NO_NODE` when unprefixed). Kept for `name()`/serialization parity.
    pub prefix: u32,
    pub next_sibling: u32,
    /// First attribute node; attributes are stored as nodes chained via
    /// `next_sibling`, with `parent` pointing back at the element.
    pub first_attr: u32,
}

impl E4XNodeDataReadOnly {
    #[inline]
    fn kind(self) -> E4XNodeKindReadOnly {
        match self.kind_parent >> PARENT_BITS {
            0 => E4XNodeKindReadOnly::Element,
            1 => E4XNodeKindReadOnly::Text,
            2 => E4XNodeKindReadOnly::Cdata,
            3 => E4XNodeKindReadOnly::Comment,
            4 => E4XNodeKindReadOnly::ProcessingInstruction,
            _ => E4XNodeKindReadOnly::Attribute,
        }
    }

    /// Parent node index, or `NO_NODE` for a root.
    #[inline]
    fn parent_idx(self) -> u32 {
        let p = self.kind_parent & PARENT_MASK;
        if p == PARENT_NONE { NO_NODE } else { p }
    }

    /// Value string ref, or `NO_NODE` if the slot holds a child index / is empty.
    #[inline]
    fn value_ref(self) -> u32 {
        if self.value_or_child == NO_NODE || self.value_or_child & CHILD_FLAG != 0 {
            NO_NODE
        } else {
            self.value_or_child
        }
    }

    /// First-child node index, or `NO_NODE` if the slot holds a value / is empty.
    #[inline]
    fn first_child_idx(self) -> u32 {
        if self.value_or_child != NO_NODE && self.value_or_child & CHILD_FLAG != 0 {
            self.value_or_child & !CHILD_FLAG
        } else {
            NO_NODE
        }
    }

    /// Store a value string ref into the union slot (replacing any child link).
    #[inline]
    fn set_value(&mut self, value: u32) {
        debug_assert!(value == NO_NODE || value & CHILD_FLAG == 0);
        self.value_or_child = value;
    }

    /// Store a first-child node index into the union slot (replacing any value).
    #[inline]
    fn set_first_child(&mut self, idx: u32) {
        self.value_or_child = if idx == NO_NODE {
            NO_NODE
        } else {
            CHILD_FLAG | idx
        };
    }
}

/// The node record must stay at 7 words. Anything larger means a field slipped
/// out of the packed layout — at millions of nodes every byte is ~5 MB.
const _: () = assert!(std::mem::size_of::<E4XNodeDataReadOnly>() == 28);

/// Fills the unused tail of a freshly-allocated node extent; overwritten by real
/// records as they are pushed, never read for a valid index.
const PLACEHOLDER_NODE: E4XNodeDataReadOnly = E4XNodeDataReadOnly {
    kind_parent: pack_kind_parent(E4XNodeKindReadOnly::Element, NO_NODE),
    name: NO_NODE,
    value_or_child: NO_NODE,
    ns: NO_NODE,
    prefix: NO_NODE,
    next_sibling: NO_NODE,
    first_attr: NO_NODE,
};

/// A single `xmlns`/`xmlns:prefix` declaration, attached to the element it
/// appears on. Backs `namespaceDeclarations()` / `inScopeNamespaces()`.
#[derive(Clone, Copy)]
pub struct NsDeclReadOnly {
    /// The element node index this declaration appears on.
    pub node: u32,
    /// String ref for the prefix (`""` for the default namespace).
    pub prefix: u32,
    /// String ref for the URI.
    pub uri: u32,
}

/// The whole parsed document, in segmented arenas. Contains no `Gc` pointers, so
/// it is a GC leaf.
#[derive(Collect)]
#[collect(require_static)]
pub struct E4XStoreReadOnly {
    /// Node records, [`NODE_CHUNK_LEN`] per extent.
    node_chunks: Vec<Box<[E4XNodeDataReadOnly]>>,
    /// Total live node count (the last extent's tail is unused placeholders, so
    /// this can't be derived from `node_chunks`). Navigation goes through links,
    /// not a linear scan, so production never reads it; kept as authoritative
    /// metadata and exercised by the parse tests.
    #[allow(dead_code)]
    node_count: u32,
    /// Packed string extents: `[len: u32 LE][utf8]` entries, [`STR_CHUNK_SIZE`]
    /// bytes per extent (an oversized string gets a dedicated larger extent).
    str_chunks: Vec<Box<[u8]>>,
    /// `xmlns` declarations across the document (usually a handful).
    decls: Vec<NsDeclReadOnly>,
    /// Top-level node indices (document order).
    roots: Box<[u32]>,
}

/// Build-time append-only node arena (chunked, never copies finalized extents).
struct NodeArena {
    chunks: Vec<Box<[E4XNodeDataReadOnly]>>,
    len: u32,
}

impl NodeArena {
    fn new() -> Self {
        Self {
            chunks: Vec::new(),
            len: 0,
        }
    }

    /// Append `node`, returning its global index.
    fn push(&mut self, node: E4XNodeDataReadOnly) -> u32 {
        let idx = self.len;
        let c = (idx >> NODE_CHUNK_SHIFT) as usize;
        if c == self.chunks.len() {
            self.chunks
                .push(vec![PLACEHOLDER_NODE; NODE_CHUNK_LEN].into_boxed_slice());
        }
        self.chunks[c][(idx & NODE_CHUNK_MASK) as usize] = node;
        self.len += 1;
        idx
    }

    /// Mutable access to an already-pushed node (to back-patch its links).
    fn at_mut(&mut self, idx: u32) -> &mut E4XNodeDataReadOnly {
        &mut self.chunks[(idx >> NODE_CHUNK_SHIFT) as usize][(idx & NODE_CHUNK_MASK) as usize]
    }
}

/// Build-time append-only string arena (chunked, bump-written, no-straddle).
struct StrArena {
    chunks: Vec<Box<[u8]>>,
    /// Write cursor within the current (last) extent.
    cur_off: usize,
}

impl StrArena {
    fn new() -> Self {
        Self {
            chunks: Vec::new(),
            cur_off: 0,
        }
    }

    /// Append `s` as a `[len][bytes]` entry that stays within a single extent,
    /// returning its packed `u32` ref. A string larger than a normal extent
    /// gets a dedicated, exactly-sized extent (placed at `local == 0`, which
    /// keeps the ref encoding valid).
    fn append(&mut self, s: &str) -> u32 {
        let bytes = s.as_bytes();
        let need = 4 + bytes.len();
        let fits = self
            .chunks
            .last()
            .is_some_and(|c| self.cur_off + need <= c.len());
        if !fits {
            let size = need.max(STR_CHUNK_SIZE);
            self.chunks.push(vec![0u8; size].into_boxed_slice());
            self.cur_off = 0;
        }
        let chunk_idx = self.chunks.len() - 1;
        let local = self.cur_off;
        let buf = self.chunks.last_mut().unwrap();
        buf[local..local + 4].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
        buf[local + 4..local + 4 + bytes.len()].copy_from_slice(bytes);
        self.cur_off += need;
        ((chunk_idx as u32) << STR_CHUNK_SHIFT) | (local as u32)
    }

    /// Read back a previously [`append`](Self::append)ed entry's bytes by its
    /// `u32` ref — lets interning compare a candidate against the stored content
    /// without keeping a separate copy of the string.
    fn get(&self, sref: u32) -> &[u8] {
        let chunk = (sref >> STR_CHUNK_SHIFT) as usize;
        let local = (sref & STR_CHUNK_MASK) as usize;
        let buf = &self.chunks[chunk];
        let len = u32::from_le_bytes(buf[local..local + 4].try_into().unwrap()) as usize;
        &buf[local + 4..local + 4 + len]
    }
}

/// Mutable scratch shared by the parser helpers.
struct Builder {
    nodes: NodeArena,
    strings: StrArena,
    /// Content-hash → string ref, for deduplicating repeated strings. Keyed by a
    /// 64-bit content hash rather than an owned `String`, so the dedup index does
    /// NOT duplicate every unique string's bytes + 24-byte `String` header on top
    /// of the packed string arena — that duplication was a large transient peak
    /// on builds with high-cardinality columns. Transient: dropped with the
    /// `Builder`, so it never weighs on the final store.
    intern: HashMap<u64, u32>,
    decls: Vec<NsDeclReadOnly>,
}

impl Builder {
    /// Intern `s`: return the ref of its (deduplicated) entry, bump-appending it
    /// the first time it is seen. The dedup index is keyed by a 64-bit content
    /// hash; on a (vanishingly rare) hash collision with *different* content the
    /// new string is simply appended without deduping — correctness holds (the
    /// previous ref stays valid in the arena), only that one dedup is skipped.
    fn intern(&mut self, s: &str) -> u32 {
        let h = hash_bytes(s.as_bytes());
        if let Some(&r) = self.intern.get(&h)
            && self.strings.get(r) == s.as_bytes()
        {
            return r;
        }
        let r = self.strings.append(s);
        self.intern.insert(h, r);
        r
    }
}

/// FNV-1a 64-bit hash of `bytes` — keys the string-interning index by content
/// without storing the content twice. Fast on the short strings that dominate
/// (element/attribute names, cell values); at 64 bits, collisions between
/// distinct contents are astronomically rare.
fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

impl E4XStoreReadOnly {
    /// Parse `source` into a flat arena, honouring the same settings as
    /// `new XML(source, ignoreComments, ignoreProcessingInstructions,
    /// ignoreWhitespace)`.
    pub fn parse(source: &str, ignore_white: bool, ignore_comments: bool, ignore_pi: bool) -> Self {
        let mut b = Builder {
            nodes: NodeArena::new(),
            strings: StrArena::new(),
            intern: HashMap::new(),
            decls: Vec::new(),
        };

        let mut open: Vec<u32> = Vec::new();
        let mut last_child: Vec<u32> = Vec::new();
        let mut roots: Vec<u32> = Vec::new();
        let mut top_last: u32 = NO_NODE;
        // Buffered text of the innermost open element. Held back so that an
        // element whose *only* child is this text can collapse it into its
        // `value` (no separate Text node). Flushed as a real Text node the
        // moment a sibling appears (another child / closing with prior content).
        let mut pending: Option<String> = None;

        let mut reader = NsReader::from_str(source);

        loop {
            // Bind the event first so the mutable borrow on `reader` is
            // released — the event borrows the source string, leaving `reader`
            // free for `resolve_element`/`resolve_attribute`.
            let ev = reader.read_event();
            match ev {
                Ok(Event::Start(bs)) => {
                    // A child element means the parent's pending text (if any) is
                    // a real sibling, not a collapse candidate — flush it first.
                    flush_pending(
                        &mut b,
                        &open,
                        &mut last_child,
                        &mut roots,
                        &mut top_last,
                        &mut pending,
                    );
                    let id = make_element(&mut b, &reader, &bs, &open);
                    attach_child(
                        &mut b.nodes,
                        &open,
                        &mut last_child,
                        &mut roots,
                        &mut top_last,
                        id,
                    );
                    open.push(id);
                    last_child.push(NO_NODE);
                }
                Ok(Event::Empty(bs)) => {
                    flush_pending(
                        &mut b,
                        &open,
                        &mut last_child,
                        &mut roots,
                        &mut top_last,
                        &mut pending,
                    );
                    let id = make_element(&mut b, &reader, &bs, &open);
                    attach_child(
                        &mut b.nodes,
                        &open,
                        &mut last_child,
                        &mut roots,
                        &mut top_last,
                        id,
                    );
                }
                Ok(Event::Text(bt)) => {
                    let raw = avm2_unescape(bt.as_ref())
                        .unwrap_or_else(|_| String::from_utf8_lossy(bt.as_ref()).into_owned());
                    let is_ws = raw
                        .bytes()
                        .all(|c| matches!(c, b'\t' | b'\n' | b'\r' | b' '));
                    if !(ignore_white && is_ws) {
                        let stored = if ignore_white {
                            raw.trim_matches(|c| matches!(c, '\t' | '\n' | '\r' | ' '))
                                .to_string()
                        } else {
                            raw
                        };
                        // Adjacent text runs stay distinct nodes (existing
                        // behaviour): flush any prior run, then hold this one.
                        flush_pending(
                            &mut b,
                            &open,
                            &mut last_child,
                            &mut roots,
                            &mut top_last,
                            &mut pending,
                        );
                        pending = Some(stored);
                    }
                }
                Ok(Event::CData(bc)) => {
                    flush_pending(
                        &mut b,
                        &open,
                        &mut last_child,
                        &mut roots,
                        &mut top_last,
                        &mut pending,
                    );
                    // CDATA content is literal: no unescaping, no whitespace trim.
                    let text = String::from_utf8_lossy(bc.as_ref()).into_owned();
                    push_text(
                        &mut b,
                        &open,
                        &mut last_child,
                        &mut roots,
                        &mut top_last,
                        E4XNodeKindReadOnly::Cdata,
                        &text,
                    );
                }
                Ok(Event::Comment(bt)) => {
                    if ignore_comments {
                        continue;
                    }
                    flush_pending(
                        &mut b,
                        &open,
                        &mut last_child,
                        &mut roots,
                        &mut top_last,
                        &mut pending,
                    );
                    let text = avm2_unescape(bt.as_ref())
                        .unwrap_or_else(|_| String::from_utf8_lossy(bt.as_ref()).into_owned());
                    push_text(
                        &mut b,
                        &open,
                        &mut last_child,
                        &mut roots,
                        &mut top_last,
                        E4XNodeKindReadOnly::Comment,
                        &text,
                    );
                }
                Ok(Event::PI(bt)) => {
                    if ignore_pi {
                        continue;
                    }
                    flush_pending(
                        &mut b,
                        &open,
                        &mut last_child,
                        &mut roots,
                        &mut top_last,
                        &mut pending,
                    );
                    let raw = String::from_utf8_lossy(bt.as_ref()).into_owned();
                    // Split `target data` into name=target, value=data.
                    let (target, data) = match raw.split_once(char::is_whitespace) {
                        Some((t, d)) => (t.to_string(), d.trim_start().to_string()),
                        None => (raw.clone(), String::new()),
                    };
                    let name = b.intern(&target);
                    let value = b.intern(&data);
                    let parent = open.last().copied().unwrap_or(NO_NODE);
                    let id = b.nodes.push(E4XNodeDataReadOnly {
                        kind_parent: pack_kind_parent(
                            E4XNodeKindReadOnly::ProcessingInstruction,
                            parent,
                        ),
                        name,
                        value_or_child: value,
                        ns: NO_NODE,
                        prefix: NO_NODE,
                        next_sibling: NO_NODE,
                        first_attr: NO_NODE,
                    });
                    attach_child(
                        &mut b.nodes,
                        &open,
                        &mut last_child,
                        &mut roots,
                        &mut top_last,
                        id,
                    );
                }
                Ok(Event::End(_)) => {
                    if let Some(&elem) = open.last() {
                        if let Some(text) = pending.take() {
                            // Sole text child → collapse into the element's
                            // `value`; if it already has children, keep it a node.
                            if b.nodes.at_mut(elem).first_child_idx() == NO_NODE {
                                let v = b.intern(&text);
                                b.nodes.at_mut(elem).set_value(v);
                            } else {
                                push_text(
                                    &mut b,
                                    &open,
                                    &mut last_child,
                                    &mut roots,
                                    &mut top_last,
                                    E4XNodeKindReadOnly::Text,
                                    &text,
                                );
                            }
                        }
                    } else {
                        // Stray End at top level: flush any pending root text.
                        flush_pending(
                            &mut b,
                            &open,
                            &mut last_child,
                            &mut roots,
                            &mut top_last,
                            &mut pending,
                        );
                    }
                    open.pop();
                    last_child.pop();
                }
                Ok(Event::Eof) => {
                    // Trailing top-level text, if any.
                    flush_pending(
                        &mut b,
                        &open,
                        &mut last_child,
                        &mut roots,
                        &mut top_last,
                        &mut pending,
                    );
                    break;
                }
                // Decl / DocType ignored.
                Ok(_) => {}
                // Tolerant parse: stop on malformed input rather than error out.
                Err(_) => break,
            }
        }

        // Only the small outer pointer `Vec`s and `decls` are shrunk; the data
        // extents are never copied. The last node/string extent keeps a small
        // unused tail (bounded by one extent) — that is the only slack.
        let Builder {
            nodes,
            strings,
            decls,
            intern: _,
        } = b;
        let NodeArena {
            chunks: mut node_chunks,
            len: node_count,
        } = nodes;
        let StrArena {
            chunks: mut str_chunks,
            ..
        } = strings;
        let mut decls = decls;
        node_chunks.shrink_to_fit();
        str_chunks.shrink_to_fit();
        decls.shrink_to_fit();

        Self {
            node_chunks,
            node_count,
            str_chunks,
            decls,
            roots: roots.into_boxed_slice(),
        }
    }

    /// Build the arena directly from an **RESP** binary recordset,
    /// skipping XML-string reconstruction and the XML parser entirely.
    ///
    /// Produces the same tree [`parse`](Self::parse) would build for the
    /// reconstructed SOAP envelope:
    /// `Envelope > Body > <rootQName> > <rowElement>*`, where each row is a
    /// sequence of `<column>value</column>` leaves (the value folds inline via
    /// the single-child-text collapse, exactly as `<tag>value</tag>` does). A
    /// `0xFF` fault marker instead appends `Body > Fault > {faultcode,
    /// faultstring}`, so the caller's `body.*::Fault` check fires.
    ///
    /// `bytes` is the already-*decompressed* RESP with no leading codec flag
    /// (the one-shot caller inflates any deflate framing). Values are stored as
    /// their raw, unescaped text — no XML escape/unescape round-trip. Malformed
    /// or truncated input stops the build tolerantly, mirroring `parse`.
    ///
    /// This one-shot entry point is just [`RespStreamBuilder`] fed the whole
    /// payload at once (`new_decompressed`), so a single RESP parser is shared
    /// with the streaming path.
    pub fn from_binary(bytes: &[u8]) -> Self {
        let mut builder = RespStreamBuilder::new_decompressed();
        builder.feed(bytes);
        builder.finish()
    }

    pub fn roots(&self) -> &[u32] {
        &self.roots
    }

    /// The node record at global index `idx` (returned by value — `Copy`, so no
    /// borrow into the arena).
    ///
    /// A `VIRTUAL_TEXT`-flagged index has no stored record: it denotes the
    /// synthetic text child of a collapsed element, materialised here on the fly
    /// from the element's `value`. (The value string is real in the arena, so a
    /// later `str()` on it borrows normally.)
    #[inline]
    fn node(&self, idx: u32) -> E4XNodeDataReadOnly {
        if idx & VIRTUAL_TEXT != 0 {
            let parent = idx & !VIRTUAL_TEXT;
            let value = self.node_chunks[(parent >> NODE_CHUNK_SHIFT) as usize]
                [(parent & NODE_CHUNK_MASK) as usize]
                .value_ref();
            return E4XNodeDataReadOnly {
                kind_parent: pack_kind_parent(E4XNodeKindReadOnly::Text, parent),
                name: NO_NODE,
                value_or_child: value,
                ns: NO_NODE,
                prefix: NO_NODE,
                next_sibling: NO_NODE,
                first_attr: NO_NODE,
            };
        }
        self.node_chunks[(idx >> NODE_CHUNK_SHIFT) as usize][(idx & NODE_CHUNK_MASK) as usize]
    }

    /// The logical first child of node `idx`. For an element whose sole text
    /// child was collapsed into its `value` (so it has no stored children), this
    /// returns the synthetic [`VIRTUAL_TEXT`] child; otherwise the stored
    /// `first_child`. Walks that must observe text nodes start here.
    #[inline]
    fn first_child(&self, idx: u32) -> u32 {
        let n = self.node(idx);
        if n.kind() == E4XNodeKindReadOnly::Element
            && n.first_child_idx() == NO_NODE
            && n.value_ref() != NO_NODE
        {
            VIRTUAL_TEXT | idx
        } else {
            n.first_child_idx()
        }
    }

    /// Resolve a string ref to its `&str`. Entries are length-prefixed
    /// `[len: u32 LE][utf8]`; the ref packs `(chunk << STR_CHUNK_SHIFT) | local`.
    #[inline]
    fn str(&self, r: u32) -> &str {
        let chunk = (r >> STR_CHUNK_SHIFT) as usize;
        let local = (r & STR_CHUNK_MASK) as usize;
        let buf = &self.str_chunks[chunk];
        let len = u32::from_le_bytes(buf[local..local + 4].try_into().unwrap()) as usize;
        // SAFETY: every entry is written from a `&str` (see `StrArena::append`),
        // so the stored bytes are always valid UTF-8.
        unsafe { std::str::from_utf8_unchecked(&buf[local + 4..local + 4 + len]) }
    }

    /// Does node `idx` match the requested multiname? Mirrors
    /// [`E4XNode::matches_name`](super::e4x::E4XNode::matches_name): handles
    /// any-name, `*`, any-namespace, and explicit namespace-URI matching.
    fn name_matches(&self, idx: u32, want: &Multiname<'_>) -> bool {
        let rec = self.node(idx);

        // A non-qname Any name matches everything.
        if want.is_any_name() {
            return true;
        }

        // Local-name check (`*` is a wildcard).
        match want.local_name() {
            Some(local) => {
                let local = local.to_utf8_lossy();
                if local.as_ref() != "*"
                    && (rec.name == NO_NODE || self.str(rec.name) != local.as_ref())
                {
                    return false;
                }
            }
            None => return false,
        }

        if rec.name == NO_NODE {
            return false;
        }

        if want.is_any_namespace() {
            return true;
        }

        let self_ns: Option<&str> = (rec.ns != NO_NODE).then(|| self.str(rec.ns));

        // No explicit namespace requested: match the empty/no namespace.
        if !want.namespace_set().iter().any(|ns| ns.is_namespace()) {
            return self_ns.is_none_or(|n| n.is_empty());
        }

        want.namespace_set().iter().any(|ns| {
            let Some(uri) = ns.as_uri_opt() else {
                return false;
            };
            match self_ns {
                Some(s) => uri.to_utf8_lossy().as_ref() == s,
                None => uri.is_empty(),
            }
        })
    }
}

/// Flush the buffered [`pending`](E4XStoreReadOnly::parse) text as a real Text
/// node (used when the text turns out *not* to be an element's sole child).
fn flush_pending(
    b: &mut Builder,
    open: &[u32],
    last_child: &mut [u32],
    roots: &mut Vec<u32>,
    top_last: &mut u32,
    pending: &mut Option<String>,
) {
    if let Some(text) = pending.take() {
        push_text(
            b,
            open,
            last_child,
            roots,
            top_last,
            E4XNodeKindReadOnly::Text,
            &text,
        );
    }
}

/// Push a childless text-like node (Text/Cdata/Comment) and link it in.
fn push_text(
    b: &mut Builder,
    open: &[u32],
    last_child: &mut [u32],
    roots: &mut Vec<u32>,
    top_last: &mut u32,
    kind: E4XNodeKindReadOnly,
    text: &str,
) {
    let value = b.intern(text);
    let parent = open.last().copied().unwrap_or(NO_NODE);
    let id = b.nodes.push(E4XNodeDataReadOnly {
        kind_parent: pack_kind_parent(kind, parent),
        name: NO_NODE,
        value_or_child: value,
        ns: NO_NODE,
        prefix: NO_NODE,
        next_sibling: NO_NODE,
        first_attr: NO_NODE,
    });
    attach_child(&mut b.nodes, open, last_child, roots, top_last, id);
}

/// Push an element node (plus its attribute nodes and `xmlns` declarations) and
/// return its index. Resolves namespaces via the `NsReader`.
fn make_element(
    b: &mut Builder,
    reader: &NsReader<&[u8]>,
    bs: &quick_xml::events::BytesStart<'_>,
    open: &[u32],
) -> u32 {
    let parent = open.last().copied().unwrap_or(NO_NODE);

    // Resolve the element's own name/namespace.
    let (ns_res, local) = reader.resolve_element(bs.name());
    let name_str = String::from_utf8_lossy(local.into_inner()).into_owned();
    let name = b.intern(&name_str);

    let (elem_ns, elem_prefix) = match ns_res {
        ResolveResult::Bound(ns) if !ns.into_inner().is_empty() => {
            let uri = String::from_utf8_lossy(ns.into_inner()).into_owned();
            let prefix = bs
                .name()
                .prefix()
                .map(|p| String::from_utf8_lossy(p.into_inner()).into_owned());
            (
                b.intern(&uri),
                prefix.map(|p| b.intern(&p)).unwrap_or(NO_NODE),
            )
        }
        _ => (NO_NODE, NO_NODE),
    };

    let idx = b.nodes.push(E4XNodeDataReadOnly {
        kind_parent: pack_kind_parent(E4XNodeKindReadOnly::Element, parent),
        name,
        value_or_child: NO_NODE,
        ns: elem_ns,
        prefix: elem_prefix,
        next_sibling: NO_NODE,
        first_attr: NO_NODE,
    });

    let mut last_attr = NO_NODE;
    for attr in bs.attributes() {
        let Ok(attr) = attr else { continue };

        let (a_ns_res, a_local) = reader.resolve_attribute(attr.key);
        let a_local_str = String::from_utf8_lossy(a_local.into_inner()).into_owned();
        let a_value = String::from_utf8_lossy(attr.value.as_ref()).into_owned();
        let a_value = avm2_unescape(attr.value.as_ref()).unwrap_or(a_value);

        // `xmlns:foo="..."` — a prefixed namespace declaration.
        if matches!(a_ns_res, ResolveResult::Bound(ns) if ns.into_inner() == XMLNS_NS) {
            let prefix = b.intern(&a_local_str);
            let uri = b.intern(&a_value);
            b.decls.push(NsDeclReadOnly {
                node: idx,
                prefix,
                uri,
            });
            continue;
        }
        // `xmlns="..."` — the default namespace declaration (quick-xml reports
        // this as Unbound with the literal name `xmlns`).
        if matches!(a_ns_res, ResolveResult::Unbound) && a_local_str == "xmlns" {
            let prefix = b.intern("");
            let uri = b.intern(&a_value);
            b.decls.push(NsDeclReadOnly {
                node: idx,
                prefix,
                uri,
            });
            continue;
        }

        let (attr_ns, attr_prefix) = match a_ns_res {
            ResolveResult::Bound(ns) if !ns.into_inner().is_empty() => {
                let uri = String::from_utf8_lossy(ns.into_inner()).into_owned();
                let prefix = attr
                    .key
                    .prefix()
                    .map(|p| String::from_utf8_lossy(p.into_inner()).into_owned());
                (
                    b.intern(&uri),
                    prefix.map(|p| b.intern(&p)).unwrap_or(NO_NODE),
                )
            }
            _ => (NO_NODE, NO_NODE),
        };

        let aname = b.intern(&a_local_str);
        let aval = b.intern(&a_value);

        let aidx = b.nodes.push(E4XNodeDataReadOnly {
            kind_parent: pack_kind_parent(E4XNodeKindReadOnly::Attribute, idx),
            name: aname,
            value_or_child: aval,
            ns: attr_ns,
            prefix: attr_prefix,
            next_sibling: NO_NODE,
            first_attr: NO_NODE,
        });

        if last_attr == NO_NODE {
            b.nodes.at_mut(idx).first_attr = aidx;
        } else {
            b.nodes.at_mut(last_attr).next_sibling = aidx;
        }
        last_attr = aidx;
    }

    idx
}

/// Link `id` as the next child of the current open element (or as a top-level
/// root when no element is open).
fn attach_child(
    nodes: &mut NodeArena,
    open: &[u32],
    last_child: &mut [u32],
    roots: &mut Vec<u32>,
    top_last: &mut u32,
    id: u32,
) {
    if let Some(&parent) = open.last() {
        let last = *last_child.last().unwrap();
        if last == NO_NODE {
            nodes.at_mut(parent).set_first_child(id);
        } else {
            nodes.at_mut(last).next_sibling = id;
        }
        *last_child.last_mut().unwrap() = id;
    } else {
        if *top_last != NO_NODE {
            nodes.at_mut(*top_last).next_sibling = id;
        }
        *top_last = id;
        roots.push(id);
    }
}

/// A freshly-built element record (no children/value/attributes yet). Shared by
/// the RESP builder ([`E4XStoreReadOnly::from_binary`]).
fn elem_record(name: u32, ns: u32, prefix: u32, parent: u32) -> E4XNodeDataReadOnly {
    E4XNodeDataReadOnly {
        kind_parent: pack_kind_parent(E4XNodeKindReadOnly::Element, parent),
        name,
        value_or_child: NO_NODE,
        ns,
        prefix,
        next_sibling: NO_NODE,
        first_attr: NO_NODE,
    }
}

/// Consume a finished [`Builder`] into the immutable store, shrinking only the
/// small outer pointer vecs (the data extents are never copied). Used by
/// [`E4XStoreReadOnly::from_binary`].
fn finish_store(b: Builder, roots: Vec<u32>) -> E4XStoreReadOnly {
    let Builder {
        nodes,
        strings,
        decls,
        intern: _,
    } = b;
    let NodeArena {
        chunks: mut node_chunks,
        len: node_count,
    } = nodes;
    let StrArena {
        chunks: mut str_chunks,
        ..
    } = strings;
    let mut decls = decls;
    node_chunks.shrink_to_fit();
    str_chunks.shrink_to_fit();
    decls.shrink_to_fit();
    E4XStoreReadOnly {
        node_chunks,
        node_count,
        str_chunks,
        decls,
        roots: roots.into_boxed_slice(),
    }
}

/// Append `<SOAP-ENV:Fault><faultcode>…</faultcode><faultstring>msg</faultstring>
/// </Fault>` as the sibling of the (possibly partial) response root under Body,
/// so the consumer's `body.*::Fault` check fires on a mid-stream fault.
fn append_fault(
    b: &mut Builder,
    body_idx: u32,
    root_idx: u32,
    soap_uri: u32,
    soap_prefix: u32,
    msg: &str,
) {
    let fault_name = b.intern("Fault");
    let fc_name = b.intern("faultcode");
    let fc_val = b.intern("SOAP-ENV:Server");
    let fs_name = b.intern("faultstring");
    let fs_val = b.intern(msg);

    let fault_idx = b
        .nodes
        .push(elem_record(fault_name, soap_uri, soap_prefix, body_idx));
    b.nodes.at_mut(root_idx).next_sibling = fault_idx;

    let fc_idx = b
        .nodes
        .push(elem_record(fc_name, NO_NODE, NO_NODE, fault_idx));
    b.nodes.at_mut(fc_idx).set_value(fc_val);
    b.nodes.at_mut(fault_idx).set_first_child(fc_idx);

    let fs_idx = b
        .nodes
        .push(elem_record(fs_name, NO_NODE, NO_NODE, fault_idx));
    b.nodes.at_mut(fs_idx).set_value(fs_val);
    b.nodes.at_mut(fc_idx).next_sibling = fs_idx;
}

/// Minimal bounds-checked cursor over an RESP binary recordset, used by
/// [`E4XStoreReadOnly::from_binary`]. Reads past the end return `None`, so a
/// truncated stream stops the build tolerantly.
struct RespCursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> RespCursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn u8(&mut self) -> Option<u8> {
        let v = *self.data.get(self.pos)?;
        self.pos += 1;
        Some(v)
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }

    /// Unsigned LEB128 varint.
    fn varint(&mut self) -> Option<usize> {
        let mut result: u64 = 0;
        let mut shift: u32 = 0;
        loop {
            let byte = self.u8()?;
            result |= u64::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift >= 64 {
                return None;
            }
        }
        Some(result as usize)
    }

    /// Length-prefixed byte string (`varint len` + `len` bytes).
    fn lp_bytes(&mut self) -> Option<&'a [u8]> {
        let len = self.varint()?;
        self.take(len)
    }

    /// Length-prefixed UTF-8 string (lossy on invalid UTF-8).
    fn lp_string(&mut self) -> Option<String> {
        self.lp_bytes()
            .map(|s| String::from_utf8_lossy(s).into_owned())
    }
}

/// Streaming builder for the RESP wire format. It accepts the **raw** response
/// bytes a chunk at a time — a leading 1-byte codec flag (`0` raw / `1` deflate)
/// then a possibly deflate-compressed RESP stream — inflates incrementally in
/// Rust, and appends every *complete* row to the arena as soon as its bytes are
/// available. The parse therefore overlaps the network/server streaming instead
/// of running in one block after the full download.
///
/// It is the single RESP parser: the one-shot [`E4XStoreReadOnly::from_binary`]
/// is this builder in "already-decompressed" mode ([`new_decompressed`](
/// Self::new_decompressed)) fed the whole payload at once.
///
/// Holds no `Gc` pointers (owned arenas + a flate2 decoder), so it is a GC leaf
/// and can be parked inside the in-progress `XMLReadOnly` between feeds.
#[derive(Collect)]
#[collect(require_static)]
pub struct RespStreamBuilder {
    /// Whether the leading codec flag byte has been consumed.
    codec_read: bool,
    /// Streaming inflater (`Some` for deflate, `None` for a raw stream).
    inflate: Option<Decompress>,
    /// Reusable scratch for one inflate step.
    scratch: Vec<u8>,
    /// Inflated-but-not-yet-parsed bytes (the partial header, or the tail after
    /// the last complete record).
    buf: Vec<u8>,
    /// Whether the RESP header (magic, qname, columns) has been parsed.
    header_done: bool,
    /// Whether a terminator/malformed marker was reached; further feeds no-op.
    finished: bool,
    /// The arena under construction.
    b: Builder,
    /// Interned column-name refs, in record order.
    col_refs: Vec<u32>,
    soap_uri: u32,
    soap_prefix: u32,
    env_idx: u32,
    body_idx: u32,
    root_idx: u32,
    row_name: u32,
    /// Last row appended under the response root (for sibling chaining).
    last_row: u32,
}

impl RespStreamBuilder {
    /// Streaming-path builder: the input begins with the 1-byte codec flag and
    /// may be deflate-compressed (inflated here, incrementally).
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let mut b = Builder {
            nodes: NodeArena::new(),
            strings: StrArena::new(),
            intern: HashMap::new(),
            decls: Vec::new(),
        };

        // SOAP envelope wrapper — emitted unconditionally so the consumer always
        // finds a Body, even on a malformed/empty stream.
        let soap_uri = b.intern("http://schemas.xmlsoap.org/soap/envelope/");
        let soap_prefix = b.intern("SOAP-ENV");
        let env_name = b.intern("Envelope");
        let body_name = b.intern("Body");
        let env_idx = b
            .nodes
            .push(elem_record(env_name, soap_uri, soap_prefix, NO_NODE));
        b.decls.push(NsDeclReadOnly {
            node: env_idx,
            prefix: soap_prefix,
            uri: soap_uri,
        });
        let body_idx = b
            .nodes
            .push(elem_record(body_name, soap_uri, soap_prefix, env_idx));
        b.nodes.at_mut(env_idx).set_first_child(body_idx);

        Self {
            codec_read: false,
            inflate: None,
            scratch: vec![0u8; 64 * 1024],
            buf: Vec::new(),
            header_done: false,
            finished: false,
            b,
            col_refs: Vec::new(),
            soap_uri,
            soap_prefix,
            env_idx,
            body_idx,
            root_idx: NO_NODE,
            row_name: NO_NODE,
            last_row: NO_NODE,
        }
    }

    /// One-shot builder for already-decompressed input with no leading codec
    /// flag: bytes are appended verbatim, no flag consumed, no inflate run.
    pub fn new_decompressed() -> Self {
        let mut me = Self::new();
        me.codec_read = true; // there is no codec flag in the buffer
        me.inflate = None; // raw passthrough
        me
    }

    /// Append a raw chunk: consume the codec flag (first chunk only), inflate
    /// into the parse buffer, then parse the header and every complete record.
    pub fn feed(&mut self, raw: &[u8]) {
        if self.finished {
            return;
        }
        let rest = self.consume_codec_flag(raw);
        self.inflate_into_buf(rest);
        if !self.header_done && !self.try_parse_header() {
            return;
        }
        self.parse_records();
    }

    /// Finalise: parse anything still buffered, then freeze the arena. Tolerant
    /// of a truncated stream (mirrors the one-shot parser).
    pub fn finish(mut self) -> E4XStoreReadOnly {
        if !self.finished {
            if !self.header_done {
                self.try_parse_header();
            }
            self.parse_records();
        }
        finish_store(self.b, vec![self.env_idx])
    }

    /// Consume the leading 1-byte codec flag (0 raw, 1 deflate) on the first
    /// chunk, selecting the inflate mode; returns the remaining bytes.
    fn consume_codec_flag<'r>(&mut self, raw: &'r [u8]) -> &'r [u8] {
        if self.codec_read {
            return raw;
        }
        match raw.split_first() {
            Some((&flag, rest)) => {
                // PHP `zlib.deflate` emits raw deflate (RFC1951, no zlib header).
                self.inflate = (flag == 1).then(|| Decompress::new(false));
                self.codec_read = true;
                rest
            }
            None => raw,
        }
    }

    /// Inflate `input` into `self.buf` (or copy verbatim in raw mode).
    fn inflate_into_buf(&mut self, mut input: &[u8]) {
        let Self {
            inflate,
            scratch,
            buf,
            finished,
            ..
        } = self;
        let Some(dec) = inflate.as_mut() else {
            buf.extend_from_slice(input);
            return;
        };
        loop {
            let in_before = dec.total_in();
            let out_before = dec.total_out();
            let status = dec.decompress(input, &mut scratch[..], FlushDecompress::None);
            let consumed = (dec.total_in() - in_before) as usize;
            let produced = (dec.total_out() - out_before) as usize;
            if produced > 0 {
                buf.extend_from_slice(&scratch[..produced]);
            }
            input = &input[consumed..];
            match status {
                Ok(Status::StreamEnd) => break,
                Err(_) => {
                    *finished = true;
                    break;
                }
                Ok(_) => {}
            }
            if consumed == 0 && produced == 0 {
                break;
            }
        }
    }

    /// Try to parse the RESP header from `self.buf`. Returns `true` once the
    /// header is resolved (parsed, or determined malformed → empty body),
    /// `false` if more bytes are needed (the buffer is left intact for a retry).
    fn try_parse_header(&mut self) -> bool {
        let (consumed, root_qname, ns_uri, row_el, names) = match scan_header(&self.buf) {
            HeaderScan::NeedMore => return false,
            HeaderScan::Bad => {
                // Bad magic / absurd column count: stop with just Envelope/Body.
                self.header_done = true;
                self.finished = true;
                return true;
            }
            HeaderScan::Ok {
                consumed,
                root_qname,
                ns_uri,
                row_el,
                names,
            } => (consumed, root_qname, ns_uri, row_el, names),
        };

        // Commit: build the response root and intern the column names.
        let Self {
            b,
            col_refs,
            row_name,
            root_idx,
            body_idx,
            ..
        } = self;
        let (root_prefix_str, root_local_str) = match root_qname.split_once(':') {
            Some((p, l)) => (Some(p), l),
            None => (None, root_qname.as_str()),
        };
        let root_local = b.intern(root_local_str);
        let root_ns = if ns_uri.is_empty() {
            NO_NODE
        } else {
            b.intern(&ns_uri)
        };
        let root_prefix = root_prefix_str.map(|p| b.intern(p)).unwrap_or(NO_NODE);
        *row_name = b.intern(&row_el);
        for name in &names {
            col_refs.push(b.intern(name));
        }
        let new_root = b
            .nodes
            .push(elem_record(root_local, root_ns, root_prefix, *body_idx));
        b.nodes.at_mut(*body_idx).set_first_child(new_root);
        if root_ns != NO_NODE {
            let decl_prefix = if root_prefix != NO_NODE {
                root_prefix
            } else {
                b.intern("")
            };
            b.decls.push(NsDeclReadOnly {
                node: new_root,
                prefix: decl_prefix,
                uri: root_ns,
            });
        }
        *root_idx = new_root;

        self.buf.drain(..consumed);
        self.header_done = true;
        true
    }

    /// Parse every complete record currently buffered, appending rows (and a
    /// trailing fault) to the arena. Stops at the first incomplete record,
    /// leaving its bytes buffered for the next feed.
    fn parse_records(&mut self) {
        if !self.header_done || self.finished {
            return;
        }
        let col_count = self.col_refs.len();
        let mut pos = 0usize;
        loop {
            match scan_record(&self.buf[pos..], col_count) {
                Scan::Incomplete => break,
                Scan::Row { len } => {
                    self.build_one_row(pos, pos + len);
                    pos += len;
                }
                Scan::End { len } => {
                    pos += len;
                    self.finished = true;
                    break;
                }
                Scan::Fault { len, msg } => {
                    append_fault(
                        &mut self.b,
                        self.body_idx,
                        self.root_idx,
                        self.soap_uri,
                        self.soap_prefix,
                        &msg,
                    );
                    pos += len;
                    self.finished = true;
                    break;
                }
                Scan::Malformed => {
                    self.finished = true;
                    break;
                }
            }
        }
        if pos > 0 {
            self.buf.drain(..pos);
        }
    }

    /// Append one complete row (`self.buf[start..end]`, known whole) as a
    /// `<rowElement>` with one `<column>value</column>` leaf per column.
    fn build_one_row(&mut self, start: usize, end: usize) {
        let Self {
            buf,
            b,
            col_refs,
            row_name,
            root_idx,
            last_row,
            ..
        } = self;
        let mut c = RespCursor::new(&buf[start..end]);
        let _marker = c.u8(); // 0x01, validated by scan_record

        let row_idx = b
            .nodes
            .push(elem_record(*row_name, NO_NODE, NO_NODE, *root_idx));
        if *last_row == NO_NODE {
            b.nodes.at_mut(*root_idx).set_first_child(row_idx);
        } else {
            b.nodes.at_mut(*last_row).next_sibling = row_idx;
        }
        *last_row = row_idx;

        let mut last_col = NO_NODE;
        for &col_name in col_refs.iter() {
            let nf = c.u8().unwrap_or(0);
            let col_idx = b
                .nodes
                .push(elem_record(col_name, NO_NODE, NO_NODE, row_idx));
            if nf == 0x01
                && let Some(slice) = c.lp_bytes()
                && !slice.is_empty()
            {
                // Raw value text stored verbatim (no XML (un)escaping): the
                // single-child-text collapse folds it into `value`.
                let cow = String::from_utf8_lossy(slice);
                let vref = b.intern(&cow);
                b.nodes.at_mut(col_idx).set_value(vref);
            }
            if last_col == NO_NODE {
                b.nodes.at_mut(row_idx).set_first_child(col_idx);
            } else {
                b.nodes.at_mut(last_col).next_sibling = col_idx;
            }
            last_col = col_idx;
        }
    }
}

/// Outcome of [`scan_header`].
enum HeaderScan {
    NeedMore,
    Bad,
    Ok {
        consumed: usize,
        root_qname: String,
        ns_uri: String,
        row_el: String,
        names: Vec<String>,
    },
}

/// Scan the RESP header at the front of `data` without touching the builder, so
/// the caller can commit only once the whole header is present.
fn scan_header(data: &[u8]) -> HeaderScan {
    let mut c = RespCursor::new(data);
    match c.take(4) {
        Some(m) if m == b"RESP" => {}
        Some(_) => return HeaderScan::Bad,
        None => return HeaderScan::NeedMore,
    }
    if c.u8().is_none() {
        return HeaderScan::NeedMore; // version
    }
    let Some(root_qname) = c.lp_string() else {
        return HeaderScan::NeedMore;
    };
    let Some(ns_uri) = c.lp_string() else {
        return HeaderScan::NeedMore;
    };
    let Some(row_el) = c.lp_string() else {
        return HeaderScan::NeedMore;
    };
    let Some(col_count) = c.varint() else {
        return HeaderScan::NeedMore;
    };
    // Defensive cap: a corrupt varint must not spin a near-infinite loop.
    if col_count > 100_000 {
        return HeaderScan::Bad;
    }
    let mut names: Vec<String> = Vec::with_capacity(col_count);
    for _ in 0..col_count {
        match c.lp_string() {
            Some(s) => names.push(s),
            None => return HeaderScan::NeedMore,
        }
    }
    HeaderScan::Ok {
        consumed: c.pos,
        root_qname,
        ns_uri,
        row_el,
        names,
    }
}

/// Outcome of scanning one record at the front of the parse buffer.
enum Scan {
    /// Not all bytes are present yet — wait for the next feed.
    Incomplete,
    /// A complete row of `len` bytes.
    Row { len: usize },
    /// The regular END terminator (`len` bytes consumed).
    End { len: usize },
    /// A fault terminator carrying `msg` (`len` bytes consumed).
    Fault { len: usize, msg: String },
    /// An unexpected marker byte — stop tolerantly.
    Malformed,
}

/// Determine whether a whole record sits at the front of `data`, without
/// building any nodes (so a truncated tail never leaves a half-row in the
/// arena). Mirrors the grammar consumed by [`RespStreamBuilder::build_one_row`].
fn scan_record(data: &[u8], col_count: usize) -> Scan {
    let mut c = RespCursor::new(data);
    let marker = match c.u8() {
        Some(m) => m,
        None => return Scan::Incomplete,
    };
    match marker {
        0x00 => Scan::End { len: c.pos },
        0xFF => match c.lp_string() {
            Some(msg) => Scan::Fault { len: c.pos, msg },
            None => Scan::Incomplete,
        },
        0x01 => {
            for _ in 0..col_count {
                let nf = match c.u8() {
                    Some(v) => v,
                    None => return Scan::Incomplete,
                };
                if nf == 0x01 && c.lp_bytes().is_none() {
                    return Scan::Incomplete;
                }
            }
            Scan::Row { len: c.pos }
        }
        _ => Scan::Malformed,
    }
}

/// A lightweight, `Copy` handle to a node inside an [`E4XStoreReadOnly`]. The
/// read-only twin of [`E4XNode`](super::e4x::E4XNode): instead of a per-node
/// `Gc`, it is `{ store, index }`.
#[derive(Clone, Copy, Collect)]
#[collect(no_drop)]
pub struct E4XNodeReadOnly<'gc> {
    pub store: Gc<'gc, E4XStoreReadOnly>,
    pub index: u32,
}

impl std::fmt::Debug for E4XNodeReadOnly<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("E4XNodeReadOnly")
            .field("index", &self.index)
            .finish()
    }
}

impl<'gc> E4XNodeReadOnly<'gc> {
    pub fn new(store: Gc<'gc, E4XStoreReadOnly>, index: u32) -> Self {
        Self { store, index }
    }

    #[inline]
    fn record(&self) -> E4XNodeDataReadOnly {
        self.store.node(self.index)
    }

    fn with_index(&self, index: u32) -> Self {
        Self {
            store: self.store,
            index,
        }
    }

    pub fn kind(&self) -> E4XNodeKindReadOnly {
        self.record().kind()
    }

    pub fn is_element(&self) -> bool {
        self.kind() == E4XNodeKindReadOnly::Element
    }

    pub fn parent(&self) -> Option<E4XNodeReadOnly<'gc>> {
        let p = self.record().parent_idx();
        (p != NO_NODE).then(|| self.with_index(p))
    }

    pub fn local_name(&self, mc: &Mutation<'gc>) -> Option<AvmString<'gc>> {
        let n = self.record();
        (n.name != NO_NODE).then(|| AvmString::new_utf8(mc, self.store.str(n.name)))
    }

    /// This node's own namespace (`None` when in no/empty namespace), as an
    /// [`E4XNamespace`] for reuse of the existing AS3 `Namespace` plumbing.
    pub fn namespace(&self, mc: &Mutation<'gc>) -> Option<E4XNamespace<'gc>> {
        let n = self.record();
        if n.ns == NO_NODE {
            return None;
        }
        let uri = AvmString::new_utf8(mc, self.store.str(n.ns));
        let prefix =
            (n.prefix != NO_NODE).then(|| AvmString::new_utf8(mc, self.store.str(n.prefix)));
        Some(E4XNamespace { uri, prefix })
    }

    /// In-scope namespaces (this node's `xmlns` declarations plus ancestors'),
    /// closest declaration winning. Mirrors
    /// [`E4XNode::in_scope_namespaces`](super::e4x::E4XNode::in_scope_namespaces).
    pub fn in_scope_namespaces(&self, mc: &Mutation<'gc>) -> Vec<E4XNamespace<'gc>> {
        let mut result: Vec<E4XNamespace<'gc>> = Vec::new();
        let mut cur = Some(*self);
        while let Some(node) = cur {
            for d in self.store.decls.iter().filter(|d| d.node == node.index) {
                let prefix = AvmString::new_utf8(mc, self.store.str(d.prefix));
                let uri = AvmString::new_utf8(mc, self.store.str(d.uri));
                let new_ns = E4XNamespace {
                    uri,
                    prefix: Some(prefix),
                };
                let found = result.iter().any(|ns| {
                    if new_ns.prefix.is_some() {
                        new_ns.prefix == ns.prefix
                    } else {
                        new_ns.uri == ns.uri
                    }
                });
                if !found {
                    result.push(new_ns);
                }
            }
            cur = node.parent();
        }
        result
    }

    pub fn matches_name(&self, want: &Multiname<'gc>) -> bool {
        self.store.name_matches(self.index, want)
    }

    /// Child elements matching `want`, in document order.
    pub fn children_matching(&self, want: &Multiname<'gc>, out: &mut Vec<E4XNodeReadOnly<'gc>>) {
        let mut c = self.record().first_child_idx();
        while c != NO_NODE {
            let cn = self.store.node(c);
            if cn.kind() == E4XNodeKindReadOnly::Element && self.store.name_matches(c, want) {
                out.push(self.with_index(c));
            }
            c = cn.next_sibling;
        }
    }

    /// Attributes matching `want`.
    pub fn attributes_matching(&self, want: &Multiname<'gc>, out: &mut Vec<E4XNodeReadOnly<'gc>>) {
        let mut a = self.record().first_attr;
        while a != NO_NODE {
            let an = self.store.node(a);
            if self.store.name_matches(a, want) {
                out.push(self.with_index(a));
            }
            a = an.next_sibling;
        }
    }

    /// Append the simple-content text of every child element matching `want`
    /// into `out`, '\n'-separated for multiple matches (mirrors
    /// `XMLList.toString`). Unlike `children_matching` + `string_value`, this
    /// allocates no intermediate `Vec`/`XMLList` — the extraction fast path used
    /// by the native sort.
    pub fn append_children_text(&self, want: &Multiname<'gc>, out: &mut String) {
        let mut c = self.record().first_child_idx();
        let mut first = true;
        while c != NO_NODE {
            let cn = self.store.node(c);
            if cn.kind() == E4XNodeKindReadOnly::Element && self.store.name_matches(c, want) {
                if !first {
                    out.push('\n');
                }
                self.with_index(c).append_text(out);
                first = false;
            }
            c = cn.next_sibling;
        }
    }

    /// As [`append_children_text`](Self::append_children_text) but over the
    /// matching attributes.
    pub fn append_attrs_text(&self, want: &Multiname<'gc>, out: &mut String) {
        let mut a = self.record().first_attr;
        let mut first = true;
        while a != NO_NODE {
            let an = self.store.node(a);
            if self.store.name_matches(a, want) {
                if !first {
                    out.push('\n');
                }
                self.with_index(a).append_text(out);
                first = false;
            }
            a = an.next_sibling;
        }
    }

    /// All descendant nodes matching `want` (depth-first). Includes attributes
    /// when `want` is an attribute multiname (E4X `..@foo`).
    pub fn descendants_matching(&self, want: &Multiname<'gc>, out: &mut Vec<E4XNodeReadOnly<'gc>>) {
        let want_attr = want.is_attribute();
        let mut c = self.record().first_child_idx();
        while c != NO_NODE {
            let cn = self.store.node(c);
            if cn.kind() == E4XNodeKindReadOnly::Element {
                if want_attr {
                    self.with_index(c).attributes_matching(want, out);
                } else if self.store.name_matches(c, want) {
                    out.push(self.with_index(c));
                }
                self.with_index(c).descendants_matching(want, out);
            }
            c = cn.next_sibling;
        }
    }

    /// The element child at logical index `idx` (element children only).
    pub fn child_element_at(&self, idx: usize) -> Option<E4XNodeReadOnly<'gc>> {
        let mut c = self.record().first_child_idx();
        let mut i = 0;
        while c != NO_NODE {
            let cn = self.store.node(c);
            if cn.kind() == E4XNodeKindReadOnly::Element {
                if i == idx {
                    return Some(self.with_index(c));
                }
                i += 1;
            }
            c = cn.next_sibling;
        }
        None
    }

    /// The ordinal of this node among its parent's children (any kind), or
    /// `None` for the root and for attributes. Mirrors
    /// [`E4XNode::child_index`](super::e4x::E4XNode::child_index).
    pub fn child_index(&self) -> Option<usize> {
        let parent = self.parent()?;
        if self.is_attribute() {
            return None;
        }
        let mut idx = 0;
        let mut c = self.store.first_child(parent.index);
        while c != NO_NODE {
            if c == self.index {
                return Some(idx);
            }
            c = self.store.node(c).next_sibling;
            idx += 1;
        }
        None
    }

    /// Append the simple-content text of this node (and its element
    /// descendants) to `out`.
    pub fn append_text(&self, out: &mut String) {
        let n = self.record();
        match n.kind() {
            E4XNodeKindReadOnly::Text
            | E4XNodeKindReadOnly::Cdata
            | E4XNodeKindReadOnly::Attribute => {
                if n.value_ref() != NO_NODE {
                    out.push_str(self.store.str(n.value_ref()));
                }
            }
            // Comments and PIs contribute no simple content.
            E4XNodeKindReadOnly::Comment | E4XNodeKindReadOnly::ProcessingInstruction => {}
            E4XNodeKindReadOnly::Element => {
                // A collapsed element holds its sole text child inline.
                if n.value_ref() != NO_NODE {
                    out.push_str(self.store.str(n.value_ref()));
                    return;
                }
                let mut c = n.first_child_idx();
                while c != NO_NODE {
                    let cn = self.store.node(c);
                    // Per E4X simple content, skip comment/PI children.
                    if !matches!(
                        cn.kind(),
                        E4XNodeKindReadOnly::Comment | E4XNodeKindReadOnly::ProcessingInstruction
                    ) {
                        self.with_index(c).append_text(out);
                    }
                    c = cn.next_sibling;
                }
            }
        }
    }

    pub fn string_value(&self) -> String {
        let mut s = String::new();
        self.append_text(&mut s);
        s
    }

    /// The raw value of a text/cdata/comment/PI/attribute node.
    pub fn value(&self) -> Option<&str> {
        let n = self.record();
        (n.value_ref() != NO_NODE).then(|| self.store.str(n.value_ref()))
    }

    pub fn is_text(&self) -> bool {
        matches!(
            self.kind(),
            E4XNodeKindReadOnly::Text | E4XNodeKindReadOnly::Cdata
        )
    }

    pub fn is_attribute(&self) -> bool {
        matches!(self.kind(), E4XNodeKindReadOnly::Attribute)
    }

    pub fn is_comment(&self) -> bool {
        matches!(self.kind(), E4XNodeKindReadOnly::Comment)
    }

    pub fn is_processing_instruction(&self) -> bool {
        matches!(self.kind(), E4XNodeKindReadOnly::ProcessingInstruction)
    }

    /// "Simple content" = no element children (E4X 9.1.1.10). Text/attribute
    /// nodes are always simple; comment/PI are not.
    pub fn has_simple_content(&self) -> bool {
        match self.kind() {
            E4XNodeKindReadOnly::Text
            | E4XNodeKindReadOnly::Cdata
            | E4XNodeKindReadOnly::Attribute => true,
            E4XNodeKindReadOnly::Comment | E4XNodeKindReadOnly::ProcessingInstruction => false,
            E4XNodeKindReadOnly::Element => {
                let mut c = self.record().first_child_idx();
                while c != NO_NODE {
                    let cn = self.store.node(c);
                    if cn.kind() == E4XNodeKindReadOnly::Element {
                        return false;
                    }
                    c = cn.next_sibling;
                }
                true
            }
        }
    }

    pub fn has_complex_content(&self) -> bool {
        match self.kind() {
            E4XNodeKindReadOnly::Element => {
                let mut c = self.record().first_child_idx();
                while c != NO_NODE {
                    let cn = self.store.node(c);
                    if cn.kind() == E4XNodeKindReadOnly::Element {
                        return true;
                    }
                    c = cn.next_sibling;
                }
                false
            }
            _ => false,
        }
    }

    /// All child nodes (any kind), in document order.
    pub fn all_children(&self, out: &mut Vec<E4XNodeReadOnly<'gc>>) {
        let mut c = self.store.first_child(self.index);
        while c != NO_NODE {
            out.push(self.with_index(c));
            c = self.store.node(c).next_sibling;
        }
    }

    /// All attributes.
    pub fn all_attributes(&self, out: &mut Vec<E4XNodeReadOnly<'gc>>) {
        let mut a = self.record().first_attr;
        while a != NO_NODE {
            out.push(self.with_index(a));
            a = self.store.node(a).next_sibling;
        }
    }

    /// Text-node (and CDATA) children only.
    pub fn text_children(&self, out: &mut Vec<E4XNodeReadOnly<'gc>>) {
        let mut c = self.store.first_child(self.index);
        while c != NO_NODE {
            let cn = self.store.node(c);
            if matches!(
                cn.kind(),
                E4XNodeKindReadOnly::Text | E4XNodeKindReadOnly::Cdata
            ) {
                out.push(self.with_index(c));
            }
            c = cn.next_sibling;
        }
    }

    /// Comment children only.
    pub fn comment_children(&self, out: &mut Vec<E4XNodeReadOnly<'gc>>) {
        let mut c = self.record().first_child_idx();
        while c != NO_NODE {
            let cn = self.store.node(c);
            if cn.kind() == E4XNodeKindReadOnly::Comment {
                out.push(self.with_index(c));
            }
            c = cn.next_sibling;
        }
    }

    /// Processing-instruction children, optionally filtered by target name.
    pub fn pi_children(&self, want: Option<&str>, out: &mut Vec<E4XNodeReadOnly<'gc>>) {
        let mut c = self.record().first_child_idx();
        while c != NO_NODE {
            let cn = self.store.node(c);
            if cn.kind() == E4XNodeKindReadOnly::ProcessingInstruction {
                let matches = match want {
                    None | Some("*") => true,
                    Some(w) => cn.name != NO_NODE && self.store.str(cn.name) == w,
                };
                if matches {
                    out.push(self.with_index(c));
                }
            }
            c = cn.next_sibling;
        }
    }

    /// Two handles denote the same node iff they share the arena and index.
    /// Backs `===` / E4X node identity for read-only nodes.
    pub fn same_node(&self, other: &E4XNodeReadOnly<'gc>) -> bool {
        Gc::ptr_eq(self.store, other.store) && self.index == other.index
    }

    /// Deep structural equality (E4X 9.1.1.9 `[[Equals]]`): same name, namespace
    /// URI, and recursively-equal attributes (order-insensitive) and children
    /// (order-sensitive). Mirrors [`E4XNode::equals`](super::e4x::E4XNode::equals).
    pub fn deep_equals(&self, other: &E4XNodeReadOnly<'gc>) -> bool {
        let a = self.record();
        let b = other.record();

        let a_name = (a.name != NO_NODE).then(|| self.store.str(a.name));
        let b_name = (b.name != NO_NODE).then(|| other.store.str(b.name));
        if a_name != b_name {
            return false;
        }

        let a_ns = (a.ns != NO_NODE).then(|| self.store.str(a.ns));
        let b_ns = (b.ns != NO_NODE).then(|| other.store.str(b.ns));
        if a_ns != b_ns {
            return false;
        }

        use E4XNodeKindReadOnly::{
            Attribute, Cdata, Comment, Element, ProcessingInstruction, Text,
        };
        match (a.kind(), b.kind()) {
            (Text | Cdata, Text | Cdata)
            | (Comment, Comment)
            | (ProcessingInstruction, ProcessingInstruction)
            | (Attribute, Attribute) => self.value() == other.value(),
            (Element, Element) => {
                let mut a_children = Vec::new();
                self.all_children(&mut a_children);
                let mut b_children = Vec::new();
                other.all_children(&mut b_children);
                let mut a_attrs = Vec::new();
                self.all_attributes(&mut a_attrs);
                let mut b_attrs = Vec::new();
                other.all_attributes(&mut b_attrs);

                if a_children.len() != b_children.len() || a_attrs.len() != b_attrs.len() {
                    return false;
                }

                // Attributes can be in any order.
                for aa in &a_attrs {
                    if !b_attrs.iter().any(|ba| aa.deep_equals(ba)) {
                        return false;
                    }
                }

                a_children
                    .iter()
                    .zip(b_children.iter())
                    .all(|(x, y)| x.deep_equals(y))
            }
            _ => false,
        }
    }

    pub fn node_kind_str(&self) -> &'static str {
        match self.kind() {
            E4XNodeKindReadOnly::Element => "element",
            E4XNodeKindReadOnly::Text | E4XNodeKindReadOnly::Cdata => "text",
            E4XNodeKindReadOnly::Comment => "comment",
            E4XNodeKindReadOnly::ProcessingInstruction => "processing-instruction",
            E4XNodeKindReadOnly::Attribute => "attribute",
        }
    }

    /// The qualified element/attribute name as stored (`prefix:local`).
    fn qualified_name(&self) -> String {
        let n = self.record();
        let local = if n.name != NO_NODE {
            self.store.str(n.name)
        } else {
            ""
        };
        if n.prefix != NO_NODE {
            format!("{}:{}", self.store.str(n.prefix), local)
        } else {
            local.to_string()
        }
    }

    /// Serialise this node to an XML string (basic, no pretty-printing).
    pub fn write_xml_string(&self, out: &mut String) {
        let n = self.record();
        match n.kind() {
            E4XNodeKindReadOnly::Text => {
                if n.value_ref() != NO_NODE {
                    escape_into(self.store.str(n.value_ref()), false, out);
                }
            }
            E4XNodeKindReadOnly::Cdata => {
                out.push_str("<![CDATA[");
                if n.value_ref() != NO_NODE {
                    out.push_str(self.store.str(n.value_ref()));
                }
                out.push_str("]]>");
            }
            E4XNodeKindReadOnly::Comment => {
                out.push_str("<!--");
                if n.value_ref() != NO_NODE {
                    out.push_str(self.store.str(n.value_ref()));
                }
                out.push_str("-->");
            }
            E4XNodeKindReadOnly::ProcessingInstruction => {
                out.push_str("<?");
                if n.name != NO_NODE {
                    out.push_str(self.store.str(n.name));
                }
                if n.value_ref() != NO_NODE && !self.store.str(n.value_ref()).is_empty() {
                    out.push(' ');
                    out.push_str(self.store.str(n.value_ref()));
                }
                out.push_str("?>");
            }
            E4XNodeKindReadOnly::Attribute => {
                if n.value_ref() != NO_NODE {
                    out.push_str(self.store.str(n.value_ref()));
                }
            }
            E4XNodeKindReadOnly::Element => {
                let name = self.qualified_name();
                out.push('<');
                out.push_str(&name);

                // `xmlns` declarations made on this element.
                for d in self.store.decls.iter().filter(|d| d.node == self.index) {
                    let prefix = self.store.str(d.prefix);
                    out.push_str(" xmlns");
                    if !prefix.is_empty() {
                        out.push(':');
                        out.push_str(prefix);
                    }
                    out.push_str("=\"");
                    escape_into(self.store.str(d.uri), true, out);
                    out.push('"');
                }

                let mut a = n.first_attr;
                while a != NO_NODE {
                    let an = self.store.node(a);
                    out.push(' ');
                    out.push_str(&self.with_index(a).qualified_name());
                    out.push_str("=\"");
                    escape_into(self.store.str(an.value_ref()), true, out);
                    out.push('"');
                    a = an.next_sibling;
                }

                if n.value_ref() != NO_NODE {
                    // Collapsed sole text child: emit it as text content.
                    out.push('>');
                    escape_into(self.store.str(n.value_ref()), false, out);
                    out.push_str("</");
                    out.push_str(&name);
                    out.push('>');
                } else if n.first_child_idx() == NO_NODE {
                    out.push_str("/>");
                } else {
                    out.push('>');
                    let mut c = n.first_child_idx();
                    while c != NO_NODE {
                        self.with_index(c).write_xml_string(out);
                        c = self.store.node(c).next_sibling;
                    }
                    out.push_str("</");
                    out.push_str(&name);
                    out.push('>');
                }
            }
        }
    }
}

/// Minimal XML escaping for text/attribute content.
fn escape_into(s: &str, attribute: bool, out: &mut String) {
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attribute => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> E4XStoreReadOnly {
        E4XStoreReadOnly::parse(src, true, true, true)
    }

    impl E4XStoreReadOnly {
        fn root0(&self) -> u32 {
            self.roots[0]
        }
        fn child_named(&self, node: u32, name: &str) -> Vec<u32> {
            let mut out = Vec::new();
            let mut c = self.node(node).first_child_idx();
            while c != NO_NODE {
                let cn = self.node(c);
                if cn.kind() == E4XNodeKindReadOnly::Element
                    && cn.name != NO_NODE
                    && self.str(cn.name) == name
                {
                    out.push(c);
                }
                c = cn.next_sibling;
            }
            out
        }
        fn attr_named(&self, node: u32, name: &str) -> Option<&str> {
            let mut a = self.node(node).first_attr;
            while a != NO_NODE {
                let an = self.node(a);
                if an.name != NO_NODE && self.str(an.name) == name {
                    return Some(self.str(an.value_ref()));
                }
                a = an.next_sibling;
            }
            None
        }
        fn ns_uri_of(&self, node: u32) -> Option<&str> {
            let n = self.node(node);
            (n.ns != NO_NODE).then(|| self.str(n.ns))
        }
    }

    #[test]
    fn parses_and_strips_prefix() {
        // Namespaced elements (prefix `ns:`) must match by local name.
        let store = parse(
            r#"<ns:catalog xmlns:ns="urn:x"><ns:item id="1">A</ns:item><ns:item id="2">B</ns:item></ns:catalog>"#,
        );
        assert_eq!(store.roots().len(), 1);
        let root = store.root0();
        let items = store.child_named(root, "item");
        assert_eq!(items.len(), 2);
        assert_eq!(store.attr_named(items[0], "id"), Some("1"));
        // xmlns declaration must not become an attribute
        assert_eq!(store.attr_named(root, "xmlns"), None);
        // ...but its URI is resolved and recorded.
        assert_eq!(store.ns_uri_of(root), Some("urn:x"));
        assert_eq!(store.ns_uri_of(items[0]), Some("urn:x"));
        // ...and captured as a declaration for namespaceDeclarations().
        assert_eq!(store.decls.len(), 1);
    }

    #[test]
    fn interns_repeated_strings() {
        // The three `<a>` elements must share a single interned name entry.
        let store = parse(r#"<r><a x="1"/><a x="2"/><a x="3"/></r>"#);
        let root = store.root0();
        let a_nodes = store.child_named(root, "a");
        assert_eq!(a_nodes.len(), 3);
        let name0 = store.node(a_nodes[0]).name;
        assert!(a_nodes.iter().all(|&n| store.node(n).name == name0));
        assert_eq!(store.str(name0), "a");
    }

    #[test]
    fn unescapes_entities_and_cdata() {
        // Entities in text and attributes are decoded; CDATA is kept literal.
        let store = parse(r#"<r a="x&amp;y">A &lt;b&gt; C<![CDATA[<raw> & ]]></r>"#);
        let root = store.root0();
        assert_eq!(store.attr_named(root, "a"), Some("x&y"));
        let mut text = String::new();
        for i in 0..store.node_count {
            let n = store.node(i);
            if matches!(
                n.kind(),
                E4XNodeKindReadOnly::Text | E4XNodeKindReadOnly::Cdata
            ) {
                text.push_str(store.str(n.value_ref()));
            }
        }
        assert!(text.contains("A <b> C"), "got: {text:?}");
        assert!(text.contains("<raw> & "), "got: {text:?}");
    }

    #[test]
    fn default_namespace_is_resolved() {
        let store = parse(r#"<root xmlns="urn:d"><child/></root>"#);
        let root = store.root0();
        assert_eq!(store.ns_uri_of(root), Some("urn:d"));
        let child = store.child_named(root, "child")[0];
        assert_eq!(store.ns_uri_of(child), Some("urn:d"));
    }

    #[test]
    fn collapses_sole_text_child() {
        // `<a>hello</a>`: the text folds into the element; no Text node stored.
        let store = parse("<a>hello</a>");
        assert_eq!(store.node_count, 1);
        let a = store.root0();
        let an = store.node(a);
        assert_eq!(an.kind(), E4XNodeKindReadOnly::Element);
        assert_eq!(an.first_child_idx(), NO_NODE);
        assert_ne!(an.value_ref(), NO_NODE);
        assert_eq!(store.str(an.value_ref()), "hello");
        // The virtual child reconstitutes the text node on demand.
        let vc = store.first_child(a);
        assert_eq!(vc & VIRTUAL_TEXT, VIRTUAL_TEXT);
        let vn = store.node(vc);
        assert_eq!(vn.kind(), E4XNodeKindReadOnly::Text);
        assert_eq!(store.str(vn.value_ref()), "hello");
        assert_eq!(vn.parent_idx(), a);
        assert_eq!(vn.next_sibling, NO_NODE);
    }

    #[test]
    fn collapses_leaves_but_not_mixed_content() {
        // Leaf elements collapse; a mixed-content element keeps real nodes.
        // Nodes: r, a, b, m, text("x"), i  → 6 (no Text nodes for a/b).
        let store = parse("<r><a>1</a><b>2</b><m>x<i/></m></r>");
        assert_eq!(store.node_count, 6);
        let r = store.root0();

        let a = store.child_named(r, "a")[0];
        assert_eq!(store.node(a).first_child_idx(), NO_NODE);
        assert_eq!(store.str(store.node(a).value_ref()), "1");

        // `m` has text *and* an element child → not collapsed.
        let m = store.child_named(r, "m")[0];
        assert_ne!(store.node(m).first_child_idx(), NO_NODE);
        assert_eq!(store.node(m).value_ref(), NO_NODE);
        let first = store.node(m).first_child_idx();
        assert_eq!(store.node(first).kind(), E4XNodeKindReadOnly::Text);
        assert_eq!(store.str(store.node(first).value_ref()), "x");
    }

    #[test]
    fn spans_multiple_node_chunks() {
        // Build more nodes than one extent to exercise the chunk boundary
        // (index split + back-patching across finalized extents).
        let n = NODE_CHUNK_LEN + 1000;
        let mut src = String::from("<r>");
        for i in 0..n {
            src.push_str(&format!("<i k=\"{i}\"/>"));
        }
        src.push_str("</r>");
        let store = parse(&src);
        let root = store.root0();
        let items = store.child_named(root, "i");
        assert_eq!(items.len(), n);
        // First and a past-the-boundary item resolve correctly.
        assert_eq!(store.attr_named(items[0], "k"), Some("0"));
        let last = n - 1;
        assert_eq!(
            store.attr_named(items[last], "k"),
            Some(last.to_string().as_str())
        );
        assert!(store.node_chunks.len() >= 2);
    }
}
