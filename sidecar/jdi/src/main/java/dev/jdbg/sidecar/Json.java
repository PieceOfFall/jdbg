package dev.jdbg.sidecar;

import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

final class Json {
    private Json() {
    }

    static Map<String, Object> object(Object... kv) {
        Map<String, Object> out = new LinkedHashMap<>();
        for (int i = 0; i < kv.length; i += 2) {
            out.put((String) kv[i], kv[i + 1]);
        }
        return out;
    }

    static List<Object> array() {
        return new ArrayList<>();
    }

    static Object parse(String text) {
        Parser parser = new Parser(text);
        Object value = parser.parseValue();
        parser.skipWhitespace();
        if (!parser.atEnd()) {
            throw new IllegalArgumentException("trailing JSON content");
        }
        return value;
    }

    static String stringify(Object value) {
        StringBuilder out = new StringBuilder();
        writeValue(out, value);
        return out.toString();
    }

    @SuppressWarnings("unchecked")
    static Map<String, Object> asObject(Object value, String label) {
        if (value instanceof Map<?, ?>) {
            return (Map<String, Object>) value;
        }
        throw new IllegalArgumentException(label + " must be an object");
    }

    @SuppressWarnings("unchecked")
    static List<Object> asList(Object value, String label) {
        if (value instanceof List<?>) {
            return (List<Object>) value;
        }
        throw new IllegalArgumentException(label + " must be an array");
    }

    static String string(Map<String, Object> map, String key) {
        Object value = map.get(key);
        if (value instanceof String) {
            return (String) value;
        }
        throw new IllegalArgumentException(key + " must be a string");
    }

    static String optionalString(Map<String, Object> map, String key, String fallback) {
        Object value = map.get(key);
        return value instanceof String ? (String) value : fallback;
    }

    static int intValue(Map<String, Object> map, String key, int fallback) {
        Object value = map.get(key);
        if (value == null) {
            return fallback;
        }
        if (value instanceof Number) {
            return ((Number) value).intValue();
        }
        throw new IllegalArgumentException(key + " must be a number");
    }

    static long longValue(Map<String, Object> map, String key, long fallback) {
        Object value = map.get(key);
        if (value == null) {
            return fallback;
        }
        if (value instanceof Number) {
            return ((Number) value).longValue();
        }
        throw new IllegalArgumentException(key + " must be a number");
    }

    private static void writeValue(StringBuilder out, Object value) {
        if (value == null) {
            out.append("null");
        } else if (value instanceof String) {
            writeString(out, (String) value);
        } else if (value instanceof Number || value instanceof Boolean) {
            out.append(value);
        } else if (value instanceof Map<?, ?>) {
            boolean first = true;
            out.append('{');
            for (Map.Entry<?, ?> entry : ((Map<?, ?>) value).entrySet()) {
                if (!first) {
                    out.append(',');
                }
                first = false;
                writeString(out, String.valueOf(entry.getKey()));
                out.append(':');
                writeValue(out, entry.getValue());
            }
            out.append('}');
        } else if (value instanceof Iterable<?>) {
            boolean first = true;
            out.append('[');
            for (Object item : (Iterable<?>) value) {
                if (!first) {
                    out.append(',');
                }
                first = false;
                writeValue(out, item);
            }
            out.append(']');
        } else {
            writeString(out, String.valueOf(value));
        }
    }

    private static void writeString(StringBuilder out, String value) {
        out.append('"');
        for (int i = 0; i < value.length(); i++) {
            char c = value.charAt(i);
            switch (c) {
                case '"':
                    out.append("\\\"");
                    break;
                case '\\':
                    out.append("\\\\");
                    break;
                case '\b':
                    out.append("\\b");
                    break;
                case '\f':
                    out.append("\\f");
                    break;
                case '\n':
                    out.append("\\n");
                    break;
                case '\r':
                    out.append("\\r");
                    break;
                case '\t':
                    out.append("\\t");
                    break;
                default:
                    if (c < 0x20) {
                        out.append(String.format("\\u%04x", (int) c));
                    } else {
                        out.append(c);
                    }
            }
        }
        out.append('"');
    }

    private static final class Parser {
        private final String text;
        private int pos;

        Parser(String text) {
            this.text = text;
        }

        boolean atEnd() {
            return pos >= text.length();
        }

        void skipWhitespace() {
            while (!atEnd() && Character.isWhitespace(text.charAt(pos))) {
                pos++;
            }
        }

        Object parseValue() {
            skipWhitespace();
            if (atEnd()) {
                throw new IllegalArgumentException("unexpected end of JSON");
            }
            char c = text.charAt(pos);
            if (c == '"') {
                return parseString();
            }
            if (c == '{') {
                return parseObject();
            }
            if (c == '[') {
                return parseArray();
            }
            if (text.startsWith("true", pos)) {
                pos += 4;
                return Boolean.TRUE;
            }
            if (text.startsWith("false", pos)) {
                pos += 5;
                return Boolean.FALSE;
            }
            if (text.startsWith("null", pos)) {
                pos += 4;
                return null;
            }
            return parseNumber();
        }

        private Map<String, Object> parseObject() {
            Map<String, Object> map = new LinkedHashMap<>();
            pos++;
            skipWhitespace();
            if (peek('}')) {
                pos++;
                return map;
            }
            while (true) {
                String key = parseString();
                skipWhitespace();
                expect(':');
                Object value = parseValue();
                map.put(key, value);
                skipWhitespace();
                if (peek('}')) {
                    pos++;
                    return map;
                }
                expect(',');
            }
        }

        private List<Object> parseArray() {
            List<Object> list = new ArrayList<>();
            pos++;
            skipWhitespace();
            if (peek(']')) {
                pos++;
                return list;
            }
            while (true) {
                list.add(parseValue());
                skipWhitespace();
                if (peek(']')) {
                    pos++;
                    return list;
                }
                expect(',');
            }
        }

        private String parseString() {
            expect('"');
            StringBuilder out = new StringBuilder();
            while (!atEnd()) {
                char c = text.charAt(pos++);
                if (c == '"') {
                    return out.toString();
                }
                if (c != '\\') {
                    out.append(c);
                    continue;
                }
                if (atEnd()) {
                    throw new IllegalArgumentException("unterminated escape");
                }
                char esc = text.charAt(pos++);
                switch (esc) {
                    case '"':
                    case '\\':
                    case '/':
                        out.append(esc);
                        break;
                    case 'b':
                        out.append('\b');
                        break;
                    case 'f':
                        out.append('\f');
                        break;
                    case 'n':
                        out.append('\n');
                        break;
                    case 'r':
                        out.append('\r');
                        break;
                    case 't':
                        out.append('\t');
                        break;
                    case 'u':
                        if (pos + 4 > text.length()) {
                            throw new IllegalArgumentException("short unicode escape");
                        }
                        out.append((char) Integer.parseInt(text.substring(pos, pos + 4), 16));
                        pos += 4;
                        break;
                    default:
                        throw new IllegalArgumentException("invalid escape: " + esc);
                }
            }
            throw new IllegalArgumentException("unterminated string");
        }

        private Number parseNumber() {
            int start = pos;
            if (peek('-')) {
                pos++;
            }
            while (!atEnd() && Character.isDigit(text.charAt(pos))) {
                pos++;
            }
            boolean floating = false;
            if (!atEnd() && text.charAt(pos) == '.') {
                floating = true;
                pos++;
                while (!atEnd() && Character.isDigit(text.charAt(pos))) {
                    pos++;
                }
            }
            if (!atEnd() && (text.charAt(pos) == 'e' || text.charAt(pos) == 'E')) {
                floating = true;
                pos++;
                if (!atEnd() && (text.charAt(pos) == '+' || text.charAt(pos) == '-')) {
                    pos++;
                }
                while (!atEnd() && Character.isDigit(text.charAt(pos))) {
                    pos++;
                }
            }
            String raw = text.substring(start, pos);
            if (floating) {
                return Double.parseDouble(raw);
            }
            return Long.parseLong(raw);
        }

        private void expect(char expected) {
            skipWhitespace();
            if (atEnd() || text.charAt(pos) != expected) {
                throw new IllegalArgumentException("expected '" + expected + "'");
            }
            pos++;
        }

        private boolean peek(char c) {
            return !atEnd() && text.charAt(pos) == c;
        }
    }
}
