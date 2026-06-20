package {
    // Read-only, immutable parallel to XML. Parsed once into a compact arena;
    // supports E4X read syntax (`.child`, `..desc`, `@attr`, `for each`, `.()`)
    // via native dispatch, but cannot be mutated and emits no change
    // notifications. Navigation returns a real XMLList (whose elements are
    // XMLReadOnly), so `is XMLList` holds and results feed XMLListCollection.
    [Ruffle(InstanceAllocator)]
    public final dynamic class XMLReadOnly {
        public function XMLReadOnly(value:* = void 0) {
            this.init(value);
        }

        private native function init(value:*):void;

        // Static fast path used by com.terna.collections.KeyedSort to sort an
        // Array of XMLReadOnly rows entirely in Rust (key extraction + sort +
        // in-place permutation), with no per-comparison ActionScript callback
        // and no intermediate XMLList allocations. See `xml_read_only::sort_keyed`.
        //   fields[j]:  ""=node itself, "@n"=attribute n, "n"=child element n
        //   kinds[j]:   0=string, 1=numeric, 2=lowercased string
        // Returns true when sorted; false if `items` aren't all XMLReadOnly.
        public static native function sortKeyed(items:Array, fields:Array, kinds:Array, descending:Array):Boolean;

        AS3 native function toString():String;
        AS3 native function toXMLString():String;
        AS3 native function length():int;
        AS3 native function localName():Object;
        AS3 native function name():Object;
        AS3 native function nodeKind():String;
        AS3 native function hasSimpleContent():Boolean;
        AS3 native function hasComplexContent():Boolean;
        AS3 native function parent():*;
        AS3 native function child(name:*):XMLList;
        AS3 native function children():XMLList;
        AS3 native function elements(name:* = "*"):XMLList;
        AS3 native function attribute(name:*):XMLList;
        AS3 native function attributes():XMLList;
        AS3 native function descendants(name:Object = "*"):XMLList;
        AS3 native function text():XMLList;
        AS3 native function copy():XMLReadOnly;
        AS3 native function comments():XMLList;
        AS3 native function processingInstructions(name:* = "*"):XMLList;
        AS3 native function inScopeNamespaces():Array;
        AS3 native function namespaceDeclarations():Array;

        AS3 native function contains(value:*):Boolean;
        AS3 native function childIndex():int;

        AS3 function toJSON(k:String):* {
            return this.toJSON(k);
        }

        // Read-only XML reports `is XML` true (Ruffle masquerade), so Flex SDK
        // code that branches on `node is XML` (e.g. UIDUtil.getUID) calls these.
        AS3 native function notification():Function;
        AS3 native function setNotification(f:Function):*;

        // Mutation API: unsupported on a read-only document. Declared so the
        // `is XML` masquerade finds them (full XML surface); each is a Ruffle
        // stub that emits a WARN and is a benign no-op (XML-returning mutators
        // return `this`, void mutators return nothing).
        AS3 native function addNamespace(ns:*):XML;
        AS3 native function appendChild(child:*):XML;
        AS3 native function prependChild(child:*):XML;
        AS3 native function insertChildAfter(child1:*, child2:*):*;
        AS3 native function insertChildBefore(child1:*, child2:*):*;
        AS3 native function normalize():XML;
        AS3 native function removeNamespace(ns:*):XML;
        AS3 native function replace(propertyName:*, value:*):XML;
        AS3 native function setChildren(value:*):XML;
        AS3 native function setLocalName(name:*):void;
        AS3 native function setName(name:*):void;
        AS3 native function setNamespace(ns:*):void;

        // `namespace` is an AS3 keyword, so it can't be a native function name.
        // Mirror XML.as: a public AS3 wrapper forwarding to a private native.
        private native function namespace_internal_impl(hasPrefix:Boolean, prefix:String = null):*;
        AS3 function namespace(prefix:* = null):* {
            return namespace_internal_impl(arguments.length > 0, prefix);
        }

        AS3 function valueOf():XMLReadOnly {
            return this;
        }

        prototype.toString = function():String {
            if (this === prototype) {
                return "";
            }
            var self:XMLReadOnly = this;
            return self.AS3::toString();
        };

        prototype.valueOf = function():XMLReadOnly {
            var self:XMLReadOnly = this;
            return self.AS3::valueOf();
        };

        prototype.toXMLString = function():String {
            var self:XMLReadOnly = this;
            return self.AS3::toXMLString();
        };

        prototype.length = function():int {
            var self:XMLReadOnly = this;
            return self.AS3::length();
        };

        prototype.localName = function():Object {
            var self:XMLReadOnly = this;
            return self.AS3::localName();
        };

        prototype.name = function():Object {
            var self:XMLReadOnly = this;
            return self.AS3::name();
        };

        prototype.nodeKind = function():String {
            var self:XMLReadOnly = this;
            return self.AS3::nodeKind();
        };

        prototype.hasSimpleContent = function():Boolean {
            var self:XMLReadOnly = this;
            return self.AS3::hasSimpleContent();
        };

        prototype.hasComplexContent = function():Boolean {
            var self:XMLReadOnly = this;
            return self.AS3::hasComplexContent();
        };

        prototype.parent = function():* {
            var self:XMLReadOnly = this;
            return self.AS3::parent();
        };

        prototype.child = function(name:*):XMLList {
            var self:XMLReadOnly = this;
            return self.AS3::child(name);
        };

        prototype.children = function():XMLList {
            var self:XMLReadOnly = this;
            return self.AS3::children();
        };

        prototype.elements = function(name:* = "*"):XMLList {
            var self:XMLReadOnly = this;
            return self.AS3::elements(name);
        };

        prototype.attribute = function(name:*):XMLList {
            var self:XMLReadOnly = this;
            return self.AS3::attribute(name);
        };

        prototype.attributes = function():XMLList {
            var self:XMLReadOnly = this;
            return self.AS3::attributes();
        };

        prototype.descendants = function(name:* = "*"):XMLList {
            var self:XMLReadOnly = this;
            return self.AS3::descendants(name);
        };

        prototype.text = function():XMLList {
            var self:XMLReadOnly = this;
            return self.AS3::text();
        };

        prototype.copy = function():XMLReadOnly {
            var self:XMLReadOnly = this;
            return self.AS3::copy();
        };

        prototype.comments = function():XMLList {
            var self:XMLReadOnly = this;
            return self.AS3::comments();
        };

        prototype.processingInstructions = function(name:* = "*"):XMLList {
            var self:XMLReadOnly = this;
            return self.AS3::processingInstructions(name);
        };

        prototype.namespace = function(prefix:* = null):* {
            var self:XMLReadOnly = this;
            return self.AS3::namespace.apply(self, arguments);
        };

        prototype.inScopeNamespaces = function():Array {
            var self:XMLReadOnly = this;
            return self.AS3::inScopeNamespaces();
        };

        prototype.namespaceDeclarations = function():Array {
            var self:XMLReadOnly = this;
            return self.AS3::namespaceDeclarations();
        };

        prototype.notification = function():Function {
            var self:XMLReadOnly = this;
            return self.AS3::notification();
        };

        prototype.setNotification = function(f:Function):* {
            var self:XMLReadOnly = this;
            return self.AS3::setNotification(f);
        };

        prototype.contains = function(value:*):Boolean {
            var self:XMLReadOnly = this;
            return self.AS3::contains(value);
        };

        prototype.childIndex = function():int {
            var self:XMLReadOnly = this;
            return self.AS3::childIndex();
        };

        prototype.toJSON = function(k:String):* {
            var self:XMLReadOnly = this;
            return self.AS3::toJSON(k);
        };

        prototype.addNamespace = function(ns:*):XML {
            var self:XMLReadOnly = this;
            return self.AS3::addNamespace(ns);
        };

        prototype.appendChild = function(child:*):XML {
            var self:XMLReadOnly = this;
            return self.AS3::appendChild(child);
        };

        prototype.prependChild = function(child:*):XML {
            var self:XMLReadOnly = this;
            return self.AS3::prependChild(child);
        };

        prototype.insertChildAfter = function(child1:*, child2:*):* {
            var self:XMLReadOnly = this;
            return self.AS3::insertChildAfter(child1, child2);
        };

        prototype.insertChildBefore = function(child1:*, child2:*):* {
            var self:XMLReadOnly = this;
            return self.AS3::insertChildBefore(child1, child2);
        };

        prototype.normalize = function():XML {
            var self:XMLReadOnly = this;
            return self.AS3::normalize();
        };

        prototype.removeNamespace = function(ns:*):XML {
            var self:XMLReadOnly = this;
            return self.AS3::removeNamespace(ns);
        };

        prototype.replace = function(propertyName:*, value:*):XML {
            var self:XMLReadOnly = this;
            return self.AS3::replace(propertyName, value);
        };

        prototype.setChildren = function(value:*):XML {
            var self:XMLReadOnly = this;
            return self.AS3::setChildren(value);
        };

        prototype.setLocalName = function(name:*):void {
            var self:XMLReadOnly = this;
            self.AS3::setLocalName(name);
        };

        prototype.setName = function(name:*):void {
            var self:XMLReadOnly = this;
            self.AS3::setName(name);
        };

        prototype.setNamespace = function(ns:*):void {
            var self:XMLReadOnly = this;
            self.AS3::setNamespace(ns);
        };

        // Prototype methods must be non-enumerable, else the `for each`/`for in`
        // proto-walk would yield them as values once the node enumeration is
        // exhausted. Same pattern as Object.as / XML.as.
        prototype.setPropertyIsEnumerable("toString", false);
        prototype.setPropertyIsEnumerable("valueOf", false);
        prototype.setPropertyIsEnumerable("toXMLString", false);
        prototype.setPropertyIsEnumerable("length", false);
        prototype.setPropertyIsEnumerable("localName", false);
        prototype.setPropertyIsEnumerable("name", false);
        prototype.setPropertyIsEnumerable("nodeKind", false);
        prototype.setPropertyIsEnumerable("hasSimpleContent", false);
        prototype.setPropertyIsEnumerable("hasComplexContent", false);
        prototype.setPropertyIsEnumerable("parent", false);
        prototype.setPropertyIsEnumerable("child", false);
        prototype.setPropertyIsEnumerable("children", false);
        prototype.setPropertyIsEnumerable("elements", false);
        prototype.setPropertyIsEnumerable("attribute", false);
        prototype.setPropertyIsEnumerable("attributes", false);
        prototype.setPropertyIsEnumerable("descendants", false);
        prototype.setPropertyIsEnumerable("text", false);
        prototype.setPropertyIsEnumerable("copy", false);
        prototype.setPropertyIsEnumerable("comments", false);
        prototype.setPropertyIsEnumerable("processingInstructions", false);
        prototype.setPropertyIsEnumerable("namespace", false);
        prototype.setPropertyIsEnumerable("inScopeNamespaces", false);
        prototype.setPropertyIsEnumerable("namespaceDeclarations", false);
        prototype.setPropertyIsEnumerable("notification", false);
        prototype.setPropertyIsEnumerable("setNotification", false);
        prototype.setPropertyIsEnumerable("contains", false);
        prototype.setPropertyIsEnumerable("childIndex", false);
        prototype.setPropertyIsEnumerable("toJSON", false);
        prototype.setPropertyIsEnumerable("addNamespace", false);
        prototype.setPropertyIsEnumerable("appendChild", false);
        prototype.setPropertyIsEnumerable("prependChild", false);
        prototype.setPropertyIsEnumerable("insertChildAfter", false);
        prototype.setPropertyIsEnumerable("insertChildBefore", false);
        prototype.setPropertyIsEnumerable("normalize", false);
        prototype.setPropertyIsEnumerable("removeNamespace", false);
        prototype.setPropertyIsEnumerable("replace", false);
        prototype.setPropertyIsEnumerable("setChildren", false);
        prototype.setPropertyIsEnumerable("setLocalName", false);
        prototype.setPropertyIsEnumerable("setName", false);
        prototype.setPropertyIsEnumerable("setNamespace", false);
    }
}
