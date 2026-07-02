package dev.jdbg.sidecar;

import com.sun.jdi.StringReference;
import com.sun.jdi.Type;

import java.io.IOException;
import java.lang.reflect.Proxy;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.List;
import java.util.Map;

public final class SidecarSelfTest {
    private SidecarSelfTest() {
    }

    public static void main(String[] args) throws Exception {
        testJsonRoundTripKeepsProtocolShape();
        testJsonRejectsMalformedInput();
        testConfigParsesEndpointTokenAndProtocolVersion();
        testConfigParsesUnixDomainSocketTransport();
        testConfigRequiresToken();
        testConfigRequiresEndpoint();
        testConfigRejectsTcpTransport();
        testUnknownRpcMethodHasStableErrorCode();
        testValueRendererAppliesStringLimit();
        testMethodSpecMatchesExactNameAndArguments();
        testLaunchMainArgumentQuoting();
        testSourceCandidatesSearchMavenRoots();
        testSourceCandidatesSearchOneLevelModuleRoots();
        System.out.println("SidecarSelfTest passed");
    }

    private static void testJsonRoundTripKeepsProtocolShape() {
        Object parsed = Json.parse("{\"type\":\"request\",\"id\":\"r1\",\"params\":{\"n\":3,\"items\":[true,\"x\"]}}");
        Map<String, Object> message = Json.asObject(parsed, "message");
        assertEquals("request", message.get("type"), "type");
        Map<String, Object> params = Json.asObject(message.get("params"), "params");
        assertEquals(3L, params.get("n"), "numeric params");
        List<Object> items = Json.asList(params.get("items"), "items");
        assertEquals(Boolean.TRUE, items.get(0), "boolean array item");

        String serialized = Json.stringify(message);
        assertTrue(serialized.contains("\"type\":\"request\""), "serialized request type");
        assertTrue(serialized.contains("\"items\":[true,\"x\"]"), "serialized array");
    }

    private static void testJsonRejectsMalformedInput() {
        assertThrows("trailing JSON content", new ThrowingRunnable() {
            @Override
            public void run() {
                Json.parse("{\"ok\":true} false");
            }
        });
        assertThrows("unterminated string", new ThrowingRunnable() {
            @Override
            public void run() {
                Json.parse("\"abc");
            }
        });
    }

    private static void testConfigParsesEndpointTokenAndProtocolVersion() {
        SidecarMain.Config config = SidecarMain.Config.parse(new String[] {
                "--transport", "named-pipe",
                "--endpoint", "\\\\.\\pipe\\jdbg-jdi-test",
                "--token", "secret-token",
                "--protocol-version", "7"
        });

        assertEquals("named-pipe", config.transport, "transport");
        assertEquals("\\\\.\\pipe\\jdbg-jdi-test", config.endpoint, "endpoint");
        assertEquals("secret-token", config.token, "token");
        assertEquals(7, config.protocolVersion, "protocol version");
    }

    private static void testConfigParsesUnixDomainSocketTransport() {
        SidecarMain.Config config = SidecarMain.Config.parse(new String[] {
                "--transport", "unix-domain-socket",
                "--endpoint", "42",
                "--token", "secret-token"
        });

        assertEquals("unix-domain-socket", config.transport, "transport");
        assertEquals("42", config.endpoint, "endpoint");
    }

    private static void testConfigRequiresToken() {
        assertThrows("--token is required", new ThrowingRunnable() {
            @Override
            public void run() {
                SidecarMain.Config.parse(new String[] {
                        "--transport", "named-pipe",
                        "--endpoint", "\\\\.\\pipe\\jdbg-jdi-test"
                });
            }
        });
        assertThrows("--token is required", new ThrowingRunnable() {
            @Override
            public void run() {
                SidecarMain.Config.parse(new String[] {
                        "--transport", "named-pipe",
                        "--endpoint", "\\\\.\\pipe\\jdbg-jdi-test",
                        "--token", ""
                });
            }
        });
    }

    private static void testConfigRequiresEndpoint() {
        assertThrows("--endpoint is required", new ThrowingRunnable() {
            @Override
            public void run() {
                SidecarMain.Config.parse(new String[] {
                        "--transport", "named-pipe",
                        "--token", "secret-token"
                });
            }
        });
        assertThrows("--transport is required", new ThrowingRunnable() {
            @Override
            public void run() {
                SidecarMain.Config.parse(new String[] {
                        "--endpoint", "\\\\.\\pipe\\jdbg-jdi-test",
                        "--token", "secret-token"
                });
            }
        });
    }

    private static void testConfigRejectsTcpTransport() {
        assertThrows("unsupported --transport", new ThrowingRunnable() {
            @Override
            public void run() {
                SidecarMain.Config.parse(new String[] {
                        "--transport", "tcp",
                        "--endpoint", "4555",
                        "--token", "secret-token"
                });
            }
        });
    }

    private static void testUnknownRpcMethodHasStableErrorCode() throws Exception {
        try {
            new JdiService(null).call("notAMethod", Json.object());
            throw new AssertionError("unknown method should throw RpcException");
        } catch (RpcException e) {
            assertEquals("unknown_method", e.code, "RPC error code");
            assertTrue(e.getMessage().contains("notAMethod"), "RPC error message");
        }
    }

    private static void testValueRendererAppliesStringLimit() {
        Map<String, Object> rendered = ValueRenderer.render(
                stringReference("abcdef"),
                Json.object("maxStringLength", 3)
        );

        assertEquals("string", rendered.get("kind"), "rendered kind");
        assertEquals("java.lang.String", rendered.get("type"), "rendered type");
        assertEquals("abc", rendered.get("value"), "truncated string value");
        assertEquals(Boolean.TRUE, rendered.get("truncated"), "truncated flag");
    }

    private static void testMethodSpecMatchesExactNameAndArguments() {
        JdiService.MethodSpec anyArgs = JdiService.MethodSpec.from("com.example.Main", "work", null, "entry", "all");
        assertTrue(anyArgs.matches("work", "()V"), "missing args should match empty signature");
        assertTrue(anyArgs.matches("work", "(I)V"), "missing args should not filter overloads");

        JdiService.MethodSpec overload = JdiService.MethodSpec.from(
                "com.example.Main",
                "work",
                "int,java.lang.String",
                "both",
                "thread"
        );
        assertTrue(overload.matches("work", "(ILjava/lang/String;)I"), "typed overload should match");
        assertTrue(!overload.matches("work", "(Ljava/lang/String;I)I"), "argument order matters");
        assertEquals("both", overload.eventKind, "event kind");
        assertEquals("thread", overload.suspend, "suspend policy");
    }

    private static void testLaunchMainArgumentQuoting() {
        String main = JdiService.buildLaunchMainArgument(
                "com.example.Main",
                java.util.Arrays.asList("plain", "two words", "quote\"inside", "slash\\inside")
        );
        assertEquals(
                "com.example.Main plain \"two words\" \"quote\\\"inside\" \"slash\\\\inside\"",
                main,
                "launch main argument"
        );
    }

    private static void testSourceCandidatesSearchMavenRoots() {
        Path root = Paths.get("mall-portal");
        List<Path> candidates = JdiService.sourceCandidates(
                java.util.Arrays.asList(root.toString()),
                java.util.Arrays.asList("MallPortalApplication.java"),
                "com.macro.mall.portal.MallPortalApplication"
        );
        Path expected = root
                .resolve("src")
                .resolve("main")
                .resolve("java")
                .resolve("com")
                .resolve("macro")
                .resolve("mall")
                .resolve("portal")
                .resolve("MallPortalApplication.java");
        assertTrue(
                candidates.contains(expected),
                "source candidates should include Maven src/main/java package path"
        );
    }

    private static void testSourceCandidatesSearchOneLevelModuleRoots() throws IOException {
        Path root = Files.createTempDirectory("jdbg-mall-root");
        Path module = root.resolve("mall-portal");
        Path sourceRoot = module.resolve("src").resolve("main").resolve("java");
        Files.createDirectories(sourceRoot);

        List<Path> candidates = JdiService.sourceCandidates(
                java.util.Arrays.asList(root.toString()),
                java.util.Arrays.asList("HomeController.java"),
                "com.macro.mall.portal.controller.HomeController"
        );
        Path expected = sourceRoot
                .resolve("com")
                .resolve("macro")
                .resolve("mall")
                .resolve("portal")
                .resolve("controller")
                .resolve("HomeController.java");
        assertTrue(
                candidates.contains(expected),
                "source candidates should include one-level Maven module source root"
        );
    }

    private static StringReference stringReference(final String value) {
        return (StringReference) Proxy.newProxyInstance(
                SidecarSelfTest.class.getClassLoader(),
                new Class<?>[] {StringReference.class},
                (proxy, method, args) -> {
                    String name = method.getName();
                    if ("value".equals(name)) {
                        return value;
                    }
                    if ("type".equals(name)) {
                        return namedType("java.lang.String");
                    }
                    if ("toString".equals(name)) {
                        return value;
                    }
                    if ("hashCode".equals(name)) {
                        return System.identityHashCode(proxy);
                    }
                    if ("equals".equals(name)) {
                        return proxy == args[0];
                    }
                    throw new UnsupportedOperationException("StringReference." + name);
                }
        );
    }

    private static Type namedType(final String name) {
        return (Type) Proxy.newProxyInstance(
                SidecarSelfTest.class.getClassLoader(),
                new Class<?>[] {Type.class},
                (proxy, method, args) -> {
                    if ("name".equals(method.getName()) || "toString".equals(method.getName())) {
                        return name;
                    }
                    if ("signature".equals(method.getName())) {
                        return "Ljava/lang/String;";
                    }
                    if ("hashCode".equals(method.getName())) {
                        return System.identityHashCode(proxy);
                    }
                    if ("equals".equals(method.getName())) {
                        return proxy == args[0];
                    }
                    throw new UnsupportedOperationException("Type." + method.getName());
                }
        );
    }

    private static void assertEquals(Object expected, Object actual, String label) {
        if (expected == null ? actual != null : !expected.equals(actual)) {
            throw new AssertionError(label + ": expected <" + expected + "> but got <" + actual + ">");
        }
    }

    private static void assertTrue(boolean value, String label) {
        if (!value) {
            throw new AssertionError(label + " should be true");
        }
    }

    private static void assertThrows(String expectedMessagePart, ThrowingRunnable runnable) {
        try {
            runnable.run();
            throw new AssertionError("expected exception containing: " + expectedMessagePart);
        } catch (RuntimeException e) {
            assertTrue(
                    e.getMessage() != null && e.getMessage().contains(expectedMessagePart),
                    "exception message containing " + expectedMessagePart + ", got " + e.getMessage()
            );
        }
    }

    private interface ThrowingRunnable {
        void run();
    }
}
