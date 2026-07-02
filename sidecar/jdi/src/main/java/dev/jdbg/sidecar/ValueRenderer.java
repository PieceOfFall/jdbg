package dev.jdbg.sidecar;

import com.sun.jdi.ArrayReference;
import com.sun.jdi.ClassType;
import com.sun.jdi.Field;
import com.sun.jdi.InterfaceType;
import com.sun.jdi.ObjectReference;
import com.sun.jdi.PrimitiveValue;
import com.sun.jdi.ReferenceType;
import com.sun.jdi.StringReference;
import com.sun.jdi.Value;

import java.util.ArrayList;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;

final class ValueRenderer {
    private ValueRenderer() {
    }

    static Map<String, Object> render(Value value, Map<String, Object> limits) {
        RenderLimits parsed = RenderLimits.from(limits);
        RenderState state = new RenderState(parsed);
        return renderValue(value, parsed.maxDepth, state);
    }

    static String display(Value value) {
        if (value == null) {
            return "null";
        }
        if (value instanceof StringReference) {
            return quote(((StringReference) value).value());
        }
        if (value instanceof PrimitiveValue) {
            return value.toString();
        }
        if (value instanceof ObjectReference) {
            ObjectReference ref = (ObjectReference) value;
            return ref.referenceType().name() + "@" + ref.uniqueID();
        }
        return value.toString();
    }

    private static Map<String, Object> renderValue(Value value, int depth, RenderState state) {
        if (value == null) {
            return Json.object("kind", "null");
        }
        if (value instanceof PrimitiveValue) {
            return Json.object(
                    "kind", "primitive",
                    "type", value.type().name(),
                    "value", value.toString()
            );
        }
        if (value instanceof StringReference) {
            String raw = ((StringReference) value).value();
            boolean truncated = raw.length() > state.limits.maxStringLength;
            String text = truncated ? raw.substring(0, state.limits.maxStringLength) : raw;
            return Json.object(
                    "kind", "string",
                    "type", value.type().name(),
                    "value", text,
                    "truncated", truncated
            );
        }
        ObjectReference ref = (ObjectReference) value;
        if (!state.enter(ref)) {
            return Json.object(
                    "kind", "cycle",
                    "type", ref.referenceType().name(),
                    "objectId", ref.uniqueID()
            );
        }
        try {
            if (depth <= 0 || state.objectCount > state.limits.maxObjects) {
                return Json.object(
                        "kind", "truncated",
                        "type", ref.referenceType().name(),
                        "objectId", ref.uniqueID()
                );
            }
            if (ref instanceof ArrayReference) {
                return renderArray((ArrayReference) ref, depth, state);
            }
            if (isEnum(ref)) {
                return renderEnum(ref);
            }
            if (isSubtypeOf(ref.referenceType(), "java.util.Map")) {
                return renderMap(ref, depth, state);
            }
            if (isSubtypeOf(ref.referenceType(), "java.util.Collection")) {
                return renderCollection(ref, depth, state);
            }
            return renderObject(ref, depth, state);
        } finally {
            state.leave(ref);
        }
    }

    private static Map<String, Object> renderArray(ArrayReference array, int depth, RenderState state) {
        int length = array.length();
        int count = Math.min(length, state.limits.maxArrayLength);
        List<Object> elements = Json.array();
        for (int i = 0; i < count; i++) {
            elements.add(Json.object(
                    "index", i,
                    "value", renderValue(array.getValue(i), depth - 1, state)
            ));
        }
        return Json.object(
                "kind", "array",
                "type", array.referenceType().name(),
                "length", length,
                "elements", elements,
                "truncated", length > count
        );
    }

    private static Map<String, Object> renderEnum(ObjectReference ref) {
        Value name = fieldValue(ref, "name");
        Value ordinal = fieldValue(ref, "ordinal");
        return Json.object(
                "kind", "enum",
                "type", ref.referenceType().name(),
                "objectId", ref.uniqueID(),
                "name", name instanceof StringReference ? ((StringReference) name).value() : null,
                "ordinal", ordinal instanceof PrimitiveValue ? ((PrimitiveValue) ordinal).intValue() : null
        );
    }

    private static Map<String, Object> renderCollection(ObjectReference ref, int depth, RenderState state) {
        ObjectReference delegate = collectionDelegate(ref);
        if (delegate != null && delegate.uniqueID() != ref.uniqueID()) {
            Map<String, Object> rendered = renderCollection(delegate, depth, state);
            rendered.put("type", ref.referenceType().name());
            rendered.put("objectId", ref.uniqueID());
            return rendered;
        }

        List<Object> elements = Json.array();
        Integer size = intField(ref, "size");

        ArrayReference backing = arrayField(ref, "elementData");
        if (backing == null) {
            backing = arrayField(ref, "a");
        }
        if (backing != null) {
            if (size == null) {
                size = backing.length();
            }
            int count = Math.min(Math.min(size, backing.length()), state.limits.maxArrayLength);
            for (int i = 0; i < count; i++) {
                elements.add(Json.object(
                        "index", i,
                        "value", safeRender(backing.getValue(i), depth - 1, state)
                ));
            }
        }

        if (elements.isEmpty()) {
            ObjectReference first = objectField(ref, "first");
            ObjectReference node = first;
            Set<Long> seen = new HashSet<>();
            while (node != null && elements.size() < state.limits.maxArrayLength && seen.add(node.uniqueID())) {
                elements.add(Json.object(
                        "index", elements.size(),
                        "value", safeRender(fieldValue(node, "item"), depth - 1, state)
                ));
                node = objectField(node, "next");
            }
        }

        if (elements.isEmpty()) {
            ArrayReference deque = arrayField(ref, "elements");
            Integer head = intField(ref, "head");
            Integer tail = intField(ref, "tail");
            if (deque != null && head != null && tail != null && deque.length() > 0) {
                int computedSize = tail >= head ? tail - head : deque.length() - head + tail;
                if (size == null) {
                    size = computedSize;
                }
                int count = Math.min(computedSize, state.limits.maxArrayLength);
                for (int i = 0; i < count; i++) {
                    int index = (head + i) % deque.length();
                    elements.add(Json.object(
                            "index", i,
                            "value", safeRender(deque.getValue(index), depth - 1, state)
                    ));
                }
            }
        }

        if (elements.isEmpty()) {
            ObjectReference map = objectField(ref, "map");
            if (map == null) {
                map = objectField(ref, "m");
            }
            if (map != null && isSubtypeOf(map.referenceType(), "java.util.Map")) {
                if (size == null) {
                    size = intField(map, "size");
                }
                List<ObjectReference> entries = mapEntryObjects(map, state.limits.maxArrayLength);
                for (ObjectReference entry : entries) {
                    elements.add(Json.object(
                            "index", elements.size(),
                            "value", safeRender(fieldValue(entry, "key"), depth - 1, state)
                    ));
                }
            }
        }

        int rendered = elements.size();
        boolean unavailable = size != null && size > 0 && rendered == 0;
        return Json.object(
                "kind", "collection",
                "type", ref.referenceType().name(),
                "objectId", ref.uniqueID(),
                "size", size,
                "elements", elements,
                "truncated", size != null && size > rendered,
                "unavailable", unavailable
        );
    }

    private static Map<String, Object> renderMap(ObjectReference ref, int depth, RenderState state) {
        ObjectReference delegate = mapDelegate(ref);
        if (delegate != null && delegate.uniqueID() != ref.uniqueID()) {
            Map<String, Object> rendered = renderMap(delegate, depth, state);
            rendered.put("type", ref.referenceType().name());
            rendered.put("objectId", ref.uniqueID());
            return rendered;
        }

        Integer size = intField(ref, "size");
        List<Object> entries = Json.array();
        for (ObjectReference entry : mapEntryObjects(ref, state.limits.maxArrayLength)) {
            entries.add(renderMapEntry(entry, depth, state));
        }

        return Json.object(
                "kind", "map",
                "type", ref.referenceType().name(),
                "objectId", ref.uniqueID(),
                "size", size,
                "entries", entries,
                "truncated", size != null && size > entries.size(),
                "unavailable", size != null && size > 0 && entries.isEmpty()
        );
    }

    private static ObjectReference collectionDelegate(ObjectReference ref) {
        String[] names = {"list", "c", "collection"};
        for (String name : names) {
            ObjectReference candidate = objectField(ref, name);
            if (candidate != null && isSubtypeOf(candidate.referenceType(), "java.util.Collection")) {
                return candidate;
            }
        }
        return null;
    }

    private static ObjectReference mapDelegate(ObjectReference ref) {
        String[] names = {"m", "map"};
        for (String name : names) {
            ObjectReference candidate = objectField(ref, name);
            if (candidate != null && isSubtypeOf(candidate.referenceType(), "java.util.Map")) {
                return candidate;
            }
        }
        return null;
    }

    private static List<ObjectReference> mapEntryObjects(ObjectReference ref, int maxEntries) {
        List<ObjectReference> entries = new ArrayList<>();
        Set<Long> seenEntries = new HashSet<>();

        ObjectReference entry = objectField(ref, "head");
        while (entry != null && entries.size() < maxEntries && seenEntries.add(entry.uniqueID())) {
            entries.add(entry);
            entry = objectField(entry, "after");
        }

        if (entries.isEmpty()) {
            ArrayReference table = arrayField(ref, "table");
            if (table != null) {
                for (int i = 0; i < table.length() && entries.size() < maxEntries; i++) {
                    Value bucket = table.getValue(i);
                    entry = bucket instanceof ObjectReference ? (ObjectReference) bucket : null;
                    while (entry != null && entries.size() < maxEntries && seenEntries.add(entry.uniqueID())) {
                        entries.add(entry);
                        entry = objectField(entry, "next");
                    }
                }
            }
        }

        if (entries.isEmpty()) {
            collectTreeEntries(objectField(ref, "root"), entries, seenEntries, maxEntries);
        }

        return entries;
    }

    private static void collectTreeEntries(
            ObjectReference node,
            List<ObjectReference> entries,
            Set<Long> seenEntries,
            int maxEntries
    ) {
        if (node == null || entries.size() >= maxEntries || !seenEntries.add(node.uniqueID())) {
            return;
        }
        collectTreeEntries(objectField(node, "left"), entries, seenEntries, maxEntries);
        if (entries.size() < maxEntries) {
            entries.add(node);
        }
        collectTreeEntries(objectField(node, "right"), entries, seenEntries, maxEntries);
    }

    private static Map<String, Object> renderMapEntry(ObjectReference entry, int depth, RenderState state) {
        return Json.object(
                "key", safeRender(fieldValue(entry, "key"), depth - 1, state),
                "value", safeRender(fieldValue(entry, "value"), depth - 1, state)
        );
    }

    private static Map<String, Object> renderObject(ObjectReference ref, int depth, RenderState state) {
        List<Object> fields = Json.array();
        int seen = 0;
        boolean truncated = false;
        for (Field field : ref.referenceType().allFields()) {
            if (!state.limits.includeStatic && field.isStatic()) {
                continue;
            }
            if (!state.limits.includeSynthetic && field.isSynthetic()) {
                continue;
            }
            if (seen >= state.limits.maxFields) {
                truncated = true;
                break;
            }
            fields.add(Json.object(
                    "name", field.name(),
                    "type", field.typeName(),
                    "static", field.isStatic(),
                    "value", safeRender(ref.getValue(field), depth - 1, state)
            ));
            seen++;
        }
        return Json.object(
                "kind", "object",
                "type", ref.referenceType().name(),
                "objectId", ref.uniqueID(),
                "fields", fields,
                "truncated", truncated
        );
    }

    private static Map<String, Object> safeRender(Value value, int depth, RenderState state) {
        try {
            return renderValue(value, depth, state);
        } catch (RuntimeException e) {
            return Json.object("kind", "unavailable", "message", e.getMessage());
        }
    }

    private static boolean isEnum(ObjectReference ref) {
        return isSubtypeOf(ref.referenceType(), "java.lang.Enum");
    }

    private static boolean isSubtypeOf(ReferenceType type, String expectedName) {
        if (type.name().equals(expectedName)) {
            return true;
        }
        if (type instanceof ClassType) {
            ClassType cls = (ClassType) type;
            for (InterfaceType iface : cls.allInterfaces()) {
                if (interfaceMatches(iface, expectedName)) {
                    return true;
                }
            }
            ClassType superClass = cls.superclass();
            while (superClass != null) {
                if (superClass.name().equals(expectedName)) {
                    return true;
                }
                for (InterfaceType iface : superClass.allInterfaces()) {
                    if (interfaceMatches(iface, expectedName)) {
                        return true;
                    }
                }
                superClass = superClass.superclass();
            }
        }
        if (type instanceof InterfaceType) {
            return interfaceMatches((InterfaceType) type, expectedName);
        }
        return false;
    }

    private static boolean interfaceMatches(InterfaceType iface, String expectedName) {
        if (iface.name().equals(expectedName)) {
            return true;
        }
        for (InterfaceType parent : iface.superinterfaces()) {
            if (interfaceMatches(parent, expectedName)) {
                return true;
            }
        }
        return false;
    }

    private static Field field(ObjectReference ref, String name) {
        for (Field field : ref.referenceType().allFields()) {
            if (field.name().equals(name)) {
                return field;
            }
        }
        return null;
    }

    private static Value fieldValue(ObjectReference ref, String name) {
        Field field = field(ref, name);
        return field == null ? null : ref.getValue(field);
    }

    private static Integer intField(ObjectReference ref, String name) {
        Value value = fieldValue(ref, name);
        return value instanceof PrimitiveValue ? ((PrimitiveValue) value).intValue() : null;
    }

    private static ObjectReference objectField(ObjectReference ref, String name) {
        Value value = fieldValue(ref, name);
        return value instanceof ObjectReference ? (ObjectReference) value : null;
    }

    private static ArrayReference arrayField(ObjectReference ref, String name) {
        Value value = fieldValue(ref, name);
        return value instanceof ArrayReference ? (ArrayReference) value : null;
    }

    private static String quote(String value) {
        return "\"" + value.replace("\\", "\\\\").replace("\"", "\\\"") + "\"";
    }

    private static final class RenderState {
        final RenderLimits limits;
        final Set<Long> path = new HashSet<>();
        int objectCount;

        RenderState(RenderLimits limits) {
            this.limits = limits;
        }

        boolean enter(ObjectReference ref) {
            objectCount++;
            return path.add(ref.uniqueID());
        }

        void leave(ObjectReference ref) {
            path.remove(ref.uniqueID());
        }
    }

    private static final class RenderLimits {
        final int maxDepth;
        final int maxFields;
        final int maxArrayLength;
        final int maxStringLength;
        final int maxObjects;
        final boolean includeStatic;
        final boolean includeSynthetic;

        RenderLimits(
                int maxDepth,
                int maxFields,
                int maxArrayLength,
                int maxStringLength,
                int maxObjects,
                boolean includeStatic,
                boolean includeSynthetic
        ) {
            this.maxDepth = maxDepth;
            this.maxFields = maxFields;
            this.maxArrayLength = maxArrayLength;
            this.maxStringLength = maxStringLength;
            this.maxObjects = maxObjects;
            this.includeStatic = includeStatic;
            this.includeSynthetic = includeSynthetic;
        }

        static RenderLimits from(Map<String, Object> limits) {
            if (limits == null) {
                limits = Json.object();
            }
            return new RenderLimits(
                    Json.intValue(limits, "maxDepth", 3),
                    Json.intValue(limits, "maxFields", 100),
                    Json.intValue(limits, "maxArrayLength", 50),
                    Json.intValue(limits, "maxStringLength", 4096),
                    Json.intValue(limits, "maxObjects", 1000),
                    Boolean.TRUE.equals(limits.get("includeStatic")),
                    Boolean.TRUE.equals(limits.get("includeSynthetic"))
            );
        }
    }
}
